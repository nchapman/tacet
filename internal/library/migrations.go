package library

import (
	"context"
	"database/sql"
	"fmt"
	"time"
)

type migration struct {
	version int
	name    string
	sql     string
}

// migrations is the append-only, ordered list of SQLite schema migrations.
// Each migration executes inside its own transaction and is recorded in
// schema_migrations (and mirrored to PRAGMA user_version for CLI inspection).
var migrations = []migration{
	{
		version: 1,
		name:    "initial_unified_library",
		sql: `
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
`,
	},
	{
		version: 2,
		name:    "app_prefs_and_credentials",
		sql: `
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
`,
	},
}

func runMigrations(ctx context.Context, db *sql.DB) error {
	if _, err := db.ExecContext(ctx, `
CREATE TABLE IF NOT EXISTS schema_migrations (
	version    INTEGER PRIMARY KEY,
	name       TEXT NOT NULL,
	applied_at INTEGER NOT NULL
);`); err != nil {
		return fmt.Errorf("create schema_migrations table: %w", err)
	}

	applied := make(map[int]bool)
	rows, err := db.QueryContext(ctx, `SELECT version FROM schema_migrations`)
	if err != nil {
		return fmt.Errorf("query applied migrations: %w", err)
	}
	for rows.Next() {
		var v int
		if err := rows.Scan(&v); err != nil {
			_ = rows.Close()
			return err
		}
		applied[v] = true
	}
	_ = rows.Close()
	if err := rows.Err(); err != nil {
		return err
	}

	now := time.Now().UTC().Unix()
	for _, m := range migrations {
		if applied[m.version] {
			continue
		}
		tx, err := db.BeginTx(ctx, nil)
		if err != nil {
			return fmt.Errorf("begin migration %d (%s): %w", m.version, m.name, err)
		}
		if _, err := tx.ExecContext(ctx, m.sql); err != nil {
			_ = tx.Rollback()
			return fmt.Errorf("apply migration %d (%s): %w", m.version, m.name, err)
		}
		if _, err := tx.ExecContext(ctx,
			`INSERT INTO schema_migrations(version, name, applied_at) VALUES (?, ?, ?)`,
			m.version, m.name, now,
		); err != nil {
			_ = tx.Rollback()
			return fmt.Errorf("record migration %d (%s): %w", m.version, m.name, err)
		}
		if _, err := tx.ExecContext(ctx, fmt.Sprintf("PRAGMA user_version = %d;", m.version)); err != nil {
			_ = tx.Rollback()
			return fmt.Errorf("update user_version to %d: %w", m.version, err)
		}
		if err := tx.Commit(); err != nil {
			return fmt.Errorf("commit migration %d (%s): %w", m.version, m.name, err)
		}
	}
	return nil
}
