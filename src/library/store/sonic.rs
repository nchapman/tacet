use chrono::Utc;
use rusqlite::params;
use serde::Serialize;

use super::super::types::{LibraryError, Result, Track};
use super::{LibraryStore, TRACK_SELECT_COLS_T, row_to_track};

#[derive(Debug, Clone, Default)]
pub struct TrackSonicHeadsWrite {
    pub heads_version: i32,
    pub engagement: Option<f32>,
    pub approachability: Option<f32>,
    pub instrumental: Option<f32>,
    pub danceability: Option<f32>,
    pub mood_relaxed: Option<f32>,
    pub mood_acoustic: Option<f32>,
    pub mood_aggressive: Option<f32>,
    pub mood_electronic: Option<f32>,
}

#[derive(Debug, Clone)]
pub struct SonicTagEntry {
    pub kind: String,
    pub tag: String,
    pub score: f64,
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct TrackSonicWrite {
    pub track_id: String,
    pub model_version: String,
    pub file_mtime_ns: i64,
    pub emb_uint8: Vec<u8>,
    pub emb_min: f32,
    pub emb_scale: f32,
    pub heads: Option<TrackSonicHeadsWrite>,
    pub tags: Vec<SonicTagEntry>,
}

#[derive(Debug, Clone)]
pub struct StoredSonicEmbedding {
    pub track_id: String,
    pub emb_uint8: Vec<u8>,
    pub emb_min: f32,
    pub emb_scale: f32,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SonicStats {
    pub total_local_tracks: i32,
    pub analyzed_tracks: i32,
    pub pending_tracks: i32,
    pub heads_computed_tracks: i32,
}

impl LibraryStore {
    /// Returns available local tracks whose `track_sonic` row is missing, stale (mtime changed),
    /// or from an older `model_version`.
    pub fn list_tracks_needing_sonic(
        &self,
        model_version: &str,
        limit: usize,
    ) -> Result<Vec<Track>> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = guard.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let sql = format!(
            "SELECT {TRACK_SELECT_COLS_T}
             FROM tracks t
             LEFT JOIN track_sonic s ON s.track_id = t.id
             WHERE t.available = 1
               AND t.source_type = 'local'
               AND t.file_path != ''
               AND (
                   s.track_id IS NULL
                   OR s.model_version != ?1
                   OR (t.file_mtime_ns > 0 AND s.file_mtime_ns != t.file_mtime_ns)
               )
             ORDER BY t.added_at DESC, t.id ASC
             LIMIT ?2"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![model_version, limit as i64], row_to_track)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn save_track_sonic(&self, rec: &TrackSonicWrite) -> Result<()> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = guard.conn.as_mut().ok_or(LibraryError::NotFound)?;
        let now = Utc::now().timestamp();
        let tx = conn.transaction()?;

        tx.execute(
            "INSERT INTO track_sonic(
                track_id, model_version, file_mtime_ns, emb_uint8, emb_min, emb_scale, analyzed_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(track_id) DO UPDATE SET
                model_version = excluded.model_version,
                file_mtime_ns = excluded.file_mtime_ns,
                emb_uint8 = excluded.emb_uint8,
                emb_min = excluded.emb_min,
                emb_scale = excluded.emb_scale,
                analyzed_at = excluded.analyzed_at",
            params![
                rec.track_id,
                rec.model_version,
                rec.file_mtime_ns,
                rec.emb_uint8,
                f64::from(rec.emb_min),
                f64::from(rec.emb_scale),
                now,
            ],
        )?;

        if let Some(h) = &rec.heads {
            tx.execute(
                "INSERT INTO track_sonic_heads(
                    track_id, heads_version, engagement, approachability, instrumental,
                    danceability, mood_relaxed, mood_acoustic, mood_aggressive,
                    mood_electronic, computed_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(track_id) DO UPDATE SET
                    heads_version = excluded.heads_version,
                    engagement = excluded.engagement,
                    approachability = excluded.approachability,
                    instrumental = excluded.instrumental,
                    danceability = excluded.danceability,
                    mood_relaxed = excluded.mood_relaxed,
                    mood_acoustic = excluded.mood_acoustic,
                    mood_aggressive = excluded.mood_aggressive,
                    mood_electronic = excluded.mood_electronic,
                    computed_at = excluded.computed_at",
                params![
                    rec.track_id,
                    h.heads_version,
                    h.engagement.map(f64::from),
                    h.approachability.map(f64::from),
                    h.instrumental.map(f64::from),
                    h.danceability.map(f64::from),
                    h.mood_relaxed.map(f64::from),
                    h.mood_acoustic.map(f64::from),
                    h.mood_aggressive.map(f64::from),
                    h.mood_electronic.map(f64::from),
                    now,
                ],
            )?;
        }

        if !rec.tags.is_empty() {
            tx.execute(
                "DELETE FROM entity_tags
                 WHERE entity_type = 'track' AND entity_id = ?1 AND source IN ('effnet', 'head')",
                params![rec.track_id],
            )?;
            for t in &rec.tags {
                let tag = t.tag.trim().to_ascii_lowercase();
                if tag.is_empty() {
                    continue;
                }
                tx.execute(
                    "INSERT OR REPLACE INTO entity_tags(entity_type, entity_id, kind, tag, score, source)
                     VALUES ('track', ?1, ?2, ?3, ?4, ?5)",
                    params![rec.track_id, t.kind, tag, t.score, t.source],
                )?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    pub fn load_all_track_sonic(&self, model_version: &str) -> Result<Vec<StoredSonicEmbedding>> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = guard.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let mut stmt = conn.prepare(
            "SELECT s.track_id, s.emb_uint8, s.emb_min, s.emb_scale
             FROM track_sonic s
             JOIN tracks t ON t.id = s.track_id
             WHERE t.available = 1 AND s.model_version = ?1",
        )?;
        let rows = stmt.query_map(params![model_version], |row| {
            let min_f64: f64 = row.get(2)?;
            let scale_f64: f64 = row.get(3)?;
            Ok(StoredSonicEmbedding {
                track_id: row.get(0)?,
                emb_uint8: row.get(1)?,
                emb_min: min_f64 as f32,
                emb_scale: scale_f64 as f32,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn get_sonic_stats(&self, model_version: &str) -> Result<SonicStats> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = guard.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let mut stats = SonicStats::default();
        conn.query_row(
            "SELECT
                COUNT(*),
                COALESCE(SUM(CASE
                    WHEN s.track_id IS NOT NULL
                     AND s.model_version = ?1
                     AND (t.file_mtime_ns <= 0 OR s.file_mtime_ns = t.file_mtime_ns)
                    THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN h.track_id IS NOT NULL THEN 1 ELSE 0 END), 0)
             FROM tracks t
             LEFT JOIN track_sonic s ON s.track_id = t.id
             LEFT JOIN track_sonic_heads h ON h.track_id = t.id
             WHERE t.available = 1 AND t.source_type = 'local' AND t.file_path != ''",
            params![model_version],
            |row| {
                stats.total_local_tracks = row.get(0)?;
                stats.analyzed_tracks = row.get(1)?;
                stats.heads_computed_tracks = row.get(2)?;
                stats.pending_tracks = (stats.total_local_tracks - stats.analyzed_tracks).max(0);
                Ok(())
            },
        )?;
        Ok(stats)
    }
}
