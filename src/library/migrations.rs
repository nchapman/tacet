use std::collections::HashSet;

use chrono::Utc;
use rusqlite::{Connection, params};

use super::types::Result;

struct Migration {
    version: i32,
    name: &'static str,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial_unified_library",
        sql: r#"
CREATE TABLE IF NOT EXISTS albums (
    id                  TEXT PRIMARY KEY,
    source_type         TEXT NOT NULL,
    provider            TEXT NOT NULL,
    external_id         TEXT NOT NULL,
    title               TEXT NOT NULL,
    sort_title          TEXT NOT NULL,
    artist              TEXT NOT NULL,
    sort_artist         TEXT NOT NULL,
    genre               TEXT NOT NULL DEFAULT '',
    year                INTEGER NOT NULL DEFAULT 0,
    track_count         INTEGER NOT NULL DEFAULT 0,
    duration_sec        INTEGER NOT NULL DEFAULT 0,
    artwork_url         TEXT NOT NULL DEFAULT '',
    primary_format      TEXT NOT NULL DEFAULT '',
    max_bit_depth       INTEGER NOT NULL DEFAULT 0,
    max_sample_rate     INTEGER NOT NULL DEFAULT 0,
    version_key         TEXT NOT NULL DEFAULT '',
    version_group_id    TEXT NOT NULL DEFAULT '',
    version_excluded    INTEGER NOT NULL DEFAULT 0,
    is_primary_override INTEGER NOT NULL DEFAULT 0,
    is_primary          INTEGER NOT NULL DEFAULT 1,
    is_partial          INTEGER NOT NULL DEFAULT 0,
    available           INTEGER NOT NULL DEFAULT 1,
    missing_since       INTEGER NOT NULL DEFAULT 0,
    added_at            INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_albums_provider ON albums(provider, available);
CREATE INDEX IF NOT EXISTS idx_albums_source ON albums(source_type, available);
CREATE INDEX IF NOT EXISTS idx_albums_sort_artist ON albums(sort_artist, sort_title);
CREATE INDEX IF NOT EXISTS idx_albums_sort_title ON albums(sort_title);
CREATE INDEX IF NOT EXISTS idx_albums_year ON albums(year DESC, sort_title);
CREATE INDEX IF NOT EXISTS idx_albums_added ON albums(added_at DESC);
CREATE INDEX IF NOT EXISTS idx_albums_version_key ON albums(version_key);
CREATE INDEX IF NOT EXISTS idx_albums_version_group ON albums(version_group_id);
CREATE INDEX IF NOT EXISTS idx_albums_genre ON albums(genre);
CREATE INDEX IF NOT EXISTS idx_albums_missing ON albums(provider, missing_since);

CREATE TABLE IF NOT EXISTS tracks (
    id             TEXT PRIMARY KEY,
    album_id       TEXT NOT NULL,
    source_type    TEXT NOT NULL,
    provider       TEXT NOT NULL,
    external_id    TEXT NOT NULL,
    title          TEXT NOT NULL,
    sort_title     TEXT NOT NULL,
    artist         TEXT NOT NULL,
    sort_artist    TEXT NOT NULL,
    album_artist   TEXT NOT NULL DEFAULT '',
    album          TEXT NOT NULL,
    sort_album     TEXT NOT NULL,
    genre          TEXT NOT NULL DEFAULT '',
    disc_number    INTEGER NOT NULL DEFAULT 0,
    track_number   INTEGER NOT NULL DEFAULT 0,
    year           INTEGER NOT NULL DEFAULT 0,
    duration_sec   INTEGER NOT NULL DEFAULT 0,
    sample_rate    INTEGER NOT NULL DEFAULT 0,
    bit_depth      INTEGER NOT NULL DEFAULT 0,
    channels       INTEGER NOT NULL DEFAULT 0,
    mime           TEXT NOT NULL DEFAULT '',
    ext            TEXT NOT NULL DEFAULT '',
    size_bytes     INTEGER NOT NULL DEFAULT 0,
    artwork_url    TEXT NOT NULL DEFAULT '',
    file_path      TEXT NOT NULL DEFAULT '',
    file_mtime_ns  INTEGER NOT NULL DEFAULT 0,
    folder_art     TEXT NOT NULL DEFAULT '',
    has_embed_art  INTEGER NOT NULL DEFAULT 0,
    qobuz_track_id INTEGER NOT NULL DEFAULT 0,
    play_count     INTEGER NOT NULL DEFAULT 0,
    last_played_at INTEGER NOT NULL DEFAULT 0,
    loved          INTEGER NOT NULL DEFAULT 0,
    available      INTEGER NOT NULL DEFAULT 1,
    missing_since  INTEGER NOT NULL DEFAULT 0,
    added_at       INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    FOREIGN KEY (album_id) REFERENCES albums(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_tracks_album ON tracks(album_id, disc_number, track_number);
CREATE INDEX IF NOT EXISTS idx_tracks_provider ON tracks(provider, available);
CREATE INDEX IF NOT EXISTS idx_tracks_file_path ON tracks(file_path);
CREATE INDEX IF NOT EXISTS idx_tracks_sort_artist ON tracks(sort_artist, sort_album, disc_number, track_number);
CREATE INDEX IF NOT EXISTS idx_tracks_sort_title ON tracks(sort_title);
CREATE INDEX IF NOT EXISTS idx_tracks_genre ON tracks(genre);
CREATE INDEX IF NOT EXISTS idx_tracks_added ON tracks(added_at DESC);
CREATE INDEX IF NOT EXISTS idx_tracks_plays ON tracks(play_count DESC);
CREATE INDEX IF NOT EXISTS idx_tracks_missing ON tracks(provider, missing_since);

CREATE TABLE IF NOT EXISTS play_events (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    track_id  TEXT NOT NULL,
    album_id  TEXT NOT NULL,
    provider  TEXT NOT NULL,
    played_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_play_events_recent ON play_events(played_at DESC, album_id);

CREATE TABLE IF NOT EXISTS provider_state (
    provider     TEXT PRIMARY KEY,
    source_type  TEXT NOT NULL,
    last_sync_at INTEGER NOT NULL DEFAULT 0,
    last_error   TEXT NOT NULL DEFAULT ''
);

CREATE VIRTUAL TABLE IF NOT EXISTS albums_fts USING fts5(
    album_id UNINDEXED,
    title,
    artist,
    genre,
    tokenize='unicode61 remove_diacritics 2'
);

CREATE VIRTUAL TABLE IF NOT EXISTS tracks_fts USING fts5(
    track_id UNINDEXED,
    title,
    artist,
    album_artist,
    album,
    genre,
    tokenize='unicode61 remove_diacritics 2'
);
"#,
    },
    Migration {
        version: 2,
        name: "app_prefs_and_credentials",
        sql: r#"
CREATE TABLE IF NOT EXISTS app_prefs (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS provider_credentials (
    provider   TEXT PRIMARY KEY,
    data_json  TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
"#,
    },
    Migration {
        version: 3,
        name: "metadata_enrichment_and_sonic",
        sql: r#"
ALTER TABLE albums ADD COLUMN upc                 TEXT NOT NULL DEFAULT '';
ALTER TABLE albums ADD COLUMN label               TEXT NOT NULL DEFAULT '';
ALTER TABLE albums ADD COLUMN release_date        TEXT NOT NULL DEFAULT '';
ALTER TABLE albums ADD COLUMN qobuz_artist_id     INTEGER NOT NULL DEFAULT 0;
ALTER TABLE albums ADD COLUMN qobuz_genre_id      INTEGER NOT NULL DEFAULT 0;
ALTER TABLE albums ADD COLUMN qobuz_label_id      INTEGER NOT NULL DEFAULT 0;
ALTER TABLE albums ADD COLUMN mb_release_id       TEXT NOT NULL DEFAULT '';
ALTER TABLE albums ADD COLUMN mb_release_group_id TEXT NOT NULL DEFAULT '';
ALTER TABLE albums ADD COLUMN mb_artist_id        TEXT NOT NULL DEFAULT '';

CREATE INDEX IF NOT EXISTS idx_albums_upc ON albums(upc) WHERE upc != '';
CREATE INDEX IF NOT EXISTS idx_albums_mb_release_group ON albums(mb_release_group_id) WHERE mb_release_group_id != '';
CREATE INDEX IF NOT EXISTS idx_albums_mb_artist ON albums(mb_artist_id) WHERE mb_artist_id != '';

ALTER TABLE tracks ADD COLUMN isrc            TEXT NOT NULL DEFAULT '';
ALTER TABLE tracks ADD COLUMN mb_recording_id TEXT NOT NULL DEFAULT '';
ALTER TABLE tracks ADD COLUMN mb_artist_id    TEXT NOT NULL DEFAULT '';
ALTER TABLE tracks ADD COLUMN qobuz_artist_id INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tracks ADD COLUMN qobuz_genre_id  INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tracks ADD COLUMN qobuz_label_id  INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tracks ADD COLUMN replaygain_gain REAL;
ALTER TABLE tracks ADD COLUMN replaygain_peak REAL;
ALTER TABLE tracks ADD COLUMN composer        TEXT NOT NULL DEFAULT '';
ALTER TABLE tracks ADD COLUMN performers      TEXT NOT NULL DEFAULT '';

CREATE INDEX IF NOT EXISTS idx_tracks_isrc ON tracks(isrc) WHERE isrc != '';
CREATE INDEX IF NOT EXISTS idx_tracks_mb_recording ON tracks(mb_recording_id) WHERE mb_recording_id != '';
CREATE INDEX IF NOT EXISTS idx_tracks_mb_artist ON tracks(mb_artist_id) WHERE mb_artist_id != '';

CREATE TABLE IF NOT EXISTS album_enrichment (
    album_id    TEXT PRIMARY KEY REFERENCES albums(id) ON DELETE CASCADE,
    status      TEXT NOT NULL DEFAULT 'pending',
    confidence  REAL NOT NULL DEFAULT 0.0,
    matched_by  TEXT NOT NULL DEFAULT '[]',
    resolved_at INTEGER NOT NULL DEFAULT 0,
    retry_after INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS track_qobuz_match (
    track_id        TEXT PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
    qobuz_track_id  INTEGER NOT NULL DEFAULT 0,
    qobuz_artist_id INTEGER NOT NULL DEFAULT 0,
    qobuz_genre_id  INTEGER NOT NULL DEFAULT 0,
    qobuz_label_id  INTEGER NOT NULL DEFAULT 0,
    match_method    TEXT NOT NULL DEFAULT '',
    matched_at      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS track_sonic (
    track_id      TEXT PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
    model_version TEXT NOT NULL,
    file_mtime_ns INTEGER NOT NULL DEFAULT 0,
    emb_uint8     BLOB NOT NULL,
    emb_min       REAL NOT NULL,
    emb_scale     REAL NOT NULL,
    bpm           REAL,
    musical_key   TEXT NOT NULL DEFAULT '',
    analyzed_at   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS track_sonic_heads (
    track_id        TEXT PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,
    heads_version   INTEGER NOT NULL DEFAULT 1,
    engagement      REAL,
    approachability REAL,
    instrumental    REAL,
    danceability    REAL,
    mood_relaxed    REAL,
    mood_acoustic   REAL,
    mood_aggressive REAL,
    mood_electronic REAL,
    computed_at     INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS entity_tags (
    entity_type TEXT NOT NULL,
    entity_id   TEXT NOT NULL,
    kind        TEXT NOT NULL,
    tag         TEXT NOT NULL,
    score       REAL NOT NULL DEFAULT 1.0,
    source      TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (entity_type, entity_id, kind, tag)
);

CREATE INDEX IF NOT EXISTS idx_entity_tags_lookup
    ON entity_tags(entity_type, kind, tag, score DESC);

CREATE TRIGGER IF NOT EXISTS trg_albums_delete_entity_tags
AFTER DELETE ON albums
BEGIN
    DELETE FROM entity_tags WHERE entity_type = 'album' AND entity_id = OLD.id;
END;

CREATE TRIGGER IF NOT EXISTS trg_tracks_delete_entity_tags
AFTER DELETE ON tracks
BEGIN
    DELETE FROM entity_tags WHERE entity_type = 'track' AND entity_id = OLD.id;
END;
"#,
    },
    Migration {
        version: 4,
        name: "listening_history_v2",
        sql: r#"
CREATE TABLE play_events_v4 (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    track_id       TEXT NOT NULL,
    album_id       TEXT NOT NULL DEFAULT '',
    provider       TEXT NOT NULL DEFAULT '',
    title          TEXT NOT NULL DEFAULT '',
    artist         TEXT NOT NULL DEFAULT '',
    album_artist   TEXT NOT NULL DEFAULT '',
    album          TEXT NOT NULL DEFAULT '',
    isrc            TEXT NOT NULL DEFAULT '',
    mb_recording_id TEXT NOT NULL DEFAULT '',
    qobuz_track_id  INTEGER,
    artwork_url    TEXT NOT NULL DEFAULT '',
    zone_id        TEXT NOT NULL DEFAULT '',
    source_context TEXT NOT NULL DEFAULT 'manual',
    started_at     INTEGER NOT NULL,
    played_at      INTEGER NOT NULL,
    listened_sec   INTEGER NOT NULL DEFAULT 0,
    duration_sec   INTEGER NOT NULL DEFAULT 0,
    outcome        TEXT NOT NULL DEFAULT 'completed'
);

INSERT INTO play_events_v4 (
    id, track_id, album_id, provider,
    title, artist, album_artist, album, isrc, mb_recording_id, qobuz_track_id, artwork_url,
    zone_id, source_context, started_at, played_at, listened_sec, duration_sec, outcome
)
SELECT
    pe.id,
    pe.track_id,
    pe.album_id,
    pe.provider,
    COALESCE(t.title, ''),
    COALESCE(t.artist, ''),
    COALESCE(t.album_artist, ''),
    COALESCE(t.album, a.title, ''),
    COALESCE(t.isrc, ''),
    COALESCE(t.mb_recording_id, ''),
    t.qobuz_track_id,
    COALESCE(a.artwork_url, ''),
    '',
    'manual',
    CASE
        WHEN COALESCE(t.duration_sec, 0) > 0 AND pe.played_at > t.duration_sec
            THEN pe.played_at - t.duration_sec
        ELSE pe.played_at
    END,
    pe.played_at,
    COALESCE(t.duration_sec, 0),
    COALESCE(t.duration_sec, 0),
    'completed'
FROM play_events pe
LEFT JOIN tracks t ON t.id = pe.track_id
LEFT JOIN albums a ON a.id = pe.album_id;

DROP TABLE play_events;
ALTER TABLE play_events_v4 RENAME TO play_events;

CREATE INDEX IF NOT EXISTS idx_play_events_recent
    ON play_events(played_at DESC, outcome);
CREATE INDEX IF NOT EXISTS idx_play_events_album
    ON play_events(album_id, played_at DESC);
CREATE INDEX IF NOT EXISTS idx_play_events_track
    ON play_events(track_id, played_at DESC);
CREATE INDEX IF NOT EXISTS idx_play_events_artist
    ON play_events(artist COLLATE NOCASE, played_at DESC);
CREATE INDEX IF NOT EXISTS idx_play_events_isrc
    ON play_events(isrc, played_at DESC)
    WHERE isrc != '';
CREATE INDEX IF NOT EXISTS idx_play_events_mbid
    ON play_events(mb_recording_id, played_at DESC)
    WHERE mb_recording_id != '';
"#,
    },
];

pub fn run_migrations(conn: &mut Connection) -> Result<()> {
    conn.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS schema_migrations (
    version    INTEGER PRIMARY KEY,
    name       TEXT NOT NULL,
    applied_at INTEGER NOT NULL
);
"#,
    )?;

    let mut applied = HashSet::new();
    {
        let mut stmt = conn.prepare("SELECT version FROM schema_migrations")?;
        let rows = stmt.query_map([], |r| r.get::<_, i32>(0))?;
        for v in rows {
            applied.insert(v?);
        }
    }

    let now = Utc::now().timestamp();
    for m in MIGRATIONS {
        if applied.contains(&m.version) {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(m.sql)?;
        tx.execute(
            "INSERT INTO schema_migrations(version, name, applied_at) VALUES (?1, ?2, ?3)",
            params![m.version, m.name, now],
        )?;
        tx.execute_batch(&format!("PRAGMA user_version = {};", m.version))?;
        tx.commit()?;
    }
    Ok(())
}
