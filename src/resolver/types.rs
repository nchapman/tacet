use serde::{Deserialize, Serialize};

use crate::library::EnrichmentStats;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveAlbumRequest {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub artist: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub album: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub upc: String,
    pub tracks: Vec<ResolveAlbumTrackInput>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveAlbumTrackInput {
    pub index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disc: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track: Option<i32>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_sec: Option<i32>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub isrc: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub fingerprint: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ResolveAlbumResponse {
    pub matched: bool,
    pub confidence: f64,
    #[serde(default)]
    pub matched_by: Vec<String>,
    pub album: Option<ResolvedAlbumInfo>,
    #[serde(default)]
    pub tracks: Vec<ResolvedAlbumTrack>,
    #[serde(default)]
    pub unmatched_indices: Vec<usize>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ResolvedAlbumInfo {
    #[serde(default)]
    pub release_mbid: String,
    #[serde(default)]
    pub release_group_mbid: String,
    #[serde(default)]
    pub artist_mbid: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    pub year: Option<i32>,
    #[serde(default)]
    pub upc: String,
    #[serde(default)]
    pub genres: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ResolvedAlbumTrack {
    pub index: usize,
    pub matched: bool,
    pub confidence: f64,
    #[serde(default)]
    pub matched_by: Vec<String>,
    #[serde(default)]
    pub recording_mbid: String,
    #[serde(default)]
    pub artist_mbid: String,
    #[serde(default)]
    pub isrc: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    pub duration_sec: Option<i32>,
    #[serde(default)]
    pub genres: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveTracksRequest {
    pub tracks: Vec<ResolveTrackInput>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveTrackInput {
    pub index: usize,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub isrc: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub artist: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_sec: Option<i32>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub fingerprint: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ResolveTracksResponse {
    #[serde(default)]
    pub tracks: Vec<ResolvedLooseTrack>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ResolvedLooseTrack {
    pub index: usize,
    pub matched: bool,
    pub confidence: f64,
    #[serde(default)]
    pub matched_by: Vec<String>,
    #[serde(default)]
    pub recording_mbid: String,
    #[serde(default)]
    pub artist_mbid: String,
    #[serde(default)]
    pub release_mbid: String,
    #[serde(default)]
    pub release_group_mbid: String,
    #[serde(default)]
    pub isrc: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    pub duration_sec: Option<i32>,
    #[serde(default)]
    pub genres: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrichmentStatus {
    pub enabled: bool,
    pub running: bool,
    #[serde(flatten)]
    pub stats: EnrichmentStats,
    pub last_run_at: Option<i64>,
    pub last_error: Option<String>,
}
