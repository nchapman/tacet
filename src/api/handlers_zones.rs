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
    #[allow(dead_code)]
    autoplay: Option<bool>,
}

pub async fn add_queue_items(
    State(st): State<Arc<AppState>>,
    Path(zone_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let req: AddQueueItemsReq = parse_strict_json(&body)?;
    let tracks = resolve_source(&st, req.source).await?;
    let snap = if req.position.eq_ignore_ascii_case("next") {
        st.manager.play_next(&zone_id, tracks).await?
    } else {
        st.manager.enqueue(&zone_id, tracks).await?
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
    let idx = if let Some(i) = req.index {
        i
    } else if let Some(item_id) = req.item_id {
        let snap = st.manager.zone_snapshot(&zone_id)?;
        snap.queue
            .iter()
            .position(|it| it.item_id == item_id)
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "QUEUE_ITEM_NOT_FOUND",
                    "queue item not found",
                    false,
                )
            })?
    } else {
        return Err(ApiError::bad_request("index or itemId is required"));
    };
    let snap = st.manager.play_index(&zone_id, idx).await?;
    Ok(Json(serde_json::to_value(snap).unwrap_or_default()))
}

pub async fn remove_queue_item(
    State(st): State<Arc<AppState>>,
    Path((zone_id, item_id)): Path<(String, String)>,
) -> ApiResult<Json<serde_json::Value>> {
    let snap = st.manager.zone_snapshot(&zone_id)?;
    let idx = snap
        .queue
        .iter()
        .position(|it| it.item_id == item_id)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "QUEUE_ITEM_NOT_FOUND",
                "queue item not found",
                false,
            )
        })?;
    let next = st.manager.remove_item(&zone_id, idx).await?;
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
    let snap = st.manager.zone_snapshot(&zone_id)?;
    let from = snap
        .queue
        .iter()
        .position(|it| it.item_id == item_id)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "QUEUE_ITEM_NOT_FOUND",
                "queue item not found",
                false,
            )
        })?;
    let next = st.manager.move_item(&zone_id, from, req.to_index).await?;
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

pub async fn events_sse(
    State(st): State<Arc<AppState>>,
    RawQuery(raw_q): RawQuery,
) -> Sse<impl futures_util::Stream<Item = Result<SseEvent, Infallible>>> {
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
    let qobuz_status = st.qobuz.as_ref().map(|q| q.status());
    let init_payload = serde_json::json!({
        "serverTime": chrono::Utc::now(),
        "zones": zones,
        "settings": st.current_settings(),
        "qobuz": qobuz_status,
        "library": st.catalog.status(),
    });
    let init_event = SseEvent::default()
        .event("snapshot")
        .data(init_payload.to_string());

    let stream = futures_util::stream::once(async move { Ok(init_event) }).chain(
        futures_util::stream::unfold((rx, zone_filters), |(mut rx, filters)| async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        if !filters.is_empty()
                            && !ev.zone_id.is_empty()
                            && !filters.iter().any(|f| f == &ev.zone_id)
                        {
                            continue;
                        }
                        let sse_ev = SseEvent::default()
                            .id(ev.id.to_string())
                            .event(ev.event_type)
                            .data(ev.data.to_string());
                        return Some((Ok(sse_ev), (rx, filters)));
                    }
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => return None,
                }
            }
        }),
    );

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(20))
            .text("ping"),
    )
}

use futures_util::StreamExt;

// --- Remote HTTP/SSE Players ---

const PLAYER_COOKIE_NAME: &str = "tacet_player_session";

fn extract_player_token(headers: &HeaderMap, query: &HashMap<String, String>) -> String {
    if let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok())
        && let Some(tok) = auth
            .strip_prefix("Bearer ")
            .or_else(|| auth.strip_prefix("bearer "))
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
    let cookie = format!(
        "{PLAYER_COOKIE_NAME}={}; Path=/api/v1/players; HttpOnly; SameSite=Lax",
        reg.token
    );
    if let Ok(hv) = HeaderValue::from_str(&cookie) {
        resp.headers_mut().insert(header::SET_COOKIE, hv);
    }
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
) -> ApiResult<Sse<impl futures_util::Stream<Item = Result<SseEvent, Infallible>>>> {
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

    let connected = SseEvent::default().event("connected").data(
        serde_json::json!({
            "zoneId": zone_id,
            "streamId": stream_id,
            "serverTime": chrono::Utc::now(),
        })
        .to_string(),
    );

    let cmd_stream = futures_util::stream::unfold(
        (stream_handle.commands, stream_handle.replaced, guard, false),
        |(mut cmds, mut replaced, guard, done)| async move {
            if done {
                return None;
            }
            tokio::select! {
                _ = &mut replaced => {
                    let ev = SseEvent::default()
                        .event("replaced")
                        .data(serde_json::json!({ "reason": "replaced" }).to_string());
                    Some((Ok(ev), (cmds, replaced, guard, true)))
                }
                cmd = cmds.recv() => {
                    let cmd = cmd?;
                    let data = serde_json::to_string(&cmd).unwrap_or_default();
                    let ev = SseEvent::default().id(cmd.id).event("command").data(data);
                    Some((Ok(ev), (cmds, replaced, guard, false)))
                }
            }
        },
    );

    let full = futures_util::stream::once(async move { Ok(connected) }).chain(cmd_stream);
    Ok(Sse::new(full).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    ))
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
) -> ApiResult<Json<serde_json::Value>> {
    let tok = extract_player_token(&headers, &q);
    st.players.unregister(&zone_id, &tok)?;
    Ok(Json(serde_json::json!({
        "unregistered": true,
        "zoneId": zone_id,
    })))
}
