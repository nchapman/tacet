use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use futures_util::TryStreamExt;
use url::Url;

use super::renderer::{
    Controller, Renderer, RendererInfo, Result, URN_AV_TRANSPORT_1, URN_CONNECTION_MANAGER_1,
    URN_LINKPLAY_PLAYQUEUE_1, URN_RENDERING_CONTROL_1, UpnpError, new_soap_http_client,
};

const MAX_UPNP_XML_BYTES: usize = 2 << 20;

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

/// Discovers UPnP/DLNA MediaRenderers on the LAN via SSDP M-SEARCH.
pub async fn discover() -> Result<Vec<Arc<Renderer>>> {
    let st = ssdp_client::SearchTarget::URN(ssdp_client::URN::service(
        "schemas-upnp-org",
        "AVTransport",
        1,
    ));
    let responses = ssdp_client::search(&st, Duration::from_secs(3), 2, None)
        .await
        .map_err(|e| UpnpError::Other(format!("SSDP discovery failed: {e}")))?;

    let mut locs: Vec<String> = Vec::new();
    let mut stream = responses;
    while let Ok(Some(resp)) = stream.try_next().await {
        let loc = resp.location().trim().to_string();
        if !loc.is_empty() && !locs.contains(&loc) {
            locs.push(loc);
        }
    }

    let mut by_id: HashMap<String, Arc<Renderer>> = HashMap::new();
    for loc in locs {
        if let Ok(r) = connect_by_url(&loc).await {
            by_id.insert(r.info().id, r);
        }
    }

    let mut out: Vec<Arc<Renderer>> = by_id.into_values().collect();
    out.sort_by(|a, b| {
        a.info()
            .name
            .to_lowercase()
            .cmp(&b.info().name.to_lowercase())
    });
    Ok(out)
}

/// Connects directly to a renderer's UPnP device description XML URL.
pub async fn connect_by_url(raw_url: &str) -> Result<Arc<Renderer>> {
    let loc = validate_renderer_url(raw_url).await?;
    let xml = fetch_xml_text(&loc).await?;
    build_renderer_from_xml(&xml, &loc).await
}

async fn fetch_xml_text(target: &Url) -> Result<String> {
    let client = new_soap_http_client(Duration::from_secs(5));
    let resp = client
        .get(target.as_str())
        .header("Connection", "close")
        .send()
        .await?;
    let status = resp.status().as_u16();
    if status != 200 {
        return Err(UpnpError::HttpStatus {
            status,
            body: format!("GET {target} returned {status}"),
        });
    }
    let bytes = resp.bytes().await?;
    if bytes.len() > MAX_UPNP_XML_BYTES {
        return Err(UpnpError::Other("UPnP XML exceeds 2 MiB limit".into()));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

struct ParsedService {
    service_type: String,
    control_url: String,
    event_sub_url: String,
    scpd_url: String,
}

async fn build_renderer_from_xml(xml: &str, loc: &Url) -> Result<Arc<Renderer>> {
    let doc = roxmltree::Document::parse(xml)
        .map_err(|e| UpnpError::Other(format!("parse UPnP description XML: {e}")))?;

    let mut url_base = loc.clone();
    if let Some(base_node) = doc
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name().eq_ignore_ascii_case("URLBase"))
        && let Some(base_txt) = base_node.text().map(str::trim).filter(|s| !s.is_empty())
        && let Ok(parsed) = Url::parse(base_txt)
    {
        url_base = parsed;
    }

    let device_node = doc
        .descendants()
        .find(|n| n.is_element() && n.tag_name().name().eq_ignore_ascii_case("device"))
        .ok_or_else(|| UpnpError::Other("no <device> in UPnP description XML".into()))?;

    let child_text = |parent: roxmltree::Node, name: &str| -> String {
        parent
            .children()
            .find(|n| n.is_element() && n.tag_name().name().eq_ignore_ascii_case(name))
            .and_then(|n| n.text())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };

    let friendly_name = child_text(device_node, "friendlyName");
    let manufacturer = child_text(device_node, "manufacturer");
    let model = child_text(device_node, "modelName");
    let udn = child_text(device_node, "UDN");

    let mut services = Vec::new();
    for node in doc.descendants() {
        if node.is_element() && node.tag_name().name().eq_ignore_ascii_case("service") {
            let st = child_text(node, "serviceType");
            let ctrl = child_text(node, "controlURL");
            let ev = child_text(node, "eventSubURL");
            let scpd = child_text(node, "SCPDURL");
            if !st.is_empty() {
                services.push(ParsedService {
                    service_type: st,
                    control_url: ctrl,
                    event_sub_url: ev,
                    scpd_url: scpd,
                });
            }
        }
    }

    let resolve_safe = |rel: &str| -> Option<String> {
        let rel = rel.trim();
        if rel.is_empty() {
            return None;
        }
        url_base.join(rel).ok().map(|u| u.to_string())
    };

    let av_svc = services
        .iter()
        .find(|s| s.service_type == URN_AV_TRANSPORT_1)
        .ok_or_else(|| {
            UpnpError::Other(format!(
                "device {friendly_name:?} has no AVTransport:1 service"
            ))
        })?;
    let av_control_raw = resolve_safe(&av_svc.control_url)
        .ok_or_else(|| UpnpError::Other("missing AVTransport controlURL".into()))?;
    let av_control = validate_renderer_url(&av_control_raw).await?.to_string();

    let av_event = match resolve_safe(&av_svc.event_sub_url) {
        Some(u) => validate_renderer_url(&u)
            .await
            .map(|x| x.to_string())
            .unwrap_or_default(),
        None => String::new(),
    };

    let rc_svc = services
        .iter()
        .find(|s| s.service_type == URN_RENDERING_CONTROL_1);
    let rc_control = match rc_svc.and_then(|s| resolve_safe(&s.control_url)) {
        Some(u) => validate_renderer_url(&u)
            .await
            .map(|x| x.to_string())
            .unwrap_or_default(),
        None => String::new(),
    };
    let rc_event = match rc_svc.and_then(|s| resolve_safe(&s.event_sub_url)) {
        Some(u) => validate_renderer_url(&u)
            .await
            .map(|x| x.to_string())
            .unwrap_or_default(),
        None => String::new(),
    };

    let cm_control = match services
        .iter()
        .find(|s| s.service_type == URN_CONNECTION_MANAGER_1)
        .and_then(|s| resolve_safe(&s.control_url))
    {
        Some(u) => validate_renderer_url(&u)
            .await
            .map(|x| x.to_string())
            .unwrap_or_default(),
        None => String::new(),
    };

    let pq_control = match services
        .iter()
        .find(|s| s.service_type == URN_LINKPLAY_PLAYQUEUE_1)
        .and_then(|s| resolve_safe(&s.control_url))
    {
        Some(u) => validate_renderer_url(&u)
            .await
            .map(|x| x.to_string())
            .unwrap_or_default(),
        None => String::new(),
    };

    // Probe SCPD for optional SetNextAVTransportURI & Seek actions
    let mut supports_set_next = true;
    let mut supports_seek = true;
    if let Some(scpd_raw) = resolve_safe(&av_svc.scpd_url)
        && let Ok(scpd_url) = validate_renderer_url(&scpd_raw).await
        && let Ok(scpd_xml) = fetch_xml_text(&scpd_url).await
        && let Ok(sdoc) = roxmltree::Document::parse(&scpd_xml)
    {
        let actions: Vec<String> = sdoc
            .descendants()
            .filter(|n| n.is_element() && n.tag_name().name().eq_ignore_ascii_case("action"))
            .filter_map(|a| {
                a.children()
                    .find(|c| c.is_element() && c.tag_name().name().eq_ignore_ascii_case("name"))
                    .and_then(|c| c.text())
                    .map(|s| s.trim().to_string())
            })
            .collect();
        if !actions.is_empty() {
            supports_set_next = actions.iter().any(|a| a == "SetNextAVTransportURI");
            supports_seek = actions.iter().any(|a| a == "Seek");
        }
    }

    let id = if !udn.is_empty() {
        udn
    } else {
        loc.to_string()
    };
    let host = loc.host_str().unwrap_or("").to_string();
    let local_ip = local_ip_for_host(&host);

    let info = RendererInfo {
        id,
        kind: "upnp".into(),
        name: if friendly_name.is_empty() {
            host.clone()
        } else {
            friendly_name
        },
        manufacturer,
        model,
        location: loc.to_string(),
        host,
        has_volume: !rc_control.is_empty(),
        supports_set_next,
        supports_seek,
        supports_events: !av_event.is_empty() || !rc_event.is_empty(),
        supports_queue: false,
        sink_protocols: Vec::new(),
    };

    let renderer = Renderer::new_internal(
        info.clone(),
        local_ip.clone(),
        av_control.clone(),
        rc_control.clone(),
        cm_control.clone(),
        av_event.clone(),
        rc_event.clone(),
        pq_control.clone(),
    );
    let sink_protocols = renderer.fetch_sink_protocols().await;
    if sink_protocols.is_empty() {
        return Ok(Arc::new(renderer));
    }
    let mut final_info = info;
    final_info.sink_protocols = sink_protocols;
    Ok(Arc::new(Renderer::new_internal(
        final_info, local_ip, av_control, rc_control, cm_control, av_event, rc_event, pq_control,
    )))
}

/// Determines the local outbound IPv4 address used to reach `host`.
pub fn local_ip_for_host(host: &str) -> String {
    let target = format!("{host}:80");
    if let Ok(sock) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        && sock.connect(&target).is_ok()
        && let Ok(SocketAddr::V4(v4)) = sock.local_addr()
    {
        return v4.ip().to_string();
    }
    "127.0.0.1".into()
}

/// Fast HTTP GET liveness probe for a UPnP description URL.
pub async fn probe_location(raw_url: &str) -> bool {
    let Ok(loc) = validate_renderer_url(raw_url).await else {
        return false;
    };
    let client = new_soap_http_client(Duration::from_secs(2));
    match client
        .get(loc.as_str())
        .header("Connection", "close")
        .send()
        .await
    {
        Ok(resp) => resp.status().as_u16() == 200,
        Err(_) => false,
    }
}
