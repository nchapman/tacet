use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::library::{Album, LibraryError, RemoteCollection, RemoteSource, SOURCE_QOBUZ, Track};

use super::Client;
use crate::qobuz::types::{AlbumLookup, QAlbum, QTrack, QobuzError, Result, norm_match_key};

impl Client {
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
        const MAX_FAVORITE_PAGES: usize = 100;
        let mut offset = 0;
        for _ in 0..MAX_FAVORITE_PAGES {
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
        for _ in 0..MAX_FAVORITE_PAGES {
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

        let albums = self.hydrate_favorite_albums(fav_albums).await?;

        Ok(RemoteCollection {
            albums,
            tracks: fav_tracks,
        })
    }

    async fn hydrate_favorite_albums(&self, fav_albums: Vec<QAlbum>) -> Result<Vec<Album>> {
        use futures_util::stream::{self, StreamExt};

        let mut indexed: Vec<(usize, Album)> = Vec::with_capacity(fav_albums.len());
        let mut futs = stream::iter(fav_albums.into_iter().enumerate().map(|(idx, qa)| async move {
            match self.get_album(&qa.id).await {
                Ok(full) => Ok((idx, full)),
                Err(QobuzError::NotFound) => {
                    tracing::warn!(album_id = %qa.id, "qobuz favorites sync: album not found; marking unavailable");
                    let mut summary = qa.to_library_album(false);
                    summary.available = false;
                    Ok((idx, summary))
                }
                Err(e @ (QobuzError::Unauthenticated | QobuzError::RateLimited)) => Err(e),
                Err(e) => {
                    tracing::warn!(album_id = %qa.id, error = %e, "qobuz favorites sync: album detail fetch failed; using summary");
                    Ok((idx, qa.to_library_album(true)))
                }
            }
        }))
        .buffer_unordered(3);

        while let Some(res) = futs.next().await {
            indexed.push(res?);
        }
        indexed.sort_by_key(|(idx, _)| *idx);
        Ok(indexed.into_iter().map(|(_, a)| a).collect())
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
            let start = std::time::Instant::now();
            let mut warmed_artists = 0usize;
            let mut warmed_albums = 0usize;
            let mut seen_a = HashSet::new();
            for name in artist_names.into_iter().take(250) {
                if cancel.is_cancelled() || !client.is_connected() {
                    return;
                }
                let key = norm_match_key(&name);
                if key.is_empty() || !seen_a.insert(key) {
                    continue;
                }
                if let Some(id) = client.get_cached_artist_name(&name) {
                    if id == 0 {
                        continue;
                    }
                    let aid_s = id.to_string();
                    if client.has_cached_json(
                        "/artist/get",
                        &[
                            ("artist_id", aid_s.as_str()),
                            ("extra", "albums"),
                            ("limit", "50"),
                            ("offset", "0"),
                        ],
                    ) {
                        continue;
                    }
                }
                match client.get_artist_by_name(&name, 50).await {
                    Ok(_) => warmed_artists += 1,
                    Err(QobuzError::RateLimited) => {
                        tracing::info!(
                            "qobuz prewarm: rate limited while warming artists; pausing pass"
                        );
                        return;
                    }
                    Err(_) => {}
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
                if let Some(alb_id) = client.get_cached_album_match(&a.artist, &a.title) {
                    if alb_id.is_empty() {
                        continue;
                    }
                    if client.has_cached_json(
                        "/album/get",
                        &[("album_id", alb_id.as_str()), ("limit", "500")],
                    ) {
                        continue;
                    }
                }
                match client.match_album(&a.artist, &a.title).await {
                    Ok(_) => warmed_albums += 1,
                    Err(QobuzError::RateLimited) => {
                        tracing::info!(
                            "qobuz prewarm: rate limited while warming albums; pausing pass"
                        );
                        return;
                    }
                    Err(_) => {}
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            if warmed_artists > 0 || warmed_albums > 0 {
                tracing::info!(
                    artists = warmed_artists,
                    albums = warmed_albums,
                    elapsed_ms = start.elapsed().as_millis(),
                    "prewarmed Qobuz SQLite cache"
                );
            }
        });
    }
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
        let id: i64 = clean.parse().ok().filter(|&id| id > 0).ok_or_else(|| {
            LibraryError::Remote(format!("invalid qobuz track id {external_id:?}"))
        })?;
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
