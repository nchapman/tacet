mod local;
mod remote;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::Notify;

use super::store::LibraryStore;
use super::types::{
    Album, AlbumFilter, ArtistSummary, FacetFilter, GenreSummary, HeavyRotationAlbum, LibraryError,
    ProviderSyncStatus, RemoteSource, Result, SOURCE_LOCAL, SOURCE_TYPE_LOCAL, SOURCE_TYPE_REMOTE,
    Status, Track, TrackFilter, UnifiedSearchResult,
};

type OnChangeFn = Arc<dyn Fn(Status) + Send + Sync>;

pub(super) struct ScanState {
    pub(super) scanning: bool,
    pub(super) notify: Option<Arc<Notify>>,
    pub(super) last_err: Option<String>,
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
    pub(super) store: Arc<LibraryStore>,
    remotes: RwLock<HashMap<String, Arc<dyn RemoteSource>>>,
    pub(super) scan_state: Mutex<ScanState>,
    pub(super) syncing_provs: Mutex<HashMap<String, bool>>,
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

    pub(super) fn notify_change(&self) {
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

    pub(super) fn remote_source(&self, provider: &str) -> Option<Arc<dyn RemoteSource>> {
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
        local::load_track_cover_art(&t)
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
        local::load_track_cover_art(&t)
    }
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
