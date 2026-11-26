use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;

use crate::library::{PREF_MANUAL_RENDERERS, PREF_ZONE_QUALITIES, Track};
use crate::linkplay;
use crate::player::{RegisterRequest, StateReport, UpdateRequest};
use crate::queue::{AudioQuality, RepeatMode};
use crate::upnp::{self, Controller};

use super::AppState;
use super::errors::{ApiError, ApiResult, parse_strict_json};

// --- Zones & Discovery ---

pub async fn list_zones(State(st): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "zones": st.manager.list_zones() }))
}

pub async fn get_zone(
    State(st): State<Arc<AppState>>,
    Path(zone_id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let snap = st.manager.zone_snapshot(&zone_id)?;
    Ok(Json(serde_json::to_value(snap).unwrap_or_default()))
}

pub async fn discover_zones(State(st): State<Arc<AppState>>) -> ApiResult<Json<serde_json::Value>> {
    let found = upnp::discover().await?;
    let count = found.len();
    for dev in found {
        st.manager.register_renderer(linkplay::wrap(dev));
    }
    let cast_found = crate::cast::discover_cast(Duration::from_secs(2)).await;
    let total = count + cast_found.len();
    for dev in cast_found {
        st.manager.register_renderer(dev);
    }
    Ok(Json(serde_json::json!({
        "discovered": total,
        "zones": st.manager.list_zones(),
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConnectZoneReq {
    url: String,
}

pub async fn connect_zone(
    State(st): State<Arc<AppState>>,
    body: Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let req: ConnectZoneReq = parse_strict_json(&body)?;
    if req.url.trim().is_empty() {
        return Err(ApiError::bad_request("url is required"));
    }
    let dev = upnp::connect_by_url(&req.url).await?;
    let info = dev.info();
    let loc = if info.location.is_empty() {
        req.url.trim().to_string()
    } else {
        info.location.clone()
    };
    st.manager.register_renderer(linkplay::wrap(dev));
    remember_manual_renderer(&st, &loc);
    let zone = st.manager.zone_snapshot(&info.id)?;
    Ok(Json(serde_json::json!({
        "zone": zone,
        "zones": st.manager.list_zones(),
    })))
}

pub async fn delete_zone(
    State(st): State<Arc<AppState>>,
    Path(zone_id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let snap = st.manager.zone_snapshot(&zone_id)?;
    if snap.renderer.kind == "player" {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "PLAYER_MANAGED_ZONE",
            "remote player zones must be unregistered via DELETE /api/v1/players/{zoneId}",
            false,
        ));
    }
    if !snap.renderer.location.is_empty() {
        forget_manual_renderer(&st, &snap.renderer.location);
    }
    st.manager.remove_renderer(&zone_id);
    Ok(Json(serde_json::json!({
        "removed": true,
        "zoneId": zone_id,
    })))
}

fn remember_manual_renderer(st: &AppState, raw_url: &str) {
    let mut urls: Vec<String> = st
        .catalog
        .get_pref(PREF_MANUAL_RENDERERS)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if !urls.iter().any(|u| u == raw_url) {
        urls.push(raw_url.to_string());
        if urls.len() > 16 {
            urls = urls[urls.len() - 16..].to_vec();
        }
        if let Ok(enc) = serde_json::to_string(&urls) {
            let _ = st.catalog.set_pref(PREF_MANUAL_RENDERERS, &enc);
        }
    }
}

fn forget_manual_renderer(st: &AppState, loc: &str) {
    let mut urls: Vec<String> = st
        .catalog
        .get_pref(PREF_MANUAL_RENDERERS)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let before = urls.len();
    urls.retain(|u| u != loc);
    if urls.len() != before
        && let Ok(enc) = serde_json::to_string(&urls)
    {
        let _ = st.catalog.set_pref(PREF_MANUAL_RENDERERS, &enc);
    }
}

// --- Queue Management ---

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct QueueSource {
    #[serde(rename = "type")]
    source_type: String,
    #[serde(default)]
    album_id: String,
    #[serde(default)]
    track_ids: Vec<String>,
}

async fn resolve_source(st: &AppState, src: QueueSource) -> ApiResult<Vec<Track>> {
    match src.source_type.as_str() {
        "album" => {
            let id = src.album_id.trim();
            if id.is_empty() {
                return Err(ApiError::bad_request("source.albumId is required"));
            }
            if let Some(alb) = st.catalog.album(id)
                && !alb.tracks.is_empty()
                && !alb.is_partial
            {
                return Ok(alb.tracks);
            }
            if id.starts_with("qobuz:") {
                let qb = st.qobuz.as_ref().ok_or_else(|| {
                    ApiError::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "QOBUZ_UNAVAILABLE",
                        "qobuz not configured",
                        false,
                    )
                })?;
                let alb = qb.get_album(id).await?;
                return Ok(alb.tracks);
            }
            let alb = st.catalog.album(id).ok_or_else(|| {
                ApiError::not_found("ALBUM_NOT_FOUND", format!("album {id:?} not found"))
            })?;
            Ok(alb.tracks)
        }
        "tracks" => {
            if src.track_ids.is_empty() {
                return Err(ApiError::bad_request("source.trackIds must not be empty"));
            }
            if src.track_ids.len() > 500 {
                return Err(ApiError::bad_request(
                    "source.trackIds exceeds maximum of 500 tracks",
                ));
            }
            let mut out = Vec::with_capacity(src.track_ids.len());
            for raw in src.track_ids {
                let id = raw.trim();
                if let Some(tr) = st.catalog.track(id) {
                    out.push(tr);
                    continue;
                }
                if let Some(rest) = id.strip_prefix("qobuz:") {
                    let qb = st.qobuz.as_ref().ok_or_else(|| {
                        ApiError::new(
                            StatusCode::SERVICE_UNAVAILABLE,
                            "QOBUZ_UNAVAILABLE",
                            "qobuz not configured",
                            false,
                        )
                    })?;
                    let num: i64 = rest.parse().map_err(|_| {
                        ApiError::bad_request(format!("invalid qobuz trackId {id:?}"))
                    })?;
                    let tr = qb.resolve_track(num).await?;
                    out.push(tr);
                    continue;
                }
                return Err(ApiError::not_found(
                    "TRACK_NOT_FOUND",
                    format!("track {id:?} not found"),
                ));
            }
            Ok(out)
        }
        _ => Err(ApiError::bad_request(
            "source.type must be 'album' or 'tracks'",
        )),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReplaceQueueReq {
    source: QueueSource,
    #[serde(default)]
    start_index: i32,
}

pub async fn replace_queue(
    State(st): State<Arc<AppState>>,
    Path(zone_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let req: ReplaceQueueReq = parse_strict_json(&body)?;
    let tracks = resolve_source(&st, req.source).await?;
    let snap = st
        .manager
        .set_queue(&zone_id, tracks, req.start_index, true)
        .await?;
    Ok(Json(serde_json::to_value(snap).unwrap_or_default()))
}

pub async fn clear_queue(
    State(st): State<Arc<AppState>>,
    Path(zone_id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let snap = st.manager.clear(&zone_id).await?;
    Ok(Json(serde_json::to_value(snap).unwrap_or_default()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AddQueueItemsReq {
    source: QueueSource,
    #[serde(default)]
    position: String,
    #[serde(default)]
    autoplay: Option<bool>,
}

pub async fn add_queue_items(
    State(st): State<Arc<AppState>>,
    Path(zone_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let req: AddQueueItemsReq = parse_strict_json(&body)?;
    let pos = req.position.trim();
    if !pos.is_empty() && !pos.eq_ignore_ascii_case("end") && !pos.eq_ignore_ascii_case("next") {
        return Err(ApiError::bad_request(
            "position must be \"end\" or \"next\"",
        ));
    }
    let tracks = resolve_source(&st, req.source).await?;
    let autoplay = req.autoplay.unwrap_or(true);
    let snap = if pos.eq_ignore_ascii_case("next") {
        st.manager
            .play_next_with_autoplay(&zone_id, tracks, autoplay)
            .await?
    } else {
        st.manager
            .enqueue_with_autoplay(&zone_id, tracks, autoplay)
            .await?
    };
    Ok(Json(serde_json::to_value(snap).unwrap_or_default()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct JumpQueueReq {
    index: Option<usize>,
    item_id: Option<String>,
}

pub async fn jump_queue(
    State(st): State<Arc<AppState>>,
    Path(zone_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let req: JumpQueueReq = parse_strict_json(&body)?;
    let snap = if let Some(i) = req.index {
        st.manager.play_index(&zone_id, i).await?
    } else if let Some(item_id) = req.item_id {
        st.manager
            .play_item_by_id(&zone_id, &item_id)
            .await
            .map_err(|e| match e {
                crate::queue::QueueError::InvalidIndex => ApiError::new(
                    StatusCode::NOT_FOUND,
                    "QUEUE_ITEM_NOT_FOUND",
                    "queue item not found",
                    false,
                ),
                other => other.into(),
            })?
    } else {
        return Err(ApiError::bad_request("index or itemId is required"));
    };
    Ok(Json(serde_json::to_value(snap).unwrap_or_default()))
}

pub async fn remove_queue_item(
    State(st): State<Arc<AppState>>,
    Path((zone_id, item_id)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let next = st
        .manager
        .remove_item_by_id(&zone_id, &item_id)
        .await
        .map_err(|e| match e {
            crate::queue::QueueError::InvalidIndex => ApiError::new(
                StatusCode::NOT_FOUND,
                "QUEUE_ITEM_NOT_FOUND",
                "queue item not found",
                false,
            ),
            other => other.into(),
        })?;
    Ok(Json(serde_json::to_value(next).unwrap_or_default()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MoveQueueItemReq {
    to_index: usize,
}

pub async fn move_queue_item(
    State(st): State<Arc<AppState>>,
    Path((zone_id, item_id)): Path<(String, String)>,
    body: Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let req: MoveQueueItemReq = parse_strict_json(&body)?;
    let next = st
        .manager
        .move_item_by_id(&zone_id, &item_id, req.to_index)
        .await
        .map_err(|e| match e {
            crate::queue::QueueError::InvalidIndex => ApiError::new(
                StatusCode::NOT_FOUND,
                "QUEUE_ITEM_NOT_FOUND",
                "queue item not found",
                false,
            ),
            other => other.into(),
        })?;
    Ok(Json(serde_json::to_value(next).unwrap_or_default()))
}

// --- Transport & Volume ---

pub async fn transport_play(
    State(st): State<Arc<AppState>>,
    Path(zone_id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(
        serde_json::to_value(st.manager.play(&zone_id).await?).unwrap_or_default(),
    ))
}

pub async fn transport_pause(
    State(st): State<Arc<AppState>>,
    Path(zone_id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(
        serde_json::to_value(st.manager.pause(&zone_id).await?).unwrap_or_default(),
    ))
}

pub async fn transport_stop(
    State(st): State<Arc<AppState>>,
    Path(zone_id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(
        serde_json::to_value(st.manager.stop(&zone_id).await?).unwrap_or_default(),
    ))
}

pub async fn transport_next(
    State(st): State<Arc<AppState>>,
    Path(zone_id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(
        serde_json::to_value(st.manager.next(&zone_id).await?).unwrap_or_default(),
    ))
}

pub async fn transport_prev(
    State(st): State<Arc<AppState>>,
    Path(zone_id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(
        serde_json::to_value(st.manager.previous(&zone_id).await?).unwrap_or_default(),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SeekReq {
    position_sec: i32,
}

pub async fn transport_seek(
    State(st): State<Arc<AppState>>,
    Path(zone_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let req: SeekReq = parse_strict_json(&body)?;
    Ok(Json(
        serde_json::to_value(st.manager.seek(&zone_id, req.position_sec).await?)
            .unwrap_or_default(),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct VolumeReq {
    volume: i32,
}

pub async fn set_volume(
    State(st): State<Arc<AppState>>,
    Path(zone_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let req: VolumeReq = parse_strict_json(&body)?;
    Ok(Json(
        serde_json::to_value(st.manager.set_volume(&zone_id, req.volume).await?)
            .unwrap_or_default(),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MuteReq {
    muted: bool,
}

pub async fn set_mute(
    State(st): State<Arc<AppState>>,
    Path(zone_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let req: MuteReq = parse_strict_json(&body)?;
    Ok(Json(
        serde_json::to_value(st.manager.set_mute(&zone_id, req.muted).await?).unwrap_or_default(),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct QualityReq {
    quality: String,
}

pub async fn set_zone_quality(
    State(st): State<Arc<AppState>>,
    Path(zone_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let req: QualityReq = parse_strict_json(&body)?;
    let q = AudioQuality::parse(&req.quality)
        .ok_or_else(|| ApiError::bad_request(format!("invalid quality {:?}", req.quality)))?;
    let snap = st.manager.set_audio_quality(&zone_id, q).await?;
    // Persist per-zone quality map
    let mut map: HashMap<String, String> = st
        .catalog
        .get_pref(PREF_ZONE_QUALITIES)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    map.insert(zone_id, q.as_str().into());
    if let Ok(enc) = serde_json::to_string(&map) {
        let _ = st.catalog.set_pref(PREF_ZONE_QUALITIES, &enc);
    }
    Ok(Json(serde_json::to_value(snap).unwrap_or_default()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ShuffleReq {
    enabled: bool,
}

pub async fn set_zone_shuffle(
    State(st): State<Arc<AppState>>,
    Path(zone_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let req: ShuffleReq = parse_strict_json(&body)?;
    Ok(Json(
        serde_json::to_value(st.manager.set_shuffle(&zone_id, req.enabled).await?)
            .unwrap_or_default(),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RepeatReq {
    mode: String,
}

pub async fn set_zone_repeat(
    State(st): State<Arc<AppState>>,
    Path(zone_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let req: RepeatReq = parse_strict_json(&body)?;
    let mode = RepeatMode::parse(&req.mode)
        .ok_or_else(|| ApiError::bad_request(format!("invalid repeat mode {:?}", req.mode)))?;
    Ok(Json(
        serde_json::to_value(st.manager.set_repeat(&zone_id, mode).await?).unwrap_or_default(),
    ))
}

// --- Live SSE Events (/api/v1/events) ---

pub async fn events_sse(State(st): State<Arc<AppState>>, RawQuery(raw_q): RawQuery) -> Response {
    let zone_filters: Vec<String> = raw_q
        .as_deref()
        .map(|qs| {
            url::form_urlencoded::parse(qs.as_bytes())
                .filter(|(k, v)| k == "zone" && !v.trim().is_empty())
                .map(|(_, v)| v.trim().to_string())
                .collect()
        })
        .unwrap_or_default();
    let rx = st.manager.subscribe();

    // Build initial snapshot event matching OpenAPI SnapshotEvent
    let zones = if zone_filters.is_empty() {
        st.manager.list_zones()
    } else {
        st.manager
            .list_zones()
            .into_iter()
            .filter(|z| zone_filters.iter().any(|f| f == &z.zone_id))
            .collect()
    };
    let qobuz_status =
        st.qobuz
            .as_ref()
            .map(|q| q.status())
            .unwrap_or_else(|| crate::qobuz::Status {
                max_format_id: crate::qobuz::FORMAT_FLAC_192,
                max_quality: crate::qobuz::quality_label(crate::qobuz::FORMAT_FLAC_192).into(),
                ..Default::default()
            });
    let init_payload = serde_json::json!({
        "serverTime": chrono::Utc::now(),
        "zones": zones,
        "settings": st.current_settings(),
        "qobuz": qobuz_status,
        "library": st.catalog.status(),
        "sonic": st.sonic().map(|w| w.status()),
        "enrichment": st.enrichment().map(|w| w.status()),
    });
    let init_event = SseEvent::default()
        .retry(Duration::from_secs(3))
        .event("snapshot")
        .data(init_payload.to_string());

    let shutdown = st.shutdown.clone();
    let stream = futures_util::stream::once(async move { Ok::<_, Infallible>(init_event) }).chain(
        futures_util::stream::unfold(
            (rx, zone_filters, shutdown, false),
            |(mut rx, filters, shutdown, done)| async move {
                if done {
                    return None;
                }
                loop {
                    tokio::select! {
                        biased;
                        _ = shutdown.cancelled() => return None,
                        res = rx.recv() => match res {
                            Ok(ev) => {
                                if !filters.is_empty()
                                    && !ev.zone_id.is_empty()
                                    && !filters.iter().any(|f| f == &ev.zone_id)
                                {
                                    continue;
                                }
                                let data_str = filter_sse_event_data(&ev, &filters);
                                let sse_ev = SseEvent::default()
                                    .id(ev.id.to_string())
                                    .event(ev.event_type)
                                    .data(data_str);
                                return Some((Ok(sse_ev), (rx, filters, shutdown, false)));
                            }
                            Err(RecvError::Lagged(_)) => {
                                let resync = SseEvent::default()
                                    .event("resync")
                                    .data(serde_json::json!({ "reason": "subscriber_lagged" }).to_string());
                                return Some((Ok(resync), (rx, filters, shutdown, true)));
                            }
                            Err(RecvError::Closed) => return None,
                        }
                    }
                }
            },
        ),
    );

    let mut resp = Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(20))
                .text("ping"),
        )
        .into_response();
    resp.headers_mut().insert(
        axum::http::HeaderName::from_static("x-accel-buffering"),
        axum::http::HeaderValue::from_static("no"),
    );
    resp
}

fn filter_sse_event_data(ev: &crate::queue::Event, filters: &[String]) -> String {
    if !filters.is_empty()
        && ev.event_type == "zones.updated"
        && let Some(arr) = ev.data.get("zones").and_then(|v| v.as_array())
    {
        let kept: Vec<&serde_json::Value> = arr
            .iter()
            .filter(|z| {
                z.get("zoneId")
                    .and_then(|id| id.as_str())
                    .is_some_and(|id| filters.iter().any(|f| f == id))
            })
            .collect();
        serde_json::json!({ "zones": kept }).to_string()
    } else {
        ev.data.to_string()
    }
}

use futures_util::StreamExt;

// --- Remote HTTP/SSE Players ---

const PLAYER_COOKIE_NAME: &str = "tacet_player_session";
const LEGACY_PLAYER_COOKIE_PATH: &str = "/api/v1/players";
const PLAYER_COOKIE_MAX_AGE_SECS: u64 = 86400 * 30;

fn player_cookie_path(zone_id: &str) -> String {
    // Match JavaScript encodeURIComponent (RFC 6265 cookie path matching is literal,
    // and browsers request /api/v1/players/player%3A<id>/commands).
    let mut encoded = String::with_capacity(zone_id.len() + 4);
    for b in zone_id.bytes() {
        let unreserved = b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            );
        if unreserved {
            encoded.push(b as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{b:02X}");
        }
    }
    format!("{LEGACY_PLAYER_COOKIE_PATH}/{encoded}")
}

fn append_player_cookies(resp: &mut Response, zone_id: &str, token: Option<&str>) {
    // Always expire any pre-existing broad-path cookie from older builds so browsers
    // never send both a stale /api/v1/players cookie and a scoped zone cookie.
    let clear_legacy = format!(
        "{PLAYER_COOKIE_NAME}=; Path={LEGACY_PLAYER_COOKIE_PATH}; HttpOnly; SameSite=Lax; Max-Age=0"
    );
    if let Ok(hv) = HeaderValue::from_str(&clear_legacy) {
        resp.headers_mut().append(header::SET_COOKIE, hv);
    }
    let scoped_path = player_cookie_path(zone_id);
    let scoped = match token {
        Some(tok) => format!(
            "{PLAYER_COOKIE_NAME}={tok}; Path={scoped_path}; HttpOnly; SameSite=Lax; Max-Age={PLAYER_COOKIE_MAX_AGE_SECS}"
        ),
        None => {
            format!("{PLAYER_COOKIE_NAME}=; Path={scoped_path}; HttpOnly; SameSite=Lax; Max-Age=0")
        }
    };
    if let Ok(hv) = HeaderValue::from_str(&scoped) {
        resp.headers_mut().append(header::SET_COOKIE, hv);
    }
}

fn extract_player_token(headers: &HeaderMap, query: &HashMap<String, String>) -> String {
    if let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok())
        && let Some((scheme, tok)) = auth.trim().split_once(' ')
        && scheme.eq_ignore_ascii_case("bearer")
    {
        return tok.trim().to_string();
    }
    if let Some(tok) = headers.get("x-player-token").and_then(|v| v.to_str().ok()) {
        return tok.trim().to_string();
    }
    if let Some(cookie_hdr) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) {
        for part in cookie_hdr.split(';') {
            if let Some((k, v)) = part.trim().split_once('=')
                && k.trim() == PLAYER_COOKIE_NAME
                && !v.trim().is_empty()
            {
                return v.trim().to_string();
            }
        }
    }
    query.get("token").cloned().unwrap_or_default()
}

pub async fn register_player(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> ApiResult<Response> {
    let req: RegisterRequest = parse_strict_json(&body)?;
    let existing_token = extract_player_token(&headers, &q);
    let reg = st.players.register(req, &existing_token)?;
    let mut resp = Json(serde_json::to_value(&reg).unwrap_or_default()).into_response();
    append_player_cookies(&mut resp, &reg.zone_id, Some(&reg.token));
    Ok(resp)
}

pub async fn update_player(
    State(st): State<Arc<AppState>>,
    Path(zone_id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let req: UpdateRequest = parse_strict_json(&body)?;
    let tok = extract_player_token(&headers, &q);
    let info = st.players.update(&zone_id, &tok, req)?;
    Ok(Json(serde_json::json!({
        "zoneId": zone_id,
        "renderer": info,
    })))
}

pub async fn player_commands_sse(
    State(st): State<Arc<AppState>>,
    Path(zone_id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    if headers
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.eq_ignore_ascii_case("cross-site"))
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "CSRF_REJECTED",
            "cross-site player command stream blocked",
            false,
        ));
    }
    let tok = extract_player_token(&headers, &q);
    let rp = st.players.get(&zone_id, &tok)?;
    let stream_handle = rp.attach_stream();
    let stream_id = stream_handle.id;
    let reg = Arc::clone(&st.players);
    let rp_drop = Arc::clone(&rp);

    struct DetachGuard {
        reg: Arc<crate::player::Registry>,
        rp: Arc<crate::player::RemotePlayer>,
        stream_id: u64,
    }
    impl Drop for DetachGuard {
        fn drop(&mut self) {
            self.reg.detach_stream(&self.rp, self.stream_id);
        }
    }
    let guard = DetachGuard {
        reg,
        rp: rp_drop,
        stream_id,
    };

    let zone_snap = st.manager.zone_snapshot(&zone_id).ok();
    let mut connected_payload = serde_json::json!({
        "zoneId": zone_id,
        "streamId": stream_id,
        "serverTime": chrono::Utc::now(),
    });
    if let Some(z) = zone_snap {
        connected_payload["zone"] = serde_json::to_value(z).unwrap_or_default();
    }
    let connected = SseEvent::default()
        .retry(Duration::from_secs(3))
        .event("connected")
        .data(connected_payload.to_string());

    let shutdown = st.shutdown.clone();
    let cmd_stream = futures_util::stream::unfold(
        (
            stream_handle.commands,
            stream_handle.replaced,
            guard,
            shutdown,
            false,
        ),
        |(mut cmds, mut replaced, guard, shutdown, done)| async move {
            if done {
                return None;
            }
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => None,
                _ = &mut replaced => {
                    let ev = SseEvent::default()
                        .event("replaced")
                        .data(serde_json::json!({ "reason": "player session taken over by another connection" }).to_string());
                    Some((Ok(ev), (cmds, replaced, guard, shutdown, true)))
                }
                cmd = cmds.recv() => {
                    let cmd = cmd?;
                    let data = serde_json::to_string(&cmd).unwrap_or_default();
                    let ev = SseEvent::default().id(cmd.id).event("command").data(data);
                    Some((Ok(ev), (cmds, replaced, guard, shutdown, false)))
                }
            }
        },
    );

    let full =
        futures_util::stream::once(async move { Ok::<_, Infallible>(connected) }).chain(cmd_stream);
    let mut resp = Sse::new(full)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(20))
                .text("keepalive"),
        )
        .into_response();
    resp.headers_mut().insert(
        axum::http::HeaderName::from_static("x-accel-buffering"),
        axum::http::HeaderValue::from_static("no"),
    );
    Ok(resp)
}

pub async fn player_state(
    State(st): State<Arc<AppState>>,
    Path(zone_id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> ApiResult<StatusCode> {
    let tok = extract_player_token(&headers, &q);
    let rp = st.players.get(&zone_id, &tok)?;
    let report: StateReport = parse_strict_json(&body)?;
    rp.report_state(report);
    Ok(StatusCode::NO_CONTENT)
}

pub async fn unregister_player(
    State(st): State<Arc<AppState>>,
    Path(zone_id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    let tok = extract_player_token(&headers, &q);
    st.players.unregister(&zone_id, &tok)?;
    let mut resp = Json(serde_json::json!({
        "unregistered": true,
        "zoneId": zone_id,
    }))
    .into_response();
    append_player_cookies(&mut resp, &zone_id, None);
    Ok(resp)
}
