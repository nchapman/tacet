use serde::{Deserialize, Deserializer, Serialize};

use crate::library::EnrichmentStats;

/// Deserializes `Option<i64>` leniently from either JSON integers or floats (e.g. `157813.4` -> `157813`).
fn deserialize_opt_i64_rounded<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::<f64>::deserialize(deserializer)?;
    Ok(opt.filter(|v| v.is_finite()).map(|v| v.round() as i64))
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RecordingResolveInput {
    pub index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disc: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track: Option<i32>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub artist_name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub release: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub isrc: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub recording_mbid: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub fingerprint: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ReleaseResolveRequest {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub artist_name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub barcode: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub release_mbid: String,
    pub recordings: Vec<RecordingResolveInput>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ResolvedRelease {
    pub release_mbid: String,
    pub release_group_mbid: String,
    pub artist_mbid: String,
    pub title: String,
    pub artist_name: String,
    pub year: Option<i32>,
    pub barcode: String,
    pub genres: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ResolvedRecording {
    pub index: usize,
    pub matched: bool,
    pub confidence: f64,
    pub matched_by: Vec<String>,
    pub recording_mbid: String,
    pub artist_mbid: String,
    pub release_mbid: String,
    pub release_group_mbid: String,
    pub isrc: String,
    pub title: String,
    pub artist_name: String,
    #[serde(default, deserialize_with = "deserialize_opt_i64_rounded")]
    pub duration_ms: Option<i64>,
    pub genres: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ReleaseResolveResponse {
    pub matched: bool,
    pub confidence: f64,
    pub matched_by: Vec<String>,
    pub release: Option<ResolvedRelease>,
    pub recordings: Vec<ResolvedRecording>,
    pub unmatched_indices: Vec<usize>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RecordingBatchResolveRequest {
    pub recordings: Vec<RecordingResolveInput>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RecordingBatchResolveResponse {
    pub recordings: Vec<ResolvedRecording>,
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
    fn resolved_recording_duration_ms_accepts_float_and_int_json() {
        let int_rec: ResolvedRecording = serde_json::from_str(
            r#"{"index":0,"matched":true,"confidence":0.95,"duration_ms":208410}"#,
        )
        .unwrap();
        assert_eq!(int_rec.duration_ms, Some(208_410));

        let float_rec: ResolvedRecording = serde_json::from_str(
            r#"{"index":1,"matched":true,"confidence":0.9,"duration_ms":208410.6}"#,
        )
        .unwrap();
        assert_eq!(float_rec.duration_ms, Some(208_411));

        let none_rec: ResolvedRecording =
            serde_json::from_str(r#"{"index":2,"matched":false,"duration_ms":null}"#).unwrap();
        assert_eq!(none_rec.duration_ms, None);
    }
}
