pub mod errors;
pub mod handlers_library;
pub mod handlers_zones;
#[cfg(test)]
mod tests;

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
            cors_origin: cors_origin.trim().trim_end_matches('/').to_string(),
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

    pub fn prewarm_artists(&self) {
        let Some(ref qb) = self.qobuz else {
            return;
        };
        if !qb.is_connected() {
            return;
        }
        let names: Vec<String> = self
            .catalog
            .list_artists(&crate::library::FacetFilter {
                limit: 500,
                ..Default::default()
            })
            .map(|(items, _)| {
                items
                    .into_iter()
                    .map(|a| a.name)
                    .filter(|n| !n.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let local_albums: Vec<crate::qobuz::AlbumLookup> = self
            .catalog
            .query_albums(&crate::library::AlbumFilter {
                source_type: crate::library::SOURCE_TYPE_LOCAL.into(),
                limit: 500,
                ..Default::default()
            })
            .map(|(items, _)| {
                items
                    .into_iter()
                    .filter(|a| !a.artist.is_empty() && !a.title.is_empty())
                    .map(|a| crate::qobuz::AlbumLookup {
                        artist: a.artist,
                        title: a.title,
                    })
                    .collect()
            })
            .unwrap_or_default();

        qb.prewarm_library(names, local_albums);
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

    pub async fn close_async(&self) {
        self.shutdown.cancel();
        self.players.close();
        self.media.close();
        if let Some(ref qb) = self.qobuz {
            qb.close();
        }
        self.manager.close_async().await;
        if let Some(ref g) = self.gena {
            g.close_async().await;
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
        .route("/api/health", get(health))
        .route("/api/v1/info", get(info))
        .route("/api/v1/openapi.yaml", get(openapi_yaml))
        .route("/api/v1/settings", get(get_settings).patch(patch_settings))
        .route("/api/v1/events", get(handlers_zones::events_sse))
        .merge(zone_and_player_routes())
        .merge(library_routes())
        .merge(qobuz_and_discover_routes())
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
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

fn zone_and_player_routes() -> Router<Arc<AppState>> {
    Router::new()
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
}

fn library_routes() -> Router<Arc<AppState>> {
    Router::new()
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
}

fn qobuz_and_discover_routes() -> Router<Arc<AppState>> {
    Router::new()
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
            "qobuz": st.qobuz.is_some(),
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

    if let Some(interval) = req.library_scan_interval_min
        && !(0..=10_080).contains(&interval)
    {
        return Err(ApiError::bad_request(
            "libraryScanIntervalMin must be between 0 and 10080",
        ));
    }

    if let Some(sm) = req.stream_mode {
        let mode = StreamMode::parse(&sm)
            .ok_or_else(|| ApiError::bad_request(format!("invalid streamMode {sm:?}")))?;
        st.manager.set_stream_mode(mode);
        let _ = st.catalog.set_pref(PREF_STREAM_MODE, mode.as_str());
    }
    if let Some(q) = req.qobuz_max_quality {
        let qb = st.qobuz.as_ref().ok_or_else(|| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "QOBUZ_UNAVAILABLE",
                "qobuz not configured",
                false,
            )
        })?;
        qb.set_max_quality(&q)?;
        let normalized = qb.status().max_quality;
        let _ = st.catalog.set_pref(PREF_QOBUZ_MAX_QUALITY, &normalized);
        st.manager.broadcast("qobuz.status", &qb.status());
    }
    if req.library_watch_enabled.is_some() || req.library_scan_interval_min.is_some() {
        let cur = st.current_settings();
        let watch = req
            .library_watch_enabled
            .unwrap_or(cur.library_watch_enabled);
        let interval_min = req
            .library_scan_interval_min
            .unwrap_or(cur.library_scan_interval_min);
        if req.library_watch_enabled.is_some() {
            let _ = st.catalog.set_pref(
                PREF_LIBRARY_WATCH_ENABLED,
                if watch { "true" } else { "false" },
            );
        }
        if req.library_scan_interval_min.is_some() {
            let _ = st
                .catalog
                .set_pref(PREF_LIBRARY_SCAN_INTERVAL_MIN, &interval_min.to_string());
        }
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
        let default_port = if u.scheme() == "https" { 443 } else { 80 };
        let origin_port = u.port().unwrap_or(default_port);
        let origin_norm = format!("{ohost}:{origin_port}");
        let host_norm = if let Ok(hu) =
            url::Url::parse(&format!("{}://{}", u.scheme(), host.trim()))
            && let Some(hh) = hu.host_str()
        {
            format!("{hh}:{}", hu.port().unwrap_or(default_port))
        } else {
            host.trim().to_string()
        };
        if origin_norm.eq_ignore_ascii_case(&host_norm) {
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
                HeaderValue::from_static(
                    "Content-Type, Authorization, X-Player-Token, If-None-Match, Last-Event-ID, Range",
                ),
            );
            h.insert(
                header::ACCESS_CONTROL_EXPOSE_HEADERS,
                HeaderValue::from_static(
                    "ETag, Allow, Content-Length, Content-Range, Accept-Ranges",
                ),
            );
            h.insert(
                header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
                HeaderValue::from_static("true"),
            );
            h.insert(header::VARY, HeaderValue::from_static("Origin"));
        } else {
            resp.headers_mut()
                .insert(header::VARY, HeaderValue::from_static("Origin"));
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
    resp.headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Origin"));
    if allowed && let Ok(hv) = HeaderValue::from_str(&origin) {
        let h = resp.headers_mut();
        h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, hv);
        h.insert(
            header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        );
        h.insert(
            header::ACCESS_CONTROL_EXPOSE_HEADERS,
            HeaderValue::from_static("ETag, Allow, Content-Length, Content-Range, Accept-Ranges"),
        );
    }
    resp
}

// --- Embedded React SPA fallback ---

async fn serve_spa(method: Method, uri: axum::http::Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path == "api" || path.starts_with("api/") {
        return ApiError::not_found("NOT_FOUND", "endpoint not found").into_response();
    }
    if path == "media" || path.starts_with("media/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    if method != Method::GET && method != Method::HEAD {
        return Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .header(header::ALLOW, "GET, HEAD")
            .body(Body::empty())
            .unwrap_or_else(|_| StatusCode::METHOD_NOT_ALLOWED.into_response());
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
        let len = file.data.len();
        let body = if method == Method::HEAD {
            Body::empty()
        } else {
            Body::from(file.data.into_owned())
        };
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime.as_ref())
            .header(header::CONTENT_LENGTH, len)
            .header(header::CACHE_CONTROL, cache)
            .body(body)
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }
    if path.starts_with("assets/") {
        return StatusCode::NOT_FOUND.into_response();
    }

    if let Some(index) = WebAssets::get("index.html") {
        let len = index.data.len();
        let body = if method == Method::HEAD {
            Body::empty()
        } else {
            Body::from(index.data.into_owned())
        };
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .header(header::CONTENT_LENGTH, len)
            .header(header::CACHE_CONTROL, "no-cache")
            .body(body)
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }
    (StatusCode::NOT_FOUND, "web UI not built").into_response()
}
