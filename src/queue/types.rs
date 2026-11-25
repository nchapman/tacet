use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::library::Track;
use crate::upnp::RendererInfo;

#[derive(Debug, Error)]
pub enum QueueError {
    #[error("zone not found: {0}")]
    ZoneNotFound(String),
    #[error("invalid index")]
    InvalidIndex,
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("empty queue")]
    EmptyQueue,
    #[error("end of queue")]
    EndOfQueue,
    #[error("capability unsupported: {0}")]
    CapabilityUnsupported(String),
    #[error("renderer error: {0}")]
    Renderer(String),
}

pub type Result<T> = std::result::Result<T, QueueError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StreamMode {
    #[default]
    Direct,
    Relay,
}

impl StreamMode {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "direct" => Some(Self::Direct),
            "relay" => Some(Self::Relay),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Relay => "relay",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PlaybackState {
    #[default]
    Idle,
    Playing,
    Paused,
    Buffering,
    Stopped,
}

impl PlaybackState {
    pub fn normalize(raw: &str) -> Self {
        match raw.trim().to_ascii_uppercase().as_str() {
            "PLAYING" => Self::Playing,
            "PAUSED_PLAYBACK" | "PAUSED_RECORDING" | "PAUSED" => Self::Paused,
            "TRANSITIONING" | "BUFFERING" => Self::Buffering,
            "STOPPED" => Self::Stopped,
            _ => Self::Idle,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RepeatMode {
    #[default]
    Off,
    All,
    One,
}

impl RepeatMode {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "off" => Some(Self::Off),
            "all" => Some(Self::All),
            "one" => Some(Self::One),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::All => "all",
            Self::One => "one",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AudioQuality {
    #[default]
    #[serde(rename = "original")]
    Original,
    #[serde(rename = "opus_96")]
    Opus96,
    #[serde(rename = "opus_128")]
    Opus128,
    #[serde(rename = "opus_160")]
    Opus160,
    #[serde(rename = "opus_256")]
    Opus256,
    #[serde(rename = "wav")]
    Wav,
}

impl AudioQuality {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "original" => Some(Self::Original),
            "opus_96" => Some(Self::Opus96),
            "opus_128" => Some(Self::Opus128),
            "opus_160" => Some(Self::Opus160),
            "opus_256" => Some(Self::Opus256),
            "wav" => Some(Self::Wav),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Original => "original",
            Self::Opus96 => "opus_96",
            Self::Opus128 => "opus_128",
            Self::Opus160 => "opus_160",
            Self::Opus256 => "opus_256",
            Self::Wav => "wav",
        }
    }

    pub fn is_opus(&self) -> bool {
        matches!(
            self,
            Self::Opus96 | Self::Opus128 | Self::Opus160 | Self::Opus256
        )
    }

    pub fn needs_transcode(&self) -> bool {
        self.is_opus() || *self == Self::Wav
    }
}

pub fn renderer_supports_opus(info: &RendererInfo) -> bool {
    if info.kind == "player" || info.sink_protocols.is_empty() {
        return true;
    }
    info.sink_protocols
        .iter()
        .any(|p| p.to_ascii_lowercase().contains("opus"))
}

pub fn renderer_supports_aac(info: &RendererInfo) -> bool {
    if info.sink_protocols.is_empty() {
        return true;
    }
    info.sink_protocols.iter().any(|p| {
        let low = p.to_ascii_lowercase();
        low.contains("audio/aac") || low.contains("audio/mp4")
    })
}

pub fn renderer_supports_dsd(info: &RendererInfo) -> bool {
    if info.kind == "player" {
        return false;
    }
    if info.sink_protocols.is_empty() {
        return true;
    }
    info.sink_protocols.iter().any(|p| {
        let low = p.to_ascii_lowercase();
        low.contains("dsd") || low.contains("dsf")
    })
}

pub fn transcoded_mime(quality: AudioQuality, container: &str) -> &'static str {
    if quality == AudioQuality::Wav {
        return "audio/wav";
    }
    match container {
        "webm" => "audio/webm",
        "mp4" => "audio/mp4",
        "adts" | "aac" => "audio/aac",
        _ => "audio/ogg",
    }
}

fn sink_has_exact_codec(protocols: &[String], container_mime: &str, codec: &str) -> bool {
    let cm = container_mime.trim().to_ascii_lowercase();
    let cd = codec.trim().to_ascii_lowercase();
    let prefix = format!("{cm};");
    protocols.iter().any(|proto| {
        let p = proto.trim().to_ascii_lowercase();
        p.starts_with(&prefix) && p.contains(&cd)
    })
}

pub fn sink_supports_mime(protocols: &[String], mime: &str) -> bool {
    let m = mime.trim().to_ascii_lowercase();
    let colon = format!(":{m}:");
    let semi = format!("{m};");
    protocols.iter().any(|proto| {
        let p = proto.trim().to_ascii_lowercase();
        if p == m || p.contains(&colon) || p.starts_with(&semi) {
            return true;
        }
        if (m == "audio/flac" || m == "audio/x-flac")
            && (p.contains("audio/flac") || p.contains("audio/x-flac"))
        {
            return true;
        }
        if (m == "audio/mp4" || m == "audio/x-m4a")
            && (p.contains("audio/mp4") || p.contains("audio/x-m4a") || p.contains("audio/aac"))
        {
            return true;
        }
        false
    })
}

pub fn check_endpoint_mime(info: &RendererInfo, mime: &str) -> Result<()> {
    if info.kind != "player" || info.sink_protocols.is_empty() {
        return Ok(());
    }
    let m = mime.trim().to_ascii_lowercase();
    if m.is_empty() || sink_supports_mime(&info.sink_protocols, &m) {
        return Ok(());
    }
    Err(QueueError::CapabilityUnsupported(format!(
        "player endpoint {:?} does not support {m}",
        info.name
    )))
}

pub fn negotiate_opus_container(info: &RendererInfo) -> &'static str {
    if info.kind == "player" && !info.sink_protocols.is_empty() {
        if sink_has_exact_codec(&info.sink_protocols, "audio/ogg", "opus") {
            return "ogg";
        }
        if sink_has_exact_codec(&info.sink_protocols, "audio/webm", "opus") {
            return "webm";
        }
        if sink_has_exact_codec(&info.sink_protocols, "audio/mp4", "opus") {
            return "mp4";
        }
        if !sink_supports_mime(&info.sink_protocols, "audio/ogg") {
            if sink_supports_mime(&info.sink_protocols, "audio/webm") {
                return "webm";
            }
            if sink_supports_mime(&info.sink_protocols, "audio/mp4") {
                return "mp4";
            }
        }
    }
    if !renderer_supports_opus(info) {
        return "adts";
    }
    "ogg"
}

pub fn is_dsd_track(track: &Track) -> bool {
    let low_mime = track.mime.to_ascii_lowercase();
    (track.bit_depth == 1 && track.sample_rate >= 2_000_000)
        || track.ext.eq_ignore_ascii_case(".dsf")
        || track.ext.eq_ignore_ascii_case(".dff")
        || low_mime.contains("dsf")
        || low_mime.contains("dsd")
}

pub fn resolve_track_quality(
    requested: AudioQuality,
    track: &Track,
    info: &RendererInfo,
    transcoder_available: bool,
) -> (AudioQuality, &'static str) {
    if is_dsd_track(track) && !renderer_supports_dsd(info) && transcoder_available {
        return (AudioQuality::Wav, "wav");
    }
    if requested.is_opus() && transcoder_available {
        let container = negotiate_opus_container(info);
        if container != "adts" || renderer_supports_aac(info) {
            return (requested, container);
        }
    }
    if requested == AudioQuality::Wav && transcoder_available {
        return (AudioQuality::Wav, "wav");
    }
    (AudioQuality::Original, "")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub item_id: String,
    pub track: Track,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ZoneSnapshot {
    pub zone_id: String,
    pub renderer: RendererInfo,
    pub revision: u64,
    pub state: PlaybackState,
    pub position_sec: i32,
    pub position_updated_at: DateTime<Utc>,
    pub duration_sec: i32,
    pub volume: i32,
    pub muted: bool,
    pub queue: Vec<Item>,
    pub current_index: i32,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub current_item_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_track: Option<Track>,
    pub audio_quality: AudioQuality,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_quality: Option<AudioQuality>,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub gapless_armed: bool,
    pub events_active: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackEvent {
    pub zone_id: String,
    pub revision: u64,
    pub state: PlaybackState,
    pub position_sec: i32,
    pub position_updated_at: DateTime<Utc>,
    pub server_time: DateTime<Utc>,
    pub duration_sec: i32,
    pub current_index: i32,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub current_item_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_track: Option<Track>,
    pub audio_quality: AudioQuality,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_quality: Option<AudioQuality>,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub gapless_armed: bool,
    pub events_active: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeEvent {
    pub zone_id: String,
    pub revision: u64,
    pub volume: i32,
    pub muted: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemsEvent {
    pub zone_id: String,
    pub revision: u64,
    pub current_index: i32,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub current_item_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_track: Option<Track>,
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ZoneErrorPayload {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ZoneErrorEvent {
    pub id: String,
    pub zone_id: String,
    pub error: ZoneErrorPayload,
    pub occurred_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub id: u64,
    pub event_type: String,
    pub zone_id: String,
    pub data: serde_json::Value,
}
