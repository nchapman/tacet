use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode};
use rand::RngCore;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::didl::parse_upnp_duration;

pub const SERVICE_AV_TRANSPORT: &str = "AVTransport";
pub const SERVICE_RENDERING_CONTROL: &str = "RenderingControl";

const DEFAULT_GENA_TIMEOUT_SEC: u64 = 300;
const MAX_GENA_BODY_BYTES: usize = 512 << 10;

#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceEvent {
    pub renderer_id: String,
    pub service: String,
    pub seq: u32,
    pub has_state: bool,
    pub state: String,
    pub has_position: bool,
    pub position: Duration,
    pub has_track_uri: bool,
    pub track_uri: String,
    pub has_duration: bool,
    pub duration: Duration,
    pub has_volume: bool,
    pub volume: i32,
    pub has_mute: bool,
    pub muted: bool,
}

impl DeviceEvent {
    pub fn has_any_update(&self) -> bool {
        self.has_state
            || self.has_position
            || self.has_track_uri
            || self.has_duration
            || self.has_volume
            || self.has_mute
    }
}

pub fn parse_notify_body(service: &str, body: &[u8]) -> DeviceEvent {
    let mut ev = DeviceEvent {
        service: service.into(),
        ..Default::default()
    };
    let text = String::from_utf8_lossy(body);
    let Ok(doc) = roxmltree::Document::parse(&text) else {
        return ev;
    };

    let mut last_change = String::new();
    for node in doc.descendants() {
        if !node.is_element() {
            continue;
        }
        match node.tag_name().name() {
            "LastChange" => {
                if let Some(t) = node.text() {
                    last_change = t.trim().to_string();
                }
            }
            "TransportState" => {
                if let Some(t) = node.text().map(str::trim).filter(|s| !s.is_empty()) {
                    ev.has_state = true;
                    ev.state = t.to_string();
                }
            }
            "CurrentTrackURI" => {
                if let Some(t) = node.text().map(str::trim).filter(|s| !s.is_empty()) {
                    ev.has_track_uri = true;
                    ev.track_uri = t.to_string();
                }
            }
            "AVTransportURI" => {
                if !ev.has_track_uri
                    && let Some(t) = node.text().map(str::trim).filter(|s| !s.is_empty())
                {
                    ev.has_track_uri = true;
                    ev.track_uri = t.to_string();
                }
            }
            _ => {}
        }
    }

    if !last_change.is_empty() {
        parse_last_change_xml(&last_change, &mut ev);
    }
    ev
}

fn parse_last_change_xml(raw: &str, ev: &mut DeviceEvent) {
    let Ok(doc) = roxmltree::Document::parse(raw) else {
        return;
    };
    let mut in_instance_zero = true;
    for node in doc.descendants() {
        if !node.is_element() {
            continue;
        }
        let attr = |name: &str| -> Option<&str> {
            node.attributes()
                .find(|a| a.name().eq_ignore_ascii_case(name))
                .map(|a| a.value().trim())
        };
        match node.tag_name().name() {
            "InstanceID" => {
                let val = attr("val").unwrap_or("");
                in_instance_zero = val.is_empty() || val == "0";
            }
            "TransportState" if in_instance_zero => {
                if let Some(v) = attr("val").filter(|s| !s.is_empty()) {
                    ev.has_state = true;
                    ev.state = v.to_string();
                }
            }
            "CurrentTrackURI" if in_instance_zero => {
                if let Some(v) = attr("val").filter(|s| !s.is_empty()) {
                    ev.has_track_uri = true;
                    ev.track_uri = v.to_string();
                }
            }
            "AVTransportURI" if in_instance_zero => {
                if !ev.has_track_uri
                    && let Some(v) = attr("val").filter(|s| !s.is_empty())
                {
                    ev.has_track_uri = true;
                    ev.track_uri = v.to_string();
                }
            }
            "CurrentTrackDuration" if in_instance_zero => {
                if let Some(v) = attr("val").filter(|s| !s.is_empty()) {
                    let d = parse_upnp_duration(v);
                    if !d.is_zero() {
                        ev.has_duration = true;
                        ev.duration = d;
                    }
                }
            }
            "Volume" if in_instance_zero => {
                let ch = attr("channel").unwrap_or("");
                if !ch.is_empty() && !ch.eq_ignore_ascii_case("Master") {
                    continue;
                }
                if let Some(v) = attr("val").and_then(|s| s.parse::<i32>().ok()) {
                    ev.has_volume = true;
                    ev.volume = v.clamp(0, 100);
                }
            }
            "Mute" if in_instance_zero => {
                let ch = attr("channel").unwrap_or("");
                if !ch.is_empty() && !ch.eq_ignore_ascii_case("Master") {
                    continue;
                }
                if let Some(v) = attr("val").filter(|s| !s.is_empty()) {
                    ev.has_mute = true;
                    ev.muted = v == "1" || v.eq_ignore_ascii_case("true");
                }
            }
            _ => {}
        }
    }
}

struct GenaSub {
    token: String,
    renderer_id: String,
    service: String,
    event_sub_url: String,
    callback_base_url: String,
    cancel: CancellationToken,
    state: RwLock<(String, bool)>, // (sid, active)
}

struct GenaInner {
    by_token: HashMap<String, Arc<GenaSub>>,
    by_renderer: HashMap<String, Vec<Arc<GenaSub>>>,
    closed: bool,
}

pub struct GenaClient {
    inner: RwLock<GenaInner>,
    event_tx: mpsc::Sender<DeviceEvent>,
    stop: CancellationToken,
}

impl GenaClient {
    pub fn new<F>(on_event: F) -> Arc<Self>
    where
        F: Fn(DeviceEvent) + Send + Sync + 'static,
    {
        let (event_tx, mut event_rx) = mpsc::channel::<DeviceEvent>(128);
        let stop = CancellationToken::new();
        let stop_loop = stop.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stop_loop.cancelled() => break,
                    Some(ev) = event_rx.recv() => on_event(ev),
                }
            }
        });
        Arc::new(Self {
            inner: RwLock::new(GenaInner {
                by_token: HashMap::new(),
                by_renderer: HashMap::new(),
                closed: false,
            }),
            event_tx,
            stop,
        })
    }

    pub fn is_active(&self, renderer_id: &str) -> bool {
        let g = self.inner.read().unwrap_or_else(|e| e.into_inner());
        g.by_renderer.get(renderer_id).is_some_and(|subs| {
            subs.iter()
                .any(|s| s.state.read().unwrap_or_else(|e| e.into_inner()).1)
        })
    }

    pub fn watch_renderer(
        self: &Arc<Self>,
        renderer_id: &str,
        callback_base_url: &str,
        av_event_url: &str,
        rc_event_url: &str,
    ) {
        let cb_base = callback_base_url.trim().trim_end_matches('/').to_string();
        if renderer_id.is_empty() || cb_base.is_empty() {
            return;
        }

        let old_subs = {
            let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
            if g.closed {
                return;
            }
            if let Some(existing) = g.by_renderer.get(renderer_id)
                && same_endpoints(existing, &cb_base, av_event_url, rc_event_url)
            {
                return;
            }
            let old = g.by_renderer.remove(renderer_id).unwrap_or_default();
            for s in &old {
                g.by_token.remove(&s.token);
            }
            old
        };

        for old in old_subs {
            old.cancel.cancel();
            tokio::spawn(async move {
                unsubscribe_sub(&old).await;
            });
        }

        let mut subs = Vec::new();
        if !av_event_url.is_empty() {
            subs.push(new_sub(
                renderer_id,
                SERVICE_AV_TRANSPORT,
                av_event_url,
                &cb_base,
            ));
        }
        if !rc_event_url.is_empty() {
            subs.push(new_sub(
                renderer_id,
                SERVICE_RENDERING_CONTROL,
                rc_event_url,
                &cb_base,
            ));
        }
        if subs.is_empty() {
            return;
        }

        {
            let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
            if g.closed {
                return;
            }
            for s in &subs {
                g.by_token.insert(s.token.clone(), Arc::clone(s));
            }
            g.by_renderer.insert(renderer_id.to_string(), subs.clone());
        }

        for sub in subs {
            tokio::spawn(sub_lifecycle(sub));
        }
    }

    pub fn unwatch_renderer(&self, renderer_id: &str) {
        let old = {
            let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
            let old = g.by_renderer.remove(renderer_id).unwrap_or_default();
            for s in &old {
                g.by_token.remove(&s.token);
            }
            old
        };
        for s in old {
            s.cancel.cancel();
            tokio::spawn(async move {
                unsubscribe_sub(&s).await;
            });
        }
    }

    pub fn close(&self) {
        let all = {
            let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
            g.closed = true;
            let subs: Vec<Arc<GenaSub>> = g.by_token.values().cloned().collect();
            g.by_token.clear();
            g.by_renderer.clear();
            subs
        };
        self.stop.cancel();
        for s in all {
            s.cancel.cancel();
        }
    }

    /// Handles incoming HTTP NOTIFY requests mounted at `/upnp/event/{token}`.
    pub async fn handle_notify(
        &self,
        method: Method,
        token: &str,
        headers: &HeaderMap,
        body: Bytes,
    ) -> StatusCode {
        if method.as_str() != "NOTIFY" {
            return StatusCode::METHOD_NOT_ALLOWED;
        }
        if body.len() > MAX_GENA_BODY_BYTES {
            return StatusCode::PAYLOAD_TOO_LARGE;
        }
        let sub = {
            let g = self.inner.read().unwrap_or_else(|e| e.into_inner());
            g.by_token.get(token).cloned()
        };
        let Some(sub) = sub else {
            return StatusCode::PRECONDITION_FAILED;
        };

        // UPnP DA 1.1 §4.2: validate SID against the active subscription when known.
        let active_sid = sub
            .state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .0
            .clone();
        if !active_sid.is_empty() {
            let hdr_sid = headers
                .get("sid")
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .unwrap_or("");
            if hdr_sid != active_sid {
                return StatusCode::PRECONDITION_FAILED;
            }
        }

        let mut ev = parse_notify_body(&sub.service, &body);
        ev.renderer_id = sub.renderer_id.clone();
        if let Some(seq_hdr) = headers.get("seq").and_then(|v| v.to_str().ok())
            && let Ok(seq) = seq_hdr.trim().parse::<u32>()
        {
            ev.seq = seq;
        }
        if ev.has_any_update() {
            let _ = self.event_tx.try_send(ev);
        }
        StatusCode::OK
    }
}

fn new_sub(renderer_id: &str, service: &str, event_sub_url: &str, cb_base: &str) -> Arc<GenaSub> {
    let mut rand_bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut rand_bytes);
    Arc::new(GenaSub {
        token: hex::encode(rand_bytes),
        renderer_id: renderer_id.into(),
        service: service.into(),
        event_sub_url: event_sub_url.into(),
        callback_base_url: cb_base.into(),
        cancel: CancellationToken::new(),
        state: RwLock::new((String::new(), false)),
    })
}

fn same_endpoints(existing: &[Arc<GenaSub>], cb_base: &str, av_url: &str, rc_url: &str) -> bool {
    let want_count = usize::from(!av_url.is_empty()) + usize::from(!rc_url.is_empty());
    if existing.len() != want_count {
        return false;
    }
    for s in existing {
        if s.callback_base_url != cb_base {
            return false;
        }
        match s.service.as_str() {
            SERVICE_AV_TRANSPORT if s.event_sub_url != av_url => return false,
            SERVICE_RENDERING_CONTROL if s.event_sub_url != rc_url => return false,
            _ => {}
        }
    }
    true
}

async fn sub_lifecycle(sub: Arc<GenaSub>) {
    let mut backoff = Duration::from_secs(5);
    loop {
        if sub.cancel.is_cancelled() {
            return;
        }
        let callback_url = format!("{}/upnp/event/{}", sub.callback_base_url, sub.token);
        match raw_gena_subscribe(&sub.event_sub_url, &callback_url, None).await {
            Ok((mut sid, timeout)) => {
                if sub.cancel.is_cancelled() {
                    let _ = raw_gena_unsubscribe(&sub.event_sub_url, &sid).await;
                    return;
                }
                {
                    let mut st = sub.state.write().unwrap_or_else(|e| e.into_inner());
                    *st = (sid.clone(), true);
                }
                backoff = Duration::from_secs(5);
                let mut cur_timeout = timeout;
                loop {
                    let renew_in = cur_timeout.mul_f64(0.75).max(Duration::from_secs(20));
                    tokio::select! {
                        _ = sub.cancel.cancelled() => {
                            let _ = raw_gena_unsubscribe(&sub.event_sub_url, &sid).await;
                            return;
                        }
                        _ = tokio::time::sleep(renew_in) => {}
                    }
                    match raw_gena_subscribe(&sub.event_sub_url, "", Some(&sid)).await {
                        Ok((next_sid, next_to)) => {
                            if !next_sid.is_empty() && next_sid != sid {
                                sid = next_sid;
                                let mut st = sub.state.write().unwrap_or_else(|e| e.into_inner());
                                st.0 = sid.clone();
                            }
                            cur_timeout = next_to;
                        }
                        Err(_) => {
                            let mut st = sub.state.write().unwrap_or_else(|e| e.into_inner());
                            st.0.clear();
                            st.1 = false;
                            break;
                        }
                    }
                }
            }
            Err(_) => {
                {
                    let mut st = sub.state.write().unwrap_or_else(|e| e.into_inner());
                    st.1 = false;
                }
                tokio::select! {
                    _ = sub.cancel.cancelled() => return,
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}

async fn unsubscribe_sub(sub: &GenaSub) {
    let sid = sub
        .state
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .0
        .clone();
    if sid.is_empty() {
        return;
    }
    let _ = raw_gena_unsubscribe(&sub.event_sub_url, &sid).await;
}

/// Sends a raw HTTP/1.1 GENA `SUBSCRIBE` request with exact uppercase UPnP headers
/// (`CALLBACK`, `NT`, `TIMEOUT`, `SID`) because embedded libupnp servers (e.g. WiiM)
/// can reject lowercase or canonicalized header names.
async fn raw_gena_subscribe(
    event_sub_url: &str,
    callback_url: &str,
    renew_sid: Option<&str>,
) -> std::result::Result<(String, Duration), String> {
    let u = Url::parse(event_sub_url).map_err(|e| e.to_string())?;
    let host = u.host_str().ok_or("missing host")?;
    let port = u.port_or_known_default().unwrap_or(80);
    let path = if let Some(q) = u.query() {
        format!("{}?{q}", u.path())
    } else {
        u.path().to_string()
    };

    let mut req = format!("SUBSCRIBE {path} HTTP/1.1\r\nHOST: {host}:{port}\r\n");
    if let Some(sid) = renew_sid {
        req.push_str(&format!("SID: {sid}\r\n"));
    } else {
        req.push_str(&format!("CALLBACK: <{callback_url}>\r\n"));
        req.push_str("NT: upnp:event\r\n");
    }
    req.push_str(&format!("TIMEOUT: Second-{DEFAULT_GENA_TIMEOUT_SEC}\r\n"));
    req.push_str("CONNECTION: close\r\n\r\n");

    let resp_text = send_raw_http(host, port, req.as_bytes()).await?;
    let mut lines = resp_text.lines();
    let status_line = lines.next().unwrap_or("");
    if !status_line.contains(" 200 ") {
        return Err(format!("GENA SUBSCRIBE failed: {status_line}"));
    }

    let mut sid = renew_sid.unwrap_or("").to_string();
    let mut timeout = Duration::from_secs(DEFAULT_GENA_TIMEOUT_SEC);
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("sid") {
                sid = v.trim().to_string();
            } else if k.trim().eq_ignore_ascii_case("timeout") {
                timeout = parse_gena_timeout(v);
            }
        }
    }
    if sid.is_empty() {
        return Err("GENA SUBSCRIBE response missing SID".into());
    }
    Ok((sid, timeout))
}

async fn raw_gena_unsubscribe(event_sub_url: &str, sid: &str) -> std::result::Result<(), String> {
    let u = Url::parse(event_sub_url).map_err(|e| e.to_string())?;
    let host = u.host_str().ok_or("missing host")?;
    let port = u.port_or_known_default().unwrap_or(80);
    let path = if let Some(q) = u.query() {
        format!("{}?{q}", u.path())
    } else {
        u.path().to_string()
    };
    let req = format!(
        "UNSUBSCRIBE {path} HTTP/1.1\r\nHOST: {host}:{port}\r\nSID: {sid}\r\nCONNECTION: close\r\n\r\n"
    );
    let _ = send_raw_http(host, port, req.as_bytes()).await?;
    Ok(())
}

async fn send_raw_http(host: &str, port: u16, req: &[u8]) -> std::result::Result<String, String> {
    let connect_host = host.trim_matches(['[', ']']);
    let mut stream = tokio::time::timeout(
        Duration::from_secs(4),
        TcpStream::connect((connect_host, port)),
    )
    .await
    .map_err(|_| "connect timeout".to_string())?
    .map_err(|e| e.to_string())?;
    tokio::time::timeout(Duration::from_secs(4), stream.write_all(req))
        .await
        .map_err(|_| "write timeout".to_string())?
        .map_err(|e| e.to_string())?;

    let mut out = Vec::with_capacity(4096);
    let mut chunk = [0u8; 2048];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    while out.len() < 16 * 1024 {
        let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
        if rem.is_zero() {
            break;
        }
        let n = tokio::time::timeout(rem, stream.read(&mut chunk))
            .await
            .map_err(|_| "read timeout".to_string())?
            .map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..n]);
        if out.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

pub fn parse_gena_timeout(hdr: &str) -> Duration {
    let lower = hdr.trim().to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("second-")
        && let Ok(sec) = rest.trim().parse::<u64>()
        && sec > 0
    {
        return Duration::from_secs(sec.clamp(30, 1800));
    }
    Duration::from_secs(DEFAULT_GENA_TIMEOUT_SEC)
}
