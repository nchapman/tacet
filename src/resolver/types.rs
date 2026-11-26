use serde::{Deserialize, Deserializer, Serialize};

use crate::library::EnrichmentStats;

/// Deserializes `Option<i32>` leniently from either JSON integers or floats (e.g. `208.41` -> `208`).
fn deserialize_opt_i32_rounded<'de, D>(deserializer: D) -> Result<Option<i32>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::<f64>::deserialize(deserializer)?;
    Ok(opt.filter(|v| v.is_finite()).map(|v| v.round() as i32))
}

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
    #[serde(default, deserialize_with = "deserialize_opt_i32_rounded")]
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
    #[serde(default, deserialize_with = "deserialize_opt_i32_rounded")]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_track_duration_accepts_float_and_int_json() {
        let alb: ResolvedAlbumTrack = serde_json::from_str(
            r#"{"index":0,"matched":true,"confidence":0.95,"durationSec":208.41}"#,
        )
        .unwrap();
        assert_eq!(alb.duration_sec, Some(208));

        let loose: ResolvedLooseTrack = serde_json::from_str(
            r#"{"index":1,"matched":true,"confidence":0.9,"durationSec":208.6}"#,
        )
        .unwrap();
        assert_eq!(loose.duration_sec, Some(209));

        let none: ResolvedAlbumTrack =
            serde_json::from_str(r#"{"index":2,"matched":false,"durationSec":null}"#).unwrap();
        assert_eq!(none.duration_sec, None);
    }
}
