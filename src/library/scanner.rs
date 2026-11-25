use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use lofty::prelude::{Accessor, TaggedFileExt};
use lofty::probe::Probe;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use walkdir::WalkDir;

use super::probe::{probe_audio, read_embedded_art};
use super::store::LibraryStore;
use super::types::{
    Album, AlbumFilter, ArtistSummary, FacetFilter, GenreSummary, HeavyRotationAlbum, LibraryError,
    ProviderSyncStatus, RemoteSource, Result, SOURCE_LOCAL, SOURCE_TYPE_LOCAL, SOURCE_TYPE_REMOTE,
    Status, Track, TrackFilter, UnifiedSearchResult, format_name_from_mime_or_ext,
    is_lossless_format, mime_for_ext, short_hash, short_hash_bytes,
};

const MAX_COVER_ART_BYTES: usize = 10 << 20; // 10 MiB

const COVER_ART_NAMES: &[&str] = &[
    "cover.jpg",
    "cover.jpeg",
    "cover.png",
    "cover.webp",
    "folder.jpg",
    "folder.jpeg",
    "folder.png",
    "front.jpg",
    "front.jpeg",
    "front.png",
    "album.jpg",
    "album.png",
];

type OnChangeFn = Arc<dyn Fn(Status) + Send + Sync>;

struct ScanState {
    scanning: bool,
    notify: Option<Arc<Notify>>,
    last_err: Option<String>,
}

type LocalAlbumMap = Arc<HashMap<String, Album>>;
type LocalTrackMap = Arc<HashMap<String, Vec<Track>>>;

struct LocalLookupCache {
    etag: String,
    albums: LocalAlbumMap,
    tracks: LocalTrackMap,
}

pub struct Catalog {
    roots: RwLock<Vec<PathBuf>>,
    store: Arc<LibraryStore>,
    remotes: RwLock<HashMap<String, Arc<dyn RemoteSource>>>,
    scan_state: Mutex<ScanState>,
    syncing_provs: Mutex<HashMap<String, bool>>,
    on_change: RwLock<Option<OnChangeFn>>,
    local_lookup: RwLock<LocalLookupCache>,
}

impl Catalog {
    pub fn new_in_memory(roots: Vec<PathBuf>) -> Arc<Self> {
        Self::open(":memory:", roots).expect("open in-memory library store")
    }

    #[cfg(test)]
    pub(crate) fn store(&self) -> &LibraryStore {
        &self.store
    }

    pub fn open(db_path: &str, roots: Vec<PathBuf>) -> Result<Arc<Self>> {
        let store = Arc::new(LibraryStore::open(db_path)?);
        let cleaned: Vec<PathBuf> = roots
            .into_iter()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| std::fs::canonicalize(&p).unwrap_or(p))
            .collect();
        Ok(Arc::new(Self {
            roots: RwLock::new(cleaned),
            store,
            remotes: RwLock::new(HashMap::new()),
            scan_state: Mutex::new(ScanState {
                scanning: false,
                notify: None,
                last_err: None,
            }),
            syncing_provs: Mutex::new(HashMap::new()),
            on_change: RwLock::new(None),
            local_lookup: RwLock::new(LocalLookupCache {
                etag: String::new(),
                albums: Arc::new(HashMap::new()),
                tracks: Arc::new(HashMap::new()),
            }),
        }))
    }

    pub fn set_missing_retention(&self, d: Duration) {
        self.store.set_missing_retention(d);
    }

    pub fn schema_version(&self) -> Result<i32> {
        self.store.schema_version()
    }

    pub fn close(&self) {
        self.store.close();
    }

    pub fn set_on_change(&self, f: OnChangeFn) {
        *self.on_change.write().unwrap_or_else(|e| e.into_inner()) = Some(f);
    }

    fn notify_change(&self) {
        let cb = self
            .on_change
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(f) = cb {
            f(self.status());
        }
    }

    pub fn register_remote_source(&self, src: Arc<dyn RemoteSource>) {
        let id = src.provider_id().trim().to_ascii_lowercase();
        if id.is_empty() {
            return;
        }
        self.remotes
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, src);
    }

    fn remote_source(&self, provider: &str) -> Option<Arc<dyn RemoteSource>> {
        self.remotes
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&provider.trim().to_ascii_lowercase())
            .cloned()
    }

    pub fn roots(&self) -> Vec<PathBuf> {
        self.roots.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn status(&self) -> Status {
        let local_scanning = self
            .scan_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .scanning;

        let (syncing_copy, any_remote_syncing) = {
            let map = self.syncing_provs.lock().unwrap_or_else(|e| e.into_inner());
            let any = map.values().any(|&v| v);
            (map.clone(), any)
        };

        let counts = self.store.counts_and_providers();
        let has_roots = !self
            .roots
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty();
        let remotes = self
            .remotes
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        let mut providers: Vec<ProviderSyncStatus> = Vec::with_capacity(remotes.len() + 1);
        if has_roots || counts.local_albums > 0 || counts.local_tracks > 0 {
            let mut ps = counts.states.get(SOURCE_LOCAL).cloned().unwrap_or_default();
            ps.provider = SOURCE_LOCAL.into();
            ps.source_type = SOURCE_TYPE_LOCAL.into();
            ps.connected = has_roots;
            ps.syncing = local_scanning;
            ps.albums = counts.local_albums;
            ps.tracks = counts.local_tracks;
            providers.push(ps);
        }

        let mut remote_ids: Vec<String> = remotes.keys().cloned().collect();
        remote_ids.sort();
        for id in remote_ids {
            if let Some(src) = remotes.get(&id) {
                let mut ps = counts.states.get(&id).cloned().unwrap_or_default();
                ps.provider = id.clone();
                ps.source_type = SOURCE_TYPE_REMOTE.into();
                ps.connected = src.is_connected();
                ps.syncing = syncing_copy.get(&id).copied().unwrap_or(false);
                providers.push(ps);
            }
        }

        Status {
            configured: has_roots || !remotes.is_empty(),
            scanning: local_scanning || any_remote_syncing,
            tracks: counts.total_tracks,
            albums: counts.total_albums,
            local_tracks: counts.local_tracks,
            local_albums: counts.local_albums,
            remote_tracks: counts.remote_tracks,
            remote_albums: counts.remote_albums,
            missing_tracks: counts.missing_tracks,
            missing_albums: counts.missing_albums,
            providers,
            etag: format!(
                "\"lib-{}-{}-{}-{}\"",
                counts.boot_id, counts.generation, counts.total_tracks, counts.total_albums
            ),
        }
    }

    pub fn etag(&self) -> String {
        self.status().etag
    }

    /// Incrementally walk configured music roots and update SQLite.
    /// Concurrent scan calls coalesce onto the active scan.
    pub async fn scan(&self) -> Result<usize> {
        let wait_notify = {
            let mut st = self.scan_state.lock().unwrap_or_else(|e| e.into_inner());
            if st.scanning {
                st.notify.clone()
            } else {
                st.scanning = true;
                st.notify = Some(Arc::new(Notify::new()));
                st.last_err = None;
                None
            }
        };

        if let Some(n) = wait_notify {
            n.notified().await;
            let err = self
                .scan_state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .last_err
                .clone();
            if let Some(msg) = err {
                return Err(LibraryError::Remote(msg));
            }
            return Ok(self.status().local_tracks as usize);
        }

        let roots = self.roots();
        let store = Arc::clone(&self.store);
        let res = tokio::task::spawn_blocking(move || scan_roots_blocking(&store, &roots))
            .await
            .map_err(|e| LibraryError::Remote(format!("scan task join: {e}")))?;

        let notify = {
            let mut st = self.scan_state.lock().unwrap_or_else(|e| e.into_inner());
            st.scanning = false;
            st.last_err = res.as_ref().err().map(ToString::to_string);
            st.notify.take()
        };
        if let Some(n) = notify {
            n.notify_waiters();
        }

        if let Ok((count, changed)) = &res {
            if *changed {
                self.notify_change();
            }
            return Ok(*count);
        }
        Err(res.unwrap_err())
    }

    pub async fn sync_remote(&self, provider: &str) -> Result<usize> {
        let prov = provider.trim().to_ascii_lowercase();
        let src = self
            .remote_source(&prov)
            .ok_or_else(|| LibraryError::ProviderNotFound(prov.clone()))?;
        if !src.is_connected() {
            return Err(LibraryError::ProviderOffline(prov));
        }

        {
            let mut map = self.syncing_provs.lock().unwrap_or_else(|e| e.into_inner());
            map.insert(prov.clone(), true);
        }

        let res = src.sync_collection().await;

        {
            let mut map = self.syncing_provs.lock().unwrap_or_else(|e| e.into_inner());
            map.insert(prov.clone(), false);
        }

        let mut coll = match res {
            Ok(c) => c,
            Err(e) => {
                let _ = self
                    .store
                    .touch_provider_sync(&prov, SOURCE_TYPE_REMOTE, &e.to_string());
                return Err(e);
            }
        };

        let mut albums_by_id: HashMap<String, Album> = HashMap::new();
        let mut tracks_by_id: HashMap<String, Track> = HashMap::new();

        for mut a in coll.albums.drain(..) {
            normalize_remote_album(&prov, &mut a);
            let aid = a.id.clone();
            let mut trs = std::mem::take(&mut a.tracks);
            for t in &mut trs {
                normalize_remote_track(&prov, &aid, t);
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
            normalize_remote_track(&prov, &aid, &mut t);
            let entry = albums_by_id.entry(aid.clone()).or_insert_with(|| {
                let mut a = Album {
                    id: aid.clone(),
                    source: prov.clone(),
                    source_type: SOURCE_TYPE_REMOTE.into(),
                    provider: prov.clone(),
                    external_id: aid.strip_prefix(&format!("{prov}:")).unwrap_or(&aid).into(),
                    title: if t.album.is_empty() {
                        t.title.clone()
                    } else {
                        t.album.clone()
                    },
                    artist: if t.album_artist.is_empty() {
                        t.artist.clone()
                    } else {
                        t.album_artist.clone()
                    },
                    genre: t.genre.clone(),
                    year: t.year,
                    artwork_url: t.artwork_url.clone(),
                    primary_format: format_name_from_mime_or_ext(&t.mime, &t.ext),
                    max_sample_rate: t.sample_rate,
                    max_bit_depth: t.bit_depth,
                    is_partial: true,
                    available: true,
                    in_library: true,
                    ..Default::default()
                };
                normalize_remote_album(&prov, &mut a);
                a
            });
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

        let albums: Vec<Album> = albums_by_id.into_values().collect();
        let tracks: Vec<Track> = tracks_by_id.into_values().collect();
        let track_count = tracks.len();
        self.store
            .replace_provider(&prov, SOURCE_TYPE_REMOTE, &albums, &tracks, false)?;
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
        src.add_favorite("album", external_id).await?;
        let mut album = src.get_album(external_id).await?;
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
        let src = self
            .remote_source(&prov)
            .ok_or_else(|| LibraryError::ProviderNotFound(prov.clone()))?;
        if !src.is_connected() {
            return Err(LibraryError::ProviderOffline(prov));
        }
        src.remove_favorite("album", external_id).await?;
        let id = if external_id.starts_with(&format!("{prov}:")) {
            external_id.to_string()
        } else {
            format!("{prov}:{external_id}")
        };
        let _ = self.store.delete_album(&id);
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
        src.add_favorite("track", external_id).await?;
        let mut track = src.get_track(external_id).await?;
        let aid = if track.album_id.is_empty() {
            format!("{prov}:track-album:{external_id}")
        } else {
            track.album_id.clone()
        };
        normalize_remote_track(&prov, &aid, &mut track);
        let album = if let Some(existing) = self.store.get_album(&aid).ok().flatten() {
            existing
        } else {
            let mut alb = Album {
                id: aid.clone(),
                source: prov.clone(),
                source_type: SOURCE_TYPE_REMOTE.into(),
                provider: prov.clone(),
                external_id: aid.strip_prefix(&format!("{prov}:")).unwrap_or(&aid).into(),
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
            normalize_remote_album(&prov, &mut alb);
            alb
        };
        self.store
            .upsert_album_with_tracks(&album, std::slice::from_ref(&track))?;
        self.notify_change();
        Ok(track)
    }

    pub async fn remove_remote_track(&self, provider: &str, external_id: &str) -> Result<()> {
        let prov = provider.trim().to_ascii_lowercase();
        let src = self
            .remote_source(&prov)
            .ok_or_else(|| LibraryError::ProviderNotFound(prov.clone()))?;
        if !src.is_connected() {
            return Err(LibraryError::ProviderOffline(prov));
        }
        src.remove_favorite("track", external_id).await?;
        let id = if external_id.starts_with(&format!("{prov}:")) {
            external_id.to_string()
        } else {
            format!("{prov}:{external_id}")
        };
        let _ = self.store.delete_track(&id);
        self.notify_change();
        Ok(())
    }

    pub fn query_albums(&self, f: &AlbumFilter) -> Result<(Vec<Album>, i32)> {
        self.store.query_albums(f)
    }

    pub fn query_tracks(&self, f: &TrackFilter) -> Result<(Vec<Track>, i32)> {
        self.store.query_tracks(f)
    }

    pub fn list_artists(&self, f: &FacetFilter) -> Result<(Vec<ArtistSummary>, i32)> {
        self.store.list_artists(f)
    }

    pub fn list_genres(&self, f: &FacetFilter) -> Result<(Vec<GenreSummary>, i32)> {
        self.store.list_genres(f)
    }

    pub fn has_album(&self, id: &str) -> bool {
        self.store.has_album(id)
    }

    pub fn album(&self, id: &str) -> Option<Album> {
        self.store.get_album(id).ok().flatten()
    }

    pub fn track(&self, id: &str) -> Option<Track> {
        self.store.get_track(id).ok().flatten()
    }

    pub fn set_primary_version(&self, album_id: &str) -> Result<Album> {
        self.store.set_primary_version(album_id)?;
        self.notify_change();
        self.store
            .get_album(album_id)?
            .ok_or(LibraryError::NotFound)
    }

    pub fn group_versions(&self, album_ids: &[String]) -> Result<String> {
        let gid = self.store.group_versions(album_ids)?;
        self.notify_change();
        Ok(gid)
    }

    pub fn ungroup_version(&self, album_id: &str) -> Result<Album> {
        self.store.ungroup_version(album_id)?;
        self.notify_change();
        self.store
            .get_album(album_id)?
            .ok_or(LibraryError::NotFound)
    }

    pub fn record_play(&self, track: &Track) {
        if let Err(e) = self.store.record_play(track) {
            tracing::warn!(track_id = %track.id, error = %e, "library record play failed");
        }
    }

    pub fn heavy_rotation(&self, days: i32, limit: i32) -> Result<Vec<HeavyRotationAlbum>> {
        self.store.heavy_rotation(days, limit)
    }

    pub fn unified_search(&self, query: &str, limit: i32) -> Result<UnifiedSearchResult> {
        self.store.unified_search(query, limit)
    }

    fn local_lookup_maps(&self) -> (LocalAlbumMap, LocalTrackMap) {
        let cur_etag = self.etag();
        {
            let g = self.local_lookup.read().unwrap_or_else(|e| e.into_inner());
            if g.etag == cur_etag && !cur_etag.is_empty() {
                return (Arc::clone(&g.albums), Arc::clone(&g.tracks));
            }
        }
        let local_albs = self
            .store
            .query_albums(&AlbumFilter {
                source_type: SOURCE_TYPE_LOCAL.into(),
                limit: 100_000,
                ..Default::default()
            })
            .map(|(v, _)| v)
            .unwrap_or_default();
        let mut alb_map = HashMap::with_capacity(local_albs.len());
        for mut la in local_albs {
            let k = format!(
                "{}|{}",
                norm_media_key(&la.artist),
                norm_media_key(&la.title)
            );
            if k != "|" {
                la.in_library = true;
                alb_map.entry(k).or_insert(la);
            }
        }
        let local_tracks = self
            .store
            .query_tracks(&TrackFilter {
                source_type: SOURCE_TYPE_LOCAL.into(),
                limit: 100_000,
                ..Default::default()
            })
            .map(|(v, _)| v)
            .unwrap_or_default();
        let mut by_key: HashMap<String, Vec<Track>> = HashMap::new();
        for mut lt in local_tracks {
            if !lt.available {
                continue;
            }
            let k = format!(
                "{}|{}",
                norm_media_key(&lt.artist),
                norm_media_key(&lt.title)
            );
            if k != "|" {
                lt.in_library = true;
                by_key.entry(k).or_default().push(lt);
            }
        }
        let albums = Arc::new(alb_map);
        let tracks = Arc::new(by_key);
        let mut g = self.local_lookup.write().unwrap_or_else(|e| e.into_inner());
        g.etag = cur_etag;
        g.albums = Arc::clone(&albums);
        g.tracks = Arc::clone(&tracks);
        (albums, tracks)
    }

    pub fn prefer_local_albums(&self, albums: Vec<Album>) -> Vec<Album> {
        if albums.is_empty() {
            return albums;
        }
        let (local_by_key, _) = self.local_lookup_maps();

        albums
            .into_iter()
            .map(|mut a| {
                if a.source == SOURCE_LOCAL {
                    a.in_library = true;
                    return a;
                }
                if let Some(mut matched) = match_local_album(&local_by_key, &a.artist, &a.title) {
                    if matched.artwork_url.is_empty() {
                        matched.artwork_url = a.artwork_url;
                    }
                    return matched;
                }
                a.in_library = self.has_album(&a.id);
                a
            })
            .collect()
    }

    pub fn prefer_local_tracks(&self, tracks: Vec<Track>) -> Vec<Track> {
        if tracks.is_empty() {
            return tracks;
        }
        let (_, by_key) = self.local_lookup_maps();

        tracks
            .into_iter()
            .map(|mut tr| {
                if tr.source == SOURCE_LOCAL {
                    tr.in_library = true;
                    return tr;
                }
                let k = format!(
                    "{}|{}",
                    norm_media_key(&tr.artist),
                    norm_media_key(&tr.title)
                );
                if let Some(cands) = by_key.get(&k)
                    && let Some(mut best) = pick_best_local_track(cands, &tr.album)
                {
                    if best.artwork_url.is_empty() {
                        best.artwork_url = tr.artwork_url;
                    }
                    return best;
                }
                if let Some(lib_tr) = self.track(&tr.id) {
                    tr.in_library = lib_tr.available;
                    tr.play_count = lib_tr.play_count;
                    tr.last_played_at = lib_tr.last_played_at;
                }
                tr
            })
            .collect()
    }

    pub fn get_pref(&self, key: &str) -> Result<Option<String>> {
        self.store.get_pref(key)
    }

    pub fn set_pref(&self, key: &str, value: &str) -> Result<()> {
        self.store.set_pref(key, value)
    }

    pub fn delete_pref(&self, key: &str) -> Result<()> {
        self.store.delete_pref(key)
    }

    pub fn list_prefs(&self) -> Result<HashMap<String, String>> {
        self.store.list_prefs()
    }

    pub fn get_provider_credentials<T: DeserializeOwned>(
        &self,
        provider: &str,
    ) -> Result<Option<T>> {
        self.store.get_provider_credentials(provider)
    }

    pub fn set_provider_credentials<T: Serialize>(&self, provider: &str, src: &T) -> Result<()> {
        self.store.set_provider_credentials(provider, src)
    }

    pub fn delete_provider_credentials(&self, provider: &str) -> Result<()> {
        self.store.delete_provider_credentials(provider)
    }

    pub fn local_art_hashes(&self) -> Result<Vec<String>> {
        self.store.list_local_art_hashes()
    }

    pub fn cover_art(&self, track_id: &str) -> Option<(Vec<u8>, String)> {
        let t = self.track(track_id)?;
        load_track_cover_art(&t)
    }

    pub fn cover_art_by_hash(&self, hash: &str) -> Option<(Vec<u8>, String)> {
        let h = hash.trim();
        if h.is_empty() {
            return None;
        }
        let t = self
            .store
            .get_track_by_artwork_url(&format!("/media/art/{h}"))
            .ok()
            .flatten()?;
        load_track_cover_art(&t)
    }
}

fn scan_roots_blocking(store: &LibraryStore, roots: &[PathBuf]) -> Result<(usize, bool)> {
    if roots.is_empty() {
        return Ok((0, false));
    }

    // Guard against unmounted NAS / disconnected drives wiping the local library.
    for root in roots {
        if std::fs::read_dir(root).is_err() {
            tracing::warn!(
                root = %root.display(),
                "skipping local library scan because configured music root is unavailable"
            );
            return Ok((0, false));
        }
    }

    let file_cache = store.load_local_file_cache().unwrap_or_default();
    let mut reused: Vec<Track> = Vec::new();
    let mut to_probe: Vec<(PathBuf, u64, i64, DateTime<Utc>)> = Vec::new();
    let mut seen_paths = std::collections::HashSet::new();

    for root in roots {
        if !root.exists() {
            continue;
        }
        for entry in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                if e.depth() == 0 {
                    return true;
                }
                let name = e.file_name().to_string_lossy();
                !name.starts_with('.')
                    && !matches!(
                        name.to_ascii_lowercase().as_str(),
                        "@eadir" | "#recycle" | "$recycle.bin" | "system volume information"
                    )
            })
            .flatten()
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| format!(".{}", s.to_ascii_lowercase()))
                .unwrap_or_default();
            if mime_for_ext(&ext).is_none() {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let size = meta.len();
            let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
            let mtime_ns = mtime
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);
            let path_str = path.to_string_lossy().to_string();
            seen_paths.insert(path_str.clone());

            if let Some(cached) = file_cache.get(&path_str)
                && cached.size == size as i64
                && cached.mtime_ns == mtime_ns
                && cached.track.available
            {
                reused.push(cached.track.clone());
                continue;
            }
            let added_at: DateTime<Utc> = mtime.into();
            to_probe.push((path.to_path_buf(), size, mtime_ns, added_at));
        }
    }

    let missing_count = file_cache
        .keys()
        .filter(|p| !seen_paths.contains(*p))
        .count();

    if to_probe.is_empty() && missing_count == 0 {
        let _ = store.touch_provider_sync(SOURCE_LOCAL, SOURCE_TYPE_LOCAL, "");
        return Ok((reused.len(), false));
    }

    let dir_art_cache: Mutex<HashMap<PathBuf, (String, String)>> = Mutex::new(HashMap::new());
    let lookup_dir_art = |dir: &Path| -> (String, String) {
        {
            let guard = dir_art_cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(v) = guard.get(dir) {
                return v.clone();
            }
        }
        let found = find_folder_art(dir);
        let hash = if found.is_empty() {
            String::new()
        } else {
            hash_cover_file(Path::new(&found))
        };
        let pair = (found, hash);
        dir_art_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(dir.to_path_buf(), pair.clone());
        pair
    };

    let mut all_tracks = reused;
    for (path, size, mtime_ns, added_at) in to_probe {
        if let Some(tr) = scan_file_with_art(&path, size, mtime_ns, added_at, &lookup_dir_art) {
            all_tracks.push(tr);
        }
    }

    let albums = build_albums_from_tracks(&all_tracks);
    let count = all_tracks.len();
    store.replace_provider(SOURCE_LOCAL, SOURCE_TYPE_LOCAL, &albums, &all_tracks, true)?;
    Ok((count, true))
}

fn scan_file_with_art(
    path: &Path,
    size: u64,
    mtime_ns: i64,
    added_at: DateTime<Utc>,
    lookup_dir_art: &impl Fn(&Path) -> (String, String),
) -> Option<Track> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| format!(".{}", s.to_ascii_lowercase()))?;
    let mime_type = mime_for_ext(&ext)?;

    let clean_str = path.to_string_lossy().to_string();
    let id = format!("local:{}", short_hash(&format!("local:{clean_str}")));
    let dir = path.parent().unwrap_or_else(|| Path::new(""));
    let (folder_art, folder_art_hash) = lookup_dir_art(dir);

    let props = probe_audio(path);
    let base_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Unknown")
        .to_string();
    let dir_name = dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("Unknown Album")
        .to_string();

    let mut t = Track {
        id,
        source: SOURCE_LOCAL.into(),
        source_type: SOURCE_TYPE_LOCAL.into(),
        provider: SOURCE_LOCAL.into(),
        external_id: clean_str.clone(),
        title: base_name,
        artist: "Unknown Artist".into(),
        album: dir_name,
        duration_sec: props.duration.as_secs_f64().round() as i32,
        sample_rate: props.sample_rate,
        bit_depth: props.bit_depth,
        channels: props.channels,
        mime: mime_type.into(),
        ext,
        size: size as i64,
        file_path: clean_str,
        file_mtime_ns: mtime_ns,
        folder_art,
        added_at: Some(added_at),
        available: true,
        in_library: true,
        ..Default::default()
    };

    let mut embed_art_hash = String::new();
    if let Ok(probe) = Probe::open(path)
        && let Ok(tagged) = probe.read()
        && let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag())
    {
        if let Some(s) = tag
            .title()
            .map(|v| v.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            t.title = s;
        }
        if let Some(s) = tag
            .artist()
            .map(|v| v.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            t.artist = s;
        }
        if let Some(s) = tag
            .get_string(&lofty::tag::ItemKey::AlbumArtist)
            .map(|v| v.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            t.album_artist = s;
        }
        if let Some(s) = tag
            .album()
            .map(|v| v.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            t.album = s;
        }
        if let Some(s) = tag
            .genre()
            .map(|v| v.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            t.genre = s;
        }
        if let Some(n) = tag.track() {
            t.track_number = n as i32;
        }
        if let Some(n) = tag.disk() {
            t.disc_number = n as i32;
        }
        if let Some(y) = tag.year() {
            t.year = y as i32;
        }
        if let Some(pic) = tag.pictures().first()
            && !pic.data().is_empty()
            && pic.data().len() <= MAX_COVER_ART_BYTES
        {
            t.has_embed_art = true;
            embed_art_hash = short_hash_bytes(pic.data());
        }
    }

    let alb_artist = if t.album_artist.is_empty() {
        &t.artist
    } else {
        &t.album_artist
    };
    t.album_id = format!(
        "local:{}",
        short_hash(&format!(
            "album:{}\0{}",
            alb_artist.to_lowercase(),
            t.album.to_lowercase()
        ))
    );

    if !embed_art_hash.is_empty() {
        t.artwork_url = format!("/media/art/{embed_art_hash}");
    } else if !folder_art_hash.is_empty() {
        t.artwork_url = format!("/media/art/{folder_art_hash}");
    } else if t.has_embed_art || !t.folder_art.is_empty() {
        t.artwork_url = format!("/media/art/{}", t.id);
    }

    Some(t)
}

fn build_albums_from_tracks(tracks: &[Track]) -> Vec<Album> {
    let mut map: HashMap<String, Album> = HashMap::new();
    for t in tracks {
        let artist = if t.album_artist.is_empty() {
            &t.artist
        } else {
            &t.album_artist
        };
        let entry = map.entry(t.album_id.clone()).or_insert_with(|| Album {
            id: t.album_id.clone(),
            source: t.source.clone(),
            source_type: t.source_type.clone(),
            provider: t.provider.clone(),
            external_id: t.album_id.clone(),
            title: t.album.clone(),
            artist: artist.to_string(),
            genre: t.genre.clone(),
            year: t.year,
            artwork_url: t.artwork_url.clone(),
            added_at: t.added_at,
            available: true,
            in_library: true,
            ..Default::default()
        });
        entry.track_count += 1;
        entry.duration_sec += t.duration_sec;
        if entry.year == 0 && t.year > 0 {
            entry.year = t.year;
        }
        if entry.genre.is_empty() && !t.genre.is_empty() {
            entry.genre = t.genre.clone();
        }
        if entry.artwork_url.is_empty() && !t.artwork_url.is_empty() {
            entry.artwork_url = t.artwork_url.clone();
        }
        if t.sample_rate > entry.max_sample_rate {
            entry.max_sample_rate = t.sample_rate;
        }
        if t.bit_depth > entry.max_bit_depth {
            entry.max_bit_depth = t.bit_depth;
        }
        let fmt = format_name_from_mime_or_ext(&t.mime, &t.ext);
        if !fmt.is_empty()
            && (entry.primary_format.is_empty()
                || (is_lossless_format(&fmt) && !is_lossless_format(&entry.primary_format)))
        {
            entry.primary_format = fmt;
        }
        if let (Some(a_dt), Some(t_dt)) = (entry.added_at, t.added_at)
            && t_dt > a_dt
        {
            entry.added_at = Some(t_dt);
        }
    }
    map.into_values().collect()
}

fn load_track_cover_art(t: &Track) -> Option<(Vec<u8>, String)> {
    if t.has_embed_art
        && !t.file_path.is_empty()
        && let Some(pic) = read_embedded_art(Path::new(&t.file_path), MAX_COVER_ART_BYTES)
    {
        return Some(pic);
    }
    if !t.folder_art.is_empty() {
        let p = Path::new(&t.folder_art);
        if let Ok(meta) = std::fs::metadata(p)
            && meta.len() > 0
            && (meta.len() as usize) <= MAX_COVER_ART_BYTES
            && let Ok(data) = std::fs::read(p)
        {
            let mime = mime_guess::from_path(p)
                .first_raw()
                .unwrap_or("image/jpeg")
                .to_string();
            return Some((data, mime));
        }
    }
    None
}

fn find_folder_art(dir: &Path) -> String {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return String::new();
    };
    let mut by_lower: HashMap<String, PathBuf> = HashMap::new();
    for e in entries.flatten() {
        // Reject symlinks to prevent exposing arbitrary files outside music roots.
        if e.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            let name = e.file_name().to_string_lossy().to_ascii_lowercase();
            by_lower.insert(name, e.path());
        }
    }
    for name in COVER_ART_NAMES {
        if let Some(full) = by_lower.get(*name) {
            return full.to_string_lossy().to_string();
        }
    }
    String::new()
}

fn hash_cover_file(path: &Path) -> String {
    let Ok(mut f) = File::open(path) else {
        return String::new();
    };
    let Ok(meta) = f.metadata() else {
        return String::new();
    };
    if meta.len() == 0 || meta.len() > MAX_COVER_ART_BYTES as u64 {
        return String::new();
    }
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    let mut remaining = MAX_COVER_ART_BYTES;
    while remaining > 0 {
        let to_read = buf.len().min(remaining);
        let n = match f.read(&mut buf[..to_read]) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => return String::new(),
        };
        hasher.update(&buf[..n]);
        remaining -= n;
    }
    let sum = hasher.finalize();
    hex::encode(&sum[..8])
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

fn norm_media_key(s: &str) -> String {
    s.trim()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

fn match_local_album(
    local_by_key: &HashMap<String, Album>,
    artist: &str,
    title: &str,
) -> Option<Album> {
    let na = norm_media_key(artist);
    let nt = norm_media_key(title);
    if na.is_empty() || nt.is_empty() {
        return None;
    }
    if let Some(la) = local_by_key.get(&format!("{na}|{nt}")) {
        return Some(la.clone());
    }
    if let Some(idx) = title.find(['(', '['])
        && idx > 0
    {
        let base = norm_media_key(&title[..idx]);
        if !base.is_empty()
            && let Some(la) = local_by_key.get(&format!("{na}|{base}"))
        {
            return Some(la.clone());
        }
    }
    None
}

fn pick_best_local_track(cands: &[Track], want_album: &str) -> Option<Track> {
    let first = cands.first()?;
    let want_key = norm_media_key(want_album);
    let mut best = first;
    let mut best_alb_match = !want_key.is_empty() && norm_media_key(&best.album) == want_key;
    for c in &cands[1..] {
        let alb_match = !want_key.is_empty() && norm_media_key(&c.album) == want_key;
        if alb_match && !best_alb_match {
            best = c;
            best_alb_match = true;
            continue;
        }
        if alb_match == best_alb_match
            && (c.sample_rate > best.sample_rate
                || (c.sample_rate == best.sample_rate && c.bit_depth > best.bit_depth))
        {
            best = c;
        }
    }
    Some(best.clone())
}
