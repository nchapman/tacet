use std::collections::HashMap;
use std::io::SeekFrom;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::library::Catalog;
use crate::qobuz::Client as QobuzClient;
use crate::upnp::{DLNA_FEATURES_SEEKABLE, DLNA_FEATURES_TRANSCODED};

use super::art::{
    ART_FORMAT_JPEG, ART_FORMAT_WEBP, ART_SIZE_SMALL, ArtCache, accepts_webp, hash_art_key,
    is_art_hash, parse_art_bucket,
};
use super::transcode::{detect_transcoder, parse_transcode_spec, spawn_ffmpeg_stream};

const MAX_REMOTE_ART_BYTES: usize = 10 << 20;

pub struct MediaHandler {
    catalog: Arc<Catalog>,
    qobuz: Option<Arc<QobuzClient>>,
    art: RwLockArt,
    relay_http: reqwest::Client,
    remote_art_http: reqwest::Client,
    prewarm_cancel: Mutex<Option<CancellationToken>>,
}

struct RwLockArt(std::sync::RwLock<Arc<ArtCache>>);

impl RwLockArt {
    fn get(&self) -> Arc<ArtCache> {
        Arc::clone(&self.0.read().unwrap_or_else(|e| e.into_inner()))
    }
    fn set(&self, next: Arc<ArtCache>) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = next;
    }
}

impl MediaHandler {
    pub fn new(catalog: Arc<Catalog>, qobuz: Option<Arc<QobuzClient>>) -> Arc<Self> {
        let relay_http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(8)
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 5 {
                    return attempt.error("too many redirects");
                }
                if attempt.url().scheme() != "https" {
                    return attempt.error("refusing non-HTTPS relay redirect");
                }
                if let Some(host) = attempt.url().host_str()
                    && let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>()
                    && is_private_or_loopback(ip)
                {
                    return attempt.error("refusing private relay redirect target");
                }
                attempt.follow()
            }))
            .build()
            .expect("build media relay reqwest client");

        let remote_art_http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 3 {
                    return attempt.error("too many remote art redirects");
                }
                if attempt.url().scheme() != "https" {
                    return attempt.error("refusing non-HTTPS remote art redirect");
                }
                let host = attempt.url().host_str().unwrap_or("");
                if !is_allowed_remote_art_host(host) {
                    return attempt.error("refusing remote art redirect off allowlisted host");
                }
                attempt.follow()
            }))
            .build()
            .expect("build remote art reqwest client");

        Arc::new(Self {
            catalog,
            qobuz,
            art: RwLockArt(std::sync::RwLock::new(ArtCache::new(None))),
            relay_http,
            remote_art_http,
            prewarm_cancel: Mutex::new(None),
        })
    }

    pub fn set_art_cache_dir(&self, dir: PathBuf) {
        self.art.set(ArtCache::new(Some(dir)));
    }

    pub fn close(&self) {
        if let Some(tok) = self
            .prewarm_cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            tok.cancel();
        }
    }

    pub fn prewarm_art(self: &Arc<Self>) {
        let art = self.art.get();
        if art.dir().is_none() {
            return;
        }
        let cancel = CancellationToken::new();
        {
            let mut g = self
                .prewarm_cancel
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(prev) = g.replace(cancel.clone()) {
                prev.cancel();
            }
        }
        let cat = Arc::clone(&self.catalog);
        tokio::spawn(async move {
            let Ok(hashes) = cat.local_art_hashes() else {
                return;
            };
            let fmt = art.preferred_format();
            let pending: Vec<String> = hashes
                .into_iter()
                .filter(|h| !art.exists_disk(h, ART_SIZE_SMALL, fmt))
                .collect();
            if pending.is_empty() {
                return;
            }
            for hash in pending {
                if cancel.is_cancelled() {
                    return;
                }
                let cat_c = Arc::clone(&cat);
                let art_c = Arc::clone(&art);
                let _ = tokio::task::spawn_blocking(move || {
                    art_c.get_or_build(&hash, ART_SIZE_SMALL, fmt, || {
                        cat_c.cover_art_by_hash(&hash)
                    })
                })
                .await;
            }
        });
    }

    /// Serves `/media/local/{id}` (optional extension stripped, optional `?q=` transcoding).
    pub async fn serve_local(
        &self,
        method: Method,
        raw_id: &str,
        query: &HashMap<String, String>,
        headers: &HeaderMap,
    ) -> Response {
        let track_id = strip_ext(raw_id);
        let Some(track) = self.catalog.track(track_id) else {
            return (StatusCode::NOT_FOUND, "track not found").into_response();
        };
        if track.file_path.is_empty() {
            return (StatusCode::NOT_FOUND, "local file missing").into_response();
        }

        let quality = query.get("q").map(String::as_str).unwrap_or("");
        let container = query
            .get("container")
            .or_else(|| query.get("c"))
            .map(String::as_str)
            .unwrap_or("");
        let tc_info = detect_transcoder();
        if let Some(spec) = parse_transcode_spec(quality, container, &track, tc_info) {
            let Some(ref ffmpeg) = tc_info.ffmpeg_path else {
                return (StatusCode::SERVICE_UNAVAILABLE, "ffmpeg unavailable").into_response();
            };
            let seek_sec = query.get("t").and_then(|s| s.parse::<f64>().ok());
            if method == Method::HEAD {
                return dlna_headers_response(
                    spec.mime,
                    DLNA_FEATURES_TRANSCODED,
                    None,
                    Body::empty(),
                );
            }
            return match spawn_ffmpeg_stream(ffmpeg, &track.file_path, &spec, seek_sec).await {
                Ok(body) => dlna_headers_response(spec.mime, DLNA_FEATURES_TRANSCODED, None, body),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("transcode failed: {e}"),
                )
                    .into_response(),
            };
        }

        serve_local_file_range(method, &track.file_path, &track.mime, headers).await
    }

    /// Serves `/media/qobuz/{id}` (relaying Qobuz CDN stream or transcoding via ffmpeg).
    pub async fn serve_qobuz(
        &self,
        method: Method,
        raw_id: &str,
        query: &HashMap<String, String>,
        headers: &HeaderMap,
    ) -> Response {
        let Some(ref qb) = self.qobuz else {
            return (StatusCode::SERVICE_UNAVAILABLE, "qobuz not configured").into_response();
        };
        let clean = strip_ext(raw_id);
        let clean = clean.strip_prefix("qobuz:").unwrap_or(clean);
        let Ok(track_id) = clean.parse::<i64>() else {
            return (StatusCode::BAD_REQUEST, "invalid qobuz track id").into_response();
        };
        let info = match qb.stream_url(track_id).await {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(error = %e, "qobuz stream URL lookup failed");
                return (StatusCode::BAD_GATEWAY, "qobuz stream URL failed").into_response();
            }
        };

        let quality = query.get("q").map(String::as_str).unwrap_or("");
        let container = query
            .get("container")
            .or_else(|| query.get("c"))
            .map(String::as_str)
            .unwrap_or("");
        let tc_info = detect_transcoder();
        let synthetic_track = crate::library::Track {
            sample_rate: info.sample_rate,
            bit_depth: info.bit_depth,
            channels: 2,
            mime: info.mime.clone(),
            ..Default::default()
        };
        if let Some(spec) = parse_transcode_spec(quality, container, &synthetic_track, tc_info) {
            let Some(ref ffmpeg) = tc_info.ffmpeg_path else {
                return (StatusCode::SERVICE_UNAVAILABLE, "ffmpeg unavailable").into_response();
            };
            let seek_sec = query.get("t").and_then(|s| s.parse::<f64>().ok());
            if method == Method::HEAD {
                return dlna_headers_response(
                    spec.mime,
                    DLNA_FEATURES_TRANSCODED,
                    None,
                    Body::empty(),
                );
            }
            return match spawn_ffmpeg_stream(ffmpeg, &info.url, &spec, seek_sec).await {
                Ok(body) => dlna_headers_response(spec.mime, DLNA_FEATURES_TRANSCODED, None, body),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("transcode failed: {e}"),
                )
                    .into_response(),
            };
        }

        if query
            .get("handoff")
            .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        {
            return Response::builder()
                .status(StatusCode::FOUND)
                .header(header::LOCATION, &info.url)
                .body(Body::empty())
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }

        self.relay_qobuz_stream(method, &info, headers).await
    }

    async fn relay_qobuz_stream(
        &self,
        method: Method,
        info: &crate::qobuz::StreamInfo,
        headers: &HeaderMap,
    ) -> Response {
        // Direct HTTP relay from Qobuz CDN with Range forwarding
        let mut req = if method == Method::HEAD {
            self.relay_http.head(&info.url)
        } else {
            self.relay_http.get(&info.url)
        };
        if let Some(range) = headers.get(header::RANGE)
            && let Ok(rs) = range.to_str()
        {
            req = req.header("Range", rs);
        }

        let upstream = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e.without_url(), "qobuz relay upstream request failed");
                return (StatusCode::BAD_GATEWAY, "qobuz relay failed").into_response();
            }
        };

        let status =
            StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let mut builder = Response::builder().status(status);
        let ct = upstream
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty() && *s != "audio/x-flac")
            .unwrap_or(&info.mime);
        builder = builder
            .header(header::CONTENT_TYPE, ct)
            .header("Accept-Ranges", "bytes")
            .header("transferMode.dlna.org", "Streaming")
            .header("contentFeatures.dlna.org", DLNA_FEATURES_SEEKABLE);
        if let Some(cl) = upstream.headers().get(header::CONTENT_LENGTH) {
            builder = builder.header(header::CONTENT_LENGTH, cl);
        }
        if let Some(cr) = upstream.headers().get(header::CONTENT_RANGE) {
            builder = builder.header(header::CONTENT_RANGE, cr);
        }

        let body = if method == Method::HEAD {
            Body::empty()
        } else {
            Body::from_stream(upstream.bytes_stream())
        };
        builder
            .body(body)
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    }

    /// Serves `/media/art/{id}` (content hash or track id) with `?w=` size bucketing and Accept WebP negotiation.
    pub async fn serve_art(
        &self,
        raw_id: &str,
        query: &HashMap<String, String>,
        headers: &HeaderMap,
    ) -> Response {
        let id = raw_id.trim();
        if id.is_empty() {
            return StatusCode::NOT_FOUND.into_response();
        }
        let bucket = parse_art_bucket(query.get("w").map(String::as_str));
        let accept = headers
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let fmt = if accepts_webp(accept) {
            ART_FORMAT_WEBP
        } else {
            ART_FORMAT_JPEG
        };

        let key = if is_art_hash(id) {
            id.to_string()
        } else {
            hash_art_key(id)
        };
        let etag = format!("\"art-{key}-{bucket}-{}\"", fmt.ext);
        if headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|m| m.trim() == etag)
        {
            return Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header(header::ETAG, etag)
                .header(header::VARY, "Accept")
                .body(Body::empty())
                .unwrap_or_else(|_| StatusCode::NOT_MODIFIED.into_response());
        }

        let cat = Arc::clone(&self.catalog);
        let art = self.art.get();
        let id_owned = id.to_string();
        let key_owned = key.clone();
        let res = tokio::task::spawn_blocking(move || {
            art.get_or_build(&key_owned, bucket, fmt, || {
                if is_art_hash(&id_owned) {
                    cat.cover_art_by_hash(&id_owned)
                } else {
                    cat.cover_art(&id_owned)
                }
            })
        })
        .await
        .ok()
        .flatten();

        let Some((bytes, mime)) = res else {
            return StatusCode::NOT_FOUND.into_response();
        };

        let cache_ctrl = if is_art_hash(id) {
            "public, max-age=31536000, immutable"
        } else {
            "public, max-age=86400"
        };

        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime)
            .header(header::CONTENT_LENGTH, bytes.len())
            .header(header::ETAG, etag)
            .header(header::CACHE_CONTROL, cache_ctrl)
            .header("X-Content-Type-Options", "nosniff")
            .header(header::VARY, "Accept")
            .body(Body::from(bytes))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    }

    /// Serves `/media/art/remote?url=...&w=...` for allowlisted remote artwork hosts (`*.qobuz.com`).
    pub async fn serve_remote_art(
        &self,
        query: &HashMap<String, String>,
        headers: &HeaderMap,
    ) -> Response {
        let Some(raw_url) = query.get("url").map(|s| s.trim()).filter(|s| !s.is_empty()) else {
            return (StatusCode::BAD_REQUEST, "missing url").into_response();
        };
        let Ok(parsed) = Url::parse(raw_url) else {
            return (StatusCode::BAD_REQUEST, "invalid url").into_response();
        };
        if parsed.scheme() != "https"
            || !is_allowed_remote_art_host(parsed.host_str().unwrap_or(""))
        {
            return (StatusCode::FORBIDDEN, "remote art host not allowed").into_response();
        }

        let bucket = parse_art_bucket(query.get("w").map(String::as_str));
        let accept = headers
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let fmt = if accepts_webp(accept) {
            ART_FORMAT_WEBP
        } else {
            ART_FORMAT_JPEG
        };
        let key = hash_art_key(raw_url);
        let art = self.art.get();

        // Check cache first
        if let Some((bytes, mime)) = art.get_or_build(&key, bucket, fmt, || None) {
            return Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime)
                .header(header::CACHE_CONTROL, "public, max-age=604800")
                .header(header::VARY, "Accept")
                .body(Body::from(bytes))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }

        let mut resp = match self.remote_art_http.get(parsed.as_str()).send().await {
            Ok(r) if r.status().is_success() => r,
            _ => return StatusCode::BAD_GATEWAY.into_response(),
        };
        if let Some(cl) = resp.content_length()
            && cl as usize > MAX_REMOTE_ART_BYTES
        {
            return StatusCode::BAD_GATEWAY.into_response();
        }
        let mut raw_bytes = Vec::new();
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    if raw_bytes.len().saturating_add(chunk.len()) > MAX_REMOTE_ART_BYTES {
                        return StatusCode::BAD_GATEWAY.into_response();
                    }
                    raw_bytes.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
            }
        }
        if raw_bytes.is_empty() {
            return StatusCode::BAD_GATEWAY.into_response();
        }
        let raw_vec = raw_bytes.to_vec();
        let res = tokio::task::spawn_blocking(move || {
            art.get_or_build(&key, bucket, fmt, || Some((raw_vec, "image/jpeg".into())))
        })
        .await
        .ok()
        .flatten();

        let Some((bytes, mime)) = res else {
            return StatusCode::BAD_GATEWAY.into_response();
        };
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime)
            .header(header::CACHE_CONTROL, "public, max-age=604800")
            .header(header::VARY, "Accept")
            .body(Body::from(bytes))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    }
}

async fn serve_local_file_range(
    method: Method,
    path: &str,
    mime: &str,
    headers: &HeaderMap,
) -> Response {
    let Ok(mut file) = tokio::fs::File::open(path).await else {
        return (StatusCode::NOT_FOUND, "file not found").into_response();
    };
    let Ok(meta) = file.metadata().await else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "stat failed").into_response();
    };
    let file_size = meta.len();
    let content_type = if mime.is_empty() || mime == "audio/x-flac" {
        "audio/flac"
    } else {
        mime
    };

    let (start, end, is_partial) =
        if let Some(range_val) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
            match parse_byte_range(range_val, file_size) {
                Some((s, e)) => (s, e, true),
                None => {
                    return Response::builder()
                        .status(StatusCode::RANGE_NOT_SATISFIABLE)
                        .header(header::CONTENT_RANGE, format!("bytes */{file_size}"))
                        .body(Body::empty())
                        .unwrap_or_else(|_| StatusCode::RANGE_NOT_SATISFIABLE.into_response());
                }
            }
        } else {
            (0, file_size.saturating_sub(1), false)
        };

    let len = if file_size == 0 { 0 } else { end - start + 1 };
    let status = if is_partial {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };

    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, len.to_string())
        .header(header::ACCEPT_RANGES, "bytes")
        .header("transferMode.dlna.org", "Streaming")
        .header("contentFeatures.dlna.org", DLNA_FEATURES_SEEKABLE);
    if is_partial {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{file_size}"),
        );
    }

    if method == Method::HEAD || len == 0 {
        return builder
            .body(Body::empty())
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }

    if start > 0 && file.seek(SeekFrom::Start(start)).await.is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let limited = file.take(len);
    let stream = tokio_util::io::ReaderStream::with_capacity(limited, 64 * 1024);

    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn dlna_headers_response(
    mime: &str,
    dlna_features: &str,
    content_length: Option<u64>,
    body: Body,
) -> Response {
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .header("transferMode.dlna.org", "Streaming")
        .header("contentFeatures.dlna.org", dlna_features);
    if let Some(cl) = content_length {
        builder = builder.header(header::CONTENT_LENGTH, HeaderValue::from(cl));
    }
    builder
        .body(body)
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

pub fn parse_byte_range(hdr: &str, total: u64) -> Option<(u64, u64)> {
    if total == 0 {
        return None;
    }
    let spec = hdr.trim().strip_prefix("bytes=")?;
    let first = spec.split(',').next()?.trim();
    let (s_str, e_str) = first.split_once('-')?;
    if s_str.is_empty() {
        let suffix: u64 = e_str.trim().parse().ok()?;
        if suffix == 0 {
            return None;
        }
        let start = total.saturating_sub(suffix);
        return Some((start, total - 1));
    }
    let start: u64 = s_str.trim().parse().ok()?;
    if start >= total {
        return None;
    }
    let end = if e_str.trim().is_empty() {
        total - 1
    } else {
        e_str.trim().parse::<u64>().ok()?.min(total - 1)
    };
    if end < start {
        return None;
    }
    Some((start, end))
}

fn strip_ext(raw: &str) -> &str {
    match raw.rsplit_once('.') {
        Some((stem, ext))
            if !stem.is_empty()
                && ext.len() <= 5
                && ext.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            stem
        }
        _ => raw,
    }
}

pub fn is_allowed_remote_art_host(host: &str) -> bool {
    let h = host.trim().to_ascii_lowercase();
    h == "static.qobuz.com" || h.ends_with(".qobuz.com")
}

fn is_private_or_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let oct = v4.octets();
            let is_cgnat = oct[0] == 100 && (oct[1] & 0b1100_0000) == 64;
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                || is_cgnat
        }
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_private_or_loopback(IpAddr::V4(mapped));
            }
            let segs = v6.segments();
            let is_ula = (segs[0] & 0xfe00) == 0xfc00;
            let is_link_local = (segs[0] & 0xffc0) == 0xfe80;
            v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() || is_ula || is_link_local
        }
    }
}
