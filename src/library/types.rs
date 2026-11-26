use std::path::Path;
use std::sync::LazyLock;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const SOURCE_LOCAL: &str = "local";
pub const SOURCE_QOBUZ: &str = "qobuz";

pub const SOURCE_TYPE_LOCAL: &str = "local";
pub const SOURCE_TYPE_REMOTE: &str = "remote";

// Well-known keys stored in the SQLite app_prefs KV table.
pub const PREF_STREAM_MODE: &str = "stream_mode";
pub const PREF_QOBUZ_MAX_QUALITY: &str = "qobuz_max_quality";
pub const PREF_MANUAL_RENDERERS: &str = "manual_renderers";
pub const PREF_ZONE_QUALITIES: &str = "zone_qualities";
pub const PREF_ZONE_QUEUES: &str = "zone_queues";
pub const PREF_LIBRARY_WATCH_ENABLED: &str = "library_watch_enabled";
pub const PREF_LIBRARY_SCAN_INTERVAL_MIN: &str = "library_scan_interval_min";
pub const PREF_SONIC_ENABLED: &str = "sonic_analysis_enabled";

pub const DEFAULT_LIBRARY_WATCH_ENABLED: bool = true;
pub const DEFAULT_LIBRARY_SCAN_INTERVAL_MIN: i32 = 60;
pub const MAX_LIBRARY_SCAN_INTERVAL_MIN: i32 = 10_080; // 7 days

#[derive(Debug, Error)]
pub enum LibraryError {
    #[error("library item not found")]
    NotFound,
    #[error("remote source provider not registered: {0}")]
    ProviderNotFound(String),
    #[error("remote source provider is not connected: {0}")]
    ProviderOffline(String),
    #[error("invalid library argument: {0}")]
    InvalidArgument(String),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("remote error: {0}")]
    Remote(String),
}

pub type Result<T> = std::result::Result<T, LibraryError>;

fn is_zero_i32(v: &i32) -> bool {
    *v == 0
}
fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}
fn is_false(v: &bool) -> bool {
    !*v
}
fn is_empty_str(s: &str) -> bool {
    s.is_empty()
}
fn is_empty_vec<T>(v: &[T]) -> bool {
    v.is_empty()
}

/// Unified representation of a playable local or remote track.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    pub id: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub source_type: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub provider: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub external_id: String,
    pub title: String,
    pub artist: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub album_artist: String,
    pub album: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub album_id: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub genre: String,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub track_number: i32,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub disc_number: i32,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub year: i32,
    #[serde(default)]
    pub duration_sec: i32,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub sample_rate: i32,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub bit_depth: i32,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub channels: i32,
    #[serde(default)]
    pub mime: String,
    #[serde(default)]
    pub ext: String,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub size: i64,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub artwork_url: String,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub play_count: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_played_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub loved: bool,
    #[serde(default)]
    pub available: bool,
    #[serde(default)]
    pub in_library: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub explicit: bool,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub isrc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaygain_gain: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaygain_peak: Option<f64>,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub composer: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub performers: String,

    // Internal fields (never exposed over JSON)
    #[serde(skip)]
    pub file_path: String,
    #[serde(skip)]
    pub file_mtime_ns: i64,
    #[serde(skip)]
    pub folder_art: String,
    #[serde(skip)]
    pub has_embed_art: bool,
    #[serde(skip)]
    pub qobuz_track_id: i64,
    #[serde(skip)]
    pub qobuz_artist_id: i64,
    #[serde(skip)]
    pub qobuz_genre_id: i64,
    #[serde(skip)]
    pub qobuz_label_id: i64,
    #[serde(skip)]
    pub mb_recording_id: String,
    #[serde(skip)]
    pub mb_artist_id: String,
    // Transient album-level tags carried from local file scan to album builder
    #[serde(skip)]
    pub scan_label: String,
    #[serde(skip)]
    pub scan_upc: String,
    #[serde(skip)]
    pub scan_release_date: String,
    #[serde(skip)]
    pub scan_mb_release_id: String,
    #[serde(skip)]
    pub scan_mb_release_group_id: String,
    #[serde(skip)]
    pub scan_mb_album_artist_id: String,
}

impl Track {
    pub fn quality_label(&self) -> String {
        let mut codec = self.ext.trim_start_matches('.').to_uppercase();
        if codec.is_empty() {
            codec = self.mime.clone();
        }
        if self.bit_depth > 0 && self.sample_rate > 0 {
            let khz = self.sample_rate as f64 / 1000.0;
            if self.sample_rate % 1000 == 0 {
                return format!("{codec} {}-bit / {:.0} kHz", self.bit_depth, khz);
            }
            return format!("{codec} {}-bit / {:.1} kHz", self.bit_depth, khz);
        }
        if self.sample_rate > 0 {
            return format!("{codec} {:.1} kHz", self.sample_rate as f64 / 1000.0);
        }
        codec
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlbumVersion {
    pub id: String,
    pub source: String,
    pub source_type: String,
    pub provider: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub year: i32,
    pub track_count: i32,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub primary_format: String,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub max_sample_rate: i32,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub max_bit_depth: i32,
    pub is_primary: bool,
    pub available: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlbumCredit {
    pub name: String,
    pub roles: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Album {
    pub id: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub source_type: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub provider: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub external_id: String,
    pub title: String,
    pub artist: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub genre: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub label: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub description: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub copyright: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub release_date: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub upc: String,
    #[serde(default, skip_serializing_if = "is_empty_vec")]
    pub awards: Vec<String>,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub booklet_url: String,
    #[serde(default, skip_serializing_if = "is_empty_vec")]
    pub credits: Vec<AlbumCredit>,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub year: i32,
    #[serde(default)]
    pub track_count: i32,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub duration_sec: i32,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub artwork_url: String,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub primary_format: String,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub max_sample_rate: i32,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub max_bit_depth: i32,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub version_group_id: String,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub version_count: i32,
    #[serde(default)]
    pub is_primary: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_partial: bool,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub play_count: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub available: bool,
    #[serde(default)]
    pub in_library: bool,
    #[serde(default, skip_serializing_if = "is_empty_vec")]
    pub tracks: Vec<Track>,
    #[serde(default, skip_serializing_if = "is_empty_vec")]
    pub versions: Vec<AlbumVersion>,

    // Internal canonical / provider IDs (never exposed over JSON)
    #[serde(skip)]
    pub qobuz_artist_id: i64,
    #[serde(skip)]
    pub qobuz_genre_id: i64,
    #[serde(skip)]
    pub qobuz_label_id: i64,
    #[serde(skip)]
    pub mb_release_id: String,
    #[serde(skip)]
    pub mb_release_group_id: String,
    #[serde(skip)]
    pub mb_artist_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtistSummary {
    pub name: String,
    pub album_count: i32,
    pub track_count: i32,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub artwork_url: String,
    pub providers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopSearchResult {
    #[serde(rename = "type")]
    pub kind: String, // "artist", "album", "track"
    pub score: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<ArtistSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<Album>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<Track>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnifiedSearchResult {
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_result: Option<TopSearchResult>,
    pub artists: Vec<ArtistSummary>,
    pub albums: Vec<Album>,
    pub tracks: Vec<Track>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenreSummary {
    pub name: String,
    pub album_count: i32,
    pub track_count: i32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeavyRotationAlbum {
    #[serde(flatten)]
    pub album: Album,
    pub recent_plays: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_played: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSyncStatus {
    pub provider: String,
    pub source_type: String,
    pub connected: bool,
    pub syncing: bool,
    pub albums: i32,
    pub tracks: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sync_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "is_empty_str")]
    pub last_error: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub configured: bool,
    pub scanning: bool,
    pub tracks: i32,
    pub albums: i32,
    pub local_tracks: i32,
    pub local_albums: i32,
    pub remote_tracks: i32,
    pub remote_albums: i32,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub missing_tracks: i32,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub missing_albums: i32,
    #[serde(default, skip_serializing_if = "is_empty_vec")]
    pub providers: Vec<ProviderSyncStatus>,
    pub etag: String,
}

#[derive(Debug, Clone, Default)]
pub struct RemoteCollection {
    pub albums: Vec<Album>,
    pub tracks: Vec<Track>,
}

#[async_trait]
pub trait RemoteSource: Send + Sync {
    fn provider_id(&self) -> &str;
    fn is_connected(&self) -> bool;
    async fn sync_collection(&self) -> Result<RemoteCollection>;
    async fn get_album(&self, external_id: &str) -> Result<Album>;
    async fn get_track(&self, external_id: &str) -> Result<Track>;
    async fn add_favorite(&self, kind: &str, external_id: &str) -> Result<()>;
    async fn remove_favorite(&self, kind: &str, external_id: &str) -> Result<()>;
}

#[derive(Debug, Clone, Default)]
pub struct AlbumFilter {
    pub query: String,
    pub source_type: String,
    pub provider: String,
    pub artist: String,
    pub genre: String,
    pub sort: String,
    pub group_versions: bool,
    pub include_unavailable: bool,
    pub limit: i32,
    pub offset: i32,
}

#[derive(Debug, Clone, Default)]
pub struct TrackFilter {
    pub query: String,
    pub source_type: String,
    pub provider: String,
    pub artist: String,
    pub album_id: String,
    pub album: String,
    pub genre: String,
    pub sort: String,
    pub sort_dir: String,
    pub include_unavailable: bool,
    pub limit: i32,
    pub offset: i32,
}

#[derive(Debug, Clone, Default)]
pub struct FacetFilter {
    pub query: String,
    pub source_type: String,
    pub provider: String,
    pub genre: String,
    pub include_unavailable: bool,
    pub limit: i32,
    pub offset: i32,
}

pub fn mime_for_ext(ext: &str) -> Option<&'static str> {
    match ext.to_ascii_lowercase().as_str() {
        ".flac" => Some("audio/flac"),
        ".wav" => Some("audio/wav"),
        ".aiff" | ".aif" => Some("audio/aiff"),
        ".m4a" | ".alac" => Some("audio/mp4"),
        ".mp3" => Some("audio/mpeg"),
        ".ogg" => Some("audio/ogg"),
        ".opus" => Some("audio/opus"),
        ".dsf" => Some("audio/x-dsf"),
        _ => None,
    }
}

pub fn is_supported_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .and_then(|e| mime_for_ext(&e))
        .is_some()
}

pub fn format_name_from_mime_or_ext(mime_type: &str, ext: &str) -> String {
    let clean_ext = ext.trim().trim_start_matches('.').to_ascii_uppercase();
    match clean_ext.as_str() {
        "FLAC" | "WAV" | "AIFF" | "ALAC" | "M4A" | "MP3" | "OGG" | "OPUS" | "DSF" => {
            return clean_ext;
        }
        "AIF" => return "AIFF".to_string(),
        _ => {}
    }
    let m = mime_type.trim().to_ascii_lowercase();
    if m.contains("flac") {
        "FLAC".into()
    } else if m.contains("dsf") || m.contains("dsd") {
        "DSF".into()
    } else if m.contains("wav") {
        "WAV".into()
    } else if m.contains("aiff") {
        "AIFF".into()
    } else if m.contains("mpeg") || m.contains("mp3") {
        "MP3".into()
    } else if m.contains("mp4") || m.contains("aac") || m.contains("alac") {
        "M4A".into()
    } else if m.contains("opus") {
        "OPUS".into()
    } else if m.contains("ogg") {
        "OGG".into()
    } else {
        clean_ext
    }
}

pub fn is_lossless_format(format: &str) -> bool {
    matches!(
        format.trim().to_ascii_uppercase().as_str(),
        "FLAC" | "WAV" | "AIFF" | "ALAC" | "DSF" | "PCM"
    )
}

pub fn sort_key(s: &str) -> String {
    let lower = s.trim().to_lowercase();
    let stripped = if let Some(rest) = lower.strip_prefix("the ")
        && !rest.is_empty()
    {
        rest.trim()
    } else if let Some(rest) = lower.strip_prefix("an ")
        && !rest.is_empty()
    {
        rest.trim()
    } else if let Some(rest) = lower.strip_prefix("a ")
        && !rest.is_empty()
    {
        rest.trim()
    } else {
        lower.as_str()
    };
    stripped.split_whitespace().collect::<Vec<_>>().join(" ")
}

static TRAILING_EDITION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\s*[\(\[]\s*([^)\]]*\b(remaster(ed)?|deluxe|super\s+deluxe|expanded|anniversary|special\s+edition|collector'?s|bonus\s+track|hi-?res|24-?bit|16-?bit|mono|stereo|explicit|clean|edition|version|reissue|legacy\s+edition)\b[^)\]]*|\d{4}\s+mix)\s*[\)\]]\s*$",
    )
    .expect("valid regex")
});

pub fn version_key(album_artist: &str, title: &str) -> String {
    let artist = normalize_version_token(&sort_key(album_artist));
    let mut base_title = title.trim().to_string();
    for _ in 0..3 {
        let next = TRAILING_EDITION_RE
            .replace_all(&base_title, "")
            .trim()
            .to_string();
        if next.is_empty() || next == base_title {
            break;
        }
        base_title = next;
    }
    let norm_title = normalize_version_token(&base_title.to_lowercase());
    if artist.is_empty() || norm_title.is_empty() {
        return String::new();
    }
    format!("{artist}\0{norm_title}")
}

fn normalize_version_token(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = false;
    for ch in s.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
            last_space = false;
        } else if ch == '&' {
            if !last_space && !out.is_empty() {
                out.push(' ');
            }
            out.push_str("and ");
            last_space = true;
        } else if !last_space && !out.is_empty() {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim().to_string()
}

pub fn normalize_isrc(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if cleaned.len() == 12 {
        cleaned
    } else {
        String::new()
    }
}

pub fn quality_rank(format: &str, bit_depth: i32, sample_rate: i32, source_type: &str) -> i64 {
    let mut score: i64 = 0;
    if is_lossless_format(format) {
        score += 1_000_000_000_000;
    }
    if format.eq_ignore_ascii_case("DSF") {
        score += 500_000_000_000;
    }
    score += i64::from(bit_depth) * 10_000_000;
    score += i64::from(sample_rate) * 10;
    if source_type == SOURCE_TYPE_LOCAL {
        score += 1;
    }
    score
}

pub fn short_hash(s: &str) -> String {
    short_hash_bytes(s.as_bytes())
}

pub fn short_hash_bytes(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    hex::encode(&digest[..8])
}
