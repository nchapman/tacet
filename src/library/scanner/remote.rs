use std::collections::HashMap;

use super::Catalog;
use crate::library::types::{
    Album, LibraryError, RemoteCollection, Result, SOURCE_TYPE_REMOTE, Track,
    format_name_from_mime_or_ext,
};

impl Catalog {
    pub async fn sync_remote(&self, provider: &str) -> Result<usize> {
        let prov = provider.trim().to_ascii_lowercase();
        let src = self
            .remote_source(&prov)
            .ok_or_else(|| LibraryError::ProviderNotFound(prov.clone()))?;
        if !src.is_connected() {
            let _ = self.set_provider_available(&prov, false);
            return Err(LibraryError::ProviderOffline(prov));
        }

        struct SyncingGuard<'a> {
            map: &'a std::sync::Mutex<std::collections::HashMap<String, bool>>,
            prov: String,
        }
        impl Drop for SyncingGuard<'_> {
            fn drop(&mut self) {
                let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
                map.insert(self.prov.clone(), false);
            }
        }

        {
            let mut map = self.syncing_provs.lock().unwrap_or_else(|e| e.into_inner());
            map.insert(prov.clone(), true);
        }
        let _sync_guard = SyncingGuard {
            map: &self.syncing_provs,
            prov: prov.clone(),
        };
        let res = src.sync_collection().await;
        drop(_sync_guard);

        let coll = match res {
            Ok(c) => c,
            Err(e) => {
                let _ = self
                    .store
                    .touch_provider_sync(&prov, SOURCE_TYPE_REMOTE, &e.to_string());
                return Err(e);
            }
        };

        let (albums, tracks) = merge_remote_collection(&prov, coll);
        let track_count = tracks.len();
        self.store
            .replace_provider(&prov, SOURCE_TYPE_REMOTE, &albums, &tracks, true)?;
        self.notify_change();
        Ok(track_count)
    }

    pub fn set_provider_available(&self, provider: &str, available: bool) -> Result<()> {
        self.store.set_provider_available(provider, available)?;
        self.notify_change();
        Ok(())
    }

    pub async fn add_remote_album(&self, provider: &str, external_id: &str) -> Result<Album> {
        let prov = provider.trim().to_ascii_lowercase();
        let src = self
            .remote_source(&prov)
            .ok_or_else(|| LibraryError::ProviderNotFound(prov.clone()))?;
        if !src.is_connected() {
            return Err(LibraryError::ProviderOffline(prov));
        }
        let mut album = src.get_album(external_id).await?;
        src.add_favorite("album", external_id).await?;
        normalize_remote_album(&prov, &mut album);
        let aid = album.id.clone();
        let mut tracks = std::mem::take(&mut album.tracks);
        for t in &mut tracks {
            normalize_remote_track(&prov, &aid, t);
        }
        self.store.upsert_album_with_tracks(&album, &tracks)?;
        self.notify_change();
        Ok(self.store.get_album(&aid)?.unwrap_or(album))
    }

    pub async fn remove_remote_album(&self, provider: &str, external_id: &str) -> Result<()> {
        let prov = provider.trim().to_ascii_lowercase();
        let id = if external_id.starts_with(&format!("{prov}:")) {
            external_id.to_string()
        } else {
            format!("{prov}:{external_id}")
        };
        let existing = self.store.get_album(&id)?.ok_or(LibraryError::NotFound)?;
        if existing.source_type != SOURCE_TYPE_REMOTE {
            return Err(LibraryError::InvalidArgument(
                "cannot remove local album via remote endpoint".into(),
            ));
        }
        if let Some(src) = self.remote_source(&prov)
            && src.is_connected()
        {
            let raw_id = id.strip_prefix(&format!("{prov}:")).unwrap_or(&id);
            src.remove_favorite("album", raw_id).await?;
        }
        self.store.delete_album(&id)?;
        self.notify_change();
        Ok(())
    }

    pub async fn add_remote_track(&self, provider: &str, external_id: &str) -> Result<Track> {
        let prov = provider.trim().to_ascii_lowercase();
        let src = self
            .remote_source(&prov)
            .ok_or_else(|| LibraryError::ProviderNotFound(prov.clone()))?;
        if !src.is_connected() {
            return Err(LibraryError::ProviderOffline(prov));
        }
        let mut track = src.get_track(external_id).await?;
        src.add_favorite("track", external_id).await?;
        let aid = if track.album_id.is_empty() {
            format!("{prov}:track-album:{external_id}")
        } else {
            track.album_id.clone()
        };
        normalize_remote_track(&prov, &aid, &mut track);
        let album = self
            .store
            .get_album(&aid)
            .ok()
            .flatten()
            .unwrap_or_else(|| build_partial_remote_album(&prov, &aid, &track));
        self.store
            .upsert_album_with_tracks(&album, std::slice::from_ref(&track))?;
        self.notify_change();
        Ok(track)
    }

    pub async fn remove_remote_track(&self, provider: &str, external_id: &str) -> Result<()> {
        let prov = provider.trim().to_ascii_lowercase();
        let id = if external_id.starts_with(&format!("{prov}:")) {
            external_id.to_string()
        } else {
            format!("{prov}:{external_id}")
        };
        let existing = self.store.get_track(&id)?.ok_or(LibraryError::NotFound)?;
        if existing.source_type != SOURCE_TYPE_REMOTE {
            return Err(LibraryError::InvalidArgument(
                "cannot remove local track via remote endpoint".into(),
            ));
        }
        if let Some(src) = self.remote_source(&prov)
            && src.is_connected()
        {
            let raw_id = id.strip_prefix(&format!("{prov}:")).unwrap_or(&id);
            src.remove_favorite("track", raw_id).await?;
        }
        self.store.delete_track(&id)?;
        self.notify_change();
        Ok(())
    }
}

fn merge_remote_collection(prov: &str, mut coll: RemoteCollection) -> (Vec<Album>, Vec<Track>) {
    let mut albums_by_id: HashMap<String, Album> = HashMap::new();
    let mut tracks_by_id: HashMap<String, Track> = HashMap::new();

    for mut a in coll.albums.drain(..) {
        normalize_remote_album(prov, &mut a);
        let aid = a.id.clone();
        let mut trs = std::mem::take(&mut a.tracks);
        for t in &mut trs {
            normalize_remote_track(prov, &aid, t);
            tracks_by_id.insert(t.id.clone(), t.clone());
        }
        if a.track_count == 0 {
            a.track_count = trs.len() as i32;
        }
        if a.duration_sec == 0 {
            a.duration_sec = trs.iter().map(|t| t.duration_sec).sum();
        }
        albums_by_id.insert(aid, a);
    }

    for mut t in coll.tracks.drain(..) {
        let aid = if t.album_id.is_empty() {
            format!("{prov}:track-album:{}", t.external_id)
        } else {
            t.album_id.clone()
        };
        normalize_remote_track(prov, &aid, &mut t);
        let entry = albums_by_id
            .entry(aid.clone())
            .or_insert_with(|| build_partial_remote_album(prov, &aid, &t));
        if !tracks_by_id.contains_key(&t.id) {
            if entry.is_partial {
                entry.track_count += 1;
                entry.duration_sec += t.duration_sec;
            }
            if t.sample_rate > entry.max_sample_rate {
                entry.max_sample_rate = t.sample_rate;
            }
            if t.bit_depth > entry.max_bit_depth {
                entry.max_bit_depth = t.bit_depth;
            }
            tracks_by_id.insert(t.id.clone(), t);
        }
    }

    (
        albums_by_id.into_values().collect(),
        tracks_by_id.into_values().collect(),
    )
}

fn build_partial_remote_album(prov: &str, aid: &str, track: &Track) -> Album {
    let mut alb = Album {
        id: aid.to_string(),
        source: prov.to_string(),
        source_type: SOURCE_TYPE_REMOTE.into(),
        provider: prov.to_string(),
        external_id: aid.strip_prefix(&format!("{prov}:")).unwrap_or(aid).into(),
        title: if track.album.is_empty() {
            track.title.clone()
        } else {
            track.album.clone()
        },
        artist: if track.album_artist.is_empty() {
            track.artist.clone()
        } else {
            track.album_artist.clone()
        },
        genre: track.genre.clone(),
        year: track.year,
        artwork_url: track.artwork_url.clone(),
        primary_format: format_name_from_mime_or_ext(&track.mime, &track.ext),
        max_sample_rate: track.sample_rate,
        max_bit_depth: track.bit_depth,
        is_partial: true,
        available: true,
        in_library: true,
        ..Default::default()
    };
    normalize_remote_album(prov, &mut alb);
    alb
}

fn normalize_remote_album(provider: &str, a: &mut Album) {
    a.source = provider.into();
    a.source_type = SOURCE_TYPE_REMOTE.into();
    a.provider = provider.into();
    if a.external_id.is_empty() {
        a.external_id =
            a.id.strip_prefix(&format!("{provider}:"))
                .unwrap_or(&a.id)
                .to_string();
    }
    if a.id.is_empty() {
        a.id = format!("{provider}:{}", a.external_id);
    }
    if a.primary_format.is_empty()
        && let Some(first) = a.tracks.first()
    {
        a.primary_format = format_name_from_mime_or_ext(&first.mime, &first.ext);
    }
    if a.primary_format.is_empty() {
        a.primary_format = "FLAC".into();
    }
    if !a.tracks.is_empty() {
        a.available = true;
    }
    a.in_library = true;
}

fn normalize_remote_track(provider: &str, album_id: &str, t: &mut Track) {
    t.source = provider.into();
    t.source_type = SOURCE_TYPE_REMOTE.into();
    t.provider = provider.into();
    if t.external_id.is_empty() {
        t.external_id =
            t.id.strip_prefix(&format!("{provider}:"))
                .unwrap_or(&t.id)
                .to_string();
    }
    if t.id.is_empty() {
        t.id = format!("{provider}:{}", t.external_id);
    }
    if t.album_id.is_empty() {
        t.album_id = album_id.to_string();
    }
    t.available = true;
    t.in_library = true;
}
