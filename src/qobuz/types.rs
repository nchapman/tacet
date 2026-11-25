use std::collections::HashSet;
use std::time::{Duration, Instant};

use md5::Digest as _;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::library::{Album, AlbumCredit, SOURCE_QOBUZ, SOURCE_TYPE_REMOTE, Track};

pub const DEFAULT_BASE_URL: &str = "https://www.qobuz.com/api.json/0.2";
pub const WEB_PLAYER_LOGIN: &str = "https://play.qobuz.com/login";

pub const FORMAT_MP3_320: i32 = 5;
pub const FORMAT_FLAC_16_44: i32 = 6;
pub const FORMAT_FLAC_24_96: i32 = 7;
pub const FORMAT_FLAC_192: i32 = 27;

#[derive(Debug, Error)]
pub enum QobuzError {
    #[error("qobuz is not authenticated")]
    Unauthenticated,
    #[error("qobuz rate limited")]
    RateLimited,
    #[error("qobuz resource not found")]
    NotFound,
    #[error("invalid qobuz max quality (use mp3_320, flac_16_44, flac_24_96, or flac_24_192)")]
    InvalidQuality,
    #[error("qobuz API returned {status}: {body}")]
    Api { status: u16, body: String },
    #[error("qobuz request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("qobuz decode failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, QobuzError>;

pub fn quality_label(format_id: i32) -> &'static str {
    match format_id {
        FORMAT_MP3_320 => "mp3_320",
        FORMAT_FLAC_16_44 => "flac_16_44",
        FORMAT_FLAC_24_96 => "flac_24_96",
        _ => "flac_24_192",
    }
}

pub fn parse_quality(q: &str) -> Result<i32> {
    match q.trim().to_ascii_lowercase().as_str() {
        "mp3_320" => Ok(FORMAT_MP3_320),
        "flac_16_44" => Ok(FORMAT_FLAC_16_44),
        "flac_24_96" => Ok(FORMAT_FLAC_24_96),
        "flac_24_192" => Ok(FORMAT_FLAC_192),
        _ => Err(QobuzError::InvalidQuality),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub configured: bool,
    pub authenticated: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub subscription: String,
    pub max_format_id: i32,
    pub max_quality: String,
}

#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub url: String,
    pub mime: String,
    pub sample_rate: i32,
    pub bit_depth: i32,
    pub duration: Duration,
    pub format_id: i32,
    pub expires_at: Instant,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Artist {
    pub id: i64,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub artwork_url: String,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub albums_count: i32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub biography: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtistDetail {
    pub artist: Artist,
    pub albums: Vec<Album>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tracks: Vec<Track>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub similar_artists: Vec<Artist>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub artists: Vec<Artist>,
    pub tracks: Vec<Track>,
    pub albums: Vec<Album>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedCredentials {
    #[serde(default)]
    pub app_id: String,
    #[serde(default)]
    pub app_secret: String,
    #[serde(default)]
    pub private_key: String,
    #[serde(default)]
    pub alt_secrets: Vec<String>,
    #[serde(default)]
    pub auth_token: String,
    #[serde(default)]
    pub user_name: String,
    #[serde(default)]
    pub subscription: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlbumLookup {
    pub artist: String,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoverGenre {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoverShelf {
    pub slug: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub subtitle: String,
    pub items: Vec<Album>,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoverFeed {
    pub genres: Vec<DiscoverGenre>,
    pub shelves: Vec<DiscoverShelf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoverShelfPage {
    pub slug: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub subtitle: String,
    pub items: Vec<Album>,
    pub has_more: bool,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub total: i32,
    pub offset: i32,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub next_offset: i32,
    pub limit: i32,
}

fn is_zero_i32(v: &i32) -> bool {
    *v == 0
}

pub(crate) fn md5_hex(s: &str) -> String {
    hex::encode(md5::Md5::digest(s.as_bytes()))
}

/// Decodes a Qobuz API JSON payload with Go `encoding/json` null-tolerance:
/// any object field or array element that is JSON `null` is stripped before
/// deserializing so `#[serde(default)]` fields receive their default value and
/// `Option<T>` fields receive `None`.
pub(crate) fn decode_qobuz_json<T: serde::de::DeserializeOwned>(raw: &[u8]) -> Result<T> {
    let mut val: serde_json::Value = serde_json::from_slice(raw)?;
    strip_json_nulls(&mut val);
    Ok(serde_json::from_value(val)?)
}

fn strip_json_nulls(val: &mut serde_json::Value) {
    match val {
        serde_json::Value::Object(map) => {
            map.retain(|_, v| !v.is_null());
            for v in map.values_mut() {
                strip_json_nulls(v);
            }
        }
        serde_json::Value::Array(items) => {
            items.retain(|v| !v.is_null());
            for item in items {
                strip_json_nulls(item);
            }
        }
        _ => {}
    }
}

pub(crate) fn normalize_sample_rate_hz(v: f64) -> i32 {
    if v <= 0.0 {
        return 0;
    }
    if v < 1000.0 {
        return (v * 1000.0).round() as i32;
    }
    v.round() as i32
}

pub(crate) fn norm_match_key(s: &str) -> String {
    s.trim()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

pub(crate) fn clean_qobuz_text(raw: &str) -> String {
    let s = raw.trim();
    if s.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    let mut tag_buf = String::new();
    for ch in s.chars() {
        if ch == '<' {
            in_tag = true;
            tag_buf.clear();
            continue;
        }
        if in_tag {
            if ch == '>' {
                in_tag = false;
                let t = tag_buf.trim().to_ascii_lowercase();
                if t.starts_with("br") || t == "/p" || t == "/div" {
                    out.push('\n');
                }
            } else {
                tag_buf.push(ch);
            }
            continue;
        }
        out.push(ch);
    }
    let decoded = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ");

    // Collapse excessive newlines
    let mut cleaned = String::with_capacity(decoded.len());
    let mut nl_run = 0;
    for line in decoded.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if nl_run == 0 && !cleaned.is_empty() {
                cleaned.push_str("\n\n");
                nl_run = 1;
            }
        } else {
            if !cleaned.is_empty() && nl_run == 0 {
                cleaned.push('\n');
            }
            cleaned.push_str(trimmed);
            nl_run = 0;
        }
    }
    cleaned.trim().to_string()
}

// --- Raw Qobuz API wire structs ---

#[derive(Debug, Default, Deserialize)]
pub(crate) struct QImage {
    #[serde(default)]
    pub small: String,
    #[serde(default)]
    pub thumbnail: String,
    #[serde(default)]
    pub large: String,
    #[serde(default)]
    pub extralarge: String,
    #[serde(default)]
    pub mega: String,
}

impl QImage {
    pub fn best(&self) -> String {
        for cand in [
            &self.large,
            &self.extralarge,
            &self.mega,
            &self.small,
            &self.thumbnail,
        ] {
            if !cand.is_empty() {
                return cand.clone();
            }
        }
        String::new()
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct QArtistImage {
    #[serde(default)]
    pub small: String,
    #[serde(default)]
    pub medium: String,
    #[serde(default)]
    pub large: String,
    #[serde(default)]
    pub extralarge: String,
    #[serde(default)]
    pub mega: String,
}

impl QArtistImage {
    pub fn best(&self) -> String {
        for cand in [
            &self.extralarge,
            &self.large,
            &self.mega,
            &self.medium,
            &self.small,
        ] {
            if !cand.is_empty() {
                return cand.clone();
            }
        }
        String::new()
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct QArtistBio {
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub content: String,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct QArtist {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub albums_count: i32,
    #[serde(default)]
    pub picture: String,
    #[serde(default)]
    pub image: Option<QArtistImage>,
    #[serde(default)]
    pub biography: Option<QArtistBio>,
}

impl QArtist {
    pub fn to_artist(&self) -> Artist {
        let mut art = self
            .image
            .as_ref()
            .map(QArtistImage::best)
            .unwrap_or_default();
        if art.is_empty() {
            art = self.picture.clone();
        }
        let bio = self
            .biography
            .as_ref()
            .map(|b| {
                let cleaned = clean_qobuz_text(&b.content);
                if cleaned.is_empty() {
                    clean_qobuz_text(&b.summary)
                } else {
                    cleaned
                }
            })
            .unwrap_or_default();
        Artist {
            id: self.id,
            name: self.name.trim().to_string(),
            artwork_url: art,
            albums_count: self.albums_count,
            biography: bio,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct QGenre {
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct QLabel {
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct QTracksList {
    #[serde(default)]
    pub items: Vec<QTrack>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct QAward {
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct QGoody {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub original_url: String,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct QAlbum {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: QArtist,
    #[serde(default)]
    pub genre: QGenre,
    #[serde(default)]
    pub label: QLabel,
    #[serde(default)]
    pub copyright: String,
    #[serde(default)]
    pub awards: Vec<QAward>,
    #[serde(default)]
    pub goodies: Vec<QGoody>,
    #[serde(default)]
    pub release_date_original: String,
    #[serde(default)]
    pub released_at: i64,
    #[serde(default)]
    pub tracks_count: i32,
    #[serde(default)]
    pub duration: i32,
    #[serde(default)]
    pub image: QImage,
    #[serde(default)]
    pub maximum_sampling_rate: f64,
    #[serde(default)]
    pub maximum_bit_depth: i32,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub release_type: String,
    #[serde(default)]
    pub streamable: Option<bool>,
    #[serde(default)]
    pub tracks: QTracksList,
}

impl QAlbum {
    pub fn to_library_album(&self, include_tracks: bool) -> Album {
        let mut year = self
            .release_date_original
            .get(..4)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if year == 0
            && self.released_at > 0
            && let Some(dt) = chrono::DateTime::from_timestamp(self.released_at, 0)
        {
            year = dt.format("%Y").to_string().parse().unwrap_or(0);
        }
        let tracks: Vec<Track> = if include_tracks {
            self.tracks
                .items
                .iter()
                .map(|qt| qt.to_library_track(Some(self)))
                .collect()
        } else {
            Vec::new()
        };
        let credits = if include_tracks {
            parse_album_credits(&self.tracks.items)
        } else {
            Vec::new()
        };
        let mut track_count = self.tracks_count;
        if track_count == 0 && !tracks.is_empty() {
            track_count = tracks.len() as i32;
        }
        let duration_sec = if include_tracks && !tracks.is_empty() {
            tracks.iter().map(|t| t.duration_sec).sum()
        } else {
            self.duration
        };
        let mut awards = Vec::new();
        let mut seen_awards = std::collections::HashSet::new();
        for aw in &self.awards {
            let name = aw.name.trim();
            if !name.is_empty() && seen_awards.insert(name.to_ascii_lowercase()) {
                awards.push(name.to_string());
            }
        }
        let booklet_url = self
            .goodies
            .iter()
            .filter_map(|g| {
                let u = g.url.trim();
                let u = if u.is_empty() {
                    g.original_url.trim()
                } else {
                    u
                };
                if u.starts_with("https://") && u.to_ascii_lowercase().ends_with(".pdf") {
                    Some(u.to_string())
                } else {
                    None
                }
            })
            .next()
            .unwrap_or_default();

        Album {
            id: format!("qobuz:{}", self.id),
            source: SOURCE_QOBUZ.into(),
            source_type: SOURCE_TYPE_REMOTE.into(),
            provider: SOURCE_QOBUZ.into(),
            external_id: self.id.clone(),
            title: self.title.trim().to_string(),
            artist: self.artist.name.trim().to_string(),
            genre: self.genre.name.trim().to_string(),
            label: self.label.name.trim().to_string(),
            description: clean_qobuz_text(&self.description),
            copyright: self.copyright.trim().to_string(),
            release_date: self.release_date_original.trim().to_string(),
            awards,
            booklet_url,
            credits,
            year,
            track_count,
            duration_sec,
            artwork_url: self.image.best(),
            primary_format: "FLAC".into(),
            max_bit_depth: self.maximum_bit_depth,
            max_sample_rate: normalize_sample_rate_hz(self.maximum_sampling_rate),
            tracks,
            available: true,
            ..Default::default()
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct QPerformer {
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct QTrack {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub track_number: i32,
    #[serde(default)]
    pub media_number: i32,
    #[serde(default)]
    pub duration: i32,
    #[serde(default)]
    pub maximum_sampling_rate: f64,
    #[serde(default)]
    pub maximum_bit_depth: i32,
    #[serde(default)]
    pub parental_warning: bool,
    #[serde(default)]
    pub performers: String,
    #[serde(default)]
    pub performer: QPerformer,
    #[serde(default)]
    pub album: Option<Box<QAlbum>>,
}

impl QTrack {
    pub fn to_library_track(&self, parent_album: Option<&QAlbum>) -> Track {
        let alb = self.album.as_deref().or(parent_album);
        let mut title = self.title.trim().to_string();
        let ver = self.version.trim();
        if !ver.is_empty() && !title.to_lowercase().contains(&ver.to_lowercase()) {
            title = format!("{title} ({ver})");
        }
        let mut artist = self.performer.name.trim().to_string();
        let mut album_artist = String::new();
        let mut album_title = String::new();
        let mut album_id = String::new();
        let mut genre = String::new();
        let mut year = 0i32;
        let mut artwork = String::new();

        if let Some(a) = alb {
            album_title = a.title.trim().to_string();
            album_artist = a.artist.name.trim().to_string();
            if artist.is_empty() {
                artist = album_artist.clone();
            }
            if !a.id.is_empty() {
                album_id = format!("qobuz:{}", a.id);
            }
            genre = a.genre.name.trim().to_string();
            year = a
                .release_date_original
                .get(..4)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            artwork = a.image.best();
        }
        let mut sr = normalize_sample_rate_hz(self.maximum_sampling_rate);
        let mut bd = self.maximum_bit_depth;
        if sr == 0
            && let Some(a) = alb
        {
            sr = normalize_sample_rate_hz(a.maximum_sampling_rate);
        }
        if bd == 0
            && let Some(a) = alb
        {
            bd = a.maximum_bit_depth;
        }

        Track {
            id: format!("qobuz:{}", self.id),
            source: SOURCE_QOBUZ.into(),
            source_type: SOURCE_TYPE_REMOTE.into(),
            provider: SOURCE_QOBUZ.into(),
            external_id: self.id.to_string(),
            title,
            artist,
            album_artist,
            album: album_title,
            album_id,
            genre,
            track_number: self.track_number,
            disc_number: self.media_number,
            year,
            duration_sec: self.duration,
            sample_rate: sr,
            bit_depth: bd,
            channels: 2,
            mime: "audio/flac".into(),
            ext: ".flac".into(),
            artwork_url: artwork,
            explicit: self.parental_warning,
            qobuz_track_id: self.id,
            available: true,
            ..Default::default()
        }
    }
}

pub(crate) fn is_primary_artist_album_or_ep(qa: &QAlbum, artist_id: i64, artist_key: &str) -> bool {
    if qa.artist.id > 0 && artist_id > 0 {
        if qa.artist.id != artist_id {
            return false;
        }
    } else if !qa.artist.name.trim().is_empty()
        && !artist_key.is_empty()
        && norm_match_key(&qa.artist.name) != artist_key
    {
        return false;
    }

    let rtype = qa.release_type.trim().to_ascii_lowercase();
    if matches!(rtype.as_str(), "single" | "compilation" | "other") {
        return false;
    }
    if qa.tracks_count > 0 && qa.tracks_count < 3 {
        return false;
    }
    true
}

fn parse_album_credits(tracks: &[QTrack]) -> Vec<AlbumCredit> {
    let mut order: Vec<String> = Vec::new();
    let mut by_name: std::collections::HashMap<String, (String, Vec<String>)> =
        std::collections::HashMap::new();
    for tr in tracks {
        for part in tr.performers.split(" - ") {
            let trimmed = part.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Some((name_raw, roles_raw)) = trimmed.split_once(',') else {
                continue;
            };
            let name = name_raw.trim();
            if name.is_empty() {
                continue;
            }
            let key = name.to_lowercase();
            let entry = by_name.entry(key.clone()).or_insert_with(|| {
                order.push(key);
                (name.to_string(), Vec::new())
            });
            for role_raw in roles_raw.split(',') {
                let role = format_credit_role(role_raw);
                if !role.is_empty() && !entry.1.iter().any(|r| r.eq_ignore_ascii_case(&role)) {
                    entry.1.push(role);
                }
            }
        }
    }
    order
        .into_iter()
        .filter_map(|k| {
            let (name, roles) = by_name.remove(&k)?;
            if roles.is_empty() {
                None
            } else {
                Some(AlbumCredit { name, roles })
            }
        })
        .collect()
}

fn format_credit_role(raw: &str) -> String {
    let s = raw.trim();
    if s.is_empty() {
        return String::new();
    }
    match s {
        "MainArtist" => "Main Artist".into(),
        "FeaturedArtist" => "Featured Artist".into(),
        "AssociatedPerformer" => "Performer".into(),
        "ComposerLyricist" => "Composer / Lyricist".into(),
        "MixingEngineer" => "Mixing Engineer".into(),
        "MasteringEngineer" => "Mastering Engineer".into(),
        "RecordingEngineer" => "Recording Engineer".into(),
        "AssistantRecordingEngineer" => "Assistant Engineer".into(),
        "StudioPersonnel" | "MusicPublisher" => String::new(),
        _ => s.to_string(),
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct QDiscoverArtist {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub roles: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct QDiscoverDates {
    #[serde(default)]
    pub original: String,
    #[serde(default)]
    pub stream: String,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct QDiscoverAudioInfo {
    #[serde(default)]
    pub maximum_sampling_rate: f64,
    #[serde(default)]
    pub maximum_bit_depth: i32,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct QDiscoverRights {
    #[serde(default)]
    pub streamable: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct QDiscoverAlbum {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub track_count: i32,
    #[serde(default)]
    pub image: QImage,
    #[serde(default)]
    pub artists: Vec<QDiscoverArtist>,
    #[serde(default)]
    pub genre: QGenre,
    #[serde(default)]
    pub dates: QDiscoverDates,
    #[serde(default)]
    pub audio_info: QDiscoverAudioInfo,
    #[serde(default)]
    pub rights: QDiscoverRights,
    #[serde(default)]
    pub ai_generated: bool,
}

impl QDiscoverAlbum {
    pub fn primary_artist(&self) -> String {
        for a in &self.artists {
            if a.roles.iter().any(|r| r == "main-artist") && !a.name.trim().is_empty() {
                return a.name.trim().to_string();
            }
        }
        for a in &self.artists {
            if !a.name.trim().is_empty() {
                return a.name.trim().to_string();
            }
        }
        String::new()
    }

    pub fn to_library_album(&self) -> Album {
        let date_str = if !self.dates.original.is_empty() {
            &self.dates.original
        } else {
            &self.dates.stream
        };
        let year = date_str.get(..4).and_then(|s| s.parse().ok()).unwrap_or(0);
        Album {
            id: format!("qobuz:{}", self.id),
            source: SOURCE_QOBUZ.into(),
            source_type: SOURCE_TYPE_REMOTE.into(),
            provider: SOURCE_QOBUZ.into(),
            external_id: self.id.clone(),
            title: self.title.trim().to_string(),
            artist: self.primary_artist(),
            genre: self.genre.name.trim().to_string(),
            year,
            track_count: self.track_count,
            artwork_url: self.image.best(),
            primary_format: "FLAC".into(),
            max_bit_depth: self.audio_info.maximum_bit_depth,
            max_sample_rate: normalize_sample_rate_hz(self.audio_info.maximum_sampling_rate),
            available: true,
            ..Default::default()
        }
    }
}

pub(crate) fn convert_discover_albums(items: &[QDiscoverAlbum]) -> Vec<Album> {
    let mut out = Vec::with_capacity(items.len());
    let mut seen = HashSet::with_capacity(items.len());
    for qa in items {
        if qa.id.is_empty() || qa.ai_generated {
            continue;
        }
        if qa.rights.streamable == Some(false) {
            continue;
        }
        if seen.insert(qa.id.clone()) {
            out.push(qa.to_library_album());
        }
    }
    out
}
