use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::{self, StreamExt};
use quick_xml::events::Event;
use socket2::{Domain, Protocol, Socket, Type};
use url::Url;

use super::renderer::{
    Controller, Renderer, RendererEndpoints, RendererInfo, Result, URN_AV_TRANSPORT_1,
    URN_LINKPLAY_PLAYQUEUE_1, UpnpError, new_upnp_get_client,
};
use super::xml::{decode_xml_bytes, lenient_reader, local_name, sanitize_xml, unescape_lenient};

const MAX_UPNP_XML_BYTES: usize = 2 << 20;
const SSDP_MULTICAST_V4: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const SSDP_PORT: u16 = 1900;
const MSEARCH_TARGETS: &[&str] = &[
    URN_AV_TRANSPORT_1,
    "urn:schemas-upnp-org:device:MediaRenderer:1",
];

/// Checks whether an IP is an allowed private LAN or Tailscale CGNAT (100.64.0.0/10) IP.
/// Rejects loopback, link-local, multicast, unspecified, and public IPs to prevent SSRF.
pub fn is_allowed_renderer_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_loopback() || v4.is_link_local() || v4.is_multicast() || v4.is_unspecified() {
                return false;
            }
            if v4.is_private() {
                return true;
            }
            // Tailscale / CGNAT 100.64.0.0/10
            let octets = v4.octets();
            octets[0] == 100 && (octets[1] & 0b1100_0000) == 64
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_allowed_renderer_ip(IpAddr::V4(v4));
            }
            if v6.is_loopback() || v6.is_multicast() || v6.is_unspecified() {
                return false;
            }
            // Unique local IPv6 (fc00::/7)
            (v6.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

/// Returns active non-loopback, non-link-local IPv4 addresses across all local interfaces.
/// Used by multi-homed SSDP M-SEARCH, SSDP NOTIFY multicast join, and LAN IP fallback.
pub(crate) fn lan_ipv4_interfaces() -> Vec<Ipv4Addr> {
    let Ok(ifaces) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for iface in ifaces {
        if iface.is_loopback() {
            continue;
        }
        if let IpAddr::V4(v4) = iface.ip()
            && !v4.is_loopback()
            && !v4.is_link_local()
            && !v4.is_unspecified()
            && !v4.is_multicast()
            && seen.insert(v4)
        {
            out.push(v4);
        }
    }
    out
}

/// Validates a renderer URL for scheme + private LAN IP and pins the resolved IP
/// into the returned URL to prevent DNS-rebinding TOCTOU.
pub async fn validate_renderer_url(raw_url: &str) -> Result<Url> {
    let trimmed = raw_url.trim();
    let loc = Url::parse(trimmed).map_err(|_| UpnpError::InvalidRendererUrl(raw_url.into()))?;
    if !matches!(loc.scheme(), "http" | "https") {
        return Err(UpnpError::InvalidRendererUrl(format!(
            "must use http or https scheme, got {:?}",
            loc.scheme()
        )));
    }
    let host = loc
        .host_str()
        .ok_or_else(|| UpnpError::InvalidRendererUrl(format!("missing host in {raw_url:?}")))?;
    let host_clean = host.trim_start_matches('[').trim_end_matches(']');

    let ips: Vec<IpAddr> = if let Ok(ip) = host_clean.parse::<IpAddr>() {
        vec![ip]
    } else {
        let port = loc.port_or_known_default().unwrap_or(80);
        let addrs = tokio::net::lookup_host((host_clean, port))
            .await
            .map_err(|_| {
                UpnpError::InvalidRendererUrl(format!("could not resolve host {host:?}"))
            })?;
        let list: Vec<IpAddr> = addrs.map(|sa| sa.ip()).collect();
        if list.is_empty() {
            return Err(UpnpError::InvalidRendererUrl(format!(
                "could not resolve host {host:?}"
            )));
        }
        list
    };

    for &ip in &ips {
        if !is_allowed_renderer_ip(ip) {
            return Err(UpnpError::InvalidRendererUrl(format!(
                "host {host:?} resolves to non-LAN IP {ip}"
            )));
        }
    }

    let mut pinned = loc.clone();
    let pinned_host = match ips[0] {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    };
    pinned
        .set_host(Some(&pinned_host))
        .map_err(|_| UpnpError::InvalidRendererUrl(raw_url.into()))?;
    Ok(pinned)
}

/// Builds a spec-compliant UPnP DA 1.1 M-SEARCH payload with exact uppercase headers.
pub(crate) fn build_msearch_payload(search_target: &str, mx_secs: u8) -> String {
    format!(
        "M-SEARCH * HTTP/1.1\r\n\
         HOST: 239.255.255.250:1900\r\n\
         MAN: \"ssdp:discover\"\r\n\
         MX: {mx_secs}\r\n\
         ST: {search_target}\r\n\
         \r\n"
    )
}

/// Leniently extracts `LOCATION` from an SSDP HTTP 200 M-SEARCH response.
/// Tolerates LF line endings, HTTP/1.0, and missing SERVER/USN/ST headers.
pub(crate) fn parse_msearch_location(raw: &str) -> Option<String> {
    let mut lines = raw.lines();
    let first = lines.next()?.trim();
    if !first.to_ascii_uppercase().starts_with("HTTP/") || !first.contains(" 200") {
        return None;
    }
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case("location")
        {
            let loc = v.trim();
            if !loc.is_empty() {
                return Some(loc.to_string());
            }
        }
    }
    None
}

async fn msearch_on_interface(
    bind_ip: Ipv4Addr,
    targets: &'static [&'static str],
    deadline: tokio::time::Instant,
) -> Vec<(String, Option<Ipv4Addr>)> {
    let Ok(sock) = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)) else {
        return Vec::new();
    };
    let _ = sock.set_reuse_address(true);
    if sock.bind(&SocketAddrV4::new(bind_ip, 0).into()).is_err() {
        return Vec::new();
    }
    if !bind_ip.is_unspecified() {
        let _ = sock.set_multicast_if_v4(&bind_ip);
    }
    let _ = sock.set_multicast_ttl_v4(2);
    if sock.set_nonblocking(true).is_err() {
        return Vec::new();
    }
    let Ok(udp) = tokio::net::UdpSocket::from_std(sock.into()) else {
        return Vec::new();
    };

    let mcast_dest = SocketAddr::V4(SocketAddrV4::new(SSDP_MULTICAST_V4, SSDP_PORT));
    let payloads: Vec<Vec<u8>> = targets
        .iter()
        .map(|st| build_msearch_payload(st, 2).into_bytes())
        .collect();

    // Send 3 bursts spaced 50ms apart (matches UPnP DA 1.1 §1.3.2 & goupnp numSends=3)
    for burst in 0..3 {
        for p in &payloads {
            let _ = udp.send_to(p, mcast_dest).await;
        }
        if burst < 2 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    let fallback_ip = if bind_ip.is_unspecified() {
        None
    } else {
        Some(bind_ip)
    };
    let mut found = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match tokio::time::timeout_at(deadline, udp.recv(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => {
                let text = String::from_utf8_lossy(&buf[..n]);
                if let Some(loc) = parse_msearch_location(&text) {
                    found.push((loc, fallback_ip));
                }
            }
            Ok(Ok(_)) => continue,
            Ok(Err(_)) | Err(_) => break,
        }
    }
    found
}

/// Sends SSDP M-SEARCH probes for `targets` across all active LAN IPv4 interfaces
/// and returns deduplicated `(location, local_ip)` pairs without performing any HTTP fetches.
pub(crate) async fn msearch_locations(
    targets: &'static [&'static str],
    timeout: Duration,
) -> Vec<(String, Option<Ipv4Addr>)> {
    let mut bind_ips = lan_ipv4_interfaces();
    if bind_ips.is_empty() {
        bind_ips.push(Ipv4Addr::UNSPECIFIED);
    }
    let deadline = tokio::time::Instant::now() + timeout;
    let mut tasks = Vec::with_capacity(bind_ips.len());
    for ip in bind_ips {
        tasks.push(tokio::spawn(msearch_on_interface(ip, targets, deadline)));
    }

    let mut locs: Vec<(String, Option<Ipv4Addr>)> = Vec::new();
    let mut seen_locs = HashSet::new();
    for t in tasks {
        if let Ok(entries) = t.await {
            for (loc, local_ip) in entries {
                if seen_locs.insert(loc.clone()) {
                    locs.push((loc, local_ip));
                }
            }
        }
    }
    locs
}

/// Discovers UPnP/DLNA MediaRenderers across all active LAN interfaces via SSDP M-SEARCH.
pub async fn discover() -> Result<Vec<Arc<Renderer>>> {
    let locs = msearch_locations(MSEARCH_TARGETS, Duration::from_millis(2500)).await;

    let mut renderers = Vec::new();
    let mut seen_ids = HashSet::new();
    let mut futs = stream::iter(locs.into_iter().map(|(loc, local_ip)| async move {
        tokio::time::timeout(
            Duration::from_secs(5),
            connect_by_url_with_local_ip(&loc, local_ip),
        )
        .await
        .ok()
        .and_then(|r| r.ok())
    }))
    .buffer_unordered(8);

    while let Some(opt_r) = futs.next().await {
        if let Some(r) = opt_r
            && seen_ids.insert(r.info().id)
        {
            renderers.push(r);
        }
    }
    Ok(renderers)
}

/// Connects directly to a UPnP MediaRenderer by its device description XML URL.
pub async fn connect_by_url(raw_url: &str) -> Result<Arc<Renderer>> {
    connect_by_url_with_local_ip(raw_url, None).await
}

async fn connect_by_url_with_local_ip(
    raw_url: &str,
    ssdp_local_ip: Option<Ipv4Addr>,
) -> Result<Arc<Renderer>> {
    let loc = validate_renderer_url(raw_url).await?;
    let http = new_upnp_get_client(Duration::from_secs(4));

    let mut resp = http.get(loc.as_str()).send().await?;
    let status = resp.status();
    if !status.is_success() {
        return Err(UpnpError::Other(format!(
            "description GET {} returned HTTP {status}",
            loc.as_str()
        )));
    }
    let bytes = read_bounded_bytes(&mut resp, MAX_UPNP_XML_BYTES).await?;
    let xml = decode_xml_bytes(&bytes);
    let mut parsed = parse_description_xml(&xml, &loc)?;
    if let Some(ref ub) = parsed.raw_url_base
        && let Ok(valid_ub) = validate_renderer_url(ub).await
    {
        parsed.url_base = valid_ub;
    }

    let (endpoints, av_scpd) = resolve_service_endpoints(&parsed, &loc, ssdp_local_ip).await?;
    let (_supports_pause, supports_seek, supports_set_next) = if !av_scpd.is_empty() {
        probe_scpd_capabilities(&av_scpd).await
    } else {
        (true, true, true)
    };

    let host = loc.host_str().unwrap_or("").to_string();
    let id = if parsed.udn.is_empty() {
        loc.to_string()
    } else {
        parsed.udn.clone()
    };
    let name = if parsed.friendly_name.is_empty() {
        host.clone()
    } else {
        parsed.friendly_name.clone()
    };

    let mut info = RendererInfo {
        id,
        kind: "upnp".into(),
        name,
        model: parsed.model_name,
        manufacturer: parsed.manufacturer,
        location: loc.to_string(),
        host,
        has_volume: !endpoints.rc_control_url.is_empty(),
        supports_seek,
        supports_set_next,
        supports_queue: !endpoints.play_queue_control_url.is_empty(),
        supports_events: endpoints.av_event_url.starts_with("http://")
            || endpoints.rc_event_url.starts_with("http://"),
        sink_protocols: Vec::new(),
    };

    let tmp_renderer = Renderer::new_internal(info.clone(), endpoints.clone());
    info.sink_protocols = tmp_renderer.fetch_sink_protocols().await;

    Ok(Arc::new(Renderer::new_internal(info, endpoints)))
}

async fn resolve_service_endpoints(
    parsed: &ParsedDescription,
    loc: &Url,
    ssdp_local_ip: Option<Ipv4Addr>,
) -> Result<(RendererEndpoints, String)> {
    // Resolve AVTransport first; then prefer RenderingControl / ConnectionManager from
    // the SAME <device> element (fixes Sonos & combined MediaServer+MediaRenderer hubs).
    let (av_dev_idx, av_svc) = find_service_in_devices(&parsed.devices, "AVTransport")
        .ok_or_else(|| UpnpError::Other("device has no AVTransport service".into()))?;
    let rc_svc = find_service_preferring_device(&parsed.devices, av_dev_idx, "RenderingControl");
    let cm_svc = find_service_preferring_device(&parsed.devices, av_dev_idx, "ConnectionManager");
    let pq_svc = find_service_exact(&parsed.devices, URN_LINKPLAY_PLAYQUEUE_1);

    let av_control_url = resolve_validated_url(&parsed.url_base, &av_svc.control_url).await?;
    let av_event_url = resolve_optional_url(&parsed.url_base, &av_svc.event_sub_url).await;
    let av_scpd = resolve_optional_url(&parsed.url_base, &av_svc.scpd_url).await;

    let (rc_control_url, rc_event_url, rc_service_urn) = if let Some(s) = rc_svc {
        (
            resolve_optional_url(&parsed.url_base, &s.control_url).await,
            resolve_optional_url(&parsed.url_base, &s.event_sub_url).await,
            s.service_type.clone(),
        )
    } else {
        (String::new(), String::new(), String::new())
    };
    let (cm_control_url, cm_service_urn) = if let Some(s) = cm_svc {
        (
            resolve_optional_url(&parsed.url_base, &s.control_url).await,
            s.service_type.clone(),
        )
    } else {
        (String::new(), String::new())
    };
    let play_queue_control_url = if let Some(s) = pq_svc {
        resolve_optional_url(&parsed.url_base, &s.control_url).await
    } else {
        String::new()
    };

    let host = loc.host_str().unwrap_or("");
    let local_ip = local_ip_for_host_with_fallback(host, ssdp_local_ip);
    Ok((
        RendererEndpoints {
            local_ip,
            av_control_url,
            av_service_urn: av_svc.service_type.clone(),
            rc_control_url,
            rc_service_urn,
            cm_control_url,
            cm_service_urn,
            av_event_url,
            rc_event_url,
            play_queue_control_url,
        },
        av_scpd,
    ))
}

/// Probes whether a previously discovered renderer's description URL is still reachable.
pub async fn probe_location(raw_url: &str) -> bool {
    let Ok(loc) = validate_renderer_url(raw_url).await else {
        return false;
    };
    let http = new_upnp_get_client(Duration::from_secs(2));
    matches!(http.get(loc.as_str()).send().await, Ok(resp) if resp.status().is_success())
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ParsedService {
    pub(crate) service_type: String,
    pub(crate) control_url: String,
    pub(crate) event_sub_url: String,
    pub(crate) scpd_url: String,
}

#[derive(Debug, Default)]
pub(crate) struct ParsedDevice {
    pub(crate) friendly_name: String,
    pub(crate) manufacturer: String,
    pub(crate) model_name: String,
    pub(crate) udn: String,
    pub(crate) services: Vec<ParsedService>,
}

#[derive(Debug)]
pub(crate) struct ParsedDescription {
    pub(crate) url_base: Url,
    pub(crate) raw_url_base: Option<String>,
    pub(crate) friendly_name: String,
    pub(crate) manufacturer: String,
    pub(crate) model_name: String,
    pub(crate) udn: String,
    pub(crate) devices: Vec<ParsedDevice>,
}

/// Leniently parses a UPnP device description XML document while preserving
/// `<device>` boundaries so combined MediaServer + MediaRenderer hubs bind
/// `ConnectionManager` and `RenderingControl` from the `MediaRenderer` sub-device.
pub(crate) fn parse_description_xml(xml: &str, loc: &Url) -> Result<ParsedDescription> {
    let clean = sanitize_xml(xml);
    let mut reader = lenient_reader(&clean);

    let mut raw_url_base: Option<String> = None;
    let mut devices: Vec<ParsedDevice> = Vec::new();
    let mut dev_stack: Vec<usize> = Vec::new();
    let mut cur_service: Option<ParsedService> = None;
    let mut cur_field: Option<String> = None;
    let mut cur_text = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let tag = local_name(e.name().as_ref());
                if tag.eq_ignore_ascii_case("device") {
                    let idx = devices.len();
                    devices.push(ParsedDevice::default());
                    dev_stack.push(idx);
                } else if tag.eq_ignore_ascii_case("service") && !dev_stack.is_empty() {
                    cur_service = Some(ParsedService::default());
                } else {
                    cur_field = Some(tag);
                    cur_text.clear();
                }
            }
            Ok(Event::Text(t)) if cur_field.is_some() => {
                if let Ok(raw) = std::str::from_utf8(t.as_ref()) {
                    cur_text.push_str(&unescape_lenient(raw));
                }
            }
            Ok(Event::CData(c)) if cur_field.is_some() => {
                if let Ok(raw) = std::str::from_utf8(c.as_ref()) {
                    cur_text.push_str(raw);
                }
            }
            Ok(Event::End(e)) => {
                let tag = local_name(e.name().as_ref());
                if tag.eq_ignore_ascii_case("device") {
                    dev_stack.pop();
                } else if tag.eq_ignore_ascii_case("service") {
                    if let (Some(svc), Some(&dev_idx)) = (cur_service.take(), dev_stack.last()) {
                        devices[dev_idx].services.push(svc);
                    }
                } else if let Some(field) = cur_field.take() {
                    let val = cur_text.trim().to_string();
                    if !val.is_empty() {
                        if field.eq_ignore_ascii_case("URLBase") && dev_stack.is_empty() {
                            raw_url_base = Some(val);
                        } else if let Some(svc) = cur_service.as_mut() {
                            if field.eq_ignore_ascii_case("serviceType") {
                                svc.service_type = val;
                            } else if field.eq_ignore_ascii_case("controlURL") {
                                svc.control_url = val;
                            } else if field.eq_ignore_ascii_case("eventSubURL") {
                                svc.event_sub_url = val;
                            } else if field.eq_ignore_ascii_case("SCPDURL") {
                                svc.scpd_url = val;
                            }
                        } else if let Some(&dev_idx) = dev_stack.last() {
                            let dev = &mut devices[dev_idx];
                            if field.eq_ignore_ascii_case("friendlyName")
                                && dev.friendly_name.is_empty()
                            {
                                dev.friendly_name = val;
                            } else if field.eq_ignore_ascii_case("manufacturer")
                                && dev.manufacturer.is_empty()
                            {
                                dev.manufacturer = val;
                            } else if field.eq_ignore_ascii_case("modelName")
                                && dev.model_name.is_empty()
                            {
                                dev.model_name = val;
                            } else if field.eq_ignore_ascii_case("UDN") && dev.udn.is_empty() {
                                dev.udn = val;
                            }
                        }
                    }
                    cur_text.clear();
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    if devices.is_empty() {
        return Err(UpnpError::Other(
            "missing <device> in description XML".into(),
        ));
    }
    let root = &devices[0];
    Ok(ParsedDescription {
        url_base: loc.clone(),
        raw_url_base,
        friendly_name: root.friendly_name.clone(),
        manufacturer: root.manufacturer.clone(),
        model_name: root.model_name.clone(),
        udn: root.udn.clone(),
        devices,
    })
}

/// Matches `urn:schemas-upnp-org:service:{family}:{ver}` (preferring `:1`, then `:2`, `:3`).
fn service_family_rank(service_type: &str, family: &str) -> Option<u8> {
    let prefix = format!("urn:schemas-upnp-org:service:{family}:");
    let low = service_type.trim().to_ascii_lowercase();
    let pref_low = prefix.to_ascii_lowercase();
    let ver = low.strip_prefix(&pref_low)?;
    match ver {
        "1" => Some(0),
        "2" => Some(1),
        "3" => Some(2),
        _ => None,
    }
}

pub(crate) fn find_service_in_devices<'a>(
    devices: &'a [ParsedDevice],
    family: &str,
) -> Option<(usize, &'a ParsedService)> {
    let mut best: Option<(u8, usize, &'a ParsedService)> = None;
    for (idx, dev) in devices.iter().enumerate() {
        for svc in &dev.services {
            if let Some(rank) = service_family_rank(&svc.service_type, family)
                && best.is_none_or(|(br, _, _)| rank < br)
            {
                best = Some((rank, idx, svc));
            }
        }
    }
    best.map(|(_, idx, svc)| (idx, svc))
}

pub(crate) fn find_service_preferring_device<'a>(
    devices: &'a [ParsedDevice],
    preferred_dev_idx: usize,
    family: &str,
) -> Option<&'a ParsedService> {
    if let Some(dev) = devices.get(preferred_dev_idx) {
        let mut best: Option<(u8, &'a ParsedService)> = None;
        for svc in &dev.services {
            if let Some(rank) = service_family_rank(&svc.service_type, family)
                && best.is_none_or(|(br, _)| rank < br)
            {
                best = Some((rank, svc));
            }
        }
        if let Some((_, svc)) = best {
            return Some(svc);
        }
    }
    find_service_in_devices(devices, family).map(|(_, s)| s)
}

fn find_service_exact<'a>(devices: &'a [ParsedDevice], urn: &str) -> Option<&'a ParsedService> {
    devices.iter().find_map(|d| {
        d.services
            .iter()
            .find(|s| s.service_type.eq_ignore_ascii_case(urn))
    })
}

async fn resolve_validated_url(base: &Url, rel: &str) -> Result<String> {
    let trimmed = rel.trim();
    if trimmed.is_empty() {
        return Err(UpnpError::Other(
            "missing required service controlURL".into(),
        ));
    }
    let joined = base
        .join(trimmed)
        .map_err(|_| UpnpError::InvalidRendererUrl(rel.into()))?;
    let validated = validate_renderer_url(joined.as_str()).await?;
    Ok(validated.to_string())
}

async fn resolve_optional_url(base: &Url, rel: &str) -> String {
    let r = rel.trim();
    if r.is_empty() {
        return String::new();
    }
    resolve_validated_url(base, r).await.unwrap_or_default()
}

async fn read_bounded_bytes(resp: &mut reqwest::Response, max_bytes: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if out.len().saturating_add(chunk.len()) > max_bytes {
            return Err(UpnpError::Other(format!(
                "UPnP XML response exceeds {max_bytes} bytes"
            )));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

async fn probe_scpd_capabilities(scpd_url: &str) -> (bool, bool, bool) {
    let http = new_upnp_get_client(Duration::from_secs(2));
    let Ok(mut resp) = http.get(scpd_url).send().await else {
        return (true, true, true);
    };
    if !resp.status().is_success() {
        return (true, true, true);
    }
    let Ok(bytes) = read_bounded_bytes(&mut resp, MAX_UPNP_XML_BYTES).await else {
        return (true, true, true);
    };
    let scpd_xml = sanitize_xml(&decode_xml_bytes(&bytes));
    let mut reader = lenient_reader(&scpd_xml);
    let mut in_action = false;
    let mut in_name = false;
    let mut name_buf = String::new();
    let mut actions: HashMap<String, bool> = HashMap::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let tag = local_name(e.name().as_ref());
                if tag.eq_ignore_ascii_case("action") {
                    in_action = true;
                } else if in_action && tag.eq_ignore_ascii_case("name") {
                    in_name = true;
                    name_buf.clear();
                }
            }
            Ok(Event::Text(t)) if in_name => {
                if let Ok(raw) = std::str::from_utf8(t.as_ref()) {
                    name_buf.push_str(raw);
                }
            }
            Ok(Event::End(e)) => {
                let tag = local_name(e.name().as_ref());
                if tag.eq_ignore_ascii_case("action") {
                    in_action = false;
                    in_name = false;
                } else if in_name && tag.eq_ignore_ascii_case("name") {
                    in_name = false;
                    let n = name_buf.trim().to_ascii_lowercase();
                    if !n.is_empty() {
                        actions.insert(n, true);
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }

    if actions.is_empty() {
        return (true, true, true);
    }
    (
        actions.contains_key("pause"),
        actions.contains_key("seek"),
        actions.contains_key("setnextavtransporturi"),
    )
}

/// Determines the local interface IP used to reach `host` (for renderer media & GENA callbacks).
pub fn local_ip_for_host(host: &str) -> String {
    local_ip_for_host_with_fallback(host, None)
}

fn local_ip_for_host_with_fallback(host: &str, ssdp_fallback: Option<Ipv4Addr>) -> String {
    let clean = host.trim().trim_matches(['[', ']']);
    let target: Option<SocketAddr> = if let Ok(ip) = clean.parse::<IpAddr>() {
        Some(SocketAddr::new(ip, 80))
    } else if !clean.is_empty() {
        std::net::ToSocketAddrs::to_socket_addrs(&(clean, 80))
            .ok()
            .and_then(|mut it| it.next())
    } else {
        None
    };

    if let Some(dest) = target {
        let bind_addr = match dest {
            SocketAddr::V4(_) => SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
            SocketAddr::V6(_) => SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0)),
        };
        if let Ok(sock) = UdpSocket::bind(bind_addr)
            && sock.connect(dest).is_ok()
            && let Ok(local) = sock.local_addr()
            && !local.ip().is_loopback()
            && !local.ip().is_unspecified()
        {
            return local.ip().to_string();
        }
    }
    if let Some(ip) = ssdp_fallback {
        return ip.to_string();
    }
    if let Some(first_lan) = lan_ipv4_interfaces().into_iter().next() {
        return first_lan.to_string();
    }
    "127.0.0.1".into()
}
