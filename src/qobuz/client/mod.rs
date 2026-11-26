mod catalog;
mod favorites;

use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::cache::{Cache, CacheOptions, Namespaced, Ttl};
use crate::library::Track;
use crate::singleflight::SingleFlight;

use super::types::{
    DEFAULT_BASE_URL, FORMAT_FLAC_192, PersistedCredentials, QobuzError, Result, Status,
    StreamInfo, decode_qobuz_json, md5_hex, norm_match_key, parse_quality, quality_label,
};

pub(super) const MAX_URL_CACHE_ENTRIES: usize = 512;
const MAX_TRACK_CACHE_ENTRIES: usize = 4096;

pub(super) const ALBUM_CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
pub(super) const ARTIST_CACHE_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
const ARTIST_NAME_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
const ARTIST_MISS_TTL: Duration = Duration::from_secs(24 * 3600);
const ALBUM_MATCH_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
const ALBUM_MISS_TTL: Duration = Duration::from_secs(24 * 3600);
pub(super) const SEARCH_CACHE_TTL: Duration = Duration::from_secs(3600);

const MAX_CONCURRENT_REQUESTS: usize = 3;
const MAX_RETRY_ATTEMPTS: usize = 3;

#[derive(Default)]
pub struct Config {
    pub base_url: String,
    pub app_id: String,
    pub app_secret: String,
    pub auth_token: String,
    pub format_id: i32,
    pub cache: Option<Arc<Cache>>,
}

pub(super) struct ClientInner {
    pub(super) app_id: String,
    pub(super) app_secret: String,
    pub(super) private_key: String,
    pub(super) alt_secrets: Vec<String>,
    pub(super) auth_token: String,
    pub(super) user_name: String,
    pub(super) subscription: String,
    pub(super) format_id: i32,
    pub(super) url_cache: HashMap<i64, StreamInfo>,
    pub(super) track_cache: HashMap<i64, Track>,
}

pub struct Client {
    base_url: String,
    http: reqwest::Client,
    pub(super) inner: RwLock<ClientInner>,
    pub(super) kv: Option<Namespaced>,
    _owned_cache: Option<Arc<Cache>>,
    pub(super) auth_gen: AtomicU64,
    sf: SingleFlight<String, Vec<u8>, String>,
    req_sem: Arc<Semaphore>,
    cooldown_until_ms: AtomicI64,
    consecutive_429: AtomicI32,
    pub(super) prewarm_cancel: Mutex<Option<CancellationToken>>,
}

impl Client {
    pub fn new(cfg: Config) -> Arc<Self> {
        let base = cfg.base_url.trim().trim_end_matches('/').to_string();
        let base_url = if base.is_empty() {
            DEFAULT_BASE_URL.into()
        } else {
            base
        };
        let format_id = if cfg.format_id == 0 {
            FORMAT_FLAC_192
        } else {
            cfg.format_id
        };

        let (backing, owned) = match cfg.cache {
            Some(c) => (Some(c), None),
            None => match Cache::open(":memory:", CacheOptions::default()) {
                Ok(c) => (Some(Arc::clone(&c)), Some(c)),
                Err(_) => (None, None),
            },
        };
        let kv = backing.map(|c| c.namespace("qobuz"));

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .pool_max_idle_per_host(8)
            .build()
            .expect("build qobuz reqwest client");

        Arc::new(Self {
            base_url,
            http,
            inner: RwLock::new(ClientInner {
                app_id: cfg.app_id.trim().to_string(),
                app_secret: cfg.app_secret.trim().to_string(),
                private_key: String::new(),
                alt_secrets: Vec::new(),
                auth_token: cfg.auth_token.trim().to_string(),
                user_name: String::new(),
                subscription: String::new(),
                format_id,
                url_cache: HashMap::new(),
                track_cache: HashMap::new(),
            }),
            kv,
            _owned_cache: owned,
            auth_gen: AtomicU64::new(0),
            sf: SingleFlight::new(),
            req_sem: Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS)),
            cooldown_until_ms: AtomicI64::new(0),
            consecutive_429: AtomicI32::new(0),
            prewarm_cancel: Mutex::new(None),
        })
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

    pub fn status(&self) -> Status {
        let g = self.inner.read().unwrap_or_else(|e| e.into_inner());
        Status {
            configured: !g.app_id.is_empty() && !g.app_secret.is_empty(),
            authenticated: !g.auth_token.is_empty() && !g.app_id.is_empty(),
            user_name: g.user_name.clone(),
            subscription: g.subscription.clone(),
            max_format_id: g.format_id,
            max_quality: quality_label(g.format_id).into(),
        }
    }

    pub fn is_connected(&self) -> bool {
        let g = self.inner.read().unwrap_or_else(|e| e.into_inner());
        !g.auth_token.is_empty() && !g.app_id.is_empty()
    }

    pub fn max_quality(&self) -> String {
        let g = self.inner.read().unwrap_or_else(|e| e.into_inner());
        quality_label(g.format_id).into()
    }

    pub fn set_max_quality(&self, quality: &str) -> Result<()> {
        let fid = parse_quality(quality)?;
        let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
        if g.format_id != fid {
            g.format_id = fid;
            g.url_cache.clear();
        }
        Ok(())
    }

    pub fn logout(&self) -> Status {
        {
            let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
            g.auth_token.clear();
            g.user_name.clear();
            g.subscription.clear();
            g.url_cache.clear();
            g.track_cache.clear();
        }
        self.auth_gen.fetch_add(1, Ordering::Relaxed);
        if let Some(kv) = &self.kv {
            let _ = kv.clear();
        }
        self.status()
    }

    pub fn export_credentials(&self) -> PersistedCredentials {
        let g = self.inner.read().unwrap_or_else(|e| e.into_inner());
        PersistedCredentials {
            app_id: g.app_id.clone(),
            app_secret: g.app_secret.clone(),
            private_key: g.private_key.clone(),
            alt_secrets: g.alt_secrets.clone(),
            auth_token: g.auth_token.clone(),
            user_name: g.user_name.clone(),
            subscription: g.subscription.clone(),
        }
    }

    pub fn restore_credentials(&self, creds: PersistedCredentials) {
        let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
        if !creds.app_id.trim().is_empty() && g.app_id.is_empty() {
            g.app_id = creds.app_id.trim().to_string();
        }
        if !creds.app_secret.trim().is_empty() && g.app_secret.is_empty() {
            g.app_secret = creds.app_secret.trim().to_string();
        }
        if !creds.private_key.trim().is_empty() && g.private_key.is_empty() {
            g.private_key = creds.private_key.trim().to_string();
        }
        if !creds.alt_secrets.is_empty() && g.alt_secrets.is_empty() {
            g.alt_secrets = creds.alt_secrets;
        }
        if !creds.auth_token.trim().is_empty() && g.auth_token.is_empty() {
            g.auth_token = creds.auth_token.trim().to_string();
            g.user_name = creds.user_name.trim().to_string();
            g.subscription = creds.subscription.trim().to_string();
        }
        g.url_cache.clear();
    }

    pub fn set_credentials(&self, app_id: &str, app_secret: &str, auth_token: &str) {
        let switched = {
            let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
            if !app_id.trim().is_empty() {
                g.app_id = app_id.trim().to_string();
            }
            if !app_secret.trim().is_empty() {
                g.app_secret = app_secret.trim().to_string();
            }
            let new_tok = auth_token.trim();
            let switched =
                !new_tok.is_empty() && !g.auth_token.is_empty() && g.auth_token != new_tok;
            if !new_tok.is_empty() {
                g.auth_token = new_tok.to_string();
            }
            g.url_cache.clear();
            if switched {
                g.track_cache.clear();
            }
            switched
        };
        if switched {
            self.auth_gen.fetch_add(1, Ordering::Relaxed);
            if let Some(kv) = &self.kv {
                let _ = kv.clear();
            }
        }
    }

    pub(crate) fn app_id(&self) -> String {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .app_id
            .clone()
    }

    pub(crate) fn private_key(&self) -> String {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .private_key
            .clone()
    }

    pub(crate) fn has_app_credentials(&self) -> (bool, bool) {
        let g = self.inner.read().unwrap_or_else(|e| e.into_inner());
        (!g.app_id.is_empty(), !g.app_secret.is_empty())
    }

    pub(crate) fn set_discovered_app_credentials(
        &self,
        app_id: String,
        secrets: Vec<String>,
        priv_key: String,
    ) {
        let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
        g.app_id = app_id;
        if let Some(first) = secrets.first() {
            g.app_secret = first.clone();
        }
        g.alt_secrets = secrets;
        if !priv_key.is_empty() {
            g.private_key = priv_key;
        }
    }

    pub(crate) fn apply_login(&self, token: String, name: String, sub: String) {
        let switched = {
            let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
            let switched = (!g.auth_token.is_empty() && g.auth_token != token)
                || (!g.user_name.is_empty() && !name.is_empty() && g.user_name != name);
            g.auth_token = token;
            g.user_name = name;
            g.subscription = sub;
            g.url_cache.clear();
            if switched {
                g.track_cache.clear();
            }
            switched
        };
        if switched {
            self.auth_gen.fetch_add(1, Ordering::Relaxed);
            if let Some(kv) = &self.kv {
                let _ = kv.clear();
            }
        }
    }

    pub(crate) fn require_auth(&self) -> Result<()> {
        let g = self.inner.read().unwrap_or_else(|e| e.into_inner());
        if g.app_id.is_empty() || g.auth_token.is_empty() {
            return Err(QobuzError::Unauthenticated);
        }
        Ok(())
    }

    pub fn is_rate_limited(&self) -> bool {
        now_ms() < self.cooldown_until_ms.load(Ordering::Relaxed)
    }

    pub(super) fn cache_track(&self, tr: Track) {
        if tr.qobuz_track_id <= 0 {
            return;
        }
        let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
        if g.track_cache.len() >= MAX_TRACK_CACHE_ENTRIES {
            g.track_cache.clear();
        }
        g.track_cache.insert(tr.qobuz_track_id, tr);
    }

    pub(super) fn cache_tracks(&self, tracks: &[Track]) {
        let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
        if g.track_cache.len() + tracks.len() > MAX_TRACK_CACHE_ENTRIES {
            g.track_cache.clear();
        }
        for tr in tracks {
            if tr.qobuz_track_id > 0 {
                g.track_cache.insert(tr.qobuz_track_id, tr.clone());
            }
        }
    }

    pub(super) fn cache_artist_name(&self, name: &str, id: i64, overwrite: bool) {
        let Some(kv) = &self.kv else { return };
        let k = format!("aname:{}", norm_match_key(name));
        if k == "aname:" {
            return;
        }
        if !overwrite
            && let Ok(Some(raw)) = kv.get(&k)
            && std::str::from_utf8(&raw)
                .ok()
                .and_then(|s| s.parse::<i64>().ok())
                .is_some_and(|existing_id| existing_id > 0)
        {
            return;
        }
        let ttl = if id == 0 {
            ARTIST_MISS_TTL
        } else {
            ARTIST_NAME_TTL
        };
        let _ = kv.set(&k, id.to_string().as_bytes(), Ttl::After(ttl));
    }

    pub(super) fn get_cached_artist_name(&self, name: &str) -> Option<i64> {
        let kv = self.kv.as_ref()?;
        let k = format!("aname:{}", norm_match_key(name));
        let raw = kv.get(&k).ok()??;
        std::str::from_utf8(&raw).ok()?.parse().ok()
    }

    pub(super) fn put_cached_album_match(&self, artist: &str, title: &str, album_id: &str) {
        let Some(kv) = &self.kv else { return };
        let k = format!(
            "amatch:{}|{}",
            norm_match_key(artist),
            norm_match_key(title)
        );
        let ttl = if album_id.is_empty() {
            ALBUM_MISS_TTL
        } else {
            ALBUM_MATCH_TTL
        };
        let _ = kv.set(&k, album_id.as_bytes(), Ttl::After(ttl));
    }

    pub(super) fn get_cached_album_match(&self, artist: &str, title: &str) -> Option<String> {
        let kv = self.kv.as_ref()?;
        let k = format!(
            "amatch:{}|{}",
            norm_match_key(artist),
            norm_match_key(title)
        );
        let raw = kv.get(&k).ok()??;
        String::from_utf8(raw).ok()
    }

    pub(crate) fn has_cached_json(&self, path: &str, params: &[(&str, &str)]) -> bool {
        let Some(kv) = &self.kv else {
            return false;
        };
        let key = cache_key_for_req(path, params);
        matches!(kv.get(&key), Ok(Some(_)))
    }

    pub(crate) async fn get_cached_json<T: DeserializeOwned>(
        &self,
        path: &str,
        params: &[(&str, &str)],
        ttl: Duration,
    ) -> Result<T> {
        self.get_cached_json_valid(path, params, ttl, |_| true)
            .await
    }

    pub(crate) async fn get_cached_json_valid<T, F>(
        &self,
        path: &str,
        params: &[(&str, &str)],
        ttl: Duration,
        valid: F,
    ) -> Result<T>
    where
        T: DeserializeOwned,
        F: Fn(&T) -> bool,
    {
        let key = cache_key_for_req(path, params);
        if let Some(kv) = &self.kv
            && let Ok(Some(raw)) = kv.get(&key)
            && let Ok(val) = decode_qobuz_json::<T>(&raw)
            && valid(&val)
        {
            return Ok(val);
        }

        let auth_gen = self.auth_gen.load(Ordering::Relaxed);
        let path_owned = path.to_string();
        let params_owned: Vec<(String, String)> = params
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();

        let sf_key = format!("{auth_gen}:{key}");
        let raw_res = self
            .sf
            .work_inline(sf_key, async {
                let refs: Vec<(&str, &str)> = params_owned
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect();
                self.get_raw_json(&path_owned, &refs, true)
                    .await
                    .map_err(|e| e.to_string())
            })
            .await;

        let raw = raw_res.map_err(|msg| {
            if msg.contains("rate limited") {
                QobuzError::RateLimited
            } else if msg.contains("returned 401") || msg.contains("not authenticated") {
                QobuzError::Unauthenticated
            } else if msg.contains("not found") || msg.contains("returned 404") {
                QobuzError::NotFound
            } else {
                QobuzError::Other(msg)
            }
        })?;

        let parsed: T = decode_qobuz_json(&raw)?;
        if self.auth_gen.load(Ordering::Relaxed) == auth_gen
            && valid(&parsed)
            && let Some(kv) = &self.kv
        {
            let _ = kv.set(&key, &raw, Ttl::After(ttl));
        }
        Ok(parsed)
    }

    pub(crate) async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        params: &[(&str, &str)],
        with_auth: bool,
    ) -> Result<T> {
        let raw = self.get_raw_json(path, params, with_auth).await?;
        decode_qobuz_json(&raw)
    }

    async fn get_raw_json(
        &self,
        path: &str,
        params: &[(&str, &str)],
        with_auth: bool,
    ) -> Result<Vec<u8>> {
        let (app_id, token) = {
            let g = self.inner.read().unwrap_or_else(|e| e.into_inner());
            (g.app_id.clone(), g.auth_token.clone())
        };
        let url = format!("{}{}", self.base_url, path);

        let mut last_err = QobuzError::Other("qobuz request failed".into());
        for attempt in 0..MAX_RETRY_ATTEMPTS {
            self.wait_for_cooldown().await?;
            let _permit = self
                .req_sem
                .acquire()
                .await
                .map_err(|_| QobuzError::Other("semaphore closed".into()))?;

            let mut req = self
                .http
                .get(&url)
                .query(params)
                .header("User-Agent", "Mozilla/5.0");
            if !app_id.is_empty() {
                if !params.iter().any(|(k, _)| *k == "app_id") {
                    req = req.query(&[("app_id", app_id.as_str())]);
                }
                req = req.header("X-App-Id", &app_id);
            }
            if with_auth && !token.is_empty() {
                req = req.header("X-User-Auth-Token", &token);
            }

            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    last_err = QobuzError::Http(e);
                    if attempt + 1 < MAX_RETRY_ATTEMPTS {
                        tokio::time::sleep(Duration::from_millis(250 * (attempt as u64 + 1))).await;
                        continue;
                    }
                    return Err(last_err);
                }
            };

            let status = resp.status().as_u16();
            if status == 429 {
                let ra = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                let delay = self.note_rate_limit(ra);
                drop(_permit);
                if attempt + 1 < MAX_RETRY_ATTEMPTS && delay <= Duration::from_secs(5) {
                    tokio::time::sleep(delay).await;
                    continue;
                }
                return Err(QobuzError::RateLimited);
            }

            if status == 401 {
                return Err(QobuzError::Unauthenticated);
            }
            if status == 404 {
                return Err(QobuzError::NotFound);
            }

            let body = resp.bytes().await.map_err(QobuzError::Http)?.to_vec();
            if (200..300).contains(&status) {
                self.consecutive_429.store(0, Ordering::Relaxed);
                return Ok(body);
            }

            let body_str = String::from_utf8_lossy(&body).chars().take(200).collect();
            last_err = QobuzError::Api {
                status,
                body: body_str,
            };
            if matches!(status, 500 | 502 | 503 | 504) && attempt + 1 < MAX_RETRY_ATTEMPTS {
                drop(_permit);
                tokio::time::sleep(Duration::from_millis(300 * (attempt as u64 + 1))).await;
                continue;
            }
            return Err(last_err);
        }
        Err(last_err)
    }

    pub(crate) async fn post_form_json<T: DeserializeOwned>(
        &self,
        path: &str,
        form: &[(&str, &str)],
        with_auth: bool,
    ) -> Result<T> {
        let token = if with_auth {
            self.inner
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .auth_token
                .clone()
        } else {
            String::new()
        };
        self.post_form_json_with_token(path, form, &token).await
    }

    pub(crate) async fn post_form_json_with_token<T: DeserializeOwned>(
        &self,
        path: &str,
        form: &[(&str, &str)],
        token: &str,
    ) -> Result<T> {
        self.wait_for_cooldown().await?;
        let app_id = self.app_id();
        let url = format!("{}{}", self.base_url, path);
        let _permit = self
            .req_sem
            .acquire()
            .await
            .map_err(|_| QobuzError::Other("semaphore closed".into()))?;

        let mut req = self
            .http
            .post(&url)
            .form(form)
            .header("User-Agent", "Mozilla/5.0");
        if !app_id.is_empty() {
            req = req.header("X-App-Id", &app_id);
        }
        if !token.is_empty() {
            req = req.header("X-User-Auth-Token", token);
        }
        let resp = req.send().await?;
        let status = resp.status().as_u16();
        if status == 429 {
            let ra = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            self.note_rate_limit(ra);
            return Err(QobuzError::RateLimited);
        }
        if status == 401 {
            return Err(QobuzError::Unauthenticated);
        }
        if status == 404 {
            return Err(QobuzError::NotFound);
        }
        let body = resp.bytes().await?;
        if !(200..300).contains(&status) {
            return Err(QobuzError::Api {
                status,
                body: String::from_utf8_lossy(&body).chars().take(200).collect(),
            });
        }
        self.consecutive_429.store(0, Ordering::Relaxed);
        decode_qobuz_json(&body)
    }

    pub(crate) async fn post_json<B: serde::Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let token = self
            .inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .auth_token
            .clone();
        if token.is_empty() {
            return Err(QobuzError::Unauthenticated);
        }
        self.wait_for_cooldown().await?;
        let app_id = self.app_id();
        let url = format!("{}{}", self.base_url, path);
        let _permit = self
            .req_sem
            .acquire()
            .await
            .map_err(|_| QobuzError::Other("semaphore closed".into()))?;

        let mut req = self
            .http
            .post(&url)
            .json(body)
            .header("User-Agent", "Mozilla/5.0")
            .header("X-User-Auth-Token", &token);
        if !app_id.is_empty() {
            req = req.header("X-App-Id", &app_id);
        }
        let resp = req.send().await?;
        let status = resp.status().as_u16();
        if status == 429 {
            let ra = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            self.note_rate_limit(ra);
            return Err(QobuzError::RateLimited);
        }
        if status == 401 {
            return Err(QobuzError::Unauthenticated);
        }
        if status == 404 {
            return Err(QobuzError::NotFound);
        }
        let resp_bytes = resp.bytes().await?;
        if !(200..300).contains(&status) {
            return Err(QobuzError::Api {
                status,
                body: String::from_utf8_lossy(&resp_bytes)
                    .chars()
                    .take(200)
                    .collect(),
            });
        }
        self.consecutive_429.store(0, Ordering::Relaxed);
        decode_qobuz_json(&resp_bytes)
    }

    pub(crate) async fn fetch_text(&self, target_url: &str) -> Result<String> {
        let resp = self
            .http
            .get(target_url)
            .header("User-Agent", "Mozilla/5.0")
            .send()
            .await?;
        let status = resp.status().as_u16();
        if status != 200 {
            return Err(QobuzError::Api {
                status,
                body: format!("GET {target_url} returned {status}"),
            });
        }
        Ok(resp.text().await?)
    }

    async fn wait_for_cooldown(&self) -> Result<()> {
        let until = self.cooldown_until_ms.load(Ordering::Relaxed);
        let now = now_ms();
        if until > now {
            let rem_ms = (until - now) as u64;
            if rem_ms > 15_000 {
                return Err(QobuzError::RateLimited);
            }
            tokio::time::sleep(Duration::from_millis(rem_ms)).await;
        }
        Ok(())
    }

    fn note_rate_limit(&self, retry_after: &str) -> Duration {
        let n = self.consecutive_429.fetch_add(1, Ordering::Relaxed) + 1;
        let secs =
            parse_retry_after_secs(retry_after).unwrap_or_else(|| (1u64 << n.min(5)).min(60));
        let delay = Duration::from_secs(secs.min(60));
        let target = now_ms() + delay.as_millis() as i64;
        self.cooldown_until_ms.fetch_max(target, Ordering::Relaxed);
        delay
    }
}

fn parse_retry_after_secs(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(s) = trimmed.parse::<u64>() {
        return (s > 0).then_some(s);
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(trimmed) {
        let delta = dt.timestamp() - chrono::Utc::now().timestamp();
        if delta > 0 {
            return Some(delta as u64);
        }
    }
    None
}

/// Computes the Qobuz MD5 request_sig for `/track/getFileUrl`.
pub fn sign_file_url(
    track_id: i64,
    format_id: i32,
    intent: &str,
    unix_ts: i64,
    app_secret: &str,
) -> String {
    let payload = format!(
        "trackgetFileUrlformat_id{format_id}intent{intent}track_id{track_id}{unix_ts}{app_secret}"
    );
    md5_hex(&payload)
}

pub(super) fn cache_key_for_req(path: &str, params: &[(&str, &str)]) -> String {
    let mut pairs: Vec<(String, String)> = params
        .iter()
        .filter(|(k, _)| *k != "app_id")
        .map(|(k, v)| {
            let val = if *k == "query" {
                v.trim().to_ascii_lowercase()
            } else {
                (*v).to_string()
            };
            ((*k).to_string(), val)
        })
        .collect();
    pairs.sort_unstable();
    let mut out = String::from(path);
    out.push('?');
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(k);
        out.push('=');
        out.push_str(v);
    }
    out
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
