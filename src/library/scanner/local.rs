use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use chrono::{DateTime, Utc};
use lofty::prelude::{Accessor, TaggedFileExt};
use lofty::probe::Probe;
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use walkdir::WalkDir;

use super::Catalog;
use crate::library::probe::{probe_audio, read_embedded_art};
use crate::library::store::{CachedLocalFile, LibraryStore};
use crate::library::types::{
    Album, LibraryError, Result, SOURCE_LOCAL, SOURCE_TYPE_LOCAL, Track,
    format_name_from_mime_or_ext, is_lossless_format, mime_for_ext, normalize_isrc, short_hash,
    short_hash_bytes,
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

struct PendingFile {
    path: PathBuf,
    size: u64,
    mtime_ns: i64,
    added_at: DateTime<Utc>,
}

impl Catalog {
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
        if wait_notify.is_none() {
            self.notify_change();
        }

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

        struct ScanGuard<'a> {
            state: &'a std::sync::Mutex<super::ScanState>,
            armed: bool,
        }
        impl Drop for ScanGuard<'_> {
            fn drop(&mut self) {
                if !self.armed {
                    return;
                }
                let notify = {
                    let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    st.scanning = false;
                    st.last_err = Some("scan cancelled".into());
                    st.notify.take()
                };
                if let Some(n) = notify {
                    n.notify_waiters();
                }
            }
        }
        let mut guard = ScanGuard {
            state: &self.scan_state,
            armed: true,
        };

        let roots = self.roots();
        let store = Arc::clone(&self.store);
        let res = tokio::task::spawn_blocking(move || scan_roots_blocking(&store, &roots))
            .await
            .map_err(|e| LibraryError::Remote(format!("scan task join: {e}")))?;

        guard.armed = false;
        let notify = {
            let mut st = self.scan_state.lock().unwrap_or_else(|e| e.into_inner());
            st.scanning = false;
            st.last_err = res.as_ref().err().map(ToString::to_string);
            st.notify.take()
        };
        if let Some(n) = notify {
            n.notify_waiters();
        }
        self.notify_change();

        if let Ok((count, _changed)) = &res {
            return Ok(*count);
        }
        Err(res.unwrap_err())
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
    let (reused, to_probe, seen_paths) = collect_audio_files(roots, &file_cache);
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
        if let Some(v) = dir_art_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(dir)
        {
            return v.clone();
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
    for pf in to_probe {
        if let Some(tr) = scan_file_with_art(&pf, &lookup_dir_art) {
            all_tracks.push(tr);
        }
    }

    // Sort deterministically before grouping into albums so album metadata
    // (year, genre, artwork, added_at) does not depend on filesystem walk order.
    all_tracks.sort_by(|a, b| {
        a.album_id
            .cmp(&b.album_id)
            .then_with(|| a.disc_number.cmp(&b.disc_number))
            .then_with(|| a.track_number.cmp(&b.track_number))
            .then_with(|| a.file_path.cmp(&b.file_path))
    });

    // Re-verify all roots are still mounted after walking so a mid-scan NAS unmount
    // cannot trigger soft-deletion of the existing local library.
    for root in roots {
        if let Err(e) = std::fs::read_dir(root) {
            tracing::warn!(root = %root.display(), error = %e, "local music root disappeared during scan; preserving existing library");
            return Ok((0, false));
        }
    }

    let albums = build_albums_from_tracks(&all_tracks);
    let count = all_tracks.len();
    store.replace_provider(SOURCE_LOCAL, SOURCE_TYPE_LOCAL, &albums, &all_tracks, true)?;
    Ok((count, true))
}

fn collect_audio_files(
    roots: &[PathBuf],
    file_cache: &HashMap<String, CachedLocalFile>,
) -> (Vec<Track>, Vec<PendingFile>, HashSet<String>) {
    let mut reused = Vec::new();
    let mut to_probe = Vec::new();
    let mut seen_paths = HashSet::new();

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
                && !cached.track.artwork_url.starts_with("/media/art/local:")
            {
                reused.push(cached.track.clone());
                continue;
            }
            to_probe.push(PendingFile {
                path: path.to_path_buf(),
                size,
                mtime_ns,
                added_at: mtime.into(),
            });
        }
    }
    (reused, to_probe, seen_paths)
}

fn scan_file_with_art(
    pf: &PendingFile,
    lookup_dir_art: &impl Fn(&Path) -> (String, String),
) -> Option<Track> {
    let ext = pf
        .path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| format!(".{}", s.to_ascii_lowercase()))?;
    let mime_type = mime_for_ext(&ext)?;

    let clean_str = pf.path.to_string_lossy().to_string();
    let dir = pf.path.parent().unwrap_or_else(|| Path::new(""));
    let (folder_art, folder_art_hash) = lookup_dir_art(dir);
    let props = probe_audio(&pf.path);

    let mut t = Track {
        id: format!("local:{}", short_hash(&format!("local:{clean_str}"))),
        source: SOURCE_LOCAL.into(),
        source_type: SOURCE_TYPE_LOCAL.into(),
        provider: SOURCE_LOCAL.into(),
        external_id: clean_str.clone(),
        title: pf
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Unknown")
            .to_string(),
        artist: "Unknown Artist".into(),
        album: dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("Unknown Album")
            .to_string(),
        duration_sec: props.duration.as_secs_f64().round() as i32,
        sample_rate: props.sample_rate,
        bit_depth: props.bit_depth,
        channels: props.channels,
        mime: mime_type.into(),
        ext,
        size: pf.size as i64,
        file_path: clean_str,
        file_mtime_ns: pf.mtime_ns,
        folder_art,
        added_at: Some(pf.added_at),
        available: true,
        in_library: true,
        ..Default::default()
    };

    let embed_art_hash = apply_lofty_tags(&pf.path, &mut t);
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

fn apply_lofty_tags(path: &Path, t: &mut Track) -> String {
    let Ok(probe) = Probe::open(path) else {
        return String::new();
    };
    let Ok(tagged) = probe.read() else {
        return String::new();
    };
    let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) else {
        return String::new();
    };

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
    apply_extended_tags(tag, t);
    if let Some(pic) = tag.pictures().first()
        && !pic.data().is_empty()
        && pic.data().len() <= MAX_COVER_ART_BYTES
    {
        t.has_embed_art = true;
        return short_hash_bytes(pic.data());
    }
    String::new()
}

fn tag_str(tag: &lofty::tag::Tag, key: &lofty::tag::ItemKey) -> Option<String> {
    tag.get_string(key)
        .map(|v| v.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn parse_replaygain_db(s: &str) -> Option<f64> {
    let trimmed = s
        .trim()
        .trim_end_matches("dB")
        .trim_end_matches("db")
        .trim_end_matches("DB")
        .trim();
    let v: f64 = trimmed.parse().ok()?;
    v.is_finite().then_some(v)
}

fn parse_replaygain_peak(s: &str) -> Option<f64> {
    let v: f64 = s.trim().parse().ok()?;
    (v.is_finite() && v >= 0.0).then_some(v)
}

fn apply_extended_tags(tag: &lofty::tag::Tag, t: &mut Track) {
    use lofty::tag::ItemKey;

    if let Some(val) = tag_str(tag, &ItemKey::Isrc) {
        let norm = normalize_isrc(&val);
        if !norm.is_empty() {
            t.isrc = norm;
        }
    }
    if let Some(s) = tag_str(tag, &ItemKey::Composer) {
        t.composer = s;
    }
    if let Some(s) = tag_str(tag, &ItemKey::ReplayGainTrackGain) {
        t.replaygain_gain = parse_replaygain_db(&s);
    }
    if let Some(s) = tag_str(tag, &ItemKey::ReplayGainTrackPeak) {
        t.replaygain_peak = parse_replaygain_peak(&s);
    }
    if let Some(s) = tag_str(tag, &ItemKey::MusicBrainzRecordingId)
        .or_else(|| tag_str(tag, &ItemKey::MusicBrainzTrackId))
    {
        t.mb_recording_id = s;
    }
    if let Some(s) = tag_str(tag, &ItemKey::MusicBrainzArtistId) {
        t.mb_artist_id = s;
    }
    if let Some(s) = tag_str(tag, &ItemKey::Label).or_else(|| tag_str(tag, &ItemKey::Publisher)) {
        t.scan_label = s;
    }
    if let Some(s) = tag_str(tag, &ItemKey::Barcode) {
        t.scan_upc = s;
    }
    if let Some(s) = tag_str(tag, &ItemKey::OriginalReleaseDate)
        .or_else(|| tag_str(tag, &ItemKey::RecordingDate))
    {
        t.scan_release_date = s;
    }
    if let Some(s) = tag_str(tag, &ItemKey::MusicBrainzReleaseId) {
        t.scan_mb_release_id = s;
    }
    if let Some(s) = tag_str(tag, &ItemKey::MusicBrainzReleaseGroupId) {
        t.scan_mb_release_group_id = s;
    }
    if let Some(s) = tag_str(tag, &ItemKey::MusicBrainzReleaseArtistId) {
        t.scan_mb_album_artist_id = s;
    }
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
            external_id: t
                .album_id
                .strip_prefix("local:")
                .unwrap_or(&t.album_id)
                .to_string(),
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
        if entry.label.is_empty() && !t.scan_label.is_empty() {
            entry.label = t.scan_label.clone();
        }
        if entry.upc.is_empty() && !t.scan_upc.is_empty() {
            entry.upc = t.scan_upc.clone();
        }
        if entry.release_date.is_empty() && !t.scan_release_date.is_empty() {
            entry.release_date = t.scan_release_date.clone();
        }
        if entry.mb_release_id.is_empty() && !t.scan_mb_release_id.is_empty() {
            entry.mb_release_id = t.scan_mb_release_id.clone();
        }
        if entry.mb_release_group_id.is_empty() && !t.scan_mb_release_group_id.is_empty() {
            entry.mb_release_group_id = t.scan_mb_release_group_id.clone();
        }
        if entry.mb_artist_id.is_empty() {
            if !t.scan_mb_album_artist_id.is_empty() {
                entry.mb_artist_id = t.scan_mb_album_artist_id.clone();
            } else if !t.mb_artist_id.is_empty() {
                entry.mb_artist_id = t.mb_artist_id.clone();
            }
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

pub(super) fn load_track_cover_art(t: &Track) -> Option<(Vec<u8>, String)> {
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
