use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::library::{Album, SOURCE_QOBUZ, SOURCE_TYPE_REMOTE, Track};

use super::{
    ALBUM_CACHE_TTL, ARTIST_CACHE_TTL, Client, MAX_URL_CACHE_ENTRIES, SEARCH_CACHE_TTL,
    cache_key_for_req, sign_file_url,
};
use crate::qobuz::types::{
    Artist, ArtistDetail, QAlbum, QArtist, QDiscoverAudioInfo, QDiscoverRights, QImage, QTrack,
    QobuzError, Result, SearchResult, StreamInfo, decode_qobuz_json, is_primary_artist_album_or_ep,
    norm_match_key, normalize_sample_rate_hz,
};

#[derive(Default)]
struct ArtistPageExtras {
    tracks: Vec<Track>,
    portrait_url: String,
    similar_artists: Vec<Artist>,
}

impl Client {
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
            .get_cached_json_valid(
                "/album/get",
                &[("album_id", raw_id), ("limit", "500")],
                ALBUM_CACHE_TTL,
                |a: &QAlbum| !a.id.is_empty() && !a.tracks.items.is_empty(),
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
        let artist_fut = self.get_cached_json_valid::<Resp, _>(
            "/artist/get",
            &artist_params,
            ARTIST_CACHE_TTL,
            |r| r.artist.id > 0 || !r.artist.name.trim().is_empty(),
        );
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
        #[derive(Default, Deserialize)]
        struct ArtistsWrap {
            #[serde(default)]
            items: Vec<QArtist>,
        }
        #[derive(Deserialize)]
        struct ArtistSearchResp {
            #[serde(default)]
            artists: ArtistsWrap,
        }
        let resp: ArtistSearchResp = self
            .get_cached_json(
                "/artist/search",
                &[("query", name), ("limit", "10"), ("offset", "0")],
                ARTIST_CACHE_TTL,
            )
            .await?;
        let picked = resp
            .artists
            .items
            .iter()
            .find(|a| a.name.trim().eq_ignore_ascii_case(name));
        let Some(art) = picked else {
            // Never overwrite a previously cached positive artist ID on a search miss.
            self.cache_artist_name(name, 0, false);
            return Err(QobuzError::NotFound);
        };
        self.cache_artist_name(name, art.id, true);
        self.get_artist(art.id, limit).await
    }

    pub async fn match_album(&self, artist: &str, title: &str) -> Result<Album> {
        let artist = artist.trim();
        let title = title.trim();
        if artist.is_empty() || title.is_empty() {
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

        if let Ok(detail) = self.get_artist_by_name(artist, 50).await
            && let Some(id) =
                pick_matching_album_id(&detail.albums, &want_artist, &want_title, false)
        {
            self.put_cached_album_match(artist, title, &id);
            return self.get_album(&id).await;
        }

        let query = format!("{artist} {title}");
        let res = self.search(&query, 10, 0).await?;
        let matched = pick_matching_album_id(&res.albums, &want_artist, &want_title, true);
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
        let Some(kv) = &self.kv else {
            return String::new();
        };
        let aid_s = id.to_string();
        let page_key = cache_key_for_req(
            "/artist/page",
            &[("artist_id", &aid_s), ("sort", "relevant")],
        );
        if let Ok(Some(raw)) = kv.get(&page_key)
            && let Ok(page) = decode_qobuz_json::<QArtistPagePortraitOnly>(&raw)
            && let Some(url) = page.portrait_url()
        {
            return url;
        }
        let key = cache_key_for_req(
            "/artist/get",
            &[
                ("artist_id", &aid_s),
                ("extra", "albums"),
                ("limit", "50"),
                ("offset", "0"),
            ],
        );
        if let Ok(Some(raw)) = kv.get(&key)
            && let Ok(qa) = decode_qobuz_json::<QArtist>(&raw)
        {
            return qa.to_artist().artwork_url;
        }
        String::new()
    }

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

    async fn fetch_artist_page_extras(&self, artist_id: i64) -> ArtistPageExtras {
        if artist_id <= 0 {
            return ArtistPageExtras::default();
        }
        let aid_s = artist_id.to_string();
        let Ok(resp) = self
            .get_cached_json::<QArtistPageResp>(
                "/artist/page",
                &[("artist_id", &aid_s), ("sort", "relevant")],
                ARTIST_CACHE_TTL,
            )
            .await
        else {
            return ArtistPageExtras::default();
        };

        let portrait_url = resp
            .images
            .as_ref()
            .and_then(|i| i.portrait.as_ref())
            .and_then(QArtistPortrait::url)
            .unwrap_or_default();

        const MAX_POPULAR_TRACKS: usize = 10;
        let tracks = resp
            .top_tracks
            .into_iter()
            .filter(|pt| pt.id > 0 && pt.rights.streamable != Some(false))
            .take(MAX_POPULAR_TRACKS)
            .map(QPageTopTrack::into_library_track)
            .collect();

        let similar_artists = resp
            .similar_artists
            .map(|s| s.items)
            .unwrap_or_default()
            .into_iter()
            .filter_map(QPageSimilarArtist::into_artist)
            .collect();

        ArtistPageExtras {
            tracks,
            portrait_url,
            similar_artists,
        }
    }
}

#[derive(Default, Deserialize)]
struct QArtistPortrait {
    #[serde(default)]
    hash: String,
    #[serde(default)]
    format: String,
}

impl QArtistPortrait {
    fn url(&self) -> Option<String> {
        let h = self.hash.trim();
        if h.is_empty() {
            return None;
        }
        let ext = self.format.trim();
        let ext = if ext.is_empty() { "jpg" } else { ext };
        Some(format!(
            "https://static.qobuz.com/images/artists/covers/large/{h}.{ext}"
        ))
    }
}

#[derive(Default, Deserialize)]
struct QArtistPageImages {
    #[serde(default)]
    portrait: Option<QArtistPortrait>,
}

#[derive(Default, Deserialize)]
struct QArtistPagePortraitOnly {
    #[serde(default)]
    images: Option<QArtistPageImages>,
}

impl QArtistPagePortraitOnly {
    fn portrait_url(&self) -> Option<String> {
        self.images
            .as_ref()
            .and_then(|i| i.portrait.as_ref())
            .and_then(QArtistPortrait::url)
    }
}

#[derive(Default, Deserialize)]
struct QDisplayName {
    #[serde(default)]
    display: String,
}

#[derive(Default, Deserialize)]
struct QPageSimilarArtist {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    name: QDisplayName,
    #[serde(default)]
    images: Option<QArtistPageImages>,
}

impl QPageSimilarArtist {
    fn into_artist(self) -> Option<Artist> {
        let name = self.name.display.trim().to_string();
        if self.id <= 0 || name.is_empty() {
            return None;
        }
        let artwork_url = self
            .images
            .as_ref()
            .and_then(|i| i.portrait.as_ref())
            .and_then(QArtistPortrait::url)
            .unwrap_or_default();
        Some(Artist {
            id: self.id,
            name,
            artwork_url,
            ..Default::default()
        })
    }
}

#[derive(Default, Deserialize)]
struct QPageSimilarWrap {
    #[serde(default)]
    items: Vec<QPageSimilarArtist>,
}

#[derive(Default, Deserialize)]
struct QPageTrackArtist {
    #[serde(default)]
    name: QDisplayName,
}

#[derive(Default, Deserialize)]
struct QPageTrackAlbum {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    image: QImage,
}

#[derive(Default, Deserialize)]
struct QPageTopTrack {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    isrc: String,
    #[serde(default)]
    duration: i32,
    #[serde(default)]
    parental_warning: bool,
    #[serde(default)]
    audio_info: QDiscoverAudioInfo,
    #[serde(default)]
    rights: QDiscoverRights,
    #[serde(default)]
    artist: QPageTrackArtist,
    #[serde(default)]
    album: QPageTrackAlbum,
}

impl QPageTopTrack {
    fn into_library_track(self) -> Track {
        let mut title = self.title.trim().to_string();
        let ver = self.version.trim();
        if !ver.is_empty() && !title.to_lowercase().contains(&ver.to_lowercase()) {
            title = format!("{title} ({ver})");
        }
        let artist_name = self.artist.name.display.trim().to_string();
        let album_id = if self.album.id.is_empty() {
            String::new()
        } else {
            format!("qobuz:{}", self.album.id)
        };
        Track {
            id: format!("qobuz:{}", self.id),
            source: SOURCE_QOBUZ.into(),
            source_type: SOURCE_TYPE_REMOTE.into(),
            provider: SOURCE_QOBUZ.into(),
            external_id: self.id.to_string(),
            qobuz_track_id: self.id,
            album_id,
            title,
            artist: artist_name.clone(),
            album_artist: artist_name,
            album: self.album.title.trim().to_string(),
            duration_sec: self.duration,
            artwork_url: self.album.image.best(),
            channels: 2,
            mime: "audio/flac".into(),
            ext: ".flac".into(),
            bit_depth: self.audio_info.maximum_bit_depth,
            sample_rate: normalize_sample_rate_hz(self.audio_info.maximum_sampling_rate),
            explicit: self.parental_warning,
            isrc: crate::library::normalize_isrc(&self.isrc),
            available: true,
            ..Default::default()
        }
    }
}

#[derive(Default, Deserialize)]
struct QArtistPageResp {
    #[serde(default)]
    images: Option<QArtistPageImages>,
    #[serde(default)]
    top_tracks: Vec<QPageTopTrack>,
    #[serde(default)]
    similar_artists: Option<QPageSimilarWrap>,
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
