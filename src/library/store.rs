use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use rand::RngCore;
use rusqlite::{Connection, OptionalExtension, Transaction, params, types::Value};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::migrations::run_migrations;
use super::types::{
    Album, AlbumFilter, AlbumVersion, ArtistSummary, FacetFilter, GenreSummary, HeavyRotationAlbum,
    LibraryError, ProviderSyncStatus, Result, SOURCE_LOCAL, SOURCE_TYPE_LOCAL, TopSearchResult,
    Track, TrackFilter, UnifiedSearchResult, format_name_from_mime_or_ext, is_lossless_format,
    quality_rank, short_hash, sort_key, version_key,
};

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

struct StoreInner {
    conn: Option<Connection>,
    generation: u64,
    missing_retention: Duration,
}

pub struct LibraryStore {
    inner: Mutex<StoreInner>,
    boot_id: String,
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

    pub fn load_local_file_cache(&self) -> Result<HashMap<String, CachedLocalFile>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let mut stmt = conn.prepare(
            "SELECT id, album_id, source_type, provider, external_id, title, artist, album_artist,
                    album, genre, disc_number, track_number, year, duration_sec, sample_rate,
                    bit_depth, channels, mime, ext, size_bytes, artwork_url, file_path,
                    file_mtime_ns, folder_art, has_embed_art, qobuz_track_id, play_count,
                    last_played_at, loved, available, added_at
             FROM tracks WHERE provider = ?1 AND file_path != ''",
        )?;
        let rows = stmt.query_map(params![SOURCE_LOCAL], row_to_track)?;
        let mut out = HashMap::new();
        for r in rows {
            let t = r?;
            let key = t.file_path.clone();
            out.insert(
                key,
                CachedLocalFile {
                    size: t.size,
                    mtime_ns: t.file_mtime_ns,
                    track: t,
                },
            );
        }
        Ok(out)
    }

    pub fn touch_provider_sync(
        &self,
        provider: &str,
        source_type: &str,
        last_error: &str,
    ) -> Result<()> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let now = Utc::now().timestamp();
        conn.execute(
            "INSERT INTO provider_state(provider, source_type, last_sync_at, last_error)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(provider) DO UPDATE SET
               source_type = excluded.source_type,
               last_sync_at = excluded.last_sync_at,
               last_error = excluded.last_error",
            params![provider, source_type, now, last_error],
        )?;
        Ok(())
    }

    pub fn replace_provider(
        &self,
        provider: &str,
        source_type: &str,
        albums: &[Album],
        tracks: &[Track],
        soft_delete_missing: bool,
    ) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let retention_secs = inner.missing_retention.as_secs() as i64;
        let conn = inner.conn.as_mut().ok_or(LibraryError::NotFound)?;
        let now = Utc::now().timestamp();

        let tx = conn.transaction()?;

        let mut seen_albums = HashSet::with_capacity(albums.len());
        for a in albums {
            seen_albums.insert(a.id.clone());
            upsert_album_row(&tx, a, provider, source_type, now)?;
        }

        let mut seen_tracks = HashSet::with_capacity(tracks.len());
        for t in tracks {
            seen_tracks.insert(t.id.clone());
            upsert_track_row(&tx, t, provider, source_type, now)?;
        }

        // Handle tracks no longer present in provider scan/sync.
        let mut existing_tracks: Vec<(String, i64)> = Vec::new();
        {
            let mut stmt =
                tx.prepare("SELECT id, missing_since FROM tracks WHERE provider = ?1")?;
            let rows = stmt.query_map(params![provider], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?;
            for r in rows {
                existing_tracks.push(r?);
            }
        }
        for (tid, missing_since) in existing_tracks {
            if seen_tracks.contains(&tid) {
                continue;
            }
            if !soft_delete_missing {
                tx.execute("DELETE FROM tracks WHERE id = ?1", params![tid])?;
            } else if missing_since == 0 {
                tx.execute(
                    "UPDATE tracks SET available = 0, missing_since = ?1, updated_at = ?1 WHERE id = ?2",
                    params![now, tid],
                )?;
            } else if retention_secs > 0 && now - missing_since >= retention_secs {
                tx.execute("DELETE FROM tracks WHERE id = ?1", params![tid])?;
            }
        }

        // Handle albums no longer present in provider scan/sync.
        let mut existing_albums: Vec<(String, i64)> = Vec::new();
        {
            let mut stmt =
                tx.prepare("SELECT id, missing_since FROM albums WHERE provider = ?1")?;
            let rows = stmt.query_map(params![provider], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?;
            for r in rows {
                existing_albums.push(r?);
            }
        }
        for (aid, missing_since) in existing_albums {
            if seen_albums.contains(&aid) {
                continue;
            }
            if !soft_delete_missing {
                tx.execute("DELETE FROM albums WHERE id = ?1", params![aid])?;
            } else if missing_since == 0 {
                tx.execute(
                    "UPDATE albums SET available = 0, missing_since = ?1, updated_at = ?1 WHERE id = ?2",
                    params![now, aid],
                )?;
            } else if retention_secs > 0 && now - missing_since >= retention_secs {
                tx.execute("DELETE FROM albums WHERE id = ?1", params![aid])?;
            }
        }

        // Rebuild FTS5 entries for this provider cleanly.
        tx.execute(
            "DELETE FROM albums_fts WHERE album_id IN (SELECT id FROM albums WHERE provider = ?1)",
            params![provider],
        )?;
        // Also remove any orphaned FTS entries whose albums were purged.
        tx.execute(
            "DELETE FROM albums_fts WHERE album_id NOT IN (SELECT id FROM albums)",
            [],
        )?;
        tx.execute(
            "INSERT INTO albums_fts(album_id, title, artist, genre)
             SELECT id, title, artist, genre FROM albums WHERE provider = ?1",
            params![provider],
        )?;

        tx.execute(
            "DELETE FROM tracks_fts WHERE track_id IN (SELECT id FROM tracks WHERE provider = ?1)",
            params![provider],
        )?;
        tx.execute(
            "DELETE FROM tracks_fts WHERE track_id NOT IN (SELECT id FROM tracks)",
            [],
        )?;
        tx.execute(
            "INSERT INTO tracks_fts(track_id, title, artist, album_artist, album, genre)
             SELECT id, title, artist, album_artist, album, genre FROM tracks WHERE provider = ?1",
            params![provider],
        )?;

        tx.execute(
            "INSERT INTO provider_state(provider, source_type, last_sync_at, last_error)
             VALUES (?1, ?2, ?3, '')
             ON CONFLICT(provider) DO UPDATE SET
               source_type = excluded.source_type,
               last_sync_at = excluded.last_sync_at,
               last_error = ''",
            params![provider, source_type, now],
        )?;

        recompute_versions_tx(&tx)?;
        tx.commit()?;
        inner.generation += 1;
        Ok(())
    }

    pub fn upsert_album_with_tracks(&self, album: &Album, tracks: &[Track]) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_mut().ok_or(LibraryError::NotFound)?;
        let now = Utc::now().timestamp();
        let tx = conn.transaction()?;

        let provider = if album.provider.is_empty() {
            album.source.as_str()
        } else {
            album.provider.as_str()
        };
        let source_type = if album.source_type.is_empty() {
            if provider == SOURCE_LOCAL {
                SOURCE_TYPE_LOCAL
            } else {
                "remote"
            }
        } else {
            album.source_type.as_str()
        };

        upsert_album_row(&tx, album, provider, source_type, now)?;
        tx.execute(
            "DELETE FROM albums_fts WHERE album_id = ?1",
            params![album.id],
        )?;
        tx.execute(
            "INSERT INTO albums_fts(album_id, title, artist, genre) VALUES (?1, ?2, ?3, ?4)",
            params![album.id, album.title, album.artist, album.genre],
        )?;

        for t in tracks {
            upsert_track_row(&tx, t, provider, source_type, now)?;
            tx.execute("DELETE FROM tracks_fts WHERE track_id = ?1", params![t.id])?;
            tx.execute(
                "INSERT INTO tracks_fts(track_id, title, artist, album_artist, album, genre)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![t.id, t.title, t.artist, t.album_artist, t.album, t.genre],
            )?;
        }

        if !tracks.is_empty() {
            refresh_album_aggregates_tx(&tx, &album.id)?;
        }
        recompute_versions_tx(&tx)?;
        tx.commit()?;
        inner.generation += 1;
        Ok(())
    }

    pub fn delete_album(&self, album_id: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_mut().ok_or(LibraryError::NotFound)?;
        let tx = conn.transaction()?;

        tx.execute(
            "DELETE FROM tracks_fts WHERE track_id IN (SELECT id FROM tracks WHERE album_id = ?1)",
            params![album_id],
        )?;
        tx.execute(
            "DELETE FROM albums_fts WHERE album_id = ?1",
            params![album_id],
        )?;
        let n = tx.execute("DELETE FROM albums WHERE id = ?1", params![album_id])?;
        if n == 0 {
            return Err(LibraryError::NotFound);
        }
        recompute_versions_tx(&tx)?;
        tx.commit()?;
        inner.generation += 1;
        Ok(())
    }

    pub fn delete_track(&self, track_id: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_mut().ok_or(LibraryError::NotFound)?;
        let tx = conn.transaction()?;

        let album_id: Option<String> = tx
            .query_row(
                "SELECT album_id FROM tracks WHERE id = ?1",
                params![track_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(album_id) = album_id else {
            return Err(LibraryError::NotFound);
        };

        tx.execute(
            "DELETE FROM tracks_fts WHERE track_id = ?1",
            params![track_id],
        )?;
        tx.execute("DELETE FROM tracks WHERE id = ?1", params![track_id])?;

        let remaining: i32 = tx.query_row(
            "SELECT COUNT(*) FROM tracks WHERE album_id = ?1",
            params![album_id],
            |r| r.get(0),
        )?;
        if remaining == 0 {
            tx.execute(
                "DELETE FROM albums_fts WHERE album_id = ?1",
                params![album_id],
            )?;
            tx.execute("DELETE FROM albums WHERE id = ?1", params![album_id])?;
        } else {
            refresh_album_aggregates_tx(&tx, &album_id)?;
        }

        recompute_versions_tx(&tx)?;
        tx.commit()?;
        inner.generation += 1;
        Ok(())
    }

    pub fn set_provider_available(&self, provider: &str, available: bool) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_mut().ok_or(LibraryError::NotFound)?;
        let tx = conn.transaction()?;
        let av = if available { 1 } else { 0 };
        tx.execute(
            "UPDATE albums SET available = ?1 WHERE provider = ?2",
            params![av, provider],
        )?;
        tx.execute(
            "UPDATE tracks SET available = ?1 WHERE provider = ?2",
            params![av, provider],
        )?;
        recompute_versions_tx(&tx)?;
        tx.commit()?;
        inner.generation += 1;
        Ok(())
    }

    pub fn set_primary_version(&self, album_id: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_mut().ok_or(LibraryError::NotFound)?;
        let tx = conn.transaction()?;

        let group_id: Option<String> = tx
            .query_row(
                "SELECT version_group_id FROM albums WHERE id = ?1",
                params![album_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(group_id) = group_id else {
            return Err(LibraryError::NotFound);
        };
        if !group_id.is_empty() {
            tx.execute(
                "UPDATE albums SET is_primary_override = 0 WHERE version_group_id = ?1",
                params![group_id],
            )?;
        }
        tx.execute(
            "UPDATE albums SET is_primary_override = 1 WHERE id = ?1",
            params![album_id],
        )?;
        recompute_versions_tx(&tx)?;
        tx.commit()?;
        inner.generation += 1;
        Ok(())
    }

    pub fn group_versions(&self, album_ids: &[String]) -> Result<String> {
        let mut unique: Vec<String> = Vec::new();
        let mut seen = HashSet::new();
        for id in album_ids {
            let trimmed = id.trim();
            if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
                unique.push(trimmed.to_string());
            }
        }
        if unique.len() < 2 {
            return Err(LibraryError::InvalidArgument(
                "at least 2 distinct albumIds are required".into(),
            ));
        }
        unique.sort();

        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_mut().ok_or(LibraryError::NotFound)?;
        let tx = conn.transaction()?;

        let group_id = format!(
            "manual:{}",
            short_hash(&format!("manual:{}", unique.join(",")))
        );
        for id in &unique {
            let n = tx.execute(
                "UPDATE albums SET version_group_id = ?1, version_excluded = 0 WHERE id = ?2",
                params![group_id, id],
            )?;
            if n == 0 {
                return Err(LibraryError::NotFound);
            }
        }
        recompute_versions_tx(&tx)?;
        tx.commit()?;
        inner.generation += 1;
        Ok(group_id)
    }

    pub fn ungroup_version(&self, album_id: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_mut().ok_or(LibraryError::NotFound)?;
        let tx = conn.transaction()?;

        let n = tx.execute(
            "UPDATE albums SET version_group_id = '', version_excluded = 1, is_primary_override = 0 WHERE id = ?1",
            params![album_id],
        )?;
        if n == 0 {
            return Err(LibraryError::NotFound);
        }
        recompute_versions_tx(&tx)?;
        tx.commit()?;
        inner.generation += 1;
        Ok(())
    }

    pub fn record_play(&self, track: &Track) -> Result<()> {
        if track.id.trim().is_empty() {
            return Ok(());
        }
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_mut().ok_or(LibraryError::NotFound)?;
        let now = Utc::now().timestamp();
        let tx = conn.transaction()?;

        let provider = if track.provider.is_empty() {
            track.source.as_str()
        } else {
            track.provider.as_str()
        };
        let album_id = if track.album_id.is_empty() {
            tx.query_row(
                "SELECT album_id FROM tracks WHERE id = ?1",
                params![track.id],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .unwrap_or_default()
        } else {
            track.album_id.clone()
        };

        tx.execute(
            "UPDATE tracks SET play_count = play_count + 1, last_played_at = ?1 WHERE id = ?2",
            params![now, track.id],
        )?;
        tx.execute(
            "INSERT INTO play_events(track_id, album_id, provider, played_at) VALUES (?1, ?2, ?3, ?4)",
            params![track.id, album_id, provider, now],
        )?;
        tx.commit()?;
        inner.generation += 1;
        Ok(())
    }

    pub fn heavy_rotation(&self, mut days: i32, mut limit: i32) -> Result<Vec<HeavyRotationAlbum>> {
        if days <= 0 {
            days = 90;
        }
        if limit <= 0 || limit > 100 {
            limit = 20;
        }
        let now = Utc::now().timestamp();
        let since = now - i64::from(days) * 86_400;
        let cut4d = now - 4 * 86_400;
        let cut14d = now - 14 * 86_400;
        let cut30d = now - 30 * 86_400;

        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;

        let mut stmt = conn.prepare(
            "SELECT a.id, a.source_type, a.provider, a.external_id, a.title, a.artist, a.genre, a.year,
                    a.track_count, a.duration_sec, a.artwork_url, a.primary_format,
                    a.max_bit_depth, a.max_sample_rate, a.version_group_id, a.is_primary,
                    a.is_partial, a.available, a.added_at,
                    COUNT(pe.id) AS recent_plays,
                    MAX(pe.played_at) AS last_played,
                    SUM(CASE
                      WHEN pe.played_at >= ?1 THEN 100
                      WHEN pe.played_at >= ?2 THEN 70
                      WHEN pe.played_at >= ?3 THEN 50
                      ELSE 20
                    END) AS frecency_score
             FROM play_events pe
             JOIN albums a ON a.id = pe.album_id
             WHERE pe.played_at >= ?4 AND a.available = 1
             GROUP BY a.id
             ORDER BY frecency_score DESC, last_played DESC
             LIMIT ?5",
        )?;

        let rows = stmt.query_map(params![cut4d, cut14d, cut30d, since, limit], |r| {
            let provider: String = r.get(2)?;
            let added_unix: i64 = r.get(18)?;
            let last_played_unix: i64 = r.get(20)?;
            let album = Album {
                id: r.get(0)?,
                source: provider.clone(),
                source_type: r.get(1)?,
                provider,
                external_id: r.get(3)?,
                title: r.get(4)?,
                artist: r.get(5)?,
                genre: r.get(6)?,
                year: r.get(7)?,
                track_count: r.get(8)?,
                duration_sec: r.get(9)?,
                artwork_url: r.get(10)?,
                primary_format: r.get(11)?,
                max_bit_depth: r.get(12)?,
                max_sample_rate: r.get(13)?,
                version_group_id: r.get(14)?,
                is_primary: r.get::<_, i32>(15)? != 0,
                is_partial: r.get::<_, i32>(16)? != 0,
                available: r.get::<_, i32>(17)? != 0,
                added_at: unix_to_dt(added_unix),
                in_library: true,
                ..Default::default()
            };
            Ok(HeavyRotationAlbum {
                album,
                recent_plays: r.get(19)?,
                last_played: unix_to_dt(last_played_unix),
            })
        })?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn query_albums(&self, f: &AlbumFilter) -> Result<(Vec<Album>, i32)> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        query_albums_conn(conn, f)
    }

    pub fn get_album(&self, id: &str) -> Result<Option<Album>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;

        let row: Option<Album> = conn
            .query_row(
                "SELECT a.id, a.source_type, a.provider, a.external_id, a.title, a.artist, a.genre, a.year,
                        a.track_count, a.duration_sec, a.artwork_url, a.primary_format,
                        a.max_bit_depth, a.max_sample_rate, a.version_group_id, a.is_primary,
                        a.is_partial, a.available, a.added_at,
                        COALESCE((SELECT SUM(t.play_count) FROM tracks t WHERE t.album_id = a.id), 0)
                 FROM albums a WHERE a.id = ?1",
                params![id],
                |r| {
                    let provider: String = r.get(2)?;
                    Ok(Album {
                        id: r.get(0)?,
                        source: provider.clone(),
                        source_type: r.get(1)?,
                        provider,
                        external_id: r.get(3)?,
                        title: r.get(4)?,
                        artist: r.get(5)?,
                        genre: r.get(6)?,
                        year: r.get(7)?,
                        track_count: r.get(8)?,
                        duration_sec: r.get(9)?,
                        artwork_url: r.get(10)?,
                        primary_format: r.get(11)?,
                        max_bit_depth: r.get(12)?,
                        max_sample_rate: r.get(13)?,
                        version_group_id: r.get(14)?,
                        is_primary: r.get::<_, i32>(15)? != 0,
                        is_partial: r.get::<_, i32>(16)? != 0,
                        available: r.get::<_, i32>(17)? != 0,
                        added_at: unix_to_dt(r.get(18)?),
                        play_count: r.get(19)?,
                        in_library: true,
                        ..Default::default()
                    })
                },
            )
            .optional()?;

        let Some(mut album) = row else {
            return Ok(None);
        };

        // Load tracks in disc/track order.
        {
            let mut stmt = conn.prepare(
                "SELECT id, album_id, source_type, provider, external_id, title, artist, album_artist,
                        album, genre, disc_number, track_number, year, duration_sec, sample_rate,
                        bit_depth, channels, mime, ext, size_bytes, artwork_url, file_path,
                        file_mtime_ns, folder_art, has_embed_art, qobuz_track_id, play_count,
                        last_played_at, loved, available, added_at
                 FROM tracks WHERE album_id = ?1
                 ORDER BY disc_number ASC, track_number ASC, sort_title ASC, id ASC",
            )?;
            let rows = stmt.query_map(params![album.id], row_to_track)?;
            for r in rows {
                album.tracks.push(r?);
            }
        }

        // Load sibling versions if part of a version group.
        if !album.version_group_id.is_empty() {
            let mut stmt = conn.prepare(
                "SELECT id, source_type, provider, title, year, track_count,
                        primary_format, max_sample_rate, max_bit_depth, is_primary, available
                 FROM albums WHERE version_group_id = ?1
                 ORDER BY is_primary DESC, max_bit_depth DESC, max_sample_rate DESC, id ASC",
            )?;
            let rows = stmt.query_map(params![album.version_group_id], |r| {
                let provider: String = r.get(2)?;
                Ok(AlbumVersion {
                    id: r.get(0)?,
                    source: provider.clone(),
                    source_type: r.get(1)?,
                    provider,
                    title: r.get(3)?,
                    year: r.get(4)?,
                    track_count: r.get(5)?,
                    primary_format: r.get(6)?,
                    max_sample_rate: r.get(7)?,
                    max_bit_depth: r.get(8)?,
                    is_primary: r.get::<_, i32>(9)? != 0,
                    available: r.get::<_, i32>(10)? != 0,
                })
            })?;
            for r in rows {
                album.versions.push(r?);
            }
            album.version_count = album.versions.len() as i32;
        } else {
            album.version_count = 1;
        }

        Ok(Some(album))
    }

    pub fn has_album(&self, id: &str) -> bool {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(conn) = inner.conn.as_ref() else {
            return false;
        };
        conn.query_row(
            "SELECT 1 FROM albums WHERE id = ?1 AND available = 1",
            params![id],
            |_| Ok(()),
        )
        .optional()
        .ok()
        .flatten()
        .is_some()
    }

    pub fn query_tracks(&self, f: &TrackFilter) -> Result<(Vec<Track>, i32)> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        query_tracks_conn(conn, f)
    }

    pub fn get_track(&self, id: &str) -> Result<Option<Track>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let row = conn
            .query_row(
                "SELECT id, album_id, source_type, provider, external_id, title, artist, album_artist,
                        album, genre, disc_number, track_number, year, duration_sec, sample_rate,
                        bit_depth, channels, mime, ext, size_bytes, artwork_url, file_path,
                        file_mtime_ns, folder_art, has_embed_art, qobuz_track_id, play_count,
                        last_played_at, loved, available, added_at
                 FROM tracks WHERE id = ?1",
                params![id],
                row_to_track,
            )
            .optional()?;
        Ok(row)
    }

    pub fn get_track_by_artwork_url(&self, artwork_url: &str) -> Result<Option<Track>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let row = conn
            .query_row(
                "SELECT id, album_id, source_type, provider, external_id, title, artist, album_artist,
                        album, genre, disc_number, track_number, year, duration_sec, sample_rate,
                        bit_depth, channels, mime, ext, size_bytes, artwork_url, file_path,
                        file_mtime_ns, folder_art, has_embed_art, qobuz_track_id, play_count,
                        last_played_at, loved, available, added_at
                 FROM tracks WHERE artwork_url = ?1 AND available = 1 LIMIT 1",
                params![artwork_url],
                row_to_track,
            )
            .optional()?;
        Ok(row)
    }

    pub fn list_local_art_hashes(&self) -> Result<Vec<String>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let mut stmt = conn.prepare(
            "SELECT DISTINCT artwork_url FROM tracks
             WHERE provider = ?1 AND available = 1 AND artwork_url LIKE '/media/art/%'",
        )?;
        let rows = stmt.query_map(params![SOURCE_LOCAL], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            let url = r?;
            if let Some(hash) = url.strip_prefix("/media/art/")
                && hash.len() == 16
                && hash.chars().all(|c| c.is_ascii_hexdigit())
            {
                out.push(hash.to_string());
            }
        }
        Ok(out)
    }

    pub fn list_artists(&self, f: &FacetFilter) -> Result<(Vec<ArtistSummary>, i32)> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        list_artists_conn(conn, f)
    }

    pub fn list_genres(&self, f: &FacetFilter) -> Result<(Vec<GenreSummary>, i32)> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        list_genres_conn(conn, f)
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

    pub fn unified_search(&self, raw_query: &str, mut limit: i32) -> Result<UnifiedSearchResult> {
        let q = raw_query.trim().to_string();
        if limit <= 0 {
            limit = 8;
        }
        if limit > 50 {
            limit = 50;
        }
        let mut res = UnifiedSearchResult {
            query: q.clone(),
            top_result: None,
            artists: Vec::new(),
            albums: Vec::new(),
            tracks: Vec::new(),
        };
        if q.is_empty() {
            return Ok(res);
        }

        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;

        let candidate_limit = (limit * 4).max(32);
        let (artists, _) = list_artists_conn(
            conn,
            &FacetFilter {
                query: q.clone(),
                limit: candidate_limit,
                ..Default::default()
            },
        )?;
        let (albums, _) = query_albums_conn(
            conn,
            &AlbumFilter {
                query: q.clone(),
                group_versions: true,
                limit: candidate_limit,
                ..Default::default()
            },
        )?;
        let (tracks, _) = query_tracks_conn(
            conn,
            &TrackFilter {
                query: q.clone(),
                limit: candidate_limit,
                ..Default::default()
            },
        )?;

        let norm_q = normalize_search_text(&q);
        let q_tokens: Vec<&str> = norm_q.split_whitespace().collect();

        let mut scored_artists: Vec<(ArtistSummary, i32)> = artists
            .into_iter()
            .map(|a| {
                let mut sc = score_search_text(&a.name, &norm_q, &q_tokens);
                if sc > 0 {
                    if normalize_search_text(&a.name) == norm_q {
                        sc += 40;
                    }
                    sc += (a.album_count * 5).min(30);
                }
                (a, sc)
            })
            .collect();
        scored_artists.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| b.0.album_count.cmp(&a.0.album_count))
                .then_with(|| a.0.name.to_lowercase().cmp(&b.0.name.to_lowercase()))
        });

        let mut scored_albums: Vec<(Album, i32)> = albums
            .into_iter()
            .map(|a| {
                let title_sc = score_search_text(&a.title, &norm_q, &q_tokens);
                let combo_sc =
                    score_search_text(&format!("{} {}", a.artist, a.title), &norm_q, &q_tokens);
                let artist_sc = score_search_text(&a.artist, &norm_q, &q_tokens) / 3;
                let mut sc = title_sc.max(combo_sc).max(artist_sc);
                if title_sc >= 500 {
                    sc += (a.play_count * 3).min(25);
                }
                (a, sc)
            })
            .collect();
        scored_albums.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| b.0.play_count.cmp(&a.0.play_count))
                .then_with(|| a.0.title.to_lowercase().cmp(&b.0.title.to_lowercase()))
        });

        let mut scored_tracks: Vec<(Track, i32)> = tracks
            .into_iter()
            .map(|t| {
                let title_sc = score_search_text(&t.title, &norm_q, &q_tokens);
                let combo_sc =
                    score_search_text(&format!("{} {}", t.artist, t.title), &norm_q, &q_tokens);
                let meta_sc = score_search_text(&t.artist, &norm_q, &q_tokens)
                    .max(score_search_text(&t.album, &norm_q, &q_tokens))
                    / 4;
                let mut sc = title_sc.max(combo_sc).max(meta_sc);
                if title_sc >= 500 {
                    sc += (t.play_count * 3).min(25);
                }
                (t, sc)
            })
            .collect();
        scored_tracks.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| b.0.play_count.cmp(&a.0.play_count))
                .then_with(|| a.0.title.to_lowercase().cmp(&b.0.title.to_lowercase()))
        });

        let mut best: Option<TopSearchResult> = None;
        if let Some((a, sc)) = scored_artists.first() {
            best = Some(TopSearchResult {
                kind: "artist".into(),
                score: *sc,
                artist: Some(a.clone()),
                album: None,
                track: None,
            });
        }
        if let Some((a, sc)) = scored_albums.first()
            && best.as_ref().is_none_or(|b| *sc > b.score)
        {
            best = Some(TopSearchResult {
                kind: "album".into(),
                score: *sc,
                artist: None,
                album: Some(a.clone()),
                track: None,
            });
        }
        if let Some((t, sc)) = scored_tracks.first()
            && best.as_ref().is_none_or(|b| *sc > b.score)
        {
            best = Some(TopSearchResult {
                kind: "track".into(),
                score: *sc,
                artist: None,
                album: None,
                track: Some(t.clone()),
            });
        }

        let lim = limit as usize;
        res.artists = scored_artists.into_iter().take(lim).map(|x| x.0).collect();
        res.albums = scored_albums.into_iter().take(lim).map(|x| x.0).collect();
        res.tracks = scored_tracks.into_iter().take(lim).map(|x| x.0).collect();
        res.top_result = best;
        Ok(res)
    }

    pub fn get_pref(&self, key: &str) -> Result<Option<String>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let val: Option<String> = conn
            .query_row(
                "SELECT value FROM app_prefs WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?;
        Ok(val)
    }

    pub fn set_pref(&self, key: &str, value: &str) -> Result<()> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let now = Utc::now().timestamp();
        conn.execute(
            "INSERT INTO app_prefs(key, value, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![key, value, now],
        )?;
        Ok(())
    }

    pub fn delete_pref(&self, key: &str) -> Result<()> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        conn.execute("DELETE FROM app_prefs WHERE key = ?1", params![key])?;
        Ok(())
    }

    pub fn list_prefs(&self) -> Result<HashMap<String, String>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let mut stmt = conn.prepare("SELECT key, value FROM app_prefs")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = HashMap::new();
        for r in rows {
            let (k, v) = r?;
            out.insert(k, v);
        }
        Ok(out)
    }

    pub fn get_provider_credentials<T: DeserializeOwned>(
        &self,
        provider: &str,
    ) -> Result<Option<T>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let raw: Option<String> = conn
            .query_row(
                "SELECT data_json FROM provider_credentials WHERE provider = ?1",
                params![provider],
                |r| r.get(0),
            )
            .optional()?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let v = serde_json::from_str(&raw)?;
        Ok(Some(v))
    }

    pub fn set_provider_credentials<T: Serialize>(&self, provider: &str, src: &T) -> Result<()> {
        let raw = serde_json::to_string(src)?;
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let now = Utc::now().timestamp();
        conn.execute(
            "INSERT INTO provider_credentials(provider, data_json, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(provider) DO UPDATE SET data_json = excluded.data_json, updated_at = excluded.updated_at",
            params![provider, raw, now],
        )?;
        Ok(())
    }

    pub fn delete_provider_credentials(&self, provider: &str) -> Result<()> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        conn.execute(
            "DELETE FROM provider_credentials WHERE provider = ?1",
            params![provider],
        )?;
        Ok(())
    }
}

fn upsert_album_row(
    tx: &Transaction<'_>,
    a: &Album,
    provider: &str,
    source_type: &str,
    now: i64,
) -> Result<()> {
    let added = a.added_at.map(|t| t.timestamp()).unwrap_or(now);
    let s_title = sort_key(&a.title);
    let s_artist = sort_key(&a.artist);
    let v_key = version_key(&a.artist, &a.title);
    let ext_id = if a.external_id.is_empty() {
        a.id.strip_prefix(&format!("{provider}:"))
            .unwrap_or(&a.id)
            .to_string()
    } else {
        a.external_id.clone()
    };

    tx.execute(
        "INSERT INTO albums(
            id, source_type, provider, external_id, title, sort_title, artist, sort_artist,
            genre, year, track_count, duration_sec, artwork_url, primary_format,
            max_bit_depth, max_sample_rate, version_key, is_partial, available,
            missing_since, added_at, updated_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, 0, ?20, ?21)
        ON CONFLICT(id) DO UPDATE SET
            source_type = excluded.source_type,
            provider = excluded.provider,
            external_id = excluded.external_id,
            title = excluded.title,
            sort_title = excluded.sort_title,
            artist = excluded.artist,
            sort_artist = excluded.sort_artist,
            genre = excluded.genre,
            year = excluded.year,
            track_count = excluded.track_count,
            duration_sec = excluded.duration_sec,
            artwork_url = CASE WHEN excluded.artwork_url != '' THEN excluded.artwork_url ELSE albums.artwork_url END,
            primary_format = excluded.primary_format,
            max_bit_depth = excluded.max_bit_depth,
            max_sample_rate = excluded.max_sample_rate,
            version_key = excluded.version_key,
            is_partial = excluded.is_partial,
            available = excluded.available,
            missing_since = 0,
            updated_at = excluded.updated_at",
        params![
            a.id,
            source_type,
            provider,
            ext_id,
            a.title,
            s_title,
            a.artist,
            s_artist,
            a.genre,
            a.year,
            a.track_count,
            a.duration_sec,
            a.artwork_url,
            a.primary_format,
            a.max_bit_depth,
            a.max_sample_rate,
            v_key,
            if a.is_partial { 1 } else { 0 },
            if a.available { 1 } else { 0 },
            added,
            now,
        ],
    )?;
    Ok(())
}

fn upsert_track_row(
    tx: &Transaction<'_>,
    t: &Track,
    provider: &str,
    source_type: &str,
    now: i64,
) -> Result<()> {
    let added = t.added_at.map(|dt| dt.timestamp()).unwrap_or(now);
    let s_title = sort_key(&t.title);
    let s_artist = sort_key(if t.album_artist.is_empty() {
        &t.artist
    } else {
        &t.album_artist
    });
    let s_album = sort_key(&t.album);
    let ext_id = if t.external_id.is_empty() {
        t.id.strip_prefix(&format!("{provider}:"))
            .unwrap_or(&t.id)
            .to_string()
    } else {
        t.external_id.clone()
    };

    tx.execute(
        "INSERT INTO tracks(
            id, album_id, source_type, provider, external_id, title, sort_title, artist, sort_artist,
            album_artist, album, sort_album, genre, disc_number, track_number, year, duration_sec,
            sample_rate, bit_depth, channels, mime, ext, size_bytes, artwork_url,
            file_path, file_mtime_ns, folder_art, has_embed_art, qobuz_track_id,
            available, missing_since, added_at, updated_at
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
            ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
            ?18, ?19, ?20, ?21, ?22, ?23, ?24,
            ?25, ?26, ?27, ?28, ?29,
            ?30, 0, ?31, ?32
        )
        ON CONFLICT(id) DO UPDATE SET
            album_id = excluded.album_id,
            source_type = excluded.source_type,
            provider = excluded.provider,
            external_id = excluded.external_id,
            title = excluded.title,
            sort_title = excluded.sort_title,
            artist = excluded.artist,
            sort_artist = excluded.sort_artist,
            album_artist = excluded.album_artist,
            album = excluded.album,
            sort_album = excluded.sort_album,
            genre = excluded.genre,
            disc_number = excluded.disc_number,
            track_number = excluded.track_number,
            year = excluded.year,
            duration_sec = excluded.duration_sec,
            sample_rate = excluded.sample_rate,
            bit_depth = excluded.bit_depth,
            channels = excluded.channels,
            mime = excluded.mime,
            ext = excluded.ext,
            size_bytes = excluded.size_bytes,
            artwork_url = CASE WHEN excluded.artwork_url != '' THEN excluded.artwork_url ELSE tracks.artwork_url END,
            file_path = excluded.file_path,
            file_mtime_ns = excluded.file_mtime_ns,
            folder_art = excluded.folder_art,
            has_embed_art = excluded.has_embed_art,
            qobuz_track_id = excluded.qobuz_track_id,
            available = excluded.available,
            missing_since = 0,
            updated_at = excluded.updated_at",
        params![
            t.id,
            t.album_id,
            source_type,
            provider,
            ext_id,
            t.title,
            s_title,
            t.artist,
            s_artist,
            t.album_artist,
            t.album,
            s_album,
            t.genre,
            t.disc_number,
            t.track_number,
            t.year,
            t.duration_sec,
            t.sample_rate,
            t.bit_depth,
            t.channels,
            t.mime,
            t.ext,
            t.size,
            t.artwork_url,
            t.file_path,
            t.file_mtime_ns,
            t.folder_art,
            if t.has_embed_art { 1 } else { 0 },
            t.qobuz_track_id,
            if t.available { 1 } else { 0 },
            added,
            now,
        ],
    )?;
    Ok(())
}

fn refresh_album_aggregates_tx(tx: &Transaction<'_>, album_id: &str) -> Result<()> {
    let mut track_count = 0i32;
    let mut duration_sec = 0i32;
    let mut max_rate = 0i32;
    let mut max_bit = 0i32;
    let mut primary_format = String::new();
    let mut best_lossless = false;

    {
        let mut stmt = tx.prepare(
            "SELECT duration_sec, sample_rate, bit_depth, mime, ext FROM tracks WHERE album_id = ?1",
        )?;
        let rows = stmt.query_map(params![album_id], |r| {
            Ok((
                r.get::<_, i32>(0)?,
                r.get::<_, i32>(1)?,
                r.get::<_, i32>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        for r in rows {
            let (dur, rate, bit, mime, ext) = r?;
            track_count += 1;
            duration_sec += dur;
            if rate > max_rate {
                max_rate = rate;
            }
            if bit > max_bit {
                max_bit = bit;
            }
            let fmt = format_name_from_mime_or_ext(&mime, &ext);
            if !fmt.is_empty() {
                let ll = is_lossless_format(&fmt);
                if primary_format.is_empty() || (ll && !best_lossless) {
                    primary_format = fmt;
                    best_lossless = ll;
                }
            }
        }
    }

    tx.execute(
        "UPDATE albums SET track_count = ?1, duration_sec = ?2, max_sample_rate = ?3, max_bit_depth = ?4, primary_format = ?5 WHERE id = ?6",
        params![track_count, duration_sec, max_rate, max_bit, primary_format, album_id],
    )?;
    Ok(())
}

fn recompute_versions_tx(tx: &Transaction<'_>) -> Result<()> {
    let mut all: Vec<VersionAlbRow> = Vec::new();
    {
        let mut stmt = tx.prepare(
            "SELECT id, source_type, primary_format, max_bit_depth, max_sample_rate,
                    track_count, version_key, version_group_id, is_primary, version_excluded,
                    is_primary_override, available
             FROM albums",
        )?;
        let rows = stmt.query_map([], |r| {
            let group_id: String = r.get(7)?;
            Ok(VersionAlbRow {
                id: r.get(0)?,
                source_type: r.get(1)?,
                format: r.get(2)?,
                bit_depth: r.get(3)?,
                sample_rate: r.get(4)?,
                track_count: r.get(5)?,
                version_key: r.get(6)?,
                orig_group_id: group_id.clone(),
                group_id,
                orig_primary: r.get::<_, i32>(8)? != 0,
                excluded: r.get::<_, i32>(9)? != 0,
                override_primary: r.get::<_, i32>(10)? != 0,
                available: r.get::<_, i32>(11)? != 0,
            })
        })?;
        for r in rows {
            all.push(r?);
        }
    }

    let mut key_counts: HashMap<String, usize> = HashMap::new();
    for r in &all {
        if r.excluded || r.group_id.starts_with("manual:") || r.version_key.is_empty() {
            continue;
        }
        *key_counts.entry(r.version_key.clone()).or_insert(0) += 1;
    }

    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, r) in all.iter_mut().enumerate() {
        if r.excluded {
            r.group_id.clear();
        } else if !r.group_id.starts_with("manual:") {
            if !r.version_key.is_empty()
                && key_counts.get(&r.version_key).copied().unwrap_or(0) >= 2
            {
                r.group_id = format!("auto:{}", short_hash(&format!("vgroup:{}", r.version_key)));
            } else {
                r.group_id.clear();
            }
        }
        if !r.group_id.is_empty() {
            groups.entry(r.group_id.clone()).or_default().push(i);
        }
    }

    let mut is_primary = vec![true; all.len()];
    for indices in groups.values() {
        if indices.len() < 2 {
            for &idx in indices {
                all[idx].group_id.clear();
                is_primary[idx] = true;
            }
            continue;
        }
        let mut best_idx = indices[0];
        for &idx in &indices[1..] {
            let a = &all[idx];
            let b = &all[best_idx];
            if is_better_primary(a, b) {
                best_idx = idx;
            }
        }
        for &idx in indices {
            is_primary[idx] = idx == best_idx;
        }
    }

    let mut stmt = None;
    for (i, r) in all.iter().enumerate() {
        if r.group_id == r.orig_group_id && is_primary[i] == r.orig_primary {
            continue;
        }
        if stmt.is_none() {
            stmt = Some(tx.prepare(
                "UPDATE albums SET version_group_id = ?1, is_primary = ?2 WHERE id = ?3",
            )?);
        }
        if let Some(s) = stmt.as_mut() {
            s.execute(params![r.group_id, if is_primary[i] { 1 } else { 0 }, r.id])?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn is_better_primary_fields(
    a_avail: bool,
    a_ovr: bool,
    a_fmt: &str,
    a_bit: i32,
    a_rate: i32,
    a_src: &str,
    a_tracks: i32,
    a_id: &str,
    b_avail: bool,
    b_ovr: bool,
    b_fmt: &str,
    b_bit: i32,
    b_rate: i32,
    b_src: &str,
    b_tracks: i32,
    b_id: &str,
) -> bool {
    if a_avail != b_avail {
        return a_avail;
    }
    if a_ovr != b_ovr {
        return a_ovr;
    }
    let ra = quality_rank(a_fmt, a_bit, a_rate, a_src);
    let rb = quality_rank(b_fmt, b_bit, b_rate, b_src);
    if ra != rb {
        return ra > rb;
    }
    if a_tracks != b_tracks {
        return a_tracks > b_tracks;
    }
    a_id < b_id
}

fn is_better_primary(a: &impl PrimaryCandidate, b: &impl PrimaryCandidate) -> bool {
    is_better_primary_fields(
        a.available(),
        a.override_primary(),
        a.format(),
        a.bit_depth(),
        a.sample_rate(),
        a.source_type(),
        a.track_count(),
        a.id(),
        b.available(),
        b.override_primary(),
        b.format(),
        b.bit_depth(),
        b.sample_rate(),
        b.source_type(),
        b.track_count(),
        b.id(),
    )
}

trait PrimaryCandidate {
    fn id(&self) -> &str;
    fn source_type(&self) -> &str;
    fn format(&self) -> &str;
    fn bit_depth(&self) -> i32;
    fn sample_rate(&self) -> i32;
    fn track_count(&self) -> i32;
    fn override_primary(&self) -> bool;
    fn available(&self) -> bool;
}

// Implement for the local row struct inside recompute_versions_tx? Rust doesn't allow impl outside scope.
// Let's keep a clean module-level helper struct instead.
struct VersionAlbRow {
    id: String,
    source_type: String,
    format: String,
    bit_depth: i32,
    sample_rate: i32,
    track_count: i32,
    version_key: String,
    orig_group_id: String,
    group_id: String,
    orig_primary: bool,
    excluded: bool,
    override_primary: bool,
    available: bool,
}

impl PrimaryCandidate for VersionAlbRow {
    fn id(&self) -> &str {
        &self.id
    }
    fn source_type(&self) -> &str {
        &self.source_type
    }
    fn format(&self) -> &str {
        &self.format
    }
    fn bit_depth(&self) -> i32 {
        self.bit_depth
    }
    fn sample_rate(&self) -> i32 {
        self.sample_rate
    }
    fn track_count(&self) -> i32 {
        self.track_count
    }
    fn override_primary(&self) -> bool {
        self.override_primary
    }
    fn available(&self) -> bool {
        self.available
    }
}

fn query_albums_conn(conn: &Connection, f: &AlbumFilter) -> Result<(Vec<Album>, i32)> {
    let mut clauses: Vec<String> = Vec::new();
    let mut bind_vals: Vec<Value> = Vec::new();

    if !f.include_unavailable {
        clauses.push("a.available = 1".into());
    }
    let st = f.source_type.trim().to_ascii_lowercase();
    if !st.is_empty() && st != "all" {
        bind_vals.push(Value::Text(st));
        clauses.push(format!("a.source_type = ?{}", bind_vals.len()));
    }
    let prov = f.provider.trim().to_ascii_lowercase();
    if !prov.is_empty() && prov != "all" {
        bind_vals.push(Value::Text(prov));
        clauses.push(format!("a.provider = ?{}", bind_vals.len()));
    }
    let artist = f.artist.trim();
    if !artist.is_empty() {
        bind_vals.push(Value::Text(artist.to_lowercase()));
        clauses.push(format!("LOWER(a.artist) = ?{}", bind_vals.len()));
    }
    let genre = f.genre.trim();
    if !genre.is_empty() {
        bind_vals.push(Value::Text(genre.to_lowercase()));
        clauses.push(format!("LOWER(a.genre) = ?{}", bind_vals.len()));
    }
    if f.group_versions {
        clauses.push("a.is_primary = 1".into());
    }
    let fts = build_fts_query(&f.query);
    if !fts.is_empty() {
        bind_vals.push(Value::Text(fts));
        clauses.push(format!(
            "a.id IN (SELECT album_id FROM albums_fts WHERE albums_fts MATCH ?{})",
            bind_vals.len()
        ));
    }

    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };

    let count_sql = format!("SELECT COUNT(*) FROM albums a {where_sql}");
    let total: i32 = conn.query_row(
        &count_sql,
        rusqlite::params_from_iter(bind_vals.iter()),
        |r| r.get(0),
    )?;

    let order_by = match f.sort.trim().to_ascii_lowercase().as_str() {
        "title" => "a.sort_title ASC, a.sort_artist ASC, a.id ASC",
        "year" => "a.year DESC, a.sort_title ASC, a.id ASC",
        "added" => "a.added_at DESC, a.sort_title ASC, a.id ASC",
        "plays" => "play_count DESC, a.sort_title ASC, a.id ASC",
        _ => "a.sort_artist ASC, a.year ASC, a.sort_title ASC, a.id ASC",
    };

    let limit = if f.limit <= 0 {
        200
    } else {
        f.limit.min(100_000)
    };
    let offset = f.offset.max(0);

    let mut query_vals = bind_vals;
    query_vals.push(Value::Integer(i64::from(limit)));
    let lim_idx = query_vals.len();
    query_vals.push(Value::Integer(i64::from(offset)));
    let off_idx = query_vals.len();

    let sql = format!(
        "SELECT a.id, a.source_type, a.provider, a.external_id, a.title, a.artist, a.genre, a.year,
                a.track_count, a.duration_sec, a.artwork_url, a.primary_format,
                a.max_bit_depth, a.max_sample_rate, a.version_group_id, a.is_primary,
                a.is_partial, a.available, a.added_at,
                COALESCE((SELECT SUM(t.play_count) FROM tracks t WHERE t.album_id = a.id), 0) AS play_count,
                CASE WHEN a.version_group_id != '' THEN
                    (SELECT COUNT(*) FROM albums b WHERE b.version_group_id = a.version_group_id)
                ELSE 1 END AS version_count
         FROM albums a
         {where_sql}
         ORDER BY {order_by}
         LIMIT ?{lim_idx} OFFSET ?{off_idx}"
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(query_vals.iter()), |r| {
        let provider: String = r.get(2)?;
        Ok(Album {
            id: r.get(0)?,
            source: provider.clone(),
            source_type: r.get(1)?,
            provider,
            external_id: r.get(3)?,
            title: r.get(4)?,
            artist: r.get(5)?,
            genre: r.get(6)?,
            year: r.get(7)?,
            track_count: r.get(8)?,
            duration_sec: r.get(9)?,
            artwork_url: r.get(10)?,
            primary_format: r.get(11)?,
            max_bit_depth: r.get(12)?,
            max_sample_rate: r.get(13)?,
            version_group_id: r.get(14)?,
            is_primary: r.get::<_, i32>(15)? != 0,
            is_partial: r.get::<_, i32>(16)? != 0,
            available: r.get::<_, i32>(17)? != 0,
            added_at: unix_to_dt(r.get(18)?),
            play_count: r.get(19)?,
            version_count: r.get(20)?,
            in_library: true,
            ..Default::default()
        })
    })?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok((out, total))
}

fn query_tracks_conn(conn: &Connection, f: &TrackFilter) -> Result<(Vec<Track>, i32)> {
    let mut clauses: Vec<String> = Vec::new();
    let mut bind_vals: Vec<Value> = Vec::new();

    if !f.include_unavailable {
        clauses.push("t.available = 1".into());
    }
    let st = f.source_type.trim().to_ascii_lowercase();
    if !st.is_empty() && st != "all" {
        bind_vals.push(Value::Text(st));
        clauses.push(format!("t.source_type = ?{}", bind_vals.len()));
    }
    let prov = f.provider.trim().to_ascii_lowercase();
    if !prov.is_empty() && prov != "all" {
        bind_vals.push(Value::Text(prov));
        clauses.push(format!("t.provider = ?{}", bind_vals.len()));
    }
    if !f.album_id.trim().is_empty() {
        bind_vals.push(Value::Text(f.album_id.trim().to_string()));
        clauses.push(format!("t.album_id = ?{}", bind_vals.len()));
    }
    if !f.artist.trim().is_empty() {
        let low = f.artist.trim().to_lowercase();
        bind_vals.push(Value::Text(low.clone()));
        let p1 = bind_vals.len();
        bind_vals.push(Value::Text(low));
        let p2 = bind_vals.len();
        clauses.push(format!(
            "(LOWER(t.artist) = ?{p1} OR LOWER(t.album_artist) = ?{p2})"
        ));
    }
    if !f.album.trim().is_empty() {
        bind_vals.push(Value::Text(f.album.trim().to_lowercase()));
        clauses.push(format!("LOWER(t.album) = ?{}", bind_vals.len()));
    }
    if !f.genre.trim().is_empty() {
        bind_vals.push(Value::Text(f.genre.trim().to_lowercase()));
        clauses.push(format!("LOWER(t.genre) = ?{}", bind_vals.len()));
    }
    let fts = build_fts_query(&f.query);
    if !fts.is_empty() {
        bind_vals.push(Value::Text(fts));
        clauses.push(format!(
            "t.id IN (SELECT track_id FROM tracks_fts WHERE tracks_fts MATCH ?{})",
            bind_vals.len()
        ));
    }

    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };

    let count_sql = format!("SELECT COUNT(*) FROM tracks t {where_sql}");
    let total: i32 = conn.query_row(
        &count_sql,
        rusqlite::params_from_iter(bind_vals.iter()),
        |r| r.get(0),
    )?;

    let dir = if f.sort_dir.eq_ignore_ascii_case("desc") {
        "DESC"
    } else {
        "ASC"
    };
    let order_by = match f.sort.trim().to_ascii_lowercase().as_str() {
        "title" => format!("t.sort_title {dir}, t.sort_artist ASC, t.id ASC"),
        "artist" => format!(
            "t.sort_artist {dir}, t.sort_album ASC, t.disc_number ASC, t.track_number ASC, t.id ASC"
        ),
        "year" => format!(
            "t.year {dir}, t.sort_album ASC, t.disc_number ASC, t.track_number ASC, t.id ASC"
        ),
        "added" => format!("t.added_at {dir}, t.sort_title ASC, t.id ASC"),
        "plays" => format!("t.play_count {dir}, t.sort_title ASC, t.id ASC"),
        "duration" => format!("t.duration_sec {dir}, t.sort_title ASC, t.id ASC"),
        "quality" => format!("t.bit_depth {dir}, t.sample_rate {dir}, t.sort_title ASC, t.id ASC"),
        _ => format!(
            "t.sort_album {dir}, t.disc_number ASC, t.track_number ASC, t.sort_title ASC, t.id ASC"
        ),
    };

    let limit = if f.limit <= 0 {
        500
    } else {
        f.limit.min(100_000)
    };
    let offset = f.offset.max(0);

    let mut query_vals = bind_vals;
    query_vals.push(Value::Integer(i64::from(limit)));
    let lim_idx = query_vals.len();
    query_vals.push(Value::Integer(i64::from(offset)));
    let off_idx = query_vals.len();

    let sql = format!(
        "SELECT t.id, t.album_id, t.source_type, t.provider, t.external_id, t.title, t.artist,
                t.album_artist, t.album, t.genre, t.disc_number, t.track_number, t.year,
                t.duration_sec, t.sample_rate, t.bit_depth, t.channels, t.mime, t.ext,
                t.size_bytes, t.artwork_url, t.file_path, t.file_mtime_ns, t.folder_art,
                t.has_embed_art, t.qobuz_track_id, t.play_count, t.last_played_at,
                t.loved, t.available, t.added_at
         FROM tracks t
         {where_sql}
         ORDER BY {order_by}
         LIMIT ?{lim_idx} OFFSET ?{off_idx}"
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(query_vals.iter()), row_to_track)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok((out, total))
}

fn list_artists_conn(conn: &Connection, f: &FacetFilter) -> Result<(Vec<ArtistSummary>, i32)> {
    let mut clauses: Vec<String> = vec!["a.artist != ''".into()];
    let mut bind_vals: Vec<Value> = Vec::new();

    if !f.include_unavailable {
        clauses.push("a.available = 1".into());
    }
    let st = f.source_type.trim().to_ascii_lowercase();
    if !st.is_empty() && st != "all" {
        bind_vals.push(Value::Text(st));
        clauses.push(format!("a.source_type = ?{}", bind_vals.len()));
    }
    let prov = f.provider.trim().to_ascii_lowercase();
    if !prov.is_empty() && prov != "all" {
        bind_vals.push(Value::Text(prov));
        clauses.push(format!("a.provider = ?{}", bind_vals.len()));
    }
    if !f.genre.trim().is_empty() {
        bind_vals.push(Value::Text(f.genre.trim().to_lowercase()));
        clauses.push(format!("LOWER(a.genre) = ?{}", bind_vals.len()));
    }
    if !f.query.trim().is_empty() {
        let like = format!("%{}%", f.query.trim().to_lowercase());
        bind_vals.push(Value::Text(like));
        clauses.push(format!("LOWER(a.artist) LIKE ?{}", bind_vals.len()));
    }

    let where_sql = format!("WHERE {}", clauses.join(" AND "));
    let count_sql = format!("SELECT COUNT(DISTINCT LOWER(a.artist)) FROM albums a {where_sql}");
    let total: i32 = conn.query_row(
        &count_sql,
        rusqlite::params_from_iter(bind_vals.iter()),
        |r| r.get(0),
    )?;

    let limit = if f.limit <= 0 { 200 } else { f.limit.min(5000) };
    let offset = f.offset.max(0);

    let mut query_vals = bind_vals;
    query_vals.push(Value::Integer(i64::from(limit)));
    let lim_idx = query_vals.len();
    query_vals.push(Value::Integer(i64::from(offset)));
    let off_idx = query_vals.len();

    let sql = format!(
        "SELECT MIN(a.artist) AS name,
                COUNT(DISTINCT CASE WHEN a.version_group_id != '' THEN a.version_group_id ELSE a.id END) AS album_count,
                COALESCE(SUM(CASE WHEN a.is_primary = 1 THEN a.track_count ELSE 0 END), 0) AS track_count,
                MAX(CASE WHEN a.artwork_url != '' THEN a.artwork_url ELSE '' END) AS artwork_url,
                GROUP_CONCAT(DISTINCT a.provider) AS providers,
                MIN(a.sort_artist) AS sort_key
         FROM albums a
         {where_sql}
         GROUP BY LOWER(a.artist)
         ORDER BY sort_key ASC, LOWER(name) ASC
         LIMIT ?{lim_idx} OFFSET ?{off_idx}"
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(query_vals.iter()), |r| {
        let raw_provs: Option<String> = r.get(4)?;
        let mut providers: Vec<String> = raw_provs
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        providers.sort();
        providers.dedup();
        Ok(ArtistSummary {
            name: r.get(0)?,
            album_count: r.get(1)?,
            track_count: r.get(2)?,
            artwork_url: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
            providers,
        })
    })?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok((out, total))
}

fn list_genres_conn(conn: &Connection, f: &FacetFilter) -> Result<(Vec<GenreSummary>, i32)> {
    let mut clauses: Vec<String> = vec!["a.genre != ''".into()];
    let mut bind_vals: Vec<Value> = Vec::new();

    if !f.include_unavailable {
        clauses.push("a.available = 1".into());
    }
    let st = f.source_type.trim().to_ascii_lowercase();
    if !st.is_empty() && st != "all" {
        bind_vals.push(Value::Text(st));
        clauses.push(format!("a.source_type = ?{}", bind_vals.len()));
    }
    let prov = f.provider.trim().to_ascii_lowercase();
    if !prov.is_empty() && prov != "all" {
        bind_vals.push(Value::Text(prov));
        clauses.push(format!("a.provider = ?{}", bind_vals.len()));
    }
    if !f.query.trim().is_empty() {
        let like = format!("%{}%", f.query.trim().to_lowercase());
        bind_vals.push(Value::Text(like));
        clauses.push(format!("LOWER(a.genre) LIKE ?{}", bind_vals.len()));
    }

    let where_sql = format!("WHERE {}", clauses.join(" AND "));
    let count_sql = format!("SELECT COUNT(DISTINCT LOWER(a.genre)) FROM albums a {where_sql}");
    let total: i32 = conn.query_row(
        &count_sql,
        rusqlite::params_from_iter(bind_vals.iter()),
        |r| r.get(0),
    )?;

    let limit = if f.limit <= 0 { 200 } else { f.limit.min(5000) };
    let offset = f.offset.max(0);

    let mut query_vals = bind_vals;
    query_vals.push(Value::Integer(i64::from(limit)));
    let lim_idx = query_vals.len();
    query_vals.push(Value::Integer(i64::from(offset)));
    let off_idx = query_vals.len();

    let sql = format!(
        "SELECT MIN(a.genre) AS name,
                COUNT(DISTINCT CASE WHEN a.version_group_id != '' THEN a.version_group_id ELSE a.id END) AS album_count,
                COALESCE(SUM(CASE WHEN a.is_primary = 1 THEN a.track_count ELSE 0 END), 0) AS track_count
         FROM albums a
         {where_sql}
         GROUP BY LOWER(a.genre)
         ORDER BY album_count DESC, LOWER(name) ASC
         LIMIT ?{lim_idx} OFFSET ?{off_idx}"
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(query_vals.iter()), |r| {
        Ok(GenreSummary {
            name: r.get(0)?,
            album_count: r.get(1)?,
            track_count: r.get(2)?,
        })
    })?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok((out, total))
}

fn row_to_track(r: &rusqlite::Row<'_>) -> rusqlite::Result<Track> {
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
        in_library: true,
        explicit: false,
    })
}

fn unix_to_dt(ts: i64) -> Option<DateTime<Utc>> {
    if ts > 0 {
        Utc.timestamp_opt(ts, 0).single()
    } else {
        None
    }
}

pub fn build_fts_query(raw: &str) -> String {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    for ch in raw.trim().chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                cur.push(lc);
            }
        } else if !cur.is_empty() {
            tokens.push(format!("\"{}\"*", cur.replace('"', "")));
            cur.clear();
        }
    }
    if !cur.is_empty() {
        tokens.push(format!("\"{}\"*", cur.replace('"', "")));
    }
    tokens.join(" ")
}

fn normalize_search_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = true;
    for ch in s.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                out.push(lc);
            }
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim().to_string()
}

fn score_search_text(candidate: &str, norm_q: &str, q_tokens: &[&str]) -> i32 {
    if norm_q.is_empty() {
        return 0;
    }
    let cand = normalize_search_text(candidate);
    if cand.is_empty() {
        return 0;
    }
    if cand == norm_q {
        return 1000;
    }
    let trimmed = cand.strip_prefix("the ").unwrap_or(&cand);
    if trimmed != cand && trimmed == norm_q {
        return 960;
    }
    if cand.starts_with(norm_q) {
        return 820 - ((cand.len().saturating_sub(norm_q.len())) as i32).min(120);
    }
    if trimmed != cand && trimmed.starts_with(norm_q) {
        return 790 - ((trimmed.len().saturating_sub(norm_q.len())) as i32).min(120);
    }
    let words: Vec<&str> = cand.split_whitespace().collect();
    if !q_tokens.is_empty() {
        let all_prefix = q_tokens
            .iter()
            .all(|qt| words.iter().any(|w| w.starts_with(qt)));
        if all_prefix {
            return 620 - ((cand.len().saturating_sub(norm_q.len())) as i32).min(120);
        }
    }
    if cand.contains(norm_q) {
        return 420 - ((cand.len().saturating_sub(norm_q.len())) as i32).min(120);
    }
    0
}
