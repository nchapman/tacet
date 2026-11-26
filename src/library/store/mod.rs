mod ingest;
mod prefs;
mod query;
mod versions;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use rand::RngCore;
use rusqlite::{Connection, params};

use super::migrations::run_migrations;
use super::types::{LibraryError, ProviderSyncStatus, Result, Track};

pub use query::build_fts_query;

pub const DEFAULT_MISSING_RETENTION: Duration = Duration::from_secs(30 * 24 * 3600);

#[derive(Debug, Clone)]
pub struct CachedLocalFile {
    pub track: Track,
    pub size: i64,
    pub mtime_ns: i64,
}

#[derive(Debug, Default)]
pub struct StoreCounts {
    pub boot_id: String,
    pub generation: u64,
    pub total_tracks: i32,
    pub total_albums: i32,
    pub local_tracks: i32,
    pub local_albums: i32,
    pub remote_tracks: i32,
    pub remote_albums: i32,
    pub missing_tracks: i32,
    pub missing_albums: i32,
    pub states: HashMap<String, ProviderSyncStatus>,
}

pub(super) struct StoreInner {
    pub(super) conn: Option<Connection>,
    pub(super) generation: u64,
    pub(super) catalog_generation: u64,
    pub(super) missing_retention: Duration,
}

pub struct LibraryStore {
    pub(super) inner: Mutex<StoreInner>,
    pub(super) boot_id: String,
}

impl LibraryStore {
    pub fn open(db_path: &str) -> Result<Self> {
        let trimmed = db_path.trim();
        let is_memory = trimmed.is_empty() || trimmed == ":memory:";

        let mut conn = if is_memory {
            Connection::open_in_memory()?
        } else {
            let path = Path::new(trimmed);
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ =
                        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
                }
            }
            Connection::open(path)?
        };

        let mut pragmas = vec![
            "PRAGMA foreign_keys = ON;",
            "PRAGMA busy_timeout = 5000;",
            "PRAGMA synchronous = NORMAL;",
        ];
        if !is_memory {
            pragmas.insert(0, "PRAGMA journal_mode = WAL;");
        }
        for p in pragmas {
            conn.execute_batch(p)?;
        }

        run_migrations(&mut conn)?;

        #[cfg(unix)]
        if !is_memory {
            use std::os::unix::fs::PermissionsExt;
            for suffix in ["", "-wal", "-shm"] {
                let p = format!("{trimmed}{suffix}");
                if Path::new(&p).exists() {
                    let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
                }
            }
        }

        let mut nonce = [0u8; 4];
        rand::rng().fill_bytes(&mut nonce);

        Ok(Self {
            inner: Mutex::new(StoreInner {
                conn: Some(conn),
                generation: 0,
                catalog_generation: 0,
                missing_retention: DEFAULT_MISSING_RETENTION,
            }),
            boot_id: hex::encode(nonce),
        })
    }

    pub fn close(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.conn.take();
    }

    pub fn set_missing_retention(&self, d: Duration) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.missing_retention = d;
    }

    pub fn schema_version(&self) -> Result<i32> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let v: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        Ok(v)
    }

    pub fn catalog_generation(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .catalog_generation
    }

    pub fn counts_and_providers(&self) -> StoreCounts {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = StoreCounts {
            boot_id: self.boot_id.clone(),
            generation: inner.generation,
            ..Default::default()
        };
        let Some(conn) = inner.conn.as_ref() else {
            return out;
        };

        let count_i32 = |sql: &str| -> i32 { conn.query_row(sql, [], |r| r.get(0)).unwrap_or(0) };
        out.total_tracks = count_i32("SELECT COUNT(*) FROM tracks WHERE available = 1");
        out.total_albums = count_i32("SELECT COUNT(*) FROM albums WHERE available = 1");
        out.local_tracks =
            count_i32("SELECT COUNT(*) FROM tracks WHERE source_type = 'local' AND available = 1");
        out.local_albums =
            count_i32("SELECT COUNT(*) FROM albums WHERE source_type = 'local' AND available = 1");
        out.remote_tracks =
            count_i32("SELECT COUNT(*) FROM tracks WHERE source_type = 'remote' AND available = 1");
        out.remote_albums =
            count_i32("SELECT COUNT(*) FROM albums WHERE source_type = 'remote' AND available = 1");
        out.missing_tracks =
            count_i32("SELECT COUNT(*) FROM tracks WHERE available = 0 AND missing_since > 0");
        out.missing_albums =
            count_i32("SELECT COUNT(*) FROM albums WHERE available = 0 AND missing_since > 0");

        if let Ok(mut stmt) = conn
            .prepare("SELECT provider, source_type, last_sync_at, last_error FROM provider_state")
            && let Ok(rows) = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })
        {
            for row in rows.flatten() {
                let (provider, source_type, last_sync_unix, last_error) = row;
                let albums: i32 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM albums WHERE provider = ?1 AND available = 1",
                        params![provider],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                let tracks: i32 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM tracks WHERE provider = ?1 AND available = 1",
                        params![provider],
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                out.states.insert(
                    provider.clone(),
                    ProviderSyncStatus {
                        provider,
                        source_type,
                        connected: false,
                        syncing: false,
                        albums,
                        tracks,
                        last_sync_at: unix_to_dt(last_sync_unix),
                        last_error,
                    },
                );
            }
        }
        out
    }
}

pub(super) const TRACK_SELECT_COLS: &str = "id, album_id, source_type, provider, external_id, title, artist, album_artist, \
    album, genre, disc_number, track_number, year, duration_sec, sample_rate, \
    bit_depth, channels, mime, ext, size_bytes, artwork_url, file_path, \
    file_mtime_ns, folder_art, has_embed_art, qobuz_track_id, play_count, \
    last_played_at, loved, available, added_at, \
    isrc, mb_recording_id, mb_artist_id, qobuz_artist_id, qobuz_genre_id, qobuz_label_id, \
    replaygain_gain, replaygain_peak, composer, performers";

pub(super) const TRACK_SELECT_COLS_T: &str = "t.id, t.album_id, t.source_type, t.provider, t.external_id, t.title, t.artist, t.album_artist, \
    t.album, t.genre, t.disc_number, t.track_number, t.year, t.duration_sec, t.sample_rate, \
    t.bit_depth, t.channels, t.mime, t.ext, t.size_bytes, t.artwork_url, t.file_path, \
    t.file_mtime_ns, t.folder_art, t.has_embed_art, t.qobuz_track_id, t.play_count, \
    t.last_played_at, t.loved, t.available, t.added_at, \
    t.isrc, t.mb_recording_id, t.mb_artist_id, t.qobuz_artist_id, t.qobuz_genre_id, t.qobuz_label_id, \
    t.replaygain_gain, t.replaygain_peak, t.composer, t.performers";

pub(super) fn row_to_track(r: &rusqlite::Row<'_>) -> rusqlite::Result<Track> {
    let provider: String = r.get(3)?;
    Ok(Track {
        id: r.get(0)?,
        album_id: r.get(1)?,
        source_type: r.get(2)?,
        source: provider.clone(),
        provider,
        external_id: r.get(4)?,
        title: r.get(5)?,
        artist: r.get(6)?,
        album_artist: r.get(7)?,
        album: r.get(8)?,
        genre: r.get(9)?,
        disc_number: r.get(10)?,
        track_number: r.get(11)?,
        year: r.get(12)?,
        duration_sec: r.get(13)?,
        sample_rate: r.get(14)?,
        bit_depth: r.get(15)?,
        channels: r.get(16)?,
        mime: r.get(17)?,
        ext: r.get(18)?,
        size: r.get(19)?,
        artwork_url: r.get(20)?,
        file_path: r.get(21)?,
        file_mtime_ns: r.get(22)?,
        folder_art: r.get(23)?,
        has_embed_art: r.get::<_, i32>(24)? != 0,
        qobuz_track_id: r.get(25)?,
        play_count: r.get(26)?,
        last_played_at: unix_to_dt(r.get(27)?),
        loved: r.get::<_, i32>(28)? != 0,
        available: r.get::<_, i32>(29)? != 0,
        added_at: unix_to_dt(r.get(30)?),
        isrc: r.get(31)?,
        mb_recording_id: r.get(32)?,
        mb_artist_id: r.get(33)?,
        qobuz_artist_id: r.get(34)?,
        qobuz_genre_id: r.get(35)?,
        qobuz_label_id: r.get(36)?,
        replaygain_gain: r.get(37)?,
        replaygain_peak: r.get(38)?,
        composer: r.get(39)?,
        performers: r.get(40)?,
        in_library: true,
        explicit: false,
        ..Default::default()
    })
}

pub(super) fn unix_to_dt(ts: i64) -> Option<DateTime<Utc>> {
    if ts > 0 {
        Utc.timestamp_opt(ts, 0).single()
    } else {
        None
    }
}
