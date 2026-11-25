use std::sync::RwLock;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;

use super::didl::{
    MediaItem, build_didl, escape_xml_text, format_upnp_duration, parse_upnp_duration,
};
use super::http::{RawHttpError, send_raw_soap_post};
use super::xml::{decode_xml_bytes, extract_element_text, parse_soap_fault_xml};

const MAX_SOAP_BODY_BYTES: usize = 512 * 1024;
const SOAP_DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const SOAP_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

pub const URN_AV_TRANSPORT_1: &str = "urn:schemas-upnp-org:service:AVTransport:1";
pub const URN_RENDERING_CONTROL_1: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
pub const URN_CONNECTION_MANAGER_1: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";
pub const URN_LINKPLAY_PLAYQUEUE_1: &str = "urn:schemas-wiimu-com:service:PlayQueue:1";

#[derive(Debug, Error)]
pub enum UpnpError {
    #[error("UPnP action unsupported by renderer")]
    ActionUnsupported,
    #[error("invalid renderer URL: {0}")]
    InvalidRendererUrl(String),
    #[error("SOAP fault {code}: {desc}")]
    SoapFault { code: i32, desc: String },
    #[error("HTTP {status}: {body}")]
    HttpStatus { status: u16, body: String },
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, UpnpError>;

impl UpnpError {
    pub fn is_action_unsupported(&self) -> bool {
        match self {
            UpnpError::ActionUnsupported => true,
            UpnpError::SoapFault { code, .. } => *code == 401 || *code == 602,
            _ => false,
        }
    }

    /// True only for early EOF / connection-reset errors (never timeouts or refused connects),
    /// matching Go's `isTransientConnErr` so non-idempotent actions are never re-fired after
    /// a slow renderer already executed them.
    pub fn is_transient_conn(&self) -> bool {
        match self {
            UpnpError::Http(e) => {
                if e.is_timeout() || e.is_connect() {
                    return false;
                }
                let msg = e.to_string();
                msg.contains("EOF") || msg.contains("reset") || msg.contains("broken pipe")
            }
            UpnpError::Other(msg) => {
                msg.contains("EOF")
                    || msg.contains("connection reset")
                    || msg.contains("Connection reset")
                    || msg.contains("broken pipe")
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RendererInfo {
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub manufacturer: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    pub location: String,
    pub host: String,
    pub has_volume: bool,
    pub supports_set_next: bool,
    pub supports_seek: bool,
    pub supports_events: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub supports_queue: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sink_protocols: Vec<String>,
}

fn is_false(v: &bool) -> bool {
    !*v
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportStatus {
    pub state: String,
    pub position: Duration,
    pub duration: Duration,
    pub track_uri: String,
    pub has_position: bool,
    pub queue_index: i32,
    pub has_queue_index: bool,
    pub volume: i32,
    pub has_vol_value: bool,
    pub muted: bool,
    pub has_mute_val: bool,
    pub has_volume: bool,
}

#[async_trait]
pub trait Controller: Send + Sync {
    fn info(&self) -> RendererInfo;
    fn local_ip(&self) -> String;
    async fn set_uri(&self, item: &MediaItem) -> Result<()>;
    async fn set_next_uri(&self, item: &MediaItem) -> Result<()>;
    async fn play(&self) -> Result<()>;
    async fn pause(&self) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn seek(&self, pos: Duration) -> Result<()>;
    async fn status(&self) -> Result<TransportStatus>;
    async fn set_volume(&self, vol: i32) -> Result<()>;
    async fn set_mute(&self, muted: bool) -> Result<()>;

    // Optional capabilities (default no-op / None)
    fn event_urls(&self) -> Option<(String, String)> {
        None
    }
    fn prefer_relative_media(&self) -> bool {
        false
    }
    fn qobuz_delivery(&self) -> &str {
        ""
    }
    fn push_connected(&self) -> bool {
        false
    }
    fn as_queue_controller(&self) -> Option<&dyn QueueController> {
        None
    }
    /// Gracefully closes any persistent device session (e.g. Cast TLS socket).
    async fn close_session(&self) {}
}

#[async_trait]
pub trait QueueController: Send + Sync {
    async fn replace_queue(&self, items: &[MediaItem], start_index: usize) -> Result<()>;
    async fn append_queue(&self, items: &[MediaItem]) -> Result<()>;
    async fn remove_queue_range(&self, start: usize, end: usize) -> Result<()>;
    async fn move_queue_item(&self, from: usize, to: usize) -> Result<()>;
    async fn play_index(&self, index: usize) -> Result<()>;
    async fn next(&self) -> Result<()>;
    async fn previous(&self) -> Result<()>;
    async fn set_loop_mode(&self, repeat: &str, shuffle: bool) -> Result<()>;
}

/// Returns a reqwest client for UPnP XML GETs (device description, SCPD, probes):
/// keep-alives disabled, gzip disabled (embedded stacks often advertise or return broken gzip),
/// redirects disallowed, and caller-provided timeout.
pub fn new_upnp_get_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(timeout.min(Duration::from_secs(3)))
        .pool_max_idle_per_host(0)
        .no_gzip()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("build UPnP GET reqwest client")
}

/// Returns a shared reqwest client for HTTPS UPnP SOAP / Linkplay HTTP API calls:
/// keep-alives disabled, gzip disabled, redirects disallowed, 5s request timeout.
pub fn new_soap_http_client() -> reqwest::Client {
    static SHARED_SOAP_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    SHARED_SOAP_CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(SOAP_DEFAULT_TIMEOUT)
                .connect_timeout(Duration::from_secs(3))
                .pool_max_idle_per_host(0)
                .no_gzip()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("build SOAP reqwest client")
        })
        .clone()
}

struct RendererCaps {
    supports_set_next: bool,
    supports_seek: bool,
}

pub struct Renderer {
    info: RendererInfo,
    caps: RwLock<RendererCaps>,
    local_ip: String,
    av_control_url: String,
    av_service_urn: String,
    rc_control_url: String,
    rc_service_urn: String,
    cm_control_url: String,
    cm_service_urn: String,
    av_event_url: String,
    rc_event_url: String,
    play_queue_control_url: String,
    soap_mu: AsyncMutex<()>,
    http: reqwest::Client,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RendererEndpoints {
    pub local_ip: String,
    pub av_control_url: String,
    pub av_service_urn: String,
    pub rc_control_url: String,
    pub rc_service_urn: String,
    pub cm_control_url: String,
    pub cm_service_urn: String,
    pub av_event_url: String,
    pub rc_event_url: String,
    pub play_queue_control_url: String,
}

impl Renderer {
    pub(crate) fn new_internal(info: RendererInfo, ep: RendererEndpoints) -> Self {
        let supports_set_next = info.supports_set_next;
        let supports_seek = info.supports_seek;
        Self {
            info,
            caps: RwLock::new(RendererCaps {
                supports_set_next,
                supports_seek,
            }),
            local_ip: ep.local_ip,
            av_control_url: ep.av_control_url,
            av_service_urn: if ep.av_service_urn.is_empty() {
                URN_AV_TRANSPORT_1.to_string()
            } else {
                ep.av_service_urn
            },
            rc_control_url: ep.rc_control_url,
            rc_service_urn: if ep.rc_service_urn.is_empty() {
                URN_RENDERING_CONTROL_1.to_string()
            } else {
                ep.rc_service_urn
            },
            cm_control_url: ep.cm_control_url,
            cm_service_urn: if ep.cm_service_urn.is_empty() {
                URN_CONNECTION_MANAGER_1.to_string()
            } else {
                ep.cm_service_urn
            },
            av_event_url: ep.av_event_url,
            rc_event_url: ep.rc_event_url,
            play_queue_control_url: ep.play_queue_control_url,
            soap_mu: AsyncMutex::new(()),
            http: new_soap_http_client(),
        }
    }

    pub fn play_queue_control_url(&self) -> &str {
        &self.play_queue_control_url
    }

    pub fn av_transport_control_url(&self) -> &str {
        &self.av_control_url
    }

    pub fn soap_client(&self) -> &reqwest::Client {
        &self.http
    }

    async fn send_soap_once(
        &self,
        control_url: &str,
        soap_action_hdr: &str,
        env: &str,
        timeout: Duration,
    ) -> Result<String> {
        let (status, body) = if control_url.starts_with("http://") {
            let resp = send_raw_soap_post(
                control_url,
                soap_action_hdr,
                env,
                timeout,
                MAX_SOAP_BODY_BYTES,
            )
            .await
            .map_err(|e| match e {
                RawHttpError::EofOrReset(m) => UpnpError::Other(format!("EOF: {m}")),
                RawHttpError::Timeout(m) | RawHttpError::Other(m) => UpnpError::Other(m),
            })?;
            (resp.status, decode_xml_bytes(&resp.body))
        } else {
            let mut resp = self
                .http
                .post(control_url)
                .timeout(timeout)
                .header("CONTENT-TYPE", r#"text/xml; charset="utf-8""#)
                .header("SOAPACTION", soap_action_hdr)
                .header("CONNECTION", "close")
                .body(env.to_string())
                .send()
                .await?;
            let status = resp.status().as_u16();
            let mut bytes = Vec::new();
            while let Some(chunk) = resp.chunk().await? {
                if bytes.len().saturating_add(chunk.len()) > MAX_SOAP_BODY_BYTES {
                    return Err(UpnpError::Other(format!(
                        "HTTPS SOAP response exceeds limit ({MAX_SOAP_BODY_BYTES} bytes)"
                    )));
                }
                bytes.extend_from_slice(&chunk);
            }
            (status, decode_xml_bytes(&bytes))
        };

        // Some embedded stacks (Linkplay/upmpdcli) return SOAP faults with HTTP 200.
        if let Some((code, desc)) = parse_soap_fault_xml(&body) {
            return Err(UpnpError::SoapFault { code, desc });
        }
        if (200..300).contains(&status) {
            return Ok(body);
        }
        Err(UpnpError::HttpStatus { status, body })
    }

    async fn soap_action_unlocked(
        &self,
        control_url: &str,
        service_urn: &str,
        action: &str,
        inner_body: &str,
        timeout: Duration,
    ) -> Result<String> {
        let env = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:{action} xmlns:u="{service_urn}">{inner_body}</u:{action}></s:Body></s:Envelope>"#
        );
        let soap_action_hdr = format!(r#""{service_urn}#{action}""#);
        match self
            .send_soap_once(control_url, &soap_action_hdr, &env, timeout)
            .await
        {
            Ok(v) => Ok(v),
            Err(e) if e.is_transient_conn() => {
                tokio::time::sleep(Duration::from_millis(120)).await;
                self.send_soap_once(control_url, &soap_action_hdr, &env, timeout)
                    .await
            }
            Err(e) => Err(e),
        }
    }

    pub async fn soap_action(
        &self,
        control_url: &str,
        service_urn: &str,
        action: &str,
        inner_body: &str,
    ) -> Result<String> {
        let _guard = self.soap_mu.lock().await;
        self.soap_action_unlocked(
            control_url,
            service_urn,
            action,
            inner_body,
            SOAP_DEFAULT_TIMEOUT,
        )
        .await
    }

    /// Sends a non-idempotent SOAP action (e.g. `AppendTracksInQueue`) serialized under
    /// `soap_mu` WITHOUT EOF retry so a lost response cannot duplicate side effects.
    pub async fn soap_action_no_retry(
        &self,
        control_url: &str,
        service_urn: &str,
        action: &str,
        inner_body: &str,
    ) -> Result<String> {
        let _guard = self.soap_mu.lock().await;
        let env = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body><u:{action} xmlns:u="{service_urn}">{inner_body}</u:{action}></s:Body></s:Envelope>"#
        );
        let soap_action_hdr = format!(r#""{service_urn}#{action}""#);
        self.send_soap_once(control_url, &soap_action_hdr, &env, SOAP_DEFAULT_TIMEOUT)
            .await
    }

    pub async fn next(&self) -> Result<()> {
        self.soap_action(
            &self.av_control_url,
            &self.av_service_urn,
            "Next",
            "<InstanceID>0</InstanceID>",
        )
        .await?;
        Ok(())
    }

    pub async fn previous(&self) -> Result<()> {
        self.soap_action(
            &self.av_control_url,
            &self.av_service_urn,
            "Previous",
            "<InstanceID>0</InstanceID>",
        )
        .await?;
        Ok(())
    }

    pub async fn fetch_sink_protocols(&self) -> Vec<String> {
        if self.cm_control_url.is_empty() {
            return Vec::new();
        }
        let _guard = self.soap_mu.lock().await;
        let Ok(xml) = self
            .soap_action_unlocked(
                &self.cm_control_url,
                &self.cm_service_urn,
                "GetProtocolInfo",
                "",
                SOAP_PROBE_TIMEOUT,
            )
            .await
        else {
            return Vec::new();
        };
        let Some(sink) = extract_xml_text(&xml, "Sink") else {
            return Vec::new();
        };
        sink.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }
}

#[async_trait]
impl Controller for Renderer {
    fn info(&self) -> RendererInfo {
        let caps = self.caps.read().unwrap_or_else(|e| e.into_inner());
        let mut info = self.info.clone();
        info.supports_set_next = caps.supports_set_next;
        info.supports_seek = caps.supports_seek;
        info
    }

    fn local_ip(&self) -> String {
        self.local_ip.clone()
    }

    async fn set_uri(&self, item: &MediaItem) -> Result<()> {
        let didl = build_didl(item, &self.info.sink_protocols);
        let body = format!(
            "<InstanceID>0</InstanceID><CurrentURI>{}</CurrentURI><CurrentURIMetaData>{}</CurrentURIMetaData>",
            escape_xml_text(&item.url),
            escape_xml_text(&didl)
        );
        self.soap_action(
            &self.av_control_url,
            &self.av_service_urn,
            "SetAVTransportURI",
            &body,
        )
        .await?;
        Ok(())
    }

    async fn set_next_uri(&self, item: &MediaItem) -> Result<()> {
        if !self
            .caps
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .supports_set_next
        {
            return Err(UpnpError::ActionUnsupported);
        }
        let didl = build_didl(item, &self.info.sink_protocols);
        let body = format!(
            "<InstanceID>0</InstanceID><NextURI>{}</NextURI><NextURIMetaData>{}</NextURIMetaData>",
            escape_xml_text(&item.url),
            escape_xml_text(&didl)
        );
        match self
            .soap_action(
                &self.av_control_url,
                &self.av_service_urn,
                "SetNextAVTransportURI",
                &body,
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.is_action_unsupported() => {
                self.caps
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .supports_set_next = false;
                Err(UpnpError::ActionUnsupported)
            }
            Err(e) => Err(e),
        }
    }

    async fn play(&self) -> Result<()> {
        self.soap_action(
            &self.av_control_url,
            &self.av_service_urn,
            "Play",
            "<InstanceID>0</InstanceID><Speed>1</Speed>",
        )
        .await?;
        Ok(())
    }

    async fn pause(&self) -> Result<()> {
        self.soap_action(
            &self.av_control_url,
            &self.av_service_urn,
            "Pause",
            "<InstanceID>0</InstanceID>",
        )
        .await?;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.soap_action(
            &self.av_control_url,
            &self.av_service_urn,
            "Stop",
            "<InstanceID>0</InstanceID>",
        )
        .await?;
        Ok(())
    }

    async fn seek(&self, pos: Duration) -> Result<()> {
        if !self
            .caps
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .supports_seek
        {
            return Err(UpnpError::ActionUnsupported);
        }
        let target = format_upnp_duration(pos);
        let body =
            format!("<InstanceID>0</InstanceID><Unit>REL_TIME</Unit><Target>{target}</Target>");
        match self
            .soap_action(&self.av_control_url, &self.av_service_urn, "Seek", &body)
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if e.is_action_unsupported() => {
                self.caps
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .supports_seek = false;
                Err(UpnpError::ActionUnsupported)
            }
            Err(e) => Err(e),
        }
    }

    async fn status(&self) -> Result<TransportStatus> {
        let mut st = TransportStatus {
            has_volume: !self.rc_control_url.is_empty(),
            ..Default::default()
        };

        // Hold soap_mu across GetTransportInfo + GetPositionInfo so state and position
        // are read as a consistent pair without interleaved transport mutations.
        let (ti_xml, pi_res) = {
            let _guard = self.soap_mu.lock().await;
            let ti = self
                .soap_action_unlocked(
                    &self.av_control_url,
                    &self.av_service_urn,
                    "GetTransportInfo",
                    "<InstanceID>0</InstanceID>",
                    SOAP_DEFAULT_TIMEOUT,
                )
                .await?;
            let pi = self
                .soap_action_unlocked(
                    &self.av_control_url,
                    &self.av_service_urn,
                    "GetPositionInfo",
                    "<InstanceID>0</InstanceID>",
                    SOAP_DEFAULT_TIMEOUT,
                )
                .await;
            (ti, pi)
        };
        st.state = extract_xml_text(&ti_xml, "CurrentTransportState").unwrap_or_default();

        if let Ok(pi_xml) = pi_res {
            if let Some(rel) = extract_xml_text(&pi_xml, "RelTime") {
                st.position = parse_upnp_duration(&rel);
                st.has_position = true;
            }
            if let Some(dur) = extract_xml_text(&pi_xml, "TrackDuration") {
                st.duration = parse_upnp_duration(&dur);
            }
            if let Some(uri) = extract_xml_text(&pi_xml, "TrackURI") {
                st.track_uri = uri;
            }
        }

        if !self.rc_control_url.is_empty() {
            if let Ok(v_xml) = self
                .soap_action(
                    &self.rc_control_url,
                    &self.rc_service_urn,
                    "GetVolume",
                    "<InstanceID>0</InstanceID><Channel>Master</Channel>",
                )
                .await
                && let Some(v_str) = extract_xml_text(&v_xml, "CurrentVolume")
                && let Ok(v) = v_str.trim().parse::<i32>()
            {
                st.volume = v.clamp(0, 100);
                st.has_vol_value = true;
            }
            if let Ok(m_xml) = self
                .soap_action(
                    &self.rc_control_url,
                    &self.rc_service_urn,
                    "GetMute",
                    "<InstanceID>0</InstanceID><Channel>Master</Channel>",
                )
                .await
                && let Some(m_str) = extract_xml_text(&m_xml, "CurrentMute")
            {
                let m = m_str.trim();
                st.muted =
                    m == "1" || m.eq_ignore_ascii_case("true") || m.eq_ignore_ascii_case("yes");
                st.has_mute_val = true;
            }
        }

        Ok(st)
    }

    async fn set_volume(&self, vol: i32) -> Result<()> {
        if self.rc_control_url.is_empty() {
            return Err(UpnpError::Other("renderer has no RenderingControl".into()));
        }
        let vol = vol.clamp(0, 100);
        let body = format!(
            "<InstanceID>0</InstanceID><Channel>Master</Channel><DesiredVolume>{vol}</DesiredVolume>"
        );
        self.soap_action(
            &self.rc_control_url,
            &self.rc_service_urn,
            "SetVolume",
            &body,
        )
        .await?;
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<()> {
        if self.rc_control_url.is_empty() {
            return Err(UpnpError::Other("renderer has no RenderingControl".into()));
        }
        let val = if muted { "1" } else { "0" };
        let body = format!(
            "<InstanceID>0</InstanceID><Channel>Master</Channel><DesiredMute>{val}</DesiredMute>"
        );
        self.soap_action(&self.rc_control_url, &self.rc_service_urn, "SetMute", &body)
            .await?;
        Ok(())
    }

    fn event_urls(&self) -> Option<(String, String)> {
        if self.av_event_url.is_empty() && self.rc_event_url.is_empty() {
            None
        } else {
            Some((self.av_event_url.clone(), self.rc_event_url.clone()))
        }
    }
}

pub fn extract_xml_text(xml: &str, local_name: &str) -> Option<String> {
    extract_element_text(xml, local_name)
}
