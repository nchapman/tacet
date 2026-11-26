use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};

use crate::library::PlayContext;
use crate::queue::ZoneSnapshot;
use crate::radio::{RadioError, RadioRequest, RadioScope, RadioSeed};

use super::AppState;
use super::errors::{ApiError, ApiResult, parse_strict_json};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RadioBody {
    kind: String,
    id: String,
    #[serde(default)]
    limit: usize,
    #[serde(default)]
    scope: RadioScope,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StationSummary {
    pub seed_kind: String,
    pub seed_label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed_track_id: Option<String>,
    pub track_count: usize,
    pub source_counts: BTreeMap<String, usize>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartRadioResponse {
    pub zone: ZoneSnapshot,
    pub station: StationSummary,
}

fn parse_radio_request(body: Bytes) -> ApiResult<RadioRequest> {
    let req: RadioBody = parse_strict_json(&body)?;
    let id = req.id.trim().to_string();
    if id.is_empty() {
        return Err(ApiError::bad_request("radio seed id is required"));
    }
    let seed = match req.kind.trim().to_lowercase().as_str() {
        "track" => RadioSeed::Track { id },
        "album" => RadioSeed::Album { id },
        "artist" => RadioSeed::Artist { id },
        other => {
            return Err(ApiError::bad_request(format!(
                "invalid radio kind: {other} (expected track, album, or artist)"
            )));
        }
    };
    Ok(RadioRequest {
        seed,
        limit: req.limit,
        scope: req.scope,
    })
}

fn map_radio_error(err: RadioError) -> ApiError {
    match err {
        RadioError::SeedNotFound(msg) => ApiError::not_found("RADIO_SEED_NOT_FOUND", msg),
        RadioError::InvalidRequest(msg) => ApiError::bad_request(msg),
        RadioError::EmptyStation => ApiError::not_found(
            "RADIO_EMPTY",
            "no playable tracks available for radio station",
        ),
    }
}

pub async fn start_zone_radio(
    State(state): State<Arc<AppState>>,
    Path(zone_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<StartRadioResponse>> {
    let req = parse_radio_request(body)?;
    let station = state
        .radio()
        .build_station(req)
        .await
        .map_err(map_radio_error)?;

    let track_count = station.tracks.len();
    let zone = state
        .manager
        .start_radio_queue(
            &zone_id,
            station.seed_track_id.as_deref(),
            station.tracks,
            PlayContext::Radio,
        )
        .await
        .map_err(ApiError::from)?;

    Ok(Json(StartRadioResponse {
        zone,
        station: StationSummary {
            seed_kind: station.seed_kind,
            seed_label: station.seed_label,
            seed_track_id: station.seed_track_id,
            track_count,
            source_counts: station.source_counts,
        },
    }))
}

pub async fn preview_radio(
    State(state): State<Arc<AppState>>,
    body: Bytes,
) -> ApiResult<Json<crate::radio::RadioStation>> {
    let req = parse_radio_request(body)?;
    let station = state
        .radio()
        .build_station(req)
        .await
        .map_err(map_radio_error)?;
    Ok(Json(station))
}
