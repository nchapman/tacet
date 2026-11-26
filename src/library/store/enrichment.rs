use chrono::Utc;
use rusqlite::params;
use serde::Serialize;

use super::super::types::{Album, LibraryError, Result, normalize_isrc};
use super::LibraryStore;
use super::query::{ALBUM_SELECT_COLS_A, row_to_album};

#[derive(Debug, Clone, Default)]
pub struct TrackEnrichmentPatch {
    pub track_id: String,
    pub isrc: String,
    pub mb_recording_id: String,
    pub mb_artist_id: String,
    pub genres: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct AlbumEnrichmentUpdate {
    pub status: String,
    pub confidence: f64,
    pub matched_by: String,
    pub retry_after: i64,
    pub upc: String,
    pub mb_release_id: String,
    pub mb_release_group_id: String,
    pub mb_artist_id: String,
    pub album_genres: Vec<String>,
    pub track_patches: Vec<TrackEnrichmentPatch>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrichmentStats {
    pub total_albums: i32,
    pub matched_albums: i32,
    pub partial_albums: i32,
    pub unmatched_albums: i32,
    pub pending_albums: i32,
    pub failed_albums: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EntityTag {
    pub kind: String,
    pub tag: String,
    pub score: f64,
    pub source: String,
}

impl LibraryStore {
    /// Returns available albums that have not yet been enriched, are marked pending,
    /// or have a past-due retry timestamp after an error/unmatched attempt.
    /// Local albums are prioritized ahead of remote albums.
    pub fn list_albums_needing_enrichment(&self, now: i64, limit: usize) -> Result<Vec<Album>> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = guard.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let sql = format!(
            "SELECT {ALBUM_SELECT_COLS_A}, 0
             FROM albums a
             LEFT JOIN album_enrichment e ON e.album_id = a.id
             WHERE a.available = 1
               AND (
                   e.album_id IS NULL
                   OR e.status = 'pending'
                   OR (e.status IN ('error', 'unmatched') AND e.retry_after > 0 AND e.retry_after <= ?1)
               )
             ORDER BY
               CASE WHEN a.source_type = 'local' THEN 0 ELSE 1 END,
               COALESCE(e.resolved_at, 0) ASC,
               a.added_at DESC
             LIMIT ?2"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut out = Vec::new();
        let rows = stmt.query_map(params![now, limit as i64], |r| row_to_album(r, false))?;
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Transactionally records an album enrichment result, fills any empty album/track
    /// identifiers without overwriting existing non-empty values, and replaces
    /// `source = 'resolver'` genre tags in `entity_tags`.
    pub fn apply_album_enrichment(
        &self,
        album_id: &str,
        update: &AlbumEnrichmentUpdate,
    ) -> Result<()> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = guard.conn.as_mut().ok_or(LibraryError::NotFound)?;
        let now = Utc::now().timestamp();
        let tx = conn.transaction()?;

        tx.execute(
            "INSERT INTO album_enrichment(
                album_id, status, confidence, matched_by, resolved_at, retry_after
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(album_id) DO UPDATE SET
                status = excluded.status,
                confidence = excluded.confidence,
                matched_by = excluded.matched_by,
                resolved_at = excluded.resolved_at,
                retry_after = excluded.retry_after",
            params![
                album_id,
                update.status,
                update.confidence,
                update.matched_by,
                now,
                update.retry_after,
            ],
        )?;

        if !update.upc.is_empty()
            || !update.mb_release_id.is_empty()
            || !update.mb_release_group_id.is_empty()
            || !update.mb_artist_id.is_empty()
        {
            tx.execute(
                "UPDATE albums SET
                    upc = CASE WHEN upc = '' AND ?2 != '' THEN ?2 ELSE upc END,
                    mb_release_id = CASE WHEN mb_release_id = '' AND ?3 != '' THEN ?3 ELSE mb_release_id END,
                    mb_release_group_id = CASE WHEN mb_release_group_id = '' AND ?4 != '' THEN ?4 ELSE mb_release_group_id END,
                    mb_artist_id = CASE WHEN mb_artist_id = '' AND ?5 != '' THEN ?5 ELSE mb_artist_id END
                 WHERE id = ?1",
                params![
                    album_id,
                    update.upc,
                    update.mb_release_id,
                    update.mb_release_group_id,
                    update.mb_artist_id,
                ],
            )?;
        }

        if !update.album_genres.is_empty() {
            replace_resolver_genres(&tx, "album", album_id, &update.album_genres)?;
        }

        for patch in &update.track_patches {
            let norm_isrc = normalize_isrc(&patch.isrc);
            if !norm_isrc.is_empty()
                || !patch.mb_recording_id.is_empty()
                || !patch.mb_artist_id.is_empty()
            {
                tx.execute(
                    "UPDATE tracks SET
                        isrc = CASE WHEN isrc = '' AND ?2 != '' THEN ?2 ELSE isrc END,
                        mb_recording_id = CASE WHEN mb_recording_id = '' AND ?3 != '' THEN ?3 ELSE mb_recording_id END,
                        mb_artist_id = CASE WHEN mb_artist_id = '' AND ?4 != '' THEN ?4 ELSE mb_artist_id END
                     WHERE id = ?1",
                    params![
                        patch.track_id,
                        norm_isrc,
                        patch.mb_recording_id,
                        patch.mb_artist_id,
                    ],
                )?;
            }
            if !patch.genres.is_empty() {
                replace_resolver_genres(&tx, "track", &patch.track_id, &patch.genres)?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    pub fn get_enrichment_stats(&self) -> Result<EnrichmentStats> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = guard.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let mut stats = EnrichmentStats::default();
        conn.query_row(
            "SELECT
                COUNT(*),
                COALESCE(SUM(CASE WHEN e.status = 'matched' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN e.status = 'partial' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN e.status = 'unmatched' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN e.album_id IS NULL OR e.status = 'pending' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN e.status = 'error' THEN 1 ELSE 0 END), 0)
             FROM albums a
             LEFT JOIN album_enrichment e ON e.album_id = a.id
             WHERE a.available = 1",
            [],
            |row| {
                stats.total_albums = row.get(0)?;
                stats.matched_albums = row.get(1)?;
                stats.partial_albums = row.get(2)?;
                stats.unmatched_albums = row.get(3)?;
                stats.pending_albums = row.get(4)?;
                stats.failed_albums = row.get(5)?;
                Ok(())
            },
        )?;
        Ok(stats)
    }

    /// Resets failed (and optionally unmatched) albums back to `pending` so they are
    /// retried on the next worker pass.
    pub fn reset_enrichment_for_retry(&self, include_unmatched: bool) -> Result<usize> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = guard.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let sql = if include_unmatched {
            "UPDATE album_enrichment
             SET status = 'pending', retry_after = 0
             WHERE status IN ('error', 'unmatched')"
        } else {
            "UPDATE album_enrichment
             SET status = 'pending', retry_after = 0
             WHERE status = 'error'"
        };
        Ok(conn.execute(sql, [])?)
    }

    pub fn list_entity_tags(&self, entity_type: &str, entity_id: &str) -> Result<Vec<EntityTag>> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = guard.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let mut stmt = conn.prepare(
            "SELECT kind, tag, score, source
             FROM entity_tags
             WHERE entity_type = ?1 AND entity_id = ?2
             ORDER BY score DESC, tag ASC",
        )?;
        let rows = stmt.query_map(params![entity_type, entity_id], |row| {
            Ok(EntityTag {
                kind: row.get(0)?,
                tag: row.get(1)?,
                score: row.get(2)?,
                source: row.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}

fn replace_resolver_genres(
    tx: &rusqlite::Transaction<'_>,
    entity_type: &str,
    entity_id: &str,
    genres: &[String],
) -> Result<()> {
    tx.execute(
        "DELETE FROM entity_tags
         WHERE entity_type = ?1 AND entity_id = ?2 AND kind = 'genre' AND source = 'resolver'",
        params![entity_type, entity_id],
    )?;
    for raw in genres {
        let tag = raw.trim().to_ascii_lowercase();
        if tag.is_empty() {
            continue;
        }
        tx.execute(
            "INSERT OR REPLACE INTO entity_tags(entity_type, entity_id, kind, tag, score, source)
             VALUES (?1, ?2, 'genre', ?3, 1.0, 'resolver')",
            params![entity_type, entity_id, tag],
        )?;
    }
    Ok(())
}
