use std::collections::HashMap;

use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{LibraryStore, unix_to_dt};
use crate::library::types::{Album, HeavyRotationAlbum, LibraryError, Result, Track};

impl LibraryStore {
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
