pub mod errors;
pub mod handlers_library;
pub mod handlers_zones;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, delete, get, patch, post, put};
use rust_embed::Embed;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::library::{
    AutoScanConfig, AutoScanner, Catalog, PREF_LIBRARY_SCAN_INTERVAL_MIN,
    PREF_LIBRARY_WATCH_ENABLED, PREF_QOBUZ_MAX_QUALITY, PREF_STREAM_MODE,
};
use crate::media::{MediaHandler, detect_transcoder};
use crate::player::Registry as PlayerRegistry;
use crate::qobuz::{Client as QobuzClient, FORMAT_FLAC_192, quality_label};
use crate::queue::{Manager as QueueManager, StreamMode};
use crate::upnp::GenaClient;

use self::errors::{ApiError, ApiResult, parse_strict_json};

pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

const OPENAPI_YAML: &str = include_str!("openapi.yaml");

#[derive(Embed)]
#[folder = "web/dist"]
struct WebAssets;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub stream_mode: StreamMode,
    pub qobuz_max_quality: String,
    pub transcode_available: bool,
    pub library_watch_enabled: bool,
    pub library_scan_interval_min: i32,
}

pub struct AppState {
    pub catalog: Arc<Catalog>,
    pub auto_scanner: RwLock<Option<Arc<AutoScanner>>>,
    pub qobuz: Option<Arc<QobuzClient>>,
    pub manager: Arc<QueueManager>,
    pub players: Arc<PlayerRegistry>,
    pub media: Arc<MediaHandler>,
    pub gena: Option<Arc<GenaClient>>,
    pub cors_origin: String,
    pub public_url: RwLock<String>,
    pub shutdown: CancellationToken,
    pub oauth_state: Mutex<HashMap<String, Instant>>,
}

impl AppState {
    pub fn new(
        catalog: Arc<Catalog>,
        qobuz: Option<Arc<QobuzClient>>,
        manager: Arc<QueueManager>,
        gena: Option<Arc<GenaClient>>,
        cors_origin: String,
    ) -> Arc<Self> {
        let mgr_reg = Arc::clone(&manager);
        let mgr_unreg = Arc::clone(&manager);
        let mgr_ev = Arc::clone(&manager);
        let players = PlayerRegistry::new(
            Some(Arc::new(move |c| mgr_reg.register_renderer(c))),
            Some(Arc::new(move |id| mgr_unreg.remove_renderer(id))),
            Some(Arc::new(move |ev| mgr_ev.apply_device_event(ev))),
        );

        let media = MediaHandler::new(Arc::clone(&catalog), qobuz.clone());

        let mgr_lib = Arc::clone(&manager);
        catalog.set_on_change(Arc::new(move |status| {
            mgr_lib.broadcast("library.updated", &status);
        }));

        Arc::new(Self {
            catalog,
            auto_scanner: RwLock::new(None),
            qobuz,
            manager,
            players,
            media,
            gena,
            cors_origin: cors_origin.trim().to_string(),
            public_url: RwLock::new(String::new()),
            shutdown: CancellationToken::new(),
            oauth_state: Mutex::new(HashMap::new()),
        })
    }

    pub fn set_auto_scanner(&self, scanner: Arc<AutoScanner>) {
        *self.auto_scanner.write().unwrap_or_else(|e| e.into_inner()) = Some(scanner);
    }

    pub fn set_public_url(&self, url: impl Into<String>) {
        *self.public_url.write().unwrap_or_else(|e| e.into_inner()) =
            url.into().trim().trim_end_matches('/').to_string();
    }

    pub fn close(&self) {
        self.shutdown.cancel();
        self.players.close();
        self.media.close();
        if let Some(ref qb) = self.qobuz {
            qb.close();
        }
        self.manager.close();
        if let Some(ref g) = self.gena {
            g.close();
        }
    }

    pub(crate) fn current_settings(&self) -> Settings {
        let max_q = self
            .qobuz
            .as_ref()
            .map(|q| q.max_quality())
            .unwrap_or_else(|| quality_label(FORMAT_FLAC_192).into());
        let (watch_enabled, interval_min) = self
            .auto_scanner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|s| {
                let c = s.config();
                (c.watch_enabled, (c.interval.as_secs() / 60) as i32)
            })
            .unwrap_or((true, 60));

        Settings {
            stream_mode: self.manager.stream_mode(),
            qobuz_max_quality: max_q,
            transcode_available: detect_transcoder().available(),
            library_watch_enabled: watch_enabled,
            library_scan_interval_min: interval_min,
        }
    }
}

pub fn build_router(state: Arc<AppState>) -> Router {
    let api_router = Router::new()
        // Health, info, OpenAPI, settings
        .route("/api/health", get(health))
        .route("/api/v1/info", get(info))
        .route("/api/v1/openapi.yaml", get(openapi_yaml))
        .route("/api/v1/settings", get(get_settings).patch(patch_settings))
        // Live SSE
        .route("/api/v1/events", get(handlers_zones::events_sse))
        // Zones & discovery
        .route("/api/v1/zones", get(handlers_zones::list_zones))
        .route(
            "/api/v1/zones/discover",
            post(handlers_zones::discover_zones),
        )
        .route("/api/v1/zones/connect", post(handlers_zones::connect_zone))
        .route(
            "/api/v1/zones/{zoneId}",
            get(handlers_zones::get_zone).delete(handlers_zones::delete_zone),
        )
        // Zone queue
        .route(
            "/api/v1/zones/{zoneId}/queue",
            put(handlers_zones::replace_queue).delete(handlers_zones::clear_queue),
        )
        .route(
            "/api/v1/zones/{zoneId}/queue/items",
            post(handlers_zones::add_queue_items),
        )
        .route(
            "/api/v1/zones/{zoneId}/queue/jump",
            post(handlers_zones::jump_queue),
        )
        .route(
            "/api/v1/zones/{zoneId}/queue/items/{itemId}",
            delete(handlers_zones::remove_queue_item),
        )
        .route(
            "/api/v1/zones/{zoneId}/queue/items/{itemId}/move",
            post(handlers_zones::move_queue_item),
        )
        // Zone transport & volume
        .route(
            "/api/v1/zones/{zoneId}/transport/play",
            post(handlers_zones::transport_play),
        )
        .route(
            "/api/v1/zones/{zoneId}/transport/pause",
            post(handlers_zones::transport_pause),
        )
        .route(
            "/api/v1/zones/{zoneId}/transport/stop",
            post(handlers_zones::transport_stop),
        )
        .route(
            "/api/v1/zones/{zoneId}/transport/next",
            post(handlers_zones::transport_next),
        )
        .route(
            "/api/v1/zones/{zoneId}/transport/prev",
            post(handlers_zones::transport_prev),
        )
        .route(
            "/api/v1/zones/{zoneId}/transport/seek",
            post(handlers_zones::transport_seek),
        )
        .route(
            "/api/v1/zones/{zoneId}/volume",
            put(handlers_zones::set_volume),
        )
        .route("/api/v1/zones/{zoneId}/mute", put(handlers_zones::set_mute))
        .route(
            "/api/v1/zones/{zoneId}/quality",
            put(handlers_zones::set_zone_quality),
        )
        .route(
            "/api/v1/zones/{zoneId}/shuffle",
            put(handlers_zones::set_zone_shuffle),
        )
        .route(
            "/api/v1/zones/{zoneId}/repeat",
            put(handlers_zones::set_zone_repeat),
        )
        // Remote HTTP/SSE players
        .route("/api/v1/players", post(handlers_zones::register_player))
        .route(
            "/api/v1/players/{zoneId}",
            patch(handlers_zones::update_player).delete(handlers_zones::unregister_player),
        )
        .route(
            "/api/v1/players/{zoneId}/commands",
            get(handlers_zones::player_commands_sse),
        )
        .route(
            "/api/v1/players/{zoneId}/state",
            post(handlers_zones::player_state),
        )
        // Unified library
        .route(
            "/api/v1/library/status",
            get(handlers_library::library_status),
        )
        .route(
            "/api/v1/library/albums",
            get(handlers_library::library_albums),
        )
        .route(
            "/api/v1/library/albums/{id}",
            get(handlers_library::library_album),
        )
        .route(
            "/api/v1/library/albums/{id}/primary",
            post(handlers_library::set_primary_version),
        )
        .route(
            "/api/v1/library/albums/{id}/version-group",
            delete(handlers_library::ungroup_version),
        )
        .route(
            "/api/v1/library/versions/group",
            post(handlers_library::group_versions),
        )
        .route(
            "/api/v1/library/tracks",
            get(handlers_library::library_tracks),
        )
        .route(
            "/api/v1/library/artists",
            get(handlers_library::library_artists),
        )
        .route(
            "/api/v1/library/genres",
            get(handlers_library::library_genres),
        )
        .route(
            "/api/v1/library/heavy-rotation",
            get(handlers_library::heavy_rotation),
        )
        .route(
            "/api/v1/library/search",
            get(handlers_library::library_search),
        )
        .route(
            "/api/v1/library/rescan",
            post(handlers_library::library_rescan),
        )
        .route("/api/v1/library/sync", post(handlers_library::library_sync))
        .route(
            "/api/v1/library/remote/albums",
            post(handlers_library::add_remote_album),
        )
        .route(
            "/api/v1/library/remote/albums/{id}",
            delete(handlers_library::remove_remote_album),
        )
        .route(
            "/api/v1/library/remote/tracks",
            post(handlers_library::add_remote_track),
        )
        .route(
            "/api/v1/library/remote/tracks/{id}",
            delete(handlers_library::remove_remote_track),
        )
        // Qobuz
        .route("/api/v1/qobuz/status", get(handlers_library::qobuz_status))
        .route(
            "/api/v1/qobuz/oauth/start",
            get(handlers_library::qobuz_oauth_start),
        )
        .route(
            "/api/v1/qobuz/oauth/callback",
            get(handlers_library::qobuz_oauth_callback),
        )
        .route("/api/v1/qobuz/login", post(handlers_library::qobuz_login))
        .route(
            "/api/v1/qobuz/session",
            delete(handlers_library::qobuz_logout),
        )
        .route("/api/v1/qobuz/search", get(handlers_library::qobuz_search))
        .route("/api/v1/qobuz/artist", get(handlers_library::qobuz_artist))
        .route(
            "/api/v1/qobuz/albums/{id}",
            get(handlers_library::qobuz_album),
        )
        // Discover
        .route("/api/v1/discover", get(handlers_library::discover_feed))
        .route(
            "/api/v1/discover/shelves/{slug}",
            get(handlers_library::discover_shelf),
        )
        .route(
            "/api/v1/albums/{id}/similar",
            get(handlers_library::similar_albums),
        )
        .route(
            "/api/v1/artists/{id}/similar",
            get(handlers_library::similar_artists),
        )
        .layer(DefaultBodyLimit::max(128 * 1024))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            cors_and_csrf_middleware,
        ));

    Router::new()
        .merge(api_router)
        // Media endpoints (no CSRF / permissive for UPnP renderers & audio tags)
        .route("/media/local/{id}", get(media_local).head(media_local))
        .route("/media/qobuz/{id}", get(media_qobuz).head(media_qobuz))
        .route("/media/art/remote", get(media_remote_art))
        .route("/media/art/{id}", get(media_art))
        // UPnP GENA NOTIFY endpoint
        .route("/upnp/event/{token}", any(upnp_gena_notify))
        // Embedded React SPA fallback
        .fallback(serve_spa)
        .with_state(state)
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn info(State(st): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "name": "tacet",
        "version": SERVER_VERSION,
        "apiVersion": 1,
        "capabilities": {
            "localLibrary": !st.catalog.roots().is_empty(),
            "qobuz": st.qobuz.as_ref().is_some_and(|q| q.is_connected()),
            "gena": st.gena.is_some(),
        },
    }))
}

async fn openapi_yaml() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/yaml; charset=utf-8")
        .body(Body::from(OPENAPI_YAML))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn get_settings(State(st): State<Arc<AppState>>) -> Json<Settings> {
    Json(st.current_settings())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PatchSettingsReq {
    stream_mode: Option<String>,
    qobuz_max_quality: Option<String>,
    library_watch_enabled: Option<bool>,
    library_scan_interval_min: Option<i32>,
}

async fn patch_settings(State(st): State<Arc<AppState>>, body: Bytes) -> ApiResult<Json<Settings>> {
    let req: PatchSettingsReq = parse_strict_json(&body)?;

    if let Some(sm) = req.stream_mode {
        let mode = StreamMode::parse(&sm)
            .ok_or_else(|| ApiError::bad_request(format!("invalid streamMode {sm:?}")))?;
        st.manager.set_stream_mode(mode);
        let _ = st.catalog.set_pref(PREF_STREAM_MODE, mode.as_str());
    }
    if let Some(q) = req.qobuz_max_quality {
        if let Some(ref qb) = st.qobuz {
            qb.set_max_quality(&q)?;
        }
        let _ = st.catalog.set_pref(PREF_QOBUZ_MAX_QUALITY, &q);
    }
    if req.library_watch_enabled.is_some() || req.library_scan_interval_min.is_some() {
        let cur = st.current_settings();
        let watch = req
            .library_watch_enabled
            .unwrap_or(cur.library_watch_enabled);
        let interval_min = req
            .library_scan_interval_min
            .unwrap_or(cur.library_scan_interval_min)
            .clamp(0, 10_080);
        let _ = st.catalog.set_pref(
            PREF_LIBRARY_WATCH_ENABLED,
            if watch { "true" } else { "false" },
        );
        let _ = st
            .catalog
            .set_pref(PREF_LIBRARY_SCAN_INTERVAL_MIN, &interval_min.to_string());
        if let Some(scanner) = st
            .auto_scanner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            scanner.configure(AutoScanConfig {
                watch_enabled: watch,
                interval: Duration::from_secs((interval_min as u64) * 60),
                debounce: Duration::from_secs(5),
            });
        }
    }

    let updated = st.current_settings();
    st.manager.broadcast("settings.updated", &updated);
    Ok(Json(updated))
}

// --- Media & GENA route adapters ---

async fn media_local(
    State(st): State<Arc<AppState>>,
    method: Method,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    st.media.serve_local(method, &id, &q, &headers).await
}

async fn media_qobuz(
    State(st): State<Arc<AppState>>,
    method: Method,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    st.media.serve_qobuz(method, &id, &q, &headers).await
}

async fn media_art(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    st.media.serve_art(&id, &q, &headers).await
}

async fn media_remote_art(
    State(st): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    st.media.serve_remote_art(&q, &headers).await
}

async fn upnp_gena_notify(
    State(st): State<Arc<AppState>>,
    method: Method,
    Path(token): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let Some(ref gena) = st.gena else {
        return StatusCode::SERVICE_UNAVAILABLE;
    };
    gena.handle_notify(method, &token, &headers, body).await
}

// --- CORS + CSRF middleware ---

fn is_allowed_origin(origin: &str, host: &str, custom_cors: &str) -> bool {
    let o = origin.trim();
    if o.is_empty() {
        return false;
    }
    if !custom_cors.is_empty() {
        return o.eq_ignore_ascii_case(custom_cors);
    }
    if o.eq_ignore_ascii_case("tauri://localhost")
        || o.eq_ignore_ascii_case("http://tauri.localhost")
        || o.eq_ignore_ascii_case("https://tauri.localhost")
    {
        return true;
    }
    if let Ok(u) = url::Url::parse(o)
        && matches!(u.scheme(), "http" | "https")
        && let Some(ohost) = u.host_str()
    {
        let origin_host_port = match u.port() {
            Some(p) => format!("{ohost}:{p}"),
            None => ohost.to_string(),
        };
        if origin_host_port.eq_ignore_ascii_case(host) {
            return true;
        }
    }
    false
}

async fn cors_and_csrf_middleware(
    State(st): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let allowed = is_allowed_origin(&origin, &host, &st.cors_origin);

    if req.method() == Method::OPTIONS {
        let mut resp = StatusCode::NO_CONTENT.into_response();
        if allowed && let Ok(hv) = HeaderValue::from_str(&origin) {
            let h = resp.headers_mut();
            h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, hv);
            h.insert(
                header::ACCESS_CONTROL_ALLOW_METHODS,
                HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, OPTIONS"),
            );
            h.insert(
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                HeaderValue::from_static("Content-Type, Authorization, X-Player-Token"),
            );
            h.insert(header::VARY, HeaderValue::from_static("Origin"));
        }
        return resp;
    }

    // CSRF check on mutating requests with an explicit cross-origin Origin header
    if matches!(
        *req.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    ) && !origin.is_empty()
        && !allowed
    {
        return ApiError::new(
            StatusCode::FORBIDDEN,
            "CSRF_REJECTED",
            "cross-origin request blocked",
            false,
        )
        .into_response();
    }

    let mut resp = next.run(req).await;
    if allowed && let Ok(hv) = HeaderValue::from_str(&origin) {
        let h = resp.headers_mut();
        h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, hv);
        h.insert(header::VARY, HeaderValue::from_static("Origin"));
    }
    resp
}

// --- Embedded React SPA fallback ---

async fn serve_spa(uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.starts_with("api/") || path.starts_with("media/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !path.is_empty()
        && let Some(file) = WebAssets::get(path)
    {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        let cache = if path.starts_with("assets/") {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        };
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime.as_ref())
            .header(header::CACHE_CONTROL, cache)
            .body(Body::from(file.data.into_owned()))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }

    if let Some(index) = WebAssets::get("index.html") {
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from(index.data.into_owned()))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }
    (StatusCode::NOT_FOUND, "web UI not built").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    #[tokio::test]
    async fn health_info_and_csrf_guard() {
        let cat = Catalog::new_in_memory(vec![]);
        let mgr = QueueManager::new(Arc::clone(&cat), None, 4320, StreamMode::Relay);
        let state = AppState::new(cat, None, mgr, None, String::new());
        let app = build_router(state);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Cross-origin POST should be rejected with 403 CSRF_REJECTED
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/library/rescan")
                    .header(header::HOST, "localhost:4320")
                    .header(header::ORIGIN, "https://evil.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Tauri desktop origin should be allowed
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::OPTIONS)
                    .uri("/api/v1/zones")
                    .header(header::HOST, "localhost:4320")
                    .header(header::ORIGIN, "tauri://localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            resp.headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "tauri://localhost"
        );
    }

    #[tokio::test]
    async fn api_wire_contract_regression() {
        use crate::queue::AudioQuality;
        use axum::body::to_bytes;

        // AudioQuality must serialize as snake_case with underscore ("opus_96")
        assert_eq!(
            serde_json::to_string(&AudioQuality::Opus96).unwrap(),
            "\"opus_96\""
        );
        assert_eq!(
            serde_json::to_string(&AudioQuality::Opus256).unwrap(),
            "\"opus_256\""
        );

        let cat = Catalog::new_in_memory(vec![]);
        let mgr = QueueManager::new(Arc::clone(&cat), None, 4320, StreamMode::Relay);
        let state = AppState::new(cat, None, mgr, None, String::new());
        let app = build_router(state);

        // GET /api/v1/info must return numeric apiVersion and capabilities
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/info")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let info_json: serde_json::Value =
            serde_json::from_slice(&to_bytes(resp.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(info_json["apiVersion"], 1);
        assert!(info_json["capabilities"].is_object());

        // POST /api/v1/players sets tacet_player_session cookie, then shuffle/repeat/delete work
        let reg_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/players")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"name":"Test Browser","playerId":"test_1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(reg_resp.status(), StatusCode::OK);
        let set_cookie = reg_resp
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(set_cookie.starts_with("tacet_player_session="));
        let reg_json: serde_json::Value =
            serde_json::from_slice(&to_bytes(reg_resp.into_body(), 65536).await.unwrap()).unwrap();
        let zone_id = reg_json["zoneId"].as_str().unwrap().to_string();
        let token = reg_json["token"].as_str().unwrap().to_string();

        // PUT /shuffle expects {"enabled": true} and PUT /repeat expects {"mode": "all"}
        let shuf = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri(format!("/api/v1/zones/{zone_id}/shuffle"))
                    .body(Body::from(r#"{"enabled":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(shuf.status(), StatusCode::OK);

        let rep = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri(format!("/api/v1/zones/{zone_id}/repeat"))
                    .body(Body::from(r#"{"mode":"all"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rep.status(), StatusCode::OK);

        // Cross-site player commands SSE must be rejected
        let cross_sse = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/players/{zone_id}/commands"))
                    .header(header::COOKIE, format!("tacet_player_session={token}"))
                    .header("sec-fetch-site", "cross-site")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cross_sse.status(), StatusCode::FORBIDDEN);

        // DELETE /players/{zoneId} returns 200 {unregistered: true, zoneId}
        let del = app
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(format!("/api/v1/players/{zone_id}"))
                    .header(header::COOKIE, format!("tacet_player_session={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(del.status(), StatusCode::OK);
        let del_json: serde_json::Value =
            serde_json::from_slice(&to_bytes(del.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(del_json["unregistered"], true);
        assert_eq!(del_json["zoneId"], zone_id);
    }

    #[tokio::test]
    async fn library_settings_and_qobuz_oauth_handlers() {
        use crate::library::{Album, SOURCE_LOCAL, SOURCE_TYPE_LOCAL, Track};
        use axum::body::to_bytes;

        let dir = tempfile::tempdir().unwrap();
        let cat = Catalog::new_in_memory(vec![dir.path().to_path_buf()]);
        let album = Album {
            id: "local:alb1".into(),
            source: SOURCE_LOCAL.into(),
            source_type: SOURCE_TYPE_LOCAL.into(),
            title: "Kind of Blue".into(),
            artist: "Miles Davis".into(),
            genre: "Jazz".into(),
            year: 1959,
            available: true,
            in_library: true,
            ..Default::default()
        };
        let track = Track {
            id: "local:tr1".into(),
            album_id: album.id.clone(),
            source: SOURCE_LOCAL.into(),
            source_type: SOURCE_TYPE_LOCAL.into(),
            title: "So What".into(),
            artist: "Miles Davis".into(),
            album: "Kind of Blue".into(),
            album_artist: "Miles Davis".into(),
            genre: "Jazz".into(),
            track_number: 1,
            disc_number: 1,
            duration_sec: 562,
            available: true,
            in_library: true,
            ..Default::default()
        };
        cat.store()
            .upsert_album_with_tracks(&album, std::slice::from_ref(&track))
            .unwrap();

        let qb = crate::qobuz::Client::new(crate::qobuz::Config::default());
        let mgr = QueueManager::new(
            Arc::clone(&cat),
            Some(Arc::clone(&qb)),
            4320,
            StreamMode::Relay,
        );
        let state = AppState::new(cat, Some(qb), mgr, None, String::new());
        let app = build_router(state);

        // Library browse & status endpoints
        for uri in [
            "/api/v1/openapi.yaml",
            "/api/v1/library/status",
            "/api/v1/library/albums",
            "/api/v1/library/albums/local:alb1",
            "/api/v1/library/tracks",
            "/api/v1/library/artists",
            "/api/v1/library/genres",
            "/api/v1/library/heavy-rotation",
            "/api/v1/library/search?q=Miles",
            "/api/v1/qobuz/status",
        ] {
            let resp = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "GET {uri}");
        }

        // Missing album returns 404
        let not_found = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/library/albums/local:missing")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(not_found.status(), StatusCode::NOT_FOUND);

        // Synchronous rescan (?wait=true) returns 200
        let rescan = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/library/rescan?wait=true")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rescan.status(), StatusCode::OK);

        // Settings GET & PATCH (valid + unknown field rejection)
        let patch_ok = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::PATCH)
                    .uri("/api/v1/settings")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"streamMode":"direct"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(patch_ok.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(patch_ok.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(body["streamMode"], "direct");

        let patch_bad = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::PATCH)
                    .uri("/api/v1/settings")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"unknownField":123}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(patch_bad.status(), StatusCode::BAD_REQUEST);

        // OAuth start rejects injected Host header
        let bad_host = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/qobuz/oauth/start")
                    .header(header::HOST, "evil.com/foo@bar")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad_host.status(), StatusCode::BAD_REQUEST);

        // Qobuz logout returns 200
        let logout = app
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri("/api/v1/qobuz/session")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(logout.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn zone_queue_by_id_transport_and_sse_shutdown() {
        use crate::library::{Album, SOURCE_LOCAL, SOURCE_TYPE_LOCAL, Track};
        use axum::body::to_bytes;

        let cat = Catalog::new_in_memory(vec![]);
        let alb = Album {
            id: "local:a1".into(),
            source: SOURCE_LOCAL.into(),
            source_type: SOURCE_TYPE_LOCAL.into(),
            title: "Test Album".into(),
            artist: "Test Artist".into(),
            available: true,
            in_library: true,
            ..Default::default()
        };
        let tracks = vec![
            Track {
                id: "local:t1".into(),
                album_id: alb.id.clone(),
                source: SOURCE_LOCAL.into(),
                source_type: SOURCE_TYPE_LOCAL.into(),
                title: "One".into(),
                artist: "Test Artist".into(),
                album: "Test Album".into(),
                track_number: 1,
                duration_sec: 120,
                available: true,
                in_library: true,
                ..Default::default()
            },
            Track {
                id: "local:t2".into(),
                album_id: alb.id.clone(),
                source: SOURCE_LOCAL.into(),
                source_type: SOURCE_TYPE_LOCAL.into(),
                title: "Two".into(),
                artist: "Test Artist".into(),
                album: "Test Album".into(),
                track_number: 2,
                duration_sec: 140,
                available: true,
                in_library: true,
                ..Default::default()
            },
        ];
        cat.store().upsert_album_with_tracks(&alb, &tracks).unwrap();

        let mgr = QueueManager::new(Arc::clone(&cat), None, 4320, StreamMode::Relay);
        let state = AppState::new(cat, None, mgr, None, String::new());
        let shutdown = state.shutdown.clone();
        let players = Arc::clone(&state.players);
        let app = build_router(state);

        // Register a remote player and spawn an ack responder so transport commands succeed
        let reg_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/players")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"name":"Desk","playerId":"desk_1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(reg_resp.status(), StatusCode::OK);
        let reg: serde_json::Value =
            serde_json::from_slice(&to_bytes(reg_resp.into_body(), 65536).await.unwrap()).unwrap();
        let zone_id = reg["zoneId"].as_str().unwrap().to_string();
        let token = reg["token"].as_str().unwrap().to_string();
        let rp = players.get(&zone_id, &token).unwrap();
        let mut pstream = rp.attach_stream();
        let rp_ack = Arc::clone(&rp);
        let ack_handle = tokio::spawn(async move {
            while let Some(cmd) = pstream.commands.recv().await {
                rp_ack.report_state(crate::player::StateReport {
                    cmd_id: cmd.id,
                    ok: Some(true),
                    state: "PLAYING".into(),
                    ..Default::default()
                });
            }
        });

        // Add 2 tracks to queue via POST /queue/items
        let rep = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/api/v1/zones/{zone_id}/queue/items"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"source":{"type":"tracks","trackIds":["local:t1","local:t2"]},"position":"end"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rep.status(), StatusCode::OK);
        let z_json: serde_json::Value =
            serde_json::from_slice(&to_bytes(rep.into_body(), 65536).await.unwrap()).unwrap();
        let items = z_json["queue"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        let item0 = items[0]["itemId"].as_str().unwrap().to_string();
        let item1 = items[1]["itemId"].as_str().unwrap().to_string();

        // Move item1 to index 0
        let mv = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/api/v1/zones/{zone_id}/queue/items/{item1}/move"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"toIndex":0}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(mv.status(), StatusCode::OK);

        // Remove item0 by ID
        let rm = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(format!("/api/v1/zones/{zone_id}/queue/items/{item0}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rm.status(), StatusCode::OK);

        // Removing missing itemId returns 422 QUEUE_ITEM_NOT_FOUND
        let rm_missing = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(format!("/api/v1/zones/{zone_id}/queue/items/no_such_item"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rm_missing.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let err_json: serde_json::Value =
            serde_json::from_slice(&to_bytes(rm_missing.into_body(), 65536).await.unwrap())
                .unwrap();
        assert_eq!(err_json["error"]["code"], "QUEUE_ITEM_NOT_FOUND");

        // Zone quality & unknown zone 404
        let qual = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::PUT)
                    .uri(format!("/api/v1/zones/{zone_id}/quality"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"quality":"opus_160"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(qual.status(), StatusCode::OK);

        let missing_zone = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/zones/zone:nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing_zone.status(), StatusCode::NOT_FOUND);

        // Transport endpoints (pause, play, seek, volume, mute, next, prev, stop)
        for (method, uri, body) in [
            (
                Method::POST,
                format!("/api/v1/zones/{zone_id}/transport/pause"),
                "",
            ),
            (
                Method::POST,
                format!("/api/v1/zones/{zone_id}/transport/play"),
                "",
            ),
            (
                Method::POST,
                format!("/api/v1/zones/{zone_id}/transport/seek"),
                r#"{"positionSec":15}"#,
            ),
            (
                Method::PUT,
                format!("/api/v1/zones/{zone_id}/volume"),
                r#"{"volume":65}"#,
            ),
            (
                Method::PUT,
                format!("/api/v1/zones/{zone_id}/mute"),
                r#"{"muted":true}"#,
            ),
            (
                Method::POST,
                format!("/api/v1/zones/{zone_id}/transport/stop"),
                "",
            ),
        ] {
            let mut req = Request::builder().method(method).uri(&uri);
            if !body.is_empty() {
                req = req.header(header::CONTENT_TYPE, "application/json");
            }
            let r = app
                .clone()
                .oneshot(req.body(Body::from(body)).unwrap())
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::OK, "{uri}");
        }

        // Clear queue
        let clr = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(format!("/api/v1/zones/{zone_id}/queue"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(clr.status(), StatusCode::OK);

        // SSE /api/v1/events emits snapshot and terminates cleanly when shutdown token cancels
        let sse_resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/events?zone={zone_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(sse_resp.status(), StatusCode::OK);
        shutdown.cancel();
        let sse_bytes = to_bytes(sse_resp.into_body(), 256 * 1024).await.unwrap();
        let sse_text = String::from_utf8_lossy(&sse_bytes);
        assert!(sse_text.contains("event: snapshot"));
        assert!(sse_text.contains(&zone_id));
        ack_handle.abort();
    }
}
