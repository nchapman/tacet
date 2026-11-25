use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use rand::RngCore;
use serde::Deserialize;

use crate::library::{AlbumFilter, FacetFilter, SOURCE_LOCAL, SOURCE_QOBUZ, TrackFilter};

use super::AppState;
use super::errors::{ApiError, ApiResult, parse_strict_json};

fn parse_page(q: &HashMap<String, String>, def_lim: i32, max_lim: i32) -> (i32, i32) {
    let limit = q
        .get("limit")
        .and_then(|s| s.parse::<i32>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(def_lim)
        .min(max_lim);
    let offset = q
        .get("offset")
        .and_then(|s| s.parse::<i32>().ok())
        .filter(|&v| v >= 0)
        .unwrap_or(0);
    (limit, offset)
}

fn parse_genre_ids(raw: Option<&str>) -> Vec<i64> {
    raw.unwrap_or("")
        .split(',')
        .filter_map(|s| s.trim().parse::<i64>().ok())
        .filter(|&id| id > 0)
        .collect()
}

// --- Library ---

pub async fn library_status(State(st): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::to_value(st.catalog.status()).unwrap_or_default())
}

pub async fn library_albums(
    State(st): State<Arc<AppState>>,
    Query(qv): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let (limit, offset) = parse_page(&qv, 200, 500);
    let q = qv
        .get("q")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let sort = qv
        .get("sort")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let source_type = qv
        .get("source")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let provider = qv
        .get("provider")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let artist = qv
        .get("artist")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let genre = qv
        .get("genre")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let group_versions = qv.get("groupVersions").is_some_and(|v| v == "true");
    let include_unavailable = qv.get("includeUnavailable").is_some_and(|v| v == "true");

    let mut etag_opt = None;
    if q.is_empty() && artist.is_empty() && genre.is_empty() {
        let base = st.catalog.etag();
        let base_trim = base.trim_matches('"');
        let etag = format!(
            "\"{base_trim}-{sort}-{source_type}-{provider}-{group_versions}-{include_unavailable}-{limit}-{offset}\""
        );
        if headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|m| m.trim() == etag)
        {
            return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response());
        }
        etag_opt = Some(etag);
    }

    let (items, total) = st.catalog.query_albums(&AlbumFilter {
        query: q,
        source_type,
        provider,
        artist,
        genre,
        sort,
        group_versions,
        include_unavailable,
        limit,
        offset,
    })?;

    let mut resp = Json(serde_json::json!({
        "items": items,
        "total": total,
        "limit": limit,
        "offset": offset,
    }))
    .into_response();
    if let Some(etag) = etag_opt
        && let Ok(hv) = HeaderValue::from_str(&etag)
    {
        resp.headers_mut().insert(header::ETAG, hv);
    }
    Ok(resp)
}

pub async fn library_album(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let mut alb = st
        .catalog
        .album(&id)
        .ok_or_else(|| ApiError::not_found("ALBUM_NOT_FOUND", format!("album {id:?} not found")))?;

    // Best-effort enrich with cached Qobuz editorial metadata
    if let Some(ref qb) = st.qobuz
        && qb.is_connected()
    {
        let q_res = if alb.source == SOURCE_QOBUZ && !alb.external_id.is_empty() {
            qb.get_album(&alb.external_id).await.ok()
        } else if alb.source == SOURCE_LOCAL && !alb.artist.is_empty() && !alb.title.is_empty() {
            tokio::time::timeout(
                Duration::from_secs(3),
                qb.match_album(&alb.artist, &alb.title),
            )
            .await
            .ok()
            .and_then(|r| r.ok())
        } else {
            None
        };
        if let Some(qa) = q_res {
            if alb.label.is_empty() {
                alb.label = qa.label;
            }
            if alb.description.is_empty() {
                alb.description = qa.description;
            }
            if alb.copyright.is_empty() {
                alb.copyright = qa.copyright;
            }
            if alb.release_date.is_empty() {
                alb.release_date = qa.release_date;
            }
            if alb.awards.is_empty() {
                alb.awards = qa.awards;
            }
            if alb.booklet_url.is_empty() {
                alb.booklet_url = qa.booklet_url;
            }
            if alb.credits.is_empty() {
                alb.credits = qa.credits;
            }
            if !qa.tracks.is_empty() {
                let explicit_by_qobuz: HashMap<i64, bool> = qa
                    .tracks
                    .iter()
                    .filter(|t| t.qobuz_track_id > 0 && t.explicit)
                    .map(|t| (t.qobuz_track_id, true))
                    .collect();
                let explicit_by_pos: HashMap<(i32, i32), bool> = qa
                    .tracks
                    .iter()
                    .filter(|t| t.explicit)
                    .map(|t| ((t.disc_number.max(1), t.track_number), true))
                    .collect();
                for tr in &mut alb.tracks {
                    if !tr.explicit
                        && (explicit_by_qobuz.contains_key(&tr.qobuz_track_id)
                            || explicit_by_pos
                                .contains_key(&(tr.disc_number.max(1), tr.track_number)))
                    {
                        tr.explicit = true;
                    }
                }
            }
        }
    }

    Ok(Json(serde_json::to_value(alb).unwrap_or_default()))
}

pub async fn set_primary_version(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let alb = st.catalog.set_primary_version(&id)?;
    Ok(Json(serde_json::to_value(alb).unwrap_or_default()))
}

pub async fn ungroup_version(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let alb = st.catalog.ungroup_version(&id)?;
    Ok(Json(serde_json::to_value(alb).unwrap_or_default()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GroupVersionsReq {
    album_ids: Vec<String>,
}

pub async fn group_versions(
    State(st): State<Arc<AppState>>,
    body: Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let req: GroupVersionsReq = parse_strict_json(&body)?;
    let group_id = st.catalog.group_versions(&req.album_ids)?;
    Ok(Json(serde_json::json!({ "versionGroupId": group_id })))
}

pub async fn library_tracks(
    State(st): State<Arc<AppState>>,
    Query(qv): Query<HashMap<String, String>>,
) -> ApiResult<Json<serde_json::Value>> {
    let (limit, offset) = parse_page(&qv, 200, 1000);
    let (items, total) = st.catalog.query_tracks(&TrackFilter {
        query: qv.get("q").cloned().unwrap_or_default(),
        source_type: qv.get("source").cloned().unwrap_or_default(),
        provider: qv.get("provider").cloned().unwrap_or_default(),
        artist: qv.get("artist").cloned().unwrap_or_default(),
        album: qv.get("album").cloned().unwrap_or_default(),
        album_id: qv.get("albumId").cloned().unwrap_or_default(),
        genre: qv.get("genre").cloned().unwrap_or_default(),
        sort: qv.get("sort").cloned().unwrap_or_default(),
        sort_dir: qv.get("dir").cloned().unwrap_or_default(),
        include_unavailable: qv.get("includeUnavailable").is_some_and(|v| v == "true"),
        limit,
        offset,
    })?;
    Ok(Json(serde_json::json!({
        "items": items,
        "total": total,
        "limit": limit,
        "offset": offset,
    })))
}

pub async fn library_artists(
    State(st): State<Arc<AppState>>,
    Query(qv): Query<HashMap<String, String>>,
) -> ApiResult<Json<serde_json::Value>> {
    let (limit, offset) = parse_page(&qv, 500, 1000);
    let (mut items, total) = st.catalog.list_artists(&FacetFilter {
        query: qv.get("q").cloned().unwrap_or_default(),
        source_type: qv.get("source").cloned().unwrap_or_default(),
        provider: qv.get("provider").cloned().unwrap_or_default(),
        genre: qv.get("genre").cloned().unwrap_or_default(),
        include_unavailable: qv.get("includeUnavailable").is_some_and(|v| v == "true"),
        limit,
        offset,
    })?;
    if let Some(ref qb) = st.qobuz {
        for a in &mut items {
            let p = qb.cached_artist_portrait(&a.name);
            if !p.is_empty() {
                a.artwork_url = p;
            }
        }
    }
    Ok(Json(serde_json::json!({
        "items": items,
        "total": total,
        "limit": limit,
        "offset": offset,
    })))
}

pub async fn library_genres(
    State(st): State<Arc<AppState>>,
    Query(qv): Query<HashMap<String, String>>,
) -> ApiResult<Json<serde_json::Value>> {
    let (limit, offset) = parse_page(&qv, 200, 500);
    let (items, total) = st.catalog.list_genres(&FacetFilter {
        query: qv.get("q").cloned().unwrap_or_default(),
        source_type: qv.get("source").cloned().unwrap_or_default(),
        provider: qv.get("provider").cloned().unwrap_or_default(),
        genre: String::new(),
        include_unavailable: qv.get("includeUnavailable").is_some_and(|v| v == "true"),
        limit,
        offset,
    })?;
    Ok(Json(serde_json::json!({
        "items": items,
        "total": total,
        "limit": limit,
        "offset": offset,
    })))
}

pub async fn heavy_rotation(
    State(st): State<Arc<AppState>>,
    Query(qv): Query<HashMap<String, String>>,
) -> ApiResult<Json<serde_json::Value>> {
    let days = qv
        .get("days")
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(30)
        .clamp(1, 365);
    let limit = qv
        .get("limit")
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(20)
        .clamp(1, 100);
    let items = st.catalog.heavy_rotation(days, limit)?;
    Ok(Json(serde_json::json!({ "days": days, "items": items })))
}

pub async fn library_search(
    State(st): State<Arc<AppState>>,
    Query(qv): Query<HashMap<String, String>>,
) -> ApiResult<Json<serde_json::Value>> {
    let q = qv.get("q").map(|s| s.trim()).unwrap_or("");
    if q.is_empty() {
        return Ok(Json(serde_json::json!({
            "query": "",
            "artists": [],
            "albums": [],
            "tracks": [],
        })));
    }
    let limit = qv
        .get("limit")
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(8)
        .clamp(1, 50);
    let res = st.catalog.unified_search(q, limit)?;
    Ok(Json(serde_json::to_value(res).unwrap_or_default()))
}

pub async fn library_rescan(
    State(st): State<Arc<AppState>>,
    Query(qv): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    let wait = qv.get("wait").is_some_and(|v| v == "true" || v == "1");
    if wait {
        st.catalog.scan().await?;
        st.media.prewarm_art();
        st.prewarm_artists();
        return Ok((
            StatusCode::OK,
            Json(serde_json::to_value(st.catalog.status()).unwrap_or_default()),
        )
            .into_response());
    }
    let st_bg = Arc::clone(&st);
    tokio::spawn(async move {
        if st_bg.catalog.scan().await.is_ok() {
            st_bg.media.prewarm_art();
            st_bg.prewarm_artists();
        }
    });
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::to_value(st.catalog.status()).unwrap_or_default()),
    )
        .into_response())
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SyncReq {
    #[serde(default)]
    provider: String,
}

pub async fn library_sync(
    State(st): State<Arc<AppState>>,
    Query(qv): Query<HashMap<String, String>>,
    body: Bytes,
) -> ApiResult<Response> {
    let req: SyncReq = if body.is_empty() {
        SyncReq::default()
    } else {
        parse_strict_json(&body)?
    };
    let prov_raw = qv
        .get("provider")
        .map(String::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&req.provider);
    let prov = if prov_raw.trim().is_empty() {
        "all".to_string()
    } else {
        prov_raw.trim().to_ascii_lowercase()
    };
    let wait = qv.get("wait").is_some_and(|v| v == "true" || v == "1");
    if wait {
        run_sync_for_provider(&st, &prov).await?;
        return Ok((
            StatusCode::OK,
            Json(serde_json::to_value(st.catalog.status()).unwrap_or_default()),
        )
            .into_response());
    }
    let st_bg = Arc::clone(&st);
    let prov_bg = prov.clone();
    tokio::spawn(async move {
        let _ = run_sync_for_provider(&st_bg, &prov_bg).await;
    });
    let mut status = st.catalog.status();
    status.scanning = true;
    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::to_value(status).unwrap_or_default()),
    )
        .into_response())
}

async fn run_sync_for_provider(st: &AppState, prov: &str) -> ApiResult<()> {
    match prov {
        "all" => {
            if !st.catalog.roots().is_empty() {
                st.catalog.scan().await?;
                st.media.prewarm_art();
            }
            if st.qobuz.as_ref().is_some_and(|q| q.is_connected()) {
                match st.catalog.sync_remote(SOURCE_QOBUZ).await {
                    Ok(_) => st.prewarm_artists(),
                    Err(e) => {
                        tracing::warn!(error = %e, "qobuz sync failed during library sync");
                        if st.catalog.roots().is_empty() {
                            return Err(e.into());
                        }
                    }
                }
            }
        }
        "local" => {
            st.catalog.scan().await?;
            st.media.prewarm_art();
            st.prewarm_artists();
        }
        other => {
            st.catalog.sync_remote(other).await?;
            st.prewarm_artists();
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RemoteIdReq {
    #[serde(default)]
    provider: String,
    #[serde(alias = "id")]
    external_id: String,
}

pub async fn add_remote_album(
    State(st): State<Arc<AppState>>,
    body: Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let req: RemoteIdReq = parse_strict_json(&body)?;
    let prov = if req.provider.is_empty() {
        SOURCE_QOBUZ
    } else {
        &req.provider
    };
    let alb = st.catalog.add_remote_album(prov, &req.external_id).await?;
    Ok(Json(serde_json::to_value(alb).unwrap_or_default()))
}

pub async fn remove_remote_album(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    st.catalog.remove_remote_album(SOURCE_QOBUZ, &id).await?;
    Ok(Json(serde_json::json!({ "removed": true, "id": id })))
}

pub async fn add_remote_track(
    State(st): State<Arc<AppState>>,
    body: Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let req: RemoteIdReq = parse_strict_json(&body)?;
    let prov = if req.provider.is_empty() {
        SOURCE_QOBUZ
    } else {
        &req.provider
    };
    let tr = st.catalog.add_remote_track(prov, &req.external_id).await?;
    Ok(Json(serde_json::to_value(tr).unwrap_or_default()))
}

pub async fn remove_remote_track(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    st.catalog.remove_remote_track(SOURCE_QOBUZ, &id).await?;
    Ok(Json(serde_json::json!({ "removed": true, "id": id })))
}

// --- Qobuz ---

pub async fn qobuz_status(State(st): State<Arc<AppState>>) -> Json<serde_json::Value> {
    match &st.qobuz {
        Some(qb) => Json(serde_json::to_value(qb.status()).unwrap_or_default()),
        None => Json(serde_json::json!({
            "configured": false,
            "authenticated": false,
            "maxFormatId": 27,
            "maxQuality": "flac_24_192",
        })),
    }
}

pub async fn qobuz_oauth_start(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let qb = st.qobuz.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "QOBUZ_UNAVAILABLE",
            "qobuz not configured",
            false,
        )
    })?;
    let pub_url = st
        .public_url
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let callback_base = if !pub_url.is_empty() {
        pub_url
    } else {
        let host = headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("localhost:4320")
            .trim();
        // Reject Host header injection (whitespace, slashes, userinfo, query/fragment).
        if host.is_empty()
            || host
                .contains(|c: char| c.is_whitespace() || matches!(c, '/' | '\\' | '@' | '?' | '#'))
        {
            return Err(ApiError::bad_request("invalid Host header"));
        }
        format!("http://{host}")
    };
    let mut state_bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut state_bytes);
    let state_tok = hex::encode(state_bytes);
    {
        let mut g = st.oauth_state.lock().unwrap_or_else(|e| e.into_inner());
        g.retain(|_, t| t.elapsed() < Duration::from_secs(600));
        if g.len() >= 64 {
            // Evict oldest pending OAuth state to bound memory
            if let Some(oldest) = g.iter().min_by_key(|(_, t)| *t).map(|(k, _)| k.clone()) {
                g.remove(&oldest);
            }
        }
        g.insert(state_tok.clone(), Instant::now());
    }
    let callback = format!("{callback_base}/api/v1/qobuz/oauth/callback?state={state_tok}");
    let url = qb.oauth_url(&callback).await?;
    Ok(Redirect::temporary(&url).into_response())
}

pub async fn qobuz_oauth_callback(
    State(st): State<Arc<AppState>>,
    Query(qv): Query<HashMap<String, String>>,
) -> ApiResult<Response> {
    let qb = st.qobuz.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "QOBUZ_UNAVAILABLE",
            "qobuz not configured",
            false,
        )
    })?;
    let state_tok = qv.get("state").cloned().unwrap_or_default();
    let valid = {
        let mut g = st.oauth_state.lock().unwrap_or_else(|e| e.into_inner());
        g.remove(&state_tok)
            .is_some_and(|t| t.elapsed() < Duration::from_secs(600))
    };
    if !valid {
        return Err(ApiError::bad_request("invalid or expired OAuth state"));
    }
    let code = qv
        .get("code_autorisation")
        .or_else(|| qv.get("code"))
        .or_else(|| qv.get("code_ext"))
        .cloned()
        .unwrap_or_default();
    if code.trim().is_empty() {
        return Err(ApiError::bad_request("missing authorization code"));
    }
    let status = qb.exchange_oauth_code(&code).await?;
    let _ = st
        .catalog
        .set_provider_credentials(SOURCE_QOBUZ, &qb.export_credentials());
    let _ = st.catalog.set_provider_available(SOURCE_QOBUZ, true);
    st.manager.broadcast("qobuz.status", &status);
    let st_bg = Arc::clone(&st);
    tokio::spawn(async move {
        let _ = st_bg.catalog.sync_remote(SOURCE_QOBUZ).await;
        st_bg.prewarm_artists();
    });

    Ok(Html(
        r#"<!doctype html><html><body><script>
        if (window.opener) { window.opener.postMessage({type:'tacet:qobuz-auth',ok:true}, '*'); window.close(); }
        else { window.location.replace('/'); }
        </script><p>Qobuz connected. You can close this window.</p></body></html>"#,
    )
    .into_response())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct QobuzLoginReq {
    #[serde(default)]
    email: String,
    #[serde(default)]
    password: String,
    #[serde(default, alias = "token")]
    auth_token: String,
    #[serde(default)]
    app_id: String,
    #[serde(default)]
    app_secret: String,
}

pub async fn qobuz_login(
    State(st): State<Arc<AppState>>,
    body: Bytes,
) -> ApiResult<Json<serde_json::Value>> {
    let qb = st.qobuz.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "QOBUZ_UNAVAILABLE",
            "qobuz not configured",
            false,
        )
    })?;
    let req: QobuzLoginReq = parse_strict_json(&body)?;
    if !req.app_id.trim().is_empty() || !req.app_secret.trim().is_empty() {
        qb.set_credentials(req.app_id.trim(), req.app_secret.trim(), "");
    }
    let status = if !req.auth_token.trim().is_empty() {
        qb.login_with_token(&req.auth_token).await?
    } else {
        qb.login(&req.email, &req.password).await?
    };
    let _ = st
        .catalog
        .set_provider_credentials(SOURCE_QOBUZ, &qb.export_credentials());
    let _ = st.catalog.set_provider_available(SOURCE_QOBUZ, true);
    st.manager.broadcast("qobuz.status", &status);
    st.prewarm_artists();
    Ok(Json(serde_json::to_value(status).unwrap_or_default()))
}

pub async fn qobuz_logout(State(st): State<Arc<AppState>>) -> ApiResult<Json<serde_json::Value>> {
    let qb = st.qobuz.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "QOBUZ_UNAVAILABLE",
            "qobuz not configured",
            false,
        )
    })?;
    let status = qb.logout();
    let _ = st.catalog.delete_provider_credentials(SOURCE_QOBUZ);
    let _ = st.catalog.set_provider_available(SOURCE_QOBUZ, false);
    st.manager.broadcast("qobuz.status", &status);
    Ok(Json(serde_json::to_value(status).unwrap_or_default()))
}

pub async fn qobuz_search(
    State(st): State<Arc<AppState>>,
    Query(qv): Query<HashMap<String, String>>,
) -> ApiResult<Json<serde_json::Value>> {
    let qb = st.qobuz.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "QOBUZ_UNAVAILABLE",
            "qobuz not configured",
            false,
        )
    })?;
    let q = qv.get("q").cloned().unwrap_or_default();
    let (limit, offset) = parse_page(&qv, 30, 100);
    let mut res = qb.search(&q, limit, offset).await?;
    res.albums = st.catalog.prefer_local_albums(res.albums);
    res.tracks = st.catalog.prefer_local_tracks(res.tracks);
    Ok(Json(serde_json::to_value(res).unwrap_or_default()))
}

pub async fn qobuz_artist(
    State(st): State<Arc<AppState>>,
    Query(qv): Query<HashMap<String, String>>,
) -> ApiResult<Json<serde_json::Value>> {
    let qb = st.qobuz.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "QOBUZ_UNAVAILABLE",
            "qobuz not configured",
            false,
        )
    })?;
    let (limit, _) = parse_page(&qv, 50, 200);
    let mut detail = if let Some(id_s) = qv.get("id")
        && let Ok(id) = id_s.parse::<i64>()
        && id > 0
    {
        qb.get_artist(id, limit).await?
    } else if let Some(name) = qv.get("name").filter(|s| !s.trim().is_empty()) {
        qb.get_artist_by_name(name, limit).await?
    } else {
        return Err(ApiError::bad_request(
            "id or name query parameter is required",
        ));
    };
    detail.albums = st.catalog.prefer_local_albums(detail.albums);
    detail.tracks = st.catalog.prefer_local_tracks(detail.tracks);
    Ok(Json(serde_json::to_value(detail).unwrap_or_default()))
}

pub async fn qobuz_album(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let qb = st.qobuz.as_ref().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "QOBUZ_UNAVAILABLE",
            "qobuz not configured",
            false,
        )
    })?;
    let mut alb = qb.get_album(&id).await?;
    if let Some(lib_alb) = st.catalog.album(&alb.id) {
        if !lib_alb.is_partial {
            alb.in_library = lib_alb.available;
            alb.version_group_id = lib_alb.version_group_id;
            alb.version_count = lib_alb.version_count;
            alb.is_primary = lib_alb.is_primary;
            alb.versions = lib_alb.versions;
            alb.added_at = lib_alb.added_at;
        }
        alb.play_count = lib_alb.play_count;
        let lib_tracks: HashMap<&str, &crate::library::Track> =
            lib_alb.tracks.iter().map(|t| (t.id.as_str(), t)).collect();
        for tr in &mut alb.tracks {
            if let Some(lt) = lib_tracks.get(tr.id.as_str()) {
                tr.in_library = lt.available;
                tr.play_count = lt.play_count;
                tr.last_played_at = lt.last_played_at;
            }
        }
    }
    Ok(Json(serde_json::to_value(alb).unwrap_or_default()))
}

// --- Discover ---

pub async fn discover_feed(
    State(st): State<Arc<AppState>>,
    Query(qv): Query<HashMap<String, String>>,
) -> ApiResult<Json<serde_json::Value>> {
    let Some(qb) = st.qobuz.as_ref().filter(|q| q.is_connected()) else {
        return Ok(Json(serde_json::json!({
            "connected": false,
            "genres": [],
            "shelves": [],
        })));
    };
    let genre_ids = parse_genre_ids(
        qv.get("genreIds")
            .or_else(|| qv.get("genre_ids"))
            .map(String::as_str),
    );
    let mut feed = qb.discover(&genre_ids).await?;
    for shelf in &mut feed.shelves {
        shelf.items = st
            .catalog
            .prefer_local_albums(std::mem::take(&mut shelf.items));
    }
    Ok(Json(serde_json::to_value(feed).unwrap_or_default()))
}

pub async fn discover_shelf(
    State(st): State<Arc<AppState>>,
    Path(slug): Path<String>,
    Query(qv): Query<HashMap<String, String>>,
) -> ApiResult<Json<serde_json::Value>> {
    let (limit, offset) = parse_page(&qv, 24, 100);
    let Some(qb) = st.qobuz.as_ref().filter(|q| q.is_connected()) else {
        return Ok(Json(serde_json::json!({
            "slug": slug,
            "items": [],
            "limit": limit,
            "offset": offset,
        })));
    };
    let genre_ids = parse_genre_ids(
        qv.get("genreIds")
            .or_else(|| qv.get("genre_ids"))
            .map(String::as_str),
    );
    let mut page = qb.discover_shelf(&slug, &genre_ids, limit, offset).await?;
    page.items = st.catalog.prefer_local_albums(page.items);
    Ok(Json(serde_json::to_value(page).unwrap_or_default()))
}

pub async fn similar_albums(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(qv): Query<HashMap<String, String>>,
) -> ApiResult<Json<serde_json::Value>> {
    let id = id.trim().to_string();
    if id.is_empty() {
        return Err(ApiError::bad_request("album id is required"));
    }
    let (limit, _) = parse_page(&qv, 12, 50);
    let Some(qb) = st.qobuz.as_ref().filter(|q| q.is_connected()) else {
        return Ok(Json(serde_json::json!({ "items": [] })));
    };

    let qobuz_id = resolve_similar_qobuz_album_id(&st, qb, &id).await;
    if qobuz_id.is_empty() {
        return Ok(Json(serde_json::json!({ "items": [] })));
    }

    let items = match qb.similar_albums(&qobuz_id, limit).await {
        Ok(items) => st.catalog.prefer_local_albums(items),
        Err(crate::qobuz::QobuzError::NotFound) => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    Ok(Json(serde_json::json!({ "items": items })))
}

async fn resolve_similar_qobuz_album_id(
    st: &AppState,
    qb: &crate::qobuz::Client,
    id: &str,
) -> String {
    if let Some(raw) = id.strip_prefix("qobuz:") {
        return raw.trim().to_string();
    }
    let Some(alb) = st.catalog.album(id) else {
        return String::new();
    };
    for v in &alb.versions {
        if let Some(raw) = v.id.strip_prefix("qobuz:") {
            return raw.trim().to_string();
        }
    }
    if qb.is_rate_limited() {
        return String::new();
    }
    let matched = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        qb.match_album(&alb.artist, &alb.title),
    )
    .await;
    match matched {
        Ok(Ok(m)) => {
            m.id.trim()
                .strip_prefix("qobuz:")
                .unwrap_or(m.id.trim())
                .to_string()
        }
        _ => String::new(),
    }
}

pub async fn similar_artists(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(qv): Query<HashMap<String, String>>,
) -> ApiResult<Json<serde_json::Value>> {
    let raw_id = id.trim();
    let numeric = raw_id.strip_prefix("qobuz:").unwrap_or(raw_id);
    let artist_id: i64 = numeric
        .parse()
        .ok()
        .filter(|&n| n > 0)
        .ok_or_else(|| ApiError::bad_request("invalid artist id"))?;
    let (limit, _) = parse_page(&qv, 12, 50);
    let Some(qb) = st.qobuz.as_ref().filter(|q| q.is_connected()) else {
        return Ok(Json(serde_json::json!({ "items": [] })));
    };
    let items = match qb.similar_artists(artist_id, limit).await {
        Ok(items) => items,
        Err(crate::qobuz::QobuzError::NotFound) => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    Ok(Json(serde_json::json!({ "items": items })))
}
