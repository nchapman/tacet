use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI32, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::cache::{Cache, CacheOptions, Namespaced, Ttl};
use crate::library::{Album, LibraryError, RemoteCollection, RemoteSource, SOURCE_QOBUZ, Track};
use crate::singleflight::SingleFlight;

use super::types::{
    AlbumLookup, Artist, ArtistDetail, DEFAULT_BASE_URL, FORMAT_FLAC_192, PersistedCredentials,
    QAlbum, QArtist, QTrack, QobuzError, Result, SearchResult, Status, StreamInfo,
    is_primary_artist_album_or_ep, md5_hex, norm_match_key, normalize_sample_rate_hz,
    parse_quality, quality_label,
};

const MAX_URL_CACHE_ENTRIES: usize = 512;
const MAX_TRACK_CACHE_ENTRIES: usize = 4096;

const ALBUM_CACHE_TTL: Duration = Duration::from_secs(24 * 3600);
const ARTIST_CACHE_TTL: Duration = Duration::from_secs(24 * 3600);
const ARTIST_NAME_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
const ARTIST_MISS_TTL: Duration = Duration::from_secs(6 * 3600);
const ALBUM_MATCH_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
const ALBUM_MISS_TTL: Duration = Duration::from_secs(6 * 3600);
const SEARCH_CACHE_TTL: Duration = Duration::from_secs(15 * 60);

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

struct ClientInner {
    app_id: String,
    app_secret: String,
    private_key: String,
    alt_secrets: Vec<String>,
    auth_token: String,
    user_name: String,
    subscription: String,
    format_id: i32,
    url_cache: HashMap<i64, StreamInfo>,
    track_cache: HashMap<i64, Track>,
}

pub struct Client {
    base_url: String,
    http: reqwest::Client,
    inner: RwLock<ClientInner>,
    kv: Option<Namespaced>,
    _owned_cache: Option<Arc<Cache>>,
    auth_gen: AtomicU64,
    sf: SingleFlight<String, Vec<u8>, String>,
    req_sem: Arc<Semaphore>,
    cooldown_until_ms: AtomicI64,
    consecutive_429: AtomicI32,
    prewarm_cancel: Mutex<Option<CancellationToken>>,
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
            configured: !g.app_id.is_empty(),
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

    // --- Catalog / Search / Metadata ---

    pub async fn search(&self, query: &str, limit: i32, offset: i32) -> Result<SearchResult> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(SearchResult::default());
        }
        self.require_auth()?;
        let limit = if limit <= 0 || limit > 100 { 25 } else { limit };
        let offset = offset.max(0);
        let auth_gen = self.auth_gen.load(Ordering::Relaxed);
        let lim_s = limit.to_string();
        let off_s = offset.to_string();

        #[derive(Deserialize)]
        struct ArtistsWrap {
            #[serde(default)]
            items: Vec<QArtist>,
        }
        #[derive(Deserialize)]
        struct TracksWrap {
            #[serde(default)]
            items: Vec<QTrack>,
        }
        #[derive(Deserialize)]
        struct AlbumsWrap {
            #[serde(default)]
            items: Vec<QAlbum>,
        }
        #[derive(Deserialize)]
        struct Resp {
            #[serde(default)]
            artists: Option<ArtistsWrap>,
            #[serde(default)]
            tracks: Option<TracksWrap>,
            #[serde(default)]
            albums: Option<AlbumsWrap>,
        }

        let resp: Resp = self
            .get_cached_json(
                "/catalog/search",
                &[("query", query), ("limit", &lim_s), ("offset", &off_s)],
                SEARCH_CACHE_TTL,
            )
            .await?;

        let artists: Vec<Artist> = resp
            .artists
            .map(|w| w.items)
            .unwrap_or_default()
            .into_iter()
            .filter(|qa| qa.id > 0 && !qa.name.trim().is_empty())
            .map(|qa| qa.to_artist())
            .collect();
        let tracks: Vec<Track> = resp
            .tracks
            .map(|w| w.items)
            .unwrap_or_default()
            .into_iter()
            .map(|qt| qt.to_library_track(None))
            .collect();
        let albums: Vec<Album> = resp
            .albums
            .map(|w| w.items)
            .unwrap_or_default()
            .into_iter()
            .filter(|qa| qa.streamable != Some(false))
            .map(|qa| qa.to_library_album(false))
            .collect();

        if self.auth_gen.load(Ordering::Relaxed) == auth_gen {
            for a in &artists {
                self.cache_artist_name(&a.name, a.id, false);
            }
            self.cache_tracks(&tracks);
        }

        Ok(SearchResult {
            artists,
            tracks,
            albums,
        })
    }

    pub async fn get_album(&self, album_id: &str) -> Result<Album> {
        self.require_auth()?;
        let raw_id = album_id
            .trim()
            .strip_prefix("qobuz:")
            .unwrap_or(album_id.trim());
        if raw_id.is_empty() {
            return Err(QobuzError::NotFound);
        }
        let qa: QAlbum = self
            .get_cached_json(
                "/album/get",
                &[("album_id", raw_id), ("limit", "500")],
                ALBUM_CACHE_TTL,
            )
            .await
            .map_err(|e| match e {
                QobuzError::Api { status: 404, .. } => QobuzError::NotFound,
                other => other,
            })?;
        if qa.id.is_empty() {
            return Err(QobuzError::NotFound);
        }
        let alb = qa.to_library_album(true);
        self.cache_tracks(&alb.tracks);
        Ok(alb)
    }

    pub async fn get_track(&self, track_id: i64) -> Result<Track> {
        self.require_auth()?;
        if track_id <= 0 {
            return Err(QobuzError::NotFound);
        }
        let tid_s = track_id.to_string();
        let qt: QTrack = self
            .get_cached_json("/track/get", &[("track_id", &tid_s)], ALBUM_CACHE_TTL)
            .await
            .map_err(|e| match e {
                QobuzError::Api { status: 404, .. } => QobuzError::NotFound,
                other => other,
            })?;
        if qt.id == 0 {
            return Err(QobuzError::NotFound);
        }
        let tr = qt.to_library_track(None);
        self.cache_track(tr.clone());
        Ok(tr)
    }

    pub async fn resolve_track(&self, track_id: i64) -> Result<Track> {
        if let Some(tr) = self
            .inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .track_cache
            .get(&track_id)
            .cloned()
        {
            return Ok(tr);
        }
        self.get_track(track_id).await
    }

    pub async fn get_artist(&self, artist_id: i64, limit: i32) -> Result<ArtistDetail> {
        self.require_auth()?;
        if artist_id <= 0 {
            return Err(QobuzError::NotFound);
        }
        let limit = if limit <= 0 || limit > 200 { 50 } else { limit };
        let auth_gen = self.auth_gen.load(Ordering::Relaxed);
        let aid_s = artist_id.to_string();
        let lim_s = limit.to_string();

        #[derive(Deserialize)]
        struct AlbumsWrap {
            #[serde(default)]
            items: Vec<QAlbum>,
        }
        #[derive(Deserialize)]
        struct Resp {
            #[serde(flatten)]
            artist: QArtist,
            #[serde(default)]
            albums: Option<AlbumsWrap>,
        }

        let artist_params = [
            ("artist_id", aid_s.as_str()),
            ("extra", "albums"),
            ("limit", lim_s.as_str()),
            ("offset", "0"),
        ];
        let artist_fut =
            self.get_cached_json::<Resp>("/artist/get", &artist_params, ARTIST_CACHE_TTL);
        let page_fut = self.fetch_artist_page_extras(artist_id);
        let (resp_res, page_res) = tokio::join!(artist_fut, page_fut);

        let resp = resp_res.map_err(|e| match e {
            QobuzError::Api { status: 404, .. } => QobuzError::NotFound,
            other => other,
        })?;
        if resp.artist.id == 0 && resp.artist.name.trim().is_empty() {
            return Err(QobuzError::NotFound);
        }

        let mut art = resp.artist.to_artist();
        if art.id == 0 {
            art.id = artist_id;
        }
        let want_artist_key = norm_match_key(&art.name);
        let albums: Vec<Album> = resp
            .albums
            .map(|w| w.items)
            .unwrap_or_default()
            .into_iter()
            .filter(|qa| {
                qa.streamable != Some(false)
                    && is_primary_artist_album_or_ep(qa, art.id, &want_artist_key)
            })
            .map(|qa| {
                let mut a = qa.to_library_album(false);
                if a.artist.is_empty() {
                    a.artist = art.name.clone();
                }
                a
            })
            .collect();

        let mut tracks = page_res.tracks;
        for t in &mut tracks {
            if t.artist.is_empty() {
                t.artist = art.name.clone();
            }
            if t.album_artist.is_empty() {
                t.album_artist = art.name.clone();
            }
        }
        self.cache_tracks(&tracks);
        if art.artwork_url.is_empty() && !page_res.portrait_url.is_empty() {
            art.artwork_url = page_res.portrait_url;
        }
        if art.artwork_url.is_empty()
            && let Some(first) = albums.first()
        {
            art.artwork_url = first.artwork_url.clone();
        }
        if self.auth_gen.load(Ordering::Relaxed) == auth_gen && !art.name.is_empty() {
            self.cache_artist_name(&art.name, art.id, true);
        }

        Ok(ArtistDetail {
            artist: art,
            albums,
            tracks,
            similar_artists: page_res.similar_artists,
        })
    }

    pub async fn get_artist_by_name(&self, name: &str, limit: i32) -> Result<ArtistDetail> {
        let name = name.trim();
        if name.is_empty() {
            return Err(QobuzError::NotFound);
        }
        if let Some(cached_id) = self.get_cached_artist_name(name) {
            if cached_id == 0 {
                return Err(QobuzError::NotFound);
            }
            return self.get_artist(cached_id, limit).await;
        }
        let res = self.search(name, 10, 0).await?;
        let want = norm_match_key(name);
        let picked = res
            .artists
            .iter()
            .find(|a| norm_match_key(&a.name) == want)
            .or_else(|| res.artists.first());
        let Some(art) = picked else {
            self.cache_artist_name(name, 0, true);
            return Err(QobuzError::NotFound);
        };
        self.cache_artist_name(name, art.id, true);
        self.get_artist(art.id, limit).await
    }

    pub async fn match_album(&self, artist: &str, title: &str) -> Result<Album> {
        let artist = artist.trim();
        let title = title.trim();
        if title.is_empty() {
            return Err(QobuzError::NotFound);
        }
        if let Some(cached_id) = self.get_cached_album_match(artist, title) {
            if cached_id.is_empty() {
                return Err(QobuzError::NotFound);
            }
            return self.get_album(&cached_id).await;
        }

        let want_artist = norm_match_key(artist);
        let want_title = norm_match_key(title);

        if !artist.is_empty()
            && let Ok(detail) = self.get_artist_by_name(artist, 50).await
            && let Some(id) =
                pick_matching_album_id(&detail.albums, &want_artist, &want_title, false)
        {
            self.put_cached_album_match(artist, title, &id);
            return self.get_album(&id).await;
        }

        let query = if artist.is_empty() {
            title.to_string()
        } else {
            format!("{artist} {title}")
        };
        let res = self.search(&query, 15, 0).await?;
        let matched =
            pick_matching_album_id(&res.albums, &want_artist, &want_title, !artist.is_empty())
                .or_else(|| pick_matching_album_id(&res.albums, "", &want_title, false));
        let Some(id) = matched else {
            self.put_cached_album_match(artist, title, "");
            return Err(QobuzError::NotFound);
        };
        self.put_cached_album_match(artist, title, &id);
        self.get_album(&id).await
    }

    pub fn cached_artist_portrait(&self, name: &str) -> String {
        let Some(id) = self.get_cached_artist_name(name) else {
            return String::new();
        };
        if id <= 0 {
            return String::new();
        }
        let aid_s = id.to_string();
        let key = cache_key_for_req(
            "/artist/get",
            &[
                ("artist_id", &aid_s),
                ("extra", "albums"),
                ("limit", "50"),
                ("offset", "0"),
            ],
        );
        let Some(kv) = &self.kv else {
            return String::new();
        };
        let Ok(Some(raw)) = kv.get(&key) else {
            return String::new();
        };
        if let Ok(qa) = serde_json::from_slice::<QArtist>(&raw) {
            return qa.to_artist().artwork_url;
        }
        String::new()
    }

    // --- Stream URL signing ---

    pub async fn stream_url(&self, track_id: i64) -> Result<StreamInfo> {
        self.require_auth()?;
        let now = Instant::now();
        let (format_id, secrets) = {
            let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
            if let Some(cached) = g.url_cache.get(&track_id) {
                if now < cached.expires_at {
                    return Ok(cached.clone());
                }
                g.url_cache.remove(&track_id);
            }
            let mut secrets = Vec::with_capacity(1 + g.alt_secrets.len());
            if !g.app_secret.is_empty() {
                secrets.push(g.app_secret.clone());
            }
            for s in &g.alt_secrets {
                if !s.is_empty() && *s != g.app_secret {
                    secrets.push(s.clone());
                }
            }
            (g.format_id, secrets)
        };

        if secrets.is_empty() {
            return Err(QobuzError::Other(
                "qobuz app_secret is not configured".into(),
            ));
        }

        let mut last_err = QobuzError::Other("no secrets".into());
        let mut primary_auth_failed = false;
        for (i, secret) in secrets.iter().enumerate() {
            match self.fetch_file_url(track_id, format_id, secret).await {
                Ok(info) => {
                    let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
                    if i == 0 || primary_auth_failed {
                        g.app_secret = secret.clone();
                    }
                    put_url_cache_locked(&mut g.url_cache, track_id, info.clone());
                    return Ok(info);
                }
                Err(e) => {
                    let is_sig = matches!(
                        e,
                        QobuzError::Api {
                            status: 400 | 401 | 403,
                            ..
                        }
                    );
                    last_err = e;
                    if !is_sig {
                        return Err(last_err);
                    }
                    if i == 0 {
                        primary_auth_failed = true;
                    }
                }
            }
        }
        Err(last_err)
    }

    async fn fetch_file_url(
        &self,
        track_id: i64,
        format_id: i32,
        secret: &str,
    ) -> Result<StreamInfo> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let intent = "stream";
        let sig = sign_file_url(track_id, format_id, intent, ts, secret);
        let tid_s = track_id.to_string();
        let fid_s = format_id.to_string();
        let ts_s = ts.to_string();

        #[derive(Deserialize)]
        struct Resp {
            #[serde(default)]
            url: String,
            #[serde(default)]
            mime_type: String,
            #[serde(default)]
            bit_depth: i32,
            #[serde(default)]
            sampling_rate: f64,
            #[serde(default)]
            duration: i64,
            #[serde(default)]
            format_id: i32,
        }

        let resp: Resp = self
            .get_json(
                "/track/getFileUrl",
                &[
                    ("track_id", &tid_s),
                    ("format_id", &fid_s),
                    ("intent", intent),
                    ("request_ts", &ts_s),
                    ("request_sig", &sig),
                ],
                true,
            )
            .await?;

        if resp.url.is_empty() {
            return Err(QobuzError::Other(
                "qobuz returned empty stream URL (check subscription or region restrictions)"
                    .into(),
            ));
        }

        let mut mime = resp.mime_type.trim().to_string();
        if mime.is_empty() || mime == "audio/x-flac" {
            mime = "audio/flac".into();
        }
        Ok(StreamInfo {
            url: resp.url,
            mime,
            sample_rate: normalize_sample_rate_hz(resp.sampling_rate),
            bit_depth: resp.bit_depth,
            duration: Duration::from_secs(resp.duration.max(0) as u64),
            format_id: resp.format_id,
            expires_at: Instant::now() + Duration::from_secs(5 * 60),
        })
    }

    // --- Favorites & RemoteSource ---

    pub fn as_remote_source(self: &Arc<Self>) -> Arc<dyn RemoteSource> {
        Arc::new(RemoteSourceAdapter {
            client: Arc::clone(self),
        })
    }

    pub async fn sync_favorites(&self) -> Result<RemoteCollection> {
        self.require_auth()?;
        const PAGE_SIZE: i32 = 500;

        #[derive(Deserialize)]
        struct AlbumsWrap {
            #[serde(default)]
            total: i32,
            #[serde(default)]
            items: Vec<QAlbum>,
        }
        #[derive(Deserialize)]
        struct TracksWrap {
            #[serde(default)]
            total: i32,
            #[serde(default)]
            items: Vec<QTrack>,
        }
        #[derive(Deserialize)]
        struct FavResp {
            #[serde(default)]
            albums: Option<AlbumsWrap>,
            #[serde(default)]
            tracks: Option<TracksWrap>,
        }

        let mut fav_albums: Vec<QAlbum> = Vec::new();
        let mut offset = 0;
        loop {
            let lim_s = PAGE_SIZE.to_string();
            let off_s = offset.to_string();
            let resp: FavResp = self
                .get_json(
                    "/favorite/getUserFavorites",
                    &[("type", "albums"), ("limit", &lim_s), ("offset", &off_s)],
                    true,
                )
                .await?;
            let wrap = resp.albums.unwrap_or(AlbumsWrap {
                total: 0,
                items: Vec::new(),
            });
            let count = wrap.items.len() as i32;
            fav_albums.extend(wrap.items);
            if count < PAGE_SIZE || (wrap.total > 0 && fav_albums.len() as i32 >= wrap.total) {
                break;
            }
            offset += PAGE_SIZE;
        }

        let mut fav_tracks: Vec<Track> = Vec::new();
        let mut offset = 0;
        loop {
            let lim_s = PAGE_SIZE.to_string();
            let off_s = offset.to_string();
            let resp: FavResp = self
                .get_json(
                    "/favorite/getUserFavorites",
                    &[("type", "tracks"), ("limit", &lim_s), ("offset", &off_s)],
                    true,
                )
                .await?;
            let wrap = resp.tracks.unwrap_or(TracksWrap {
                total: 0,
                items: Vec::new(),
            });
            let count = wrap.items.len() as i32;
            for qt in wrap.items {
                let tr = qt.to_library_track(None);
                self.cache_track(tr.clone());
                fav_tracks.push(tr);
            }
            if count < PAGE_SIZE || (wrap.total > 0 && fav_tracks.len() as i32 >= wrap.total) {
                break;
            }
            offset += PAGE_SIZE;
        }

        // Hydrate favorited albums with bounded concurrency (3)
        let mut albums = Vec::with_capacity(fav_albums.len());
        for qa in fav_albums {
            match self.get_album(&qa.id).await {
                Ok(full) => albums.push(full),
                Err(QobuzError::NotFound) => {
                    tracing::warn!(album_id = %qa.id, "qobuz favorites sync: album not found; marking unavailable");
                    let mut summary = qa.to_library_album(false);
                    summary.available = false;
                    albums.push(summary);
                }
                Err(e) => return Err(e),
            }
        }

        Ok(RemoteCollection {
            albums,
            tracks: fav_tracks,
        })
    }

    pub async fn add_favorite(&self, kind: &str, external_id: &str) -> Result<()> {
        self.mutate_favorite("/favorite/create", kind, external_id)
            .await
    }

    pub async fn remove_favorite(&self, kind: &str, external_id: &str) -> Result<()> {
        self.mutate_favorite("/favorite/delete", kind, external_id)
            .await
    }

    async fn mutate_favorite(&self, endpoint: &str, kind: &str, external_id: &str) -> Result<()> {
        self.require_auth()?;
        let clean = external_id
            .trim()
            .strip_prefix("qobuz:")
            .unwrap_or(external_id.trim());
        if clean.is_empty() {
            return Err(QobuzError::Other(format!(
                "qobuz favorite {kind}: empty id"
            )));
        }
        let field = match kind.trim().to_ascii_lowercase().as_str() {
            "album" => "album_ids",
            "track" => "track_ids",
            other => {
                return Err(QobuzError::Other(format!(
                    "unsupported qobuz favorite kind {other:?}"
                )));
            }
        };
        let _: serde_json::Value = self
            .post_form_json(endpoint, &[(field, clean)], true)
            .await?;
        Ok(())
    }

    pub fn prewarm_library(
        self: &Arc<Self>,
        artist_names: Vec<String>,
        local_albums: Vec<AlbumLookup>,
    ) {
        if !self.is_connected() || (artist_names.is_empty() && local_albums.is_empty()) {
            return;
        }
        let cancel = CancellationToken::new();
        {
            let mut guard = self
                .prewarm_cancel
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(prev) = guard.replace(cancel.clone()) {
                prev.cancel();
            }
        }
        let client = Arc::clone(self);
        tokio::spawn(async move {
            let mut seen_a = HashSet::new();
            for name in artist_names.into_iter().take(250) {
                if cancel.is_cancelled() || !client.is_connected() {
                    return;
                }
                let key = norm_match_key(&name);
                if key.is_empty() || !seen_a.insert(key) {
                    continue;
                }
                if client.get_cached_artist_name(&name).is_some() {
                    continue;
                }
                if let Err(QobuzError::RateLimited) = client.get_artist_by_name(&name, 50).await {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            let mut seen_alb = HashSet::new();
            for a in local_albums.into_iter().take(250) {
                if cancel.is_cancelled() || !client.is_connected() {
                    return;
                }
                let key = format!("{}|{}", norm_match_key(&a.artist), norm_match_key(&a.title));
                if key == "|" || !seen_alb.insert(key) {
                    continue;
                }
                if client.get_cached_album_match(&a.artist, &a.title).is_some() {
                    continue;
                }
                if let Err(QobuzError::RateLimited) = client.match_album(&a.artist, &a.title).await
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        });
    }

    // --- Internal artist/page extras ---

    async fn fetch_artist_page_extras(&self, artist_id: i64) -> ArtistPageExtras {
        let aid_s = artist_id.to_string();
        #[derive(Deserialize)]
        struct Portrait {
            #[serde(default)]
            hash: String,
            #[serde(default)]
            format: String,
        }
        #[derive(Deserialize)]
        struct Images {
            #[serde(default)]
            portrait: Option<Portrait>,
        }
        #[derive(Deserialize)]
        struct TopTracks {
            #[serde(default)]
            items: Vec<QTrack>,
        }
        #[derive(Deserialize)]
        struct Similar {
            #[serde(default)]
            items: Vec<QArtist>,
        }
        #[derive(Deserialize)]
        struct PageResp {
            #[serde(default)]
            images: Option<Images>,
            #[serde(default)]
            top_tracks: Option<TopTracks>,
            #[serde(default)]
            similar_artists: Option<Similar>,
        }

        let Ok(resp) = self
            .get_cached_json::<PageResp>("/artist/page", &[("artist_id", &aid_s)], ARTIST_CACHE_TTL)
            .await
        else {
            return ArtistPageExtras::default();
        };

        let portrait_url = resp
            .images
            .and_then(|i| i.portrait)
            .filter(|p| !p.hash.is_empty())
            .map(|p| {
                let ext = if p.format.is_empty() {
                    "jpg"
                } else {
                    &p.format
                };
                format!(
                    "https://static.qobuz.com/images/artists/covers/large/{}.{ext}",
                    p.hash
                )
            })
            .unwrap_or_default();

        let tracks = resp
            .top_tracks
            .map(|t| t.items)
            .unwrap_or_default()
            .into_iter()
            .map(|qt| qt.to_library_track(None))
            .collect();

        let similar_artists = resp
            .similar_artists
            .map(|s| s.items)
            .unwrap_or_default()
            .into_iter()
            .map(|qa| qa.to_artist())
            .filter(|a| a.id > 0 && !a.name.is_empty())
            .collect();

        ArtistPageExtras {
            tracks,
            portrait_url,
            similar_artists,
        }
    }

    // --- Caching & HTTP plumbing ---

    fn cache_track(&self, tr: Track) {
        if tr.qobuz_track_id <= 0 {
            return;
        }
        let mut g = self.inner.write().unwrap_or_else(|e| e.into_inner());
        if g.track_cache.len() >= MAX_TRACK_CACHE_ENTRIES {
            g.track_cache.clear();
        }
        g.track_cache.insert(tr.qobuz_track_id, tr);
    }

    fn cache_tracks(&self, tracks: &[Track]) {
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

    fn cache_artist_name(&self, name: &str, id: i64, overwrite: bool) {
        let Some(kv) = &self.kv else { return };
        let k = format!("aname:{}", norm_match_key(name));
        if k == "aname:" {
            return;
        }
        if !overwrite && kv.get(&k).ok().flatten().is_some() {
            return;
        }
        let ttl = if id == 0 {
            ARTIST_MISS_TTL
        } else {
            ARTIST_NAME_TTL
        };
        let _ = kv.set(&k, id.to_string().as_bytes(), Ttl::After(ttl));
    }

    fn get_cached_artist_name(&self, name: &str) -> Option<i64> {
        let kv = self.kv.as_ref()?;
        let k = format!("aname:{}", norm_match_key(name));
        let raw = kv.get(&k).ok()??;
        std::str::from_utf8(&raw).ok()?.parse().ok()
    }

    fn put_cached_album_match(&self, artist: &str, title: &str, album_id: &str) {
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

    fn get_cached_album_match(&self, artist: &str, title: &str) -> Option<String> {
        let kv = self.kv.as_ref()?;
        let k = format!(
            "amatch:{}|{}",
            norm_match_key(artist),
            norm_match_key(title)
        );
        let raw = kv.get(&k).ok()??;
        String::from_utf8(raw).ok()
    }

    pub(crate) async fn get_cached_json<T: DeserializeOwned>(
        &self,
        path: &str,
        params: &[(&str, &str)],
        ttl: Duration,
    ) -> Result<T> {
        let key = cache_key_for_req(path, params);
        if let Some(kv) = &self.kv
            && let Ok(Some(raw)) = kv.get(&key)
            && let Ok(val) = serde_json::from_slice::<T>(&raw)
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
            } else if msg.contains("returned 404") {
                QobuzError::Api {
                    status: 404,
                    body: msg,
                }
            } else {
                QobuzError::Other(msg)
            }
        })?;

        let parsed: T = serde_json::from_slice(&raw)?;
        if self.auth_gen.load(Ordering::Relaxed) == auth_gen
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
        Ok(serde_json::from_slice(&raw)?)
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
            self.wait_for_cooldown().await;
            let _permit = self
                .req_sem
                .acquire()
                .await
                .map_err(|_| QobuzError::Other("semaphore closed".into()))?;

            let mut req = self.http.get(&url).query(params);
            if !app_id.is_empty() {
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
        self.wait_for_cooldown().await;
        let app_id = self.app_id();
        let url = format!("{}{}", self.base_url, path);
        let _permit = self
            .req_sem
            .acquire()
            .await
            .map_err(|_| QobuzError::Other("semaphore closed".into()))?;

        let mut req = self.http.post(&url).form(form);
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
        let body = resp.bytes().await?;
        if !(200..300).contains(&status) {
            return Err(QobuzError::Api {
                status,
                body: String::from_utf8_lossy(&body).chars().take(200).collect(),
            });
        }
        Ok(serde_json::from_slice(&body)?)
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

    async fn wait_for_cooldown(&self) {
        let until = self.cooldown_until_ms.load(Ordering::Relaxed);
        let now = now_ms();
        if until > now {
            let wait = Duration::from_millis((until - now).min(5000) as u64);
            tokio::time::sleep(wait).await;
        }
    }

    fn note_rate_limit(&self, retry_after: &str) -> Duration {
        let n = self.consecutive_429.fetch_add(1, Ordering::Relaxed) + 1;
        let secs = retry_after
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|&s| s > 0)
            .unwrap_or_else(|| (1u64 << n.min(4)).min(30));
        let delay = Duration::from_secs(secs.min(60));
        let target = now_ms() + delay.as_millis() as i64;
        self.cooldown_until_ms.fetch_max(target, Ordering::Relaxed);
        delay
    }
}

#[derive(Default)]
struct ArtistPageExtras {
    tracks: Vec<Track>,
    portrait_url: String,
    similar_artists: Vec<Artist>,
}

struct RemoteSourceAdapter {
    client: Arc<Client>,
}

#[async_trait]
impl RemoteSource for RemoteSourceAdapter {
    fn provider_id(&self) -> &str {
        SOURCE_QOBUZ
    }

    fn is_connected(&self) -> bool {
        self.client.is_connected()
    }

    async fn sync_collection(&self) -> crate::library::Result<RemoteCollection> {
        self.client
            .sync_favorites()
            .await
            .map_err(|e| LibraryError::Remote(e.to_string()))
    }

    async fn get_album(&self, external_id: &str) -> crate::library::Result<Album> {
        self.client
            .get_album(external_id)
            .await
            .map_err(|e| LibraryError::Remote(e.to_string()))
    }

    async fn get_track(&self, external_id: &str) -> crate::library::Result<Track> {
        let clean = external_id
            .trim()
            .strip_prefix("qobuz:")
            .unwrap_or(external_id.trim());
        let id: i64 = clean
            .parse()
            .map_err(|_| LibraryError::Remote(format!("invalid qobuz track id {external_id:?}")))?;
        self.client
            .get_track(id)
            .await
            .map_err(|e| LibraryError::Remote(e.to_string()))
    }

    async fn add_favorite(&self, kind: &str, external_id: &str) -> crate::library::Result<()> {
        self.client
            .add_favorite(kind, external_id)
            .await
            .map_err(|e| LibraryError::Remote(e.to_string()))
    }

    async fn remove_favorite(&self, kind: &str, external_id: &str) -> crate::library::Result<()> {
        self.client
            .remove_favorite(kind, external_id)
            .await
            .map_err(|e| LibraryError::Remote(e.to_string()))
    }
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

fn cache_key_for_req(path: &str, params: &[(&str, &str)]) -> String {
    let mut pairs: Vec<(&str, &str)> = params.to_vec();
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

fn put_url_cache_locked(cache: &mut HashMap<i64, StreamInfo>, track_id: i64, info: StreamInfo) {
    let now = Instant::now();
    cache.retain(|_, v| now < v.expires_at);
    if cache.len() >= MAX_URL_CACHE_ENTRIES
        && let Some((&oldest_key, _)) = cache.iter().min_by_key(|(_, v)| v.expires_at)
    {
        cache.remove(&oldest_key);
    }
    cache.insert(track_id, info);
}

fn pick_matching_album_id(
    albums: &[Album],
    want_artist: &str,
    want_title: &str,
    require_artist_match: bool,
) -> Option<String> {
    let mut fallback: Option<String> = None;
    for a in albums {
        if require_artist_match
            && !want_artist.is_empty()
            && norm_match_key(&a.artist) != want_artist
        {
            continue;
        }
        let raw_id = a.id.trim().strip_prefix("qobuz:").unwrap_or(a.id.trim());
        if raw_id.is_empty() {
            continue;
        }
        if norm_match_key(&a.title) == want_title {
            return Some(raw_id.to_string());
        }
        if fallback.is_none()
            && let Some(idx) = a.title.find(['(', '['])
            && idx > 0
            && norm_match_key(&a.title[..idx]) == want_title
        {
            fallback = Some(raw_id.to_string());
        }
    }
    fallback
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
