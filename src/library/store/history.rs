use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::{LibraryStore, unix_to_dt};
use crate::library::types::{Album, HeavyRotationAlbum, LibraryError, Result, Track};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PlayOutcome {
    #[default]
    Completed,
    Skipped,
    Partial,
}

impl PlayOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Skipped => "skipped",
            Self::Partial => "partial",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "completed" => Some(Self::Completed),
            "skipped" => Some(Self::Skipped),
            "partial" => Some(Self::Partial),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PlayContext {
    #[default]
    Manual,
    Album,
    Playlist,
    Radio,
    Autoplay,
    Shuffle,
}

impl PlayContext {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Album => "album",
            Self::Playlist => "playlist",
            Self::Radio => "radio",
            Self::Autoplay => "autoplay",
            Self::Shuffle => "shuffle",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "manual" => Some(Self::Manual),
            "album" => Some(Self::Album),
            "playlist" => Some(Self::Playlist),
            "radio" => Some(Self::Radio),
            "autoplay" => Some(Self::Autoplay),
            "shuffle" => Some(Self::Shuffle),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlayEventRecord {
    pub track: Track,
    pub zone_id: String,
    pub source_context: PlayContext,
    pub started_at: DateTime<Utc>,
    pub played_at: DateTime<Utc>,
    pub listened_sec: i32,
    pub duration_sec: i32,
    pub outcome: PlayOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayHistoryEntry {
    pub id: i64,
    pub track_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub album_id: String,
    pub provider: String,
    pub title: String,
    pub artist: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub album_artist: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub album: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub isrc: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mb_recording_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qobuz_track_id: Option<i64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub artwork_url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub zone_id: String,
    pub source_context: PlayContext,
    pub started_at: DateTime<Utc>,
    pub played_at: DateTime<Utc>,
    pub listened_sec: i32,
    pub duration_sec: i32,
    pub outcome: PlayOutcome,
}

#[derive(Debug, Clone, Default)]
pub struct TrackFeedback {
    pub completions: i32,
    pub skips: i32,
    pub last_played_at: i64,
    pub last_skipped_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopArtistEntry {
    pub artist: String,
    pub plays: i64,
    pub listened_sec: i64,
    pub last_played_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListeningStats {
    pub days: i32,
    pub total_plays: i64,
    pub total_skips: i64,
    pub total_listened_sec: i64,
    pub top_artists: Vec<TopArtistEntry>,
}

struct TrackSnapshot {
    album_id: String,
    title: String,
    artist: String,
    album_artist: String,
    album: String,
    isrc: String,
    mb_recording_id: String,
    qobuz_track_id: Option<i64>,
    artwork_url: String,
    duration_sec: i32,
}

fn resolve_track_snapshot(
    tx: &rusqlite::Transaction<'_>,
    track: &Track,
    duration_override: i32,
) -> Result<TrackSnapshot> {
    let mut snap = TrackSnapshot {
        album_id: track.album_id.clone(),
        title: track.title.clone(),
        artist: track.artist.clone(),
        album_artist: track.album_artist.clone(),
        album: track.album.clone(),
        isrc: track.isrc.clone(),
        mb_recording_id: track.mb_recording_id.clone(),
        qobuz_track_id: (track.qobuz_track_id > 0).then_some(track.qobuz_track_id),
        artwork_url: track.artwork_url.clone(),
        duration_sec: if duration_override > 0 {
            duration_override
        } else {
            track.duration_sec.max(0)
        },
    };

    if let Some(db) = tx
        .query_row(
            "SELECT album_id, title, artist, album_artist, album, isrc, mb_recording_id, qobuz_track_id, duration_sec
             FROM tracks WHERE id = ?1",
            params![track.id],
            |r| {
                Ok(TrackSnapshot {
                    album_id: r.get(0)?,
                    title: r.get(1)?,
                    artist: r.get(2)?,
                    album_artist: r.get(3)?,
                    album: r.get(4)?,
                    isrc: r.get(5)?,
                    mb_recording_id: r.get(6)?,
                    qobuz_track_id: r.get::<_, Option<i64>>(7)?.filter(|&v| v > 0),
                    artwork_url: String::new(),
                    duration_sec: r.get(8)?,
                })
            },
        )
        .optional()?
    {
        snap.album_id = non_empty_or(&snap.album_id, &db.album_id);
        snap.title = non_empty_or(&snap.title, &db.title);
        snap.artist = non_empty_or(&snap.artist, &db.artist);
        snap.album_artist = non_empty_or(&snap.album_artist, &db.album_artist);
        snap.album = non_empty_or(&snap.album, &db.album);
        snap.isrc = non_empty_or(&snap.isrc, &db.isrc);
        snap.mb_recording_id = non_empty_or(&snap.mb_recording_id, &db.mb_recording_id);
        snap.qobuz_track_id = snap.qobuz_track_id.or(db.qobuz_track_id);
        if snap.duration_sec <= 0 {
            snap.duration_sec = db.duration_sec.max(0);
        }
    }

    if snap.artwork_url.is_empty() && !snap.album_id.is_empty() {
        snap.artwork_url = tx
            .query_row(
                "SELECT artwork_url FROM albums WHERE id = ?1",
                params![snap.album_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .unwrap_or_default();
    }

    Ok(snap)
}

impl LibraryStore {
    /// Records a completed, skipped, or partial playback event with full metadata snapshot.
    /// Only `PlayOutcome::Completed` increments `tracks.play_count` and `tracks.last_played_at`.
    pub fn record_play_event(&self, event: &PlayEventRecord) -> Result<()> {
        let track = &event.track;
        if track.id.trim().is_empty() {
            return Ok(());
        }
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_mut().ok_or(LibraryError::NotFound)?;
        let tx = conn.transaction()?;

        let provider = if track.provider.is_empty() {
            track.source.as_str()
        } else {
            track.provider.as_str()
        };
        let snap = resolve_track_snapshot(&tx, track, event.duration_sec)?;
        let listened_sec = event.listened_sec.max(0);
        let started_unix = event.started_at.timestamp();
        let played_unix = event.played_at.timestamp().max(started_unix);

        if event.outcome == PlayOutcome::Completed {
            tx.execute(
                "UPDATE tracks SET play_count = play_count + 1, last_played_at = ?1 WHERE id = ?2",
                params![played_unix, track.id],
            )?;
        }

        tx.execute(
            "INSERT INTO play_events (
                track_id, album_id, provider,
                title, artist, album_artist, album, isrc, mb_recording_id, qobuz_track_id, artwork_url,
                zone_id, source_context, started_at, played_at, listened_sec, duration_sec, outcome
             ) VALUES (
                ?1, ?2, ?3,
                ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                ?12, ?13, ?14, ?15, ?16, ?17, ?18
             )",
            params![
                track.id,
                snap.album_id,
                provider,
                snap.title,
                snap.artist,
                snap.album_artist,
                snap.album,
                snap.isrc,
                snap.mb_recording_id,
                snap.qobuz_track_id,
                snap.artwork_url,
                event.zone_id,
                event.source_context.as_str(),
                started_unix,
                played_unix,
                listened_sec,
                snap.duration_sec,
                event.outcome.as_str(),
            ],
        )?;

        tx.commit()?;
        inner.generation += 1;
        Ok(())
    }

    /// Convenience helper recording a completed manual play right now.
    pub fn record_play(&self, track: &Track) -> Result<()> {
        let now = Utc::now();
        let dur = track.duration_sec.max(0);
        let started = now - chrono::Duration::seconds(i64::from(dur));
        self.record_play_event(&PlayEventRecord {
            track: track.clone(),
            zone_id: String::new(),
            source_context: PlayContext::Manual,
            started_at: started,
            played_at: now,
            listened_sec: dur,
            duration_sec: dur,
            outcome: PlayOutcome::Completed,
        })
    }

    /// Returns library albums ranked by recency- and context-weighted completed plays.
    /// Passive autoplay (0.25x) and radio (0.5x) are down-weighted so unattended listening
    /// does not drown out deliberately played albums.
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
                    SUM(
                      (CASE
                        WHEN pe.played_at >= ?1 THEN 100.0
                        WHEN pe.played_at >= ?2 THEN 70.0
                        WHEN pe.played_at >= ?3 THEN 50.0
                        ELSE 20.0
                      END) *
                      (CASE pe.source_context
                        WHEN 'autoplay' THEN 0.25
                        WHEN 'radio'    THEN 0.50
                        ELSE 1.0
                      END)
                    ) AS frecency_score
             FROM play_events pe
             JOIN albums a ON a.id = pe.album_id
             WHERE pe.played_at >= ?4 AND pe.outcome = 'completed' AND a.available = 1
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
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Paginated listening history across both local and non-library streaming tracks.
    pub fn play_history(
        &self,
        mut limit: i32,
        mut offset: i32,
        outcome: Option<PlayOutcome>,
        zone_id: Option<&str>,
    ) -> Result<(Vec<PlayHistoryEntry>, i64)> {
        if limit <= 0 || limit > 200 {
            limit = 50;
        }
        if offset < 0 {
            offset = 0;
        }
        let outcome_str = outcome.map(|o| o.as_str()).unwrap_or("");
        let zone_str = zone_id.map(str::trim).unwrap_or("");

        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;

        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM play_events
             WHERE (?1 = '' OR outcome = ?1)
               AND (?2 = '' OR zone_id = ?2)",
            params![outcome_str, zone_str],
            |r| r.get(0),
        )?;

        let mut stmt = conn.prepare(
            "SELECT id, track_id, album_id, provider,
                    title, artist, album_artist, album, isrc, mb_recording_id, qobuz_track_id, artwork_url,
                    zone_id, source_context, started_at, played_at, listened_sec, duration_sec, outcome
             FROM play_events
             WHERE (?1 = '' OR outcome = ?1)
               AND (?2 = '' OR zone_id = ?2)
             ORDER BY played_at DESC, id DESC
             LIMIT ?3 OFFSET ?4",
        )?;

        let rows = stmt.query_map(params![outcome_str, zone_str, limit, offset], |r| {
            let ctx_raw: String = r.get(13)?;
            let started_unix: i64 = r.get(14)?;
            let played_unix: i64 = r.get(15)?;
            let out_raw: String = r.get(18)?;
            Ok(PlayHistoryEntry {
                id: r.get(0)?,
                track_id: r.get(1)?,
                album_id: r.get(2)?,
                provider: r.get(3)?,
                title: r.get(4)?,
                artist: r.get(5)?,
                album_artist: r.get(6)?,
                album: r.get(7)?,
                isrc: r.get(8)?,
                mb_recording_id: r.get(9)?,
                qobuz_track_id: r.get(10)?,
                artwork_url: r.get(11)?,
                zone_id: r.get(12)?,
                source_context: PlayContext::parse(&ctx_raw).unwrap_or(PlayContext::Manual),
                started_at: unix_to_dt(started_unix).unwrap_or_else(Utc::now),
                played_at: unix_to_dt(played_unix).unwrap_or_else(Utc::now),
                listened_sec: r.get(16)?,
                duration_sec: r.get(17)?,
                outcome: PlayOutcome::parse(&out_raw).unwrap_or(PlayOutcome::Completed),
            })
        })?;

        let mut items = Vec::new();
        for row in rows {
            items.push(row?);
        }
        Ok((items, total))
    }

    /// Aggregated listening statistics and top artists over the last `days` days.
    pub fn listening_stats(&self, mut days: i32, mut top_limit: i32) -> Result<ListeningStats> {
        if days <= 0 {
            days = 90;
        }
        if top_limit <= 0 || top_limit > 100 {
            top_limit = 20;
        }
        let now = Utc::now().timestamp();
        let since = now - i64::from(days) * 86_400;

        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;

        let (total_plays, total_skips, total_listened_sec): (i64, i64, i64) = conn.query_row(
            "SELECT
                COALESCE(SUM(CASE WHEN outcome = 'completed' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN outcome = 'skipped' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(listened_sec), 0)
             FROM play_events
             WHERE played_at >= ?1",
            params![since],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;

        let mut stmt = conn.prepare(
            "SELECT artist,
                    COUNT(*) AS plays,
                    COALESCE(SUM(listened_sec), 0) AS listened_sec,
                    MAX(played_at) AS last_played
             FROM play_events
             WHERE played_at >= ?1 AND outcome = 'completed' AND artist != ''
             GROUP BY LOWER(artist)
             ORDER BY plays DESC, listened_sec DESC, last_played DESC
             LIMIT ?2",
        )?;

        let rows = stmt.query_map(params![since, top_limit], |r| {
            let last_unix: i64 = r.get(3)?;
            Ok(TopArtistEntry {
                artist: r.get(0)?,
                plays: r.get(1)?,
                listened_sec: r.get(2)?,
                last_played_at: unix_to_dt(last_unix).unwrap_or_else(Utc::now),
            })
        })?;

        let mut top_artists = Vec::new();
        for row in rows {
            top_artists.push(row?);
        }

        Ok(ListeningStats {
            days,
            total_plays,
            total_skips,
            total_listened_sec,
            top_artists,
        })
    }

    /// Track IDs played or skipped since `since_unix` (used for Radio/Autoplay cooldowns).
    pub fn recent_played_track_ids(&self, since_unix: i64) -> Result<HashSet<String>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let mut stmt =
            conn.prepare("SELECT DISTINCT track_id FROM play_events WHERE played_at >= ?1")?;
        let rows = stmt.query_map(params![since_unix], |r| r.get::<_, String>(0))?;
        let mut out = HashSet::new();
        for row in rows {
            out.insert(row?);
        }
        Ok(out)
    }

    /// Lowercased artist -> most recent `played_at` unix timestamp since `since_unix`.
    pub fn recent_played_artists(&self, since_unix: i64) -> Result<HashMap<String, i64>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let mut stmt = conn.prepare(
            "SELECT LOWER(artist), MAX(played_at)
             FROM play_events
             WHERE played_at >= ?1 AND artist != ''
             GROUP BY LOWER(artist)",
        )?;
        let rows = stmt.query_map(params![since_unix], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let (artist, ts) = row?;
            out.insert(artist, ts);
        }
        Ok(out)
    }

    /// Per-track completion and skip counts since `since_unix` for Radio/Autoplay ranking.
    pub fn track_feedback_since(&self, since_unix: i64) -> Result<HashMap<String, TrackFeedback>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let conn = inner.conn.as_ref().ok_or(LibraryError::NotFound)?;
        let mut stmt = conn.prepare(
            "SELECT track_id,
                    SUM(CASE WHEN outcome = 'completed' THEN 1 ELSE 0 END) AS completions,
                    SUM(CASE WHEN outcome = 'skipped' THEN 1 ELSE 0 END) AS skips,
                    MAX(CASE WHEN outcome = 'completed' THEN played_at ELSE 0 END) AS last_played,
                    MAX(CASE WHEN outcome = 'skipped' THEN played_at ELSE 0 END) AS last_skipped
             FROM play_events
             WHERE played_at >= ?1
             GROUP BY track_id",
        )?;
        let rows = stmt.query_map(params![since_unix], |r| {
            Ok((
                r.get::<_, String>(0)?,
                TrackFeedback {
                    completions: r.get(1)?,
                    skips: r.get(2)?,
                    last_played_at: r.get(3)?,
                    last_skipped_at: r.get(4)?,
                },
            ))
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let (id, fb) = row?;
            out.insert(id, fb);
        }
        Ok(out)
    }
}

fn non_empty_or(primary: &str, fallback: &str) -> String {
    if !primary.trim().is_empty() {
        primary.to_string()
    } else {
        fallback.to_string()
    }
}
