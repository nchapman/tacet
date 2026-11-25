use std::collections::{HashMap, HashSet};

use chrono::Utc;
use rusqlite::{OptionalExtension, Transaction, params};

use super::versions::recompute_versions_tx;
use super::{CachedLocalFile, LibraryStore, row_to_track};
use crate::library::types::{
    Album, LibraryError, Result, SOURCE_LOCAL, SOURCE_TYPE_LOCAL, Track,
    format_name_from_mime_or_ext, is_lossless_format, sort_key, version_key,
};

impl LibraryStore {
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

        reconcile_missing_rows(
            &tx,
            "tracks",
            provider,
            &seen_tracks,
            soft_delete_missing,
            now,
            retention_secs,
        )?;
        reconcile_missing_rows(
            &tx,
            "albums",
            provider,
            &seen_albums,
            soft_delete_missing,
            now,
            retention_secs,
        )?;
        rebuild_provider_fts(&tx, provider)?;

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
}

fn reconcile_missing_rows(
    tx: &Transaction<'_>,
    table: &str,
    provider: &str,
    seen: &HashSet<String>,
    soft_delete_missing: bool,
    now: i64,
    retention_secs: i64,
) -> Result<()> {
    let mut existing: Vec<(String, i64)> = Vec::new();
    {
        let sql = format!("SELECT id, missing_since FROM {table} WHERE provider = ?1");
        let mut stmt = tx.prepare(&sql)?;
        let rows = stmt.query_map(params![provider], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        for r in rows {
            existing.push(r?);
        }
    }
    let del_sql = format!("DELETE FROM {table} WHERE id = ?1");
    let soft_sql = format!(
        "UPDATE {table} SET available = 0, missing_since = ?1, updated_at = ?1 WHERE id = ?2"
    );
    for (id, missing_since) in existing {
        if seen.contains(&id) {
            continue;
        }
        if !soft_delete_missing {
            tx.execute(&del_sql, params![id])?;
        } else if missing_since == 0 {
            tx.execute(&soft_sql, params![now, id])?;
        } else if retention_secs > 0 && now - missing_since >= retention_secs {
            tx.execute(&del_sql, params![id])?;
        }
    }
    Ok(())
}

fn rebuild_provider_fts(tx: &Transaction<'_>, provider: &str) -> Result<()> {
    tx.execute(
        "DELETE FROM albums_fts WHERE album_id IN (SELECT id FROM albums WHERE provider = ?1)",
        params![provider],
    )?;
    tx.execute(
        "DELETE FROM albums_fts WHERE album_id NOT IN (SELECT id FROM albums)",
        [],
    )?;
    tx.execute(
        "INSERT INTO albums_fts(album_id, title, artist, genre)
         SELECT id, title, artist, genre FROM albums WHERE provider = ?1 AND available = 1",
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
         SELECT id, title, artist, album_artist, album, genre FROM tracks WHERE provider = ?1 AND available = 1",
        params![provider],
    )?;
    Ok(())
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
            genre = CASE WHEN excluded.genre != '' THEN excluded.genre ELSE albums.genre END,
            year = CASE WHEN excluded.year > 0 THEN excluded.year ELSE albums.year END,
            track_count = excluded.track_count,
            duration_sec = excluded.duration_sec,
            artwork_url = CASE WHEN excluded.artwork_url != '' THEN excluded.artwork_url ELSE albums.artwork_url END,
            primary_format = CASE WHEN excluded.primary_format != '' THEN excluded.primary_format ELSE albums.primary_format END,
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
            "SELECT duration_sec, sample_rate, bit_depth, mime, ext FROM tracks WHERE album_id = ?1 AND available = 1",
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
