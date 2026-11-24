package library

import (
	"context"
	"crypto/rand"
	"database/sql"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"
	"unicode"

	_ "modernc.org/sqlite" // Pure-Go zero-CGo SQLite driver.
)

// Well-known keys stored in the SQLite app_prefs KV table.
const (
	PrefStreamMode             = "stream_mode"
	PrefQobuzMaxQuality        = "qobuz_max_quality"
	PrefManualRenderers        = "manual_renderers"
	PrefZoneQualities          = "zone_qualities"
	PrefZoneQueues             = "zone_queues"
	PrefLibraryWatchEnabled    = "library_watch_enabled"
	PrefLibraryScanIntervalMin = "library_scan_interval_min"
)

// Defaults for automatic local library scanning.
const (
	DefaultLibraryWatchEnabled    = true
	DefaultLibraryScanIntervalMin = 60
	MaxLibraryScanIntervalMin     = 10080 // 7 days
)

const (
	// DefaultMissingRetention is how long soft-deleted missing music is preserved
	// before a scan permanently purges it (protects against temporary NAS drive disconnects).
	DefaultMissingRetention = 30 * 24 * time.Hour
)

var (
	// ErrNotFound is returned when a requested library item does not exist.
	ErrNotFound = errors.New("library item not found")
	// ErrProviderNotFound is returned when a requested remote source is not registered.
	ErrProviderNotFound = errors.New("remote source provider not registered")
	// ErrProviderOffline is returned when a remote source is not authenticated/connected.
	ErrProviderOffline = errors.New("remote source provider is not connected")
	// ErrInvalidArgument is returned for invalid library mutations (e.g. grouping <2 albums).
	ErrInvalidArgument = errors.New("invalid library argument")
)

type libraryStore struct {
	mu               sync.RWMutex
	db               *sql.DB
	closed           bool
	dbPath           string
	bootID           string
	gen              uint64
	missingRetention time.Duration
	nowFn            func() time.Time
}

func openStore(dbPath string) (*libraryStore, error) {
	dsn := ":memory:"
	if strings.TrimSpace(dbPath) != "" && dbPath != ":memory:" {
		abs, err := filepath.Abs(dbPath)
		if err != nil {
			return nil, fmt.Errorf("resolve db path: %w", err)
		}
		dir := filepath.Dir(abs)
		if err := os.MkdirAll(dir, 0o700); err != nil {
			return nil, fmt.Errorf("create data dir: %w", err)
		}
		// Tighten data directory permissions because the DB stores provider credentials.
		_ = os.Chmod(dir, 0o700) //nolint:gosec // directory requires owner execute bit (0700) to traverse
		dsn = abs
	}

	db, err := sql.Open("sqlite", dsn)
	if err != nil {
		return nil, fmt.Errorf("open sqlite %s: %w", dsn, err)
	}
	// Single connection eliminates SQLITE_BUSY contention across goroutines
	// and keeps :memory: databases shared across all calls.
	db.SetMaxOpenConns(1)
	db.SetMaxIdleConns(1)

	ctx := context.Background()
	pragmas := []string{
		"PRAGMA foreign_keys = ON;",
		"PRAGMA busy_timeout = 5000;",
		"PRAGMA synchronous = NORMAL;",
	}
	if dsn != ":memory:" {
		pragmas = append([]string{"PRAGMA journal_mode = WAL;"}, pragmas...)
	}
	for _, p := range pragmas {
		if _, err := db.ExecContext(ctx, p); err != nil {
			_ = db.Close()
			if dsn != ":memory:" {
				return nil, fmt.Errorf("sqlite pragma %q on %s (uid=%d gid=%d): %w", p, dsn, os.Getuid(), os.Getgid(), err)
			}
			return nil, fmt.Errorf("sqlite pragma %q: %w", p, err)
		}
	}

	var nonce [4]byte
	_, _ = rand.Read(nonce[:])
	s := &libraryStore{
		db:               db,
		dbPath:           dsn,
		bootID:           hex.EncodeToString(nonce[:]),
		missingRetention: DefaultMissingRetention,
		nowFn:            func() time.Time { return time.Now().UTC() },
	}
	if err := runMigrations(ctx, db); err != nil {
		_ = db.Close()
		return nil, err
	}
	if dsn != ":memory:" {
		for _, p := range []string{dsn, dsn + "-wal", dsn + "-shm"} {
			if _, statErr := os.Stat(p); statErr == nil {
				_ = os.Chmod(p, 0o600)
			}
		}
	}
	return s, nil
}

func (s *libraryStore) Close() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.closed || s.db == nil {
		return nil
	}
	s.closed = true
	// Keep s.db non-nil so any concurrent background goroutine still in flight
	// receives sql.ErrConnDone cleanly instead of a nil-pointer panic.
	return s.db.Close()
}

func (s *libraryStore) now() time.Time {
	if s.nowFn != nil {
		return s.nowFn().UTC()
	}
	return time.Now().UTC()
}

func (s *libraryStore) setMissingRetention(d time.Duration) {
	s.mu.Lock()
	s.missingRetention = d
	s.mu.Unlock()
}

func (s *libraryStore) schemaVersion(ctx context.Context) (int, error) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	var ver int
	err := s.db.QueryRowContext(ctx, "PRAGMA user_version;").Scan(&ver)
	return ver, err
}

type localFileCacheEntry struct {
	Track     Track
	Mtime     int64
	Size      int64
	Available bool
}

func (s *libraryStore) loadLocalFileCache(ctx context.Context) (map[string]localFileCacheEntry, error) {
	s.mu.RLock()
	defer s.mu.RUnlock()

	rows, err := s.db.QueryContext(ctx, `
SELECT id, album_id, title, artist, album_artist, album, genre,
       disc_number, track_number, year, duration_sec,
       sample_rate, bit_depth, channels, mime, ext, size_bytes,
       artwork_url, file_path, file_mtime_ns, folder_art, has_embed_art, available, added_at
FROM tracks
WHERE provider = 'local' AND file_path != ''`)
	if err != nil {
		return nil, err
	}
	defer func() { _ = rows.Close() }()

	cache := make(map[string]localFileCacheEntry)
	for rows.Next() {
		var t Track
		var hasEmbed, avail int
		var addedUnix int64
		if err := rows.Scan(
			&t.ID, &t.AlbumID, &t.Title, &t.Artist, &t.AlbumArtist, &t.Album, &t.Genre,
			&t.DiscNumber, &t.TrackNumber, &t.Year, &t.DurationSec,
			&t.SampleRate, &t.BitDepth, &t.Channels, &t.MIME, &t.Ext, &t.Size,
			&t.ArtworkURL, &t.FilePath, &t.FileMtimeNS, &t.FolderArt, &hasEmbed, &avail, &addedUnix,
		); err != nil {
			return nil, err
		}
		t.Source = SourceLocal
		t.SourceType = SourceTypeLocal
		t.Provider = string(SourceLocal)
		t.ExternalID = t.FilePath
		t.HasEmbedArt = hasEmbed != 0
		t.Available = true
		t.InLibrary = true
		if addedUnix > 0 {
			ts := time.Unix(addedUnix, 0).UTC()
			t.AddedAt = &ts
		}
		cache[t.FilePath] = localFileCacheEntry{
			Track:     t,
			Mtime:     t.FileMtimeNS,
			Size:      t.Size,
			Available: avail != 0,
		}
	}
	return cache, rows.Err()
}

// touchProviderSync records a clean no-op sync timestamp without rewriting rows
// when an incremental scan verified that zero files were added, modified, or removed.
func (s *libraryStore) touchProviderSync(ctx context.Context, sourceType, provider string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	now := s.now().Unix()
	_, err := s.db.ExecContext(ctx, `
INSERT INTO provider_state(provider, source_type, last_sync_at, last_error)
VALUES (?, ?, ?, '')
ON CONFLICT(provider) DO UPDATE SET last_sync_at = excluded.last_sync_at, last_error = ''`,
		provider, sourceType, now)
	return err
}

// replaceProvider synchronizes all tracks & albums belonging to a single provider.
// Previously indexed rows absent from this pass are soft-deleted (available=0,
// missing_since=now) so temporary NAS drive disconnects never wipe play counts,
// history, or version groupings. Soft-deleted rows are permanently purged only
// after missingRetention elapses (default 30 days).
func (s *libraryStore) replaceProvider(ctx context.Context, sourceType, provider string, albums []Album, tracks []Track, syncErr error) error {
	s.mu.Lock()
	defer s.mu.Unlock()

	now := s.now().Unix()
	retention := s.missingRetention

	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		return err
	}
	defer func() { _ = tx.Rollback() }()

	if syncErr != nil {
		if _, execErr := tx.ExecContext(ctx, `
INSERT INTO provider_state(provider, source_type, last_sync_at, last_error)
VALUES (?, ?, ?, ?)
ON CONFLICT(provider) DO UPDATE SET last_error = excluded.last_error`,
			provider, sourceType, now, syncErr.Error(),
		); execErr != nil {
			return execErr
		}
		return tx.Commit()
	}

	existingAlbumIDs := make(map[string]struct{})
	if aRows, qErr := tx.QueryContext(ctx, `SELECT id FROM albums WHERE provider = ?`, provider); qErr == nil {
		for aRows.Next() {
			var id string
			if aRows.Scan(&id) == nil {
				existingAlbumIDs[id] = struct{}{}
			}
		}
		_ = aRows.Close()
	}

	existingTrackIDs := make(map[string]struct{})
	if tRows, qErr := tx.QueryContext(ctx, `SELECT id FROM tracks WHERE provider = ?`, provider); qErr == nil {
		for tRows.Next() {
			var id string
			if tRows.Scan(&id) == nil {
				existingTrackIDs[id] = struct{}{}
			}
		}
		_ = tRows.Close()
	}

	albStmt, err := tx.PrepareContext(ctx, `
INSERT INTO albums(
	id, source_type, provider, external_id,
	title, sort_title, artist, sort_artist, genre, year,
	track_count, duration_sec, artwork_url,
	primary_format, max_bit_depth, max_sample_rate,
	version_key, is_partial, available, missing_since, added_at, updated_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(id) DO UPDATE SET
	title = excluded.title,
	sort_title = excluded.sort_title,
	artist = excluded.artist,
	sort_artist = excluded.sort_artist,
	genre = excluded.genre,
	year = excluded.year,
	track_count = excluded.track_count,
	duration_sec = excluded.duration_sec,
	artwork_url = excluded.artwork_url,
	primary_format = excluded.primary_format,
	max_bit_depth = excluded.max_bit_depth,
	max_sample_rate = excluded.max_sample_rate,
	version_key = excluded.version_key,
	is_partial = excluded.is_partial,
	available = excluded.available,
	missing_since = CASE WHEN excluded.available = 1 THEN 0 WHEN albums.missing_since > 0 THEN albums.missing_since ELSE excluded.missing_since END,
	updated_at = excluded.updated_at`)
	if err != nil {
		return err
	}
	defer func() { _ = albStmt.Close() }()

	seenAlbums := make(map[string]struct{}, len(albums))
	for _, a := range albums {
		seenAlbums[a.ID] = struct{}{}
		addedAt := now
		if a.AddedAt != nil && !a.AddedAt.IsZero() {
			addedAt = a.AddedAt.UTC().Unix()
		}
		extID := a.ExternalID
		if extID == "" {
			extID = strings.TrimPrefix(a.ID, provider+":")
		}
		vKey := VersionKey(a.Artist, a.Title)
		missingSince := int64(0)
		if !a.Available {
			missingSince = now
		}
		if _, err = albStmt.ExecContext(ctx,
			a.ID, sourceType, provider, extID,
			a.Title, SortKey(a.Title), a.Artist, SortKey(a.Artist), a.Genre, a.Year,
			a.TrackCount, a.DurationSec, a.ArtworkURL,
			a.PrimaryFormat, a.MaxBit, a.MaxRate,
			vKey, boolToInt(a.IsPartial), boolToInt(a.Available), missingSince, addedAt, now,
		); err != nil {
			return fmt.Errorf("upsert album %s: %w", a.ID, err)
		}
	}

	trkStmt, err := tx.PrepareContext(ctx, `
INSERT INTO tracks(
	id, album_id, source_type, provider, external_id,
	title, sort_title, artist, sort_artist, album_artist, album, sort_album,
	genre, disc_number, track_number, year, duration_sec,
	sample_rate, bit_depth, channels, mime, ext, size_bytes,
	artwork_url, file_path, file_mtime_ns, folder_art, has_embed_art, qobuz_track_id,
	available, missing_since, added_at, updated_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, 0, ?, ?)
ON CONFLICT(id) DO UPDATE SET
	album_id = excluded.album_id,
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
	artwork_url = excluded.artwork_url,
	file_path = excluded.file_path,
	file_mtime_ns = excluded.file_mtime_ns,
	folder_art = excluded.folder_art,
	has_embed_art = excluded.has_embed_art,
	qobuz_track_id = excluded.qobuz_track_id,
	available = 1,
	missing_since = 0,
	updated_at = excluded.updated_at`)
	if err != nil {
		return err
	}
	defer func() { _ = trkStmt.Close() }()

	seenTracks := make(map[string]struct{}, len(tracks))
	for _, t := range tracks {
		seenTracks[t.ID] = struct{}{}
		addedAt := now
		if t.AddedAt != nil && !t.AddedAt.IsZero() {
			addedAt = t.AddedAt.UTC().Unix()
		}
		extID := t.ExternalID
		if extID == "" {
			extID = strings.TrimPrefix(t.ID, provider+":")
		}
		albArtist := t.AlbumArtist
		if albArtist == "" {
			albArtist = t.Artist
		}
		if _, err = trkStmt.ExecContext(ctx,
			t.ID, t.AlbumID, sourceType, provider, extID,
			t.Title, SortKey(t.Title), t.Artist, SortKey(albArtist), t.AlbumArtist, t.Album, SortKey(t.Album),
			t.Genre, t.DiscNumber, t.TrackNumber, t.Year, t.DurationSec,
			t.SampleRate, t.BitDepth, t.Channels, t.MIME, t.Ext, t.Size,
			t.ArtworkURL, t.FilePath, t.FileMtimeNS, t.FolderArt, boolToInt(t.HasEmbedArt), t.QobuzTrackID,
			addedAt, now,
		); err != nil {
			return fmt.Errorf("upsert track %s: %w", t.ID, err)
		}
	}

	// Soft-delete tracks and albums that were not seen in this pass (preserving first missing_since).
	softTrackStmt, err := tx.PrepareContext(ctx, `
UPDATE tracks
SET available = 0,
    missing_since = CASE WHEN missing_since = 0 THEN ? ELSE missing_since END,
    updated_at = ?
WHERE id = ?`)
	if err != nil {
		return err
	}
	defer func() { _ = softTrackStmt.Close() }()

	for id := range existingTrackIDs {
		if _, seen := seenTracks[id]; !seen {
			if _, err = softTrackStmt.ExecContext(ctx, now, now, id); err != nil {
				return err
			}
		}
	}

	softAlbumStmt, err := tx.PrepareContext(ctx, `
UPDATE albums
SET available = 0,
    missing_since = CASE WHEN missing_since = 0 THEN ? ELSE missing_since END,
    updated_at = ?
WHERE id = ?`)
	if err != nil {
		return err
	}
	defer func() { _ = softAlbumStmt.Close() }()

	for id := range existingAlbumIDs {
		if _, seen := seenAlbums[id]; !seen {
			if _, err := softAlbumStmt.ExecContext(ctx, now, now, id); err != nil {
				return err
			}
		}
	}

	// Permanently purge rows that have been missing longer than the retention window.
	if retention > 0 {
		cutoff := now - int64(retention.Seconds())
		if _, err := tx.ExecContext(ctx, `
DELETE FROM tracks
WHERE provider = ? AND available = 0 AND missing_since > 0 AND missing_since <= ?`,
			provider, cutoff); err != nil {
			return err
		}
		if _, err := tx.ExecContext(ctx, `
DELETE FROM albums
WHERE provider = ? AND available = 0 AND missing_since > 0 AND missing_since <= ?`,
			provider, cutoff); err != nil {
			return err
		}
	}

	// Refresh FTS indexes for this provider from currently available rows only.
	if _, err := tx.ExecContext(ctx, `DELETE FROM tracks_fts WHERE track_id IN (SELECT id FROM tracks WHERE provider = ?)`, provider); err != nil {
		return err
	}
	if _, err := tx.ExecContext(ctx, `
INSERT INTO tracks_fts(track_id, title, artist, album_artist, album, genre)
SELECT id, title, artist, album_artist, album, genre FROM tracks WHERE provider = ? AND available = 1`, provider); err != nil {
		return err
	}
	if _, err := tx.ExecContext(ctx, `DELETE FROM albums_fts WHERE album_id IN (SELECT id FROM albums WHERE provider = ?)`, provider); err != nil {
		return err
	}
	if _, err := tx.ExecContext(ctx, `
INSERT INTO albums_fts(album_id, title, artist, genre)
SELECT id, title, artist, genre FROM albums WHERE provider = ? AND available = 1`, provider); err != nil {
		return err
	}

	if _, err := tx.ExecContext(ctx, `
INSERT INTO provider_state(provider, source_type, last_sync_at, last_error)
VALUES (?, ?, ?, '')
ON CONFLICT(provider) DO UPDATE SET last_sync_at = excluded.last_sync_at, last_error = ''`,
		provider, sourceType, now,
	); err != nil {
		return err
	}

	if err := recomputeVersionsTx(ctx, tx); err != nil {
		return err
	}
	if err := tx.Commit(); err != nil {
		return err
	}
	s.gen++
	return nil
}

// upsertAlbumWithTracks inserts or updates a single album and its tracks (e.g. "+ Add to Library").
func (s *libraryStore) upsertAlbumWithTracks(ctx context.Context, alb Album, tracks []Track) error {
	s.mu.Lock()
	defer s.mu.Unlock()

	now := time.Now().UTC().Unix()
	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		return err
	}
	defer func() { _ = tx.Rollback() }()

	if err := upsertAlbumTx(ctx, tx, alb, now); err != nil {
		return err
	}
	for _, t := range tracks {
		if t.AlbumID == "" {
			t.AlbumID = alb.ID
		}
		if err := upsertTrackTx(ctx, tx, t, now); err != nil {
			return err
		}
	}
	if err := refreshAlbumAggregatesTx(ctx, tx, alb.ID); err != nil {
		return err
	}
	if err := recomputeVersionsTx(ctx, tx); err != nil {
		return err
	}
	if err := tx.Commit(); err != nil {
		return err
	}
	s.gen++
	return nil
}

func upsertAlbumTx(ctx context.Context, tx *sql.Tx, a Album, now int64) error {
	sourceType := a.SourceType
	if sourceType == "" {
		if a.Source == SourceLocal {
			sourceType = SourceTypeLocal
		} else {
			sourceType = SourceTypeRemote
		}
	}
	provider := a.Provider
	if provider == "" {
		provider = string(a.Source)
	}
	extID := a.ExternalID
	if extID == "" {
		extID = strings.TrimPrefix(a.ID, provider+":")
	}
	vKey := VersionKey(a.Artist, a.Title)
	addedAt := now
	if a.AddedAt != nil && !a.AddedAt.IsZero() {
		addedAt = a.AddedAt.UTC().Unix()
	}

	if _, err := tx.ExecContext(ctx, `
INSERT INTO albums(
	id, source_type, provider, external_id,
	title, sort_title, artist, sort_artist, genre, year,
	track_count, duration_sec, artwork_url,
	primary_format, max_bit_depth, max_sample_rate,
	version_key, is_partial, available, added_at, updated_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?)
ON CONFLICT(id) DO UPDATE SET
	title = excluded.title,
	sort_title = excluded.sort_title,
	artist = excluded.artist,
	sort_artist = excluded.sort_artist,
	genre = CASE WHEN excluded.genre != '' THEN excluded.genre ELSE albums.genre END,
	year = CASE WHEN excluded.year > 0 THEN excluded.year ELSE albums.year END,
	artwork_url = CASE WHEN excluded.artwork_url != '' THEN excluded.artwork_url ELSE albums.artwork_url END,
	primary_format = CASE WHEN excluded.primary_format != '' THEN excluded.primary_format ELSE albums.primary_format END,
	max_bit_depth = MAX(albums.max_bit_depth, excluded.max_bit_depth),
	max_sample_rate = MAX(albums.max_sample_rate, excluded.max_sample_rate),
	version_key = excluded.version_key,
	is_partial = CASE WHEN excluded.is_partial = 0 THEN 0 ELSE albums.is_partial END,
	available = 1,
	missing_since = 0,
	updated_at = excluded.updated_at`,
		a.ID, sourceType, provider, extID,
		a.Title, SortKey(a.Title), a.Artist, SortKey(a.Artist), a.Genre, a.Year,
		a.TrackCount, a.DurationSec, a.ArtworkURL,
		a.PrimaryFormat, a.MaxBit, a.MaxRate,
		vKey, boolToInt(a.IsPartial), addedAt, now,
	); err != nil {
		return err
	}
	if _, err := tx.ExecContext(ctx, `DELETE FROM albums_fts WHERE album_id = ?`, a.ID); err != nil {
		return err
	}
	_, err := tx.ExecContext(ctx, `INSERT INTO albums_fts(album_id, title, artist, genre) VALUES (?, ?, ?, ?)`, a.ID, a.Title, a.Artist, a.Genre)
	return err
}

func upsertTrackTx(ctx context.Context, tx *sql.Tx, t Track, now int64) error {
	sourceType := t.SourceType
	if sourceType == "" {
		if t.Source == SourceLocal {
			sourceType = SourceTypeLocal
		} else {
			sourceType = SourceTypeRemote
		}
	}
	provider := t.Provider
	if provider == "" {
		provider = string(t.Source)
	}
	extID := t.ExternalID
	if extID == "" {
		extID = strings.TrimPrefix(t.ID, provider+":")
	}
	albArtist := t.AlbumArtist
	if albArtist == "" {
		albArtist = t.Artist
	}
	addedAt := now
	if t.AddedAt != nil && !t.AddedAt.IsZero() {
		addedAt = t.AddedAt.UTC().Unix()
	}

	if _, err := tx.ExecContext(ctx, `
INSERT INTO tracks(
	id, album_id, source_type, provider, external_id,
	title, sort_title, artist, sort_artist, album_artist, album, sort_album,
	genre, disc_number, track_number, year, duration_sec,
	sample_rate, bit_depth, channels, mime, ext, size_bytes,
	artwork_url, file_path, file_mtime_ns, folder_art, has_embed_art, qobuz_track_id,
	available, added_at, updated_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?)
ON CONFLICT(id) DO UPDATE SET
	album_id = excluded.album_id,
	title = excluded.title,
	sort_title = excluded.sort_title,
	artist = excluded.artist,
	sort_artist = excluded.sort_artist,
	album_artist = excluded.album_artist,
	album = excluded.album,
	sort_album = excluded.sort_album,
	genre = CASE WHEN excluded.genre != '' THEN excluded.genre ELSE tracks.genre END,
	disc_number = excluded.disc_number,
	track_number = excluded.track_number,
	year = CASE WHEN excluded.year > 0 THEN excluded.year ELSE tracks.year END,
	duration_sec = excluded.duration_sec,
	sample_rate = excluded.sample_rate,
	bit_depth = excluded.bit_depth,
	channels = excluded.channels,
	mime = excluded.mime,
	ext = excluded.ext,
	size_bytes = excluded.size_bytes,
	artwork_url = CASE WHEN excluded.artwork_url != '' THEN excluded.artwork_url ELSE tracks.artwork_url END,
	qobuz_track_id = excluded.qobuz_track_id,
	available = 1,
	missing_since = 0,
	updated_at = excluded.updated_at`,
		t.ID, t.AlbumID, sourceType, provider, extID,
		t.Title, SortKey(t.Title), t.Artist, SortKey(albArtist), t.AlbumArtist, t.Album, SortKey(t.Album),
		t.Genre, t.DiscNumber, t.TrackNumber, t.Year, t.DurationSec,
		t.SampleRate, t.BitDepth, t.Channels, t.MIME, t.Ext, t.Size,
		t.ArtworkURL, t.FilePath, t.FileMtimeNS, t.FolderArt, boolToInt(t.HasEmbedArt), t.QobuzTrackID,
		addedAt, now,
	); err != nil {
		return err
	}
	if _, err := tx.ExecContext(ctx, `DELETE FROM tracks_fts WHERE track_id = ?`, t.ID); err != nil {
		return err
	}
	_, err := tx.ExecContext(ctx, `INSERT INTO tracks_fts(track_id, title, artist, album_artist, album, genre) VALUES (?, ?, ?, ?, ?, ?)`,
		t.ID, t.Title, t.Artist, t.AlbumArtist, t.Album, t.Genre)
	return err
}

func refreshAlbumAggregatesTx(ctx context.Context, tx *sql.Tx, albumID string) error {
	var count, dur, maxBit, maxRate int
	var mimeType, ext string
	if err := tx.QueryRowContext(ctx, `
SELECT COUNT(*), COALESCE(SUM(duration_sec), 0), COALESCE(MAX(bit_depth), 0), COALESCE(MAX(sample_rate), 0),
       COALESCE(MAX(mime), ''), COALESCE(MAX(ext), '')
FROM tracks WHERE album_id = ? AND available = 1`, albumID).Scan(&count, &dur, &maxBit, &maxRate, &mimeType, &ext); err != nil {
		return err
	}
	format := FormatNameFromMIMEOrExt(mimeType, ext)
	_, err := tx.ExecContext(ctx, `
UPDATE albums
SET track_count = ?, duration_sec = ?, max_bit_depth = ?, max_sample_rate = ?,
    primary_format = CASE WHEN ? != '' THEN ? ELSE primary_format END
WHERE id = ?`, count, dur, maxBit, maxRate, format, format, albumID)
	return err
}

func (s *libraryStore) deleteAlbum(ctx context.Context, albumID string) error {
	s.mu.Lock()
	defer s.mu.Unlock()

	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		return err
	}
	defer func() { _ = tx.Rollback() }()

	if _, err = tx.ExecContext(ctx, `DELETE FROM tracks_fts WHERE track_id IN (SELECT id FROM tracks WHERE album_id = ?)`, albumID); err != nil {
		return err
	}
	if _, err = tx.ExecContext(ctx, `DELETE FROM albums_fts WHERE album_id = ?`, albumID); err != nil {
		return err
	}
	res, err := tx.ExecContext(ctx, `DELETE FROM albums WHERE id = ?`, albumID)
	if err != nil {
		return err
	}
	if n, _ := res.RowsAffected(); n == 0 {
		return ErrNotFound
	}
	if err := recomputeVersionsTx(ctx, tx); err != nil {
		return err
	}
	if err := tx.Commit(); err != nil {
		return err
	}
	s.gen++
	return nil
}

func (s *libraryStore) deleteTrack(ctx context.Context, trackID string) error {
	s.mu.Lock()
	defer s.mu.Unlock()

	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		return err
	}
	defer func() { _ = tx.Rollback() }()

	var albumID string
	if err := tx.QueryRowContext(ctx, `SELECT album_id FROM tracks WHERE id = ?`, trackID).Scan(&albumID); err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return ErrNotFound
		}
		return err
	}
	if _, err := tx.ExecContext(ctx, `DELETE FROM tracks_fts WHERE track_id = ?`, trackID); err != nil {
		return err
	}
	if _, err := tx.ExecContext(ctx, `DELETE FROM tracks WHERE id = ?`, trackID); err != nil {
		return err
	}
	var remaining int
	if err := tx.QueryRowContext(ctx, `SELECT COUNT(*) FROM tracks WHERE album_id = ?`, albumID).Scan(&remaining); err != nil {
		return err
	}
	if remaining == 0 {
		if _, err := tx.ExecContext(ctx, `DELETE FROM albums_fts WHERE album_id = ?`, albumID); err != nil {
			return err
		}
		if _, err := tx.ExecContext(ctx, `DELETE FROM albums WHERE id = ?`, albumID); err != nil {
			return err
		}
	} else if err := refreshAlbumAggregatesTx(ctx, tx, albumID); err != nil {
		return err
	}
	if err := recomputeVersionsTx(ctx, tx); err != nil {
		return err
	}
	if err := tx.Commit(); err != nil {
		return err
	}
	s.gen++
	return nil
}

func (s *libraryStore) setProviderAvailable(ctx context.Context, provider string, available bool) error {
	s.mu.Lock()
	defer s.mu.Unlock()

	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		return err
	}
	defer func() { _ = tx.Rollback() }()

	avail := boolToInt(available)
	if _, err := tx.ExecContext(ctx, `UPDATE albums SET available = ? WHERE provider = ?`, avail, provider); err != nil {
		return err
	}
	if _, err := tx.ExecContext(ctx, `UPDATE tracks SET available = ? WHERE provider = ?`, avail, provider); err != nil {
		return err
	}
	if err := recomputeVersionsTx(ctx, tx); err != nil {
		return err
	}
	if err := tx.Commit(); err != nil {
		return err
	}
	s.gen++
	return nil
}

// recomputeVersionsTx updates automatic version_group_id and selects the Primary Version
// for every group based on user override or QualityRank (lossless > bitDepth > sampleRate > local).
func recomputeVersionsTx(ctx context.Context, tx *sql.Tx) error {
	type albRow struct {
		id          string
		sourceType  string
		format      string
		bitDepth    int
		sampleRate  int
		trackCount  int
		versionKey  string
		origGroupID string
		groupID     string
		origPrimary bool
		excluded    bool
		override    bool
		available   bool
	}

	rows, err := tx.QueryContext(ctx, `
SELECT id, source_type, primary_format, max_bit_depth, max_sample_rate,
       track_count, version_key, version_group_id, is_primary, version_excluded,
       is_primary_override, available
FROM albums`)
	if err != nil {
		return err
	}
	var all []albRow
	for rows.Next() {
		var r albRow
		var prim, exc, ovr, avail int
		if err = rows.Scan(
			&r.id, &r.sourceType, &r.format, &r.bitDepth, &r.sampleRate,
			&r.trackCount, &r.versionKey, &r.groupID, &prim, &exc, &ovr, &avail,
		); err != nil {
			_ = rows.Close()
			return err
		}
		r.origGroupID = r.groupID
		r.origPrimary = prim != 0
		r.excluded = exc != 0
		r.override = ovr != 0
		r.available = avail != 0
		all = append(all, r)
	}
	_ = rows.Close()
	if err = rows.Err(); err != nil {
		return err
	}

	// Count eligible albums per version_key for automatic grouping.
	keyCounts := make(map[string]int)
	for _, r := range all {
		if r.excluded || strings.HasPrefix(r.groupID, "manual:") || r.versionKey == "" {
			continue
		}
		keyCounts[r.versionKey]++
	}

	// Assign group IDs and collect members per group.
	groups := make(map[string][]int)
	for i := range all {
		r := &all[i]
		if r.excluded {
			r.groupID = ""
		} else if !strings.HasPrefix(r.groupID, "manual:") {
			if r.versionKey != "" && keyCounts[r.versionKey] >= 2 {
				r.groupID = "auto:" + shortHash("vgroup:"+r.versionKey)
			} else {
				r.groupID = ""
			}
		}
		if r.groupID != "" {
			groups[r.groupID] = append(groups[r.groupID], i)
		}
	}

	// Dissolve manual groups that fell below 2 albums.
	for gid, idxs := range groups {
		if len(idxs) < 2 {
			all[idxs[0]].groupID = ""
			delete(groups, gid)
		}
	}

	// Pick Primary Version for each group.
	isPrimary := make([]bool, len(all))
	for i := range all {
		if all[i].groupID == "" {
			isPrimary[i] = true
		}
	}

	for _, idxs := range groups {
		bestIdx := idxs[0]
		for _, idx := range idxs[1:] {
			a := all[idx]
			b := all[bestIdx]
			if isBetterPrimary(a.available, a.override, a.format, a.bitDepth, a.sampleRate, a.sourceType, a.trackCount, a.id,
				b.available, b.override, b.format, b.bitDepth, b.sampleRate, b.sourceType, b.trackCount, b.id) {
				bestIdx = idx
			}
		}
		isPrimary[bestIdx] = true
	}

	var stmt *sql.Stmt
	defer func() {
		if stmt != nil {
			_ = stmt.Close()
		}
	}()

	for i, r := range all {
		if r.groupID == r.origGroupID && isPrimary[i] == r.origPrimary {
			continue
		}
		if stmt == nil {
			stmt, err = tx.PrepareContext(ctx, `UPDATE albums SET version_group_id = ?, is_primary = ? WHERE id = ?`)
			if err != nil {
				return err
			}
		}
		if _, err := stmt.ExecContext(ctx, r.groupID, boolToInt(isPrimary[i]), r.id); err != nil {
			return err
		}
	}
	return nil
}

func isBetterPrimary(
	aAvail, aOvr bool, aFmt string, aBit, aRate int, aSrc string, aTracks int, aID string,
	bAvail, bOvr bool, bFmt string, bBit, bRate int, bSrc string, bTracks int, bID string,
) bool {
	if aAvail != bAvail {
		return aAvail
	}
	if aOvr != bOvr {
		return aOvr
	}
	ra := QualityRank(aFmt, aBit, aRate, aSrc)
	rb := QualityRank(bFmt, bBit, bRate, bSrc)
	if ra != rb {
		return ra > rb
	}
	if aTracks != bTracks {
		return aTracks > bTracks
	}
	return aID < bID
}

func (s *libraryStore) setPrimaryVersion(ctx context.Context, albumID string) error {
	s.mu.Lock()
	defer s.mu.Unlock()

	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		return err
	}
	defer func() { _ = tx.Rollback() }()

	var groupID string
	if err := tx.QueryRowContext(ctx, `SELECT version_group_id FROM albums WHERE id = ?`, albumID).Scan(&groupID); err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return ErrNotFound
		}
		return err
	}
	if groupID != "" {
		if _, err := tx.ExecContext(ctx, `UPDATE albums SET is_primary_override = 0 WHERE version_group_id = ?`, groupID); err != nil {
			return err
		}
	}
	if _, err := tx.ExecContext(ctx, `UPDATE albums SET is_primary_override = 1 WHERE id = ?`, albumID); err != nil {
		return err
	}
	if err := recomputeVersionsTx(ctx, tx); err != nil {
		return err
	}
	if err := tx.Commit(); err != nil {
		return err
	}
	s.gen++
	return nil
}

func (s *libraryStore) groupVersions(ctx context.Context, albumIDs []string) (string, error) {
	seen := make(map[string]struct{}, len(albumIDs))
	unique := make([]string, 0, len(albumIDs))
	for _, id := range albumIDs {
		id = strings.TrimSpace(id)
		if id == "" {
			continue
		}
		if _, dup := seen[id]; !dup {
			seen[id] = struct{}{}
			unique = append(unique, id)
		}
	}
	if len(unique) < 2 {
		return "", fmt.Errorf("%w: at least 2 distinct albumIds are required", ErrInvalidArgument)
	}
	sort.Strings(unique)

	s.mu.Lock()
	defer s.mu.Unlock()

	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		return "", err
	}
	defer func() { _ = tx.Rollback() }()

	groupID := "manual:" + shortHash("manual:"+strings.Join(unique, ","))
	for _, id := range unique {
		res, err := tx.ExecContext(ctx, `UPDATE albums SET version_group_id = ?, version_excluded = 0 WHERE id = ?`, groupID, id)
		if err != nil {
			return "", err
		}
		if n, _ := res.RowsAffected(); n == 0 {
			return "", fmt.Errorf("%w: album %s", ErrNotFound, id)
		}
	}
	if err := recomputeVersionsTx(ctx, tx); err != nil {
		return "", err
	}
	if err := tx.Commit(); err != nil {
		return "", err
	}
	s.gen++
	return groupID, nil
}

func (s *libraryStore) ungroupVersion(ctx context.Context, albumID string) error {
	s.mu.Lock()
	defer s.mu.Unlock()

	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		return err
	}
	defer func() { _ = tx.Rollback() }()

	res, err := tx.ExecContext(ctx, `
UPDATE albums
SET version_group_id = '', version_excluded = 1, is_primary_override = 0
WHERE id = ?`, albumID)
	if err != nil {
		return err
	}
	if n, _ := res.RowsAffected(); n == 0 {
		return ErrNotFound
	}
	if err := recomputeVersionsTx(ctx, tx); err != nil {
		return err
	}
	if err := tx.Commit(); err != nil {
		return err
	}
	s.gen++
	return nil
}

func (s *libraryStore) recordPlay(ctx context.Context, track Track) error {
	if strings.TrimSpace(track.ID) == "" {
		return nil
	}
	s.mu.Lock()
	defer s.mu.Unlock()

	now := time.Now().UTC().Unix()
	tx, err := s.db.BeginTx(ctx, nil)
	if err != nil {
		return err
	}
	defer func() { _ = tx.Rollback() }()

	provider := track.Provider
	if provider == "" {
		provider = string(track.Source)
	}
	albumID := track.AlbumID
	if albumID == "" {
		_ = tx.QueryRowContext(ctx, `SELECT album_id FROM tracks WHERE id = ?`, track.ID).Scan(&albumID)
	}

	if _, err := tx.ExecContext(ctx, `
UPDATE tracks SET play_count = play_count + 1, last_played_at = ? WHERE id = ?`,
		now, track.ID); err != nil {
		return err
	}
	if _, err := tx.ExecContext(ctx, `
INSERT INTO play_events(track_id, album_id, provider, played_at) VALUES (?, ?, ?, ?)`,
		track.ID, albumID, provider, now); err != nil {
		return err
	}
	if err := tx.Commit(); err != nil {
		return err
	}
	s.gen++
	return nil
}

func (s *libraryStore) heavyRotation(ctx context.Context, days, limit int) ([]HeavyRotationAlbum, error) {
	if days <= 0 {
		days = 90
	}
	if limit <= 0 || limit > 100 {
		limit = 20
	}
	now := time.Now().UTC()
	since := now.AddDate(0, 0, -days).Unix()
	cut4d := now.AddDate(0, 0, -4).Unix()
	cut14d := now.AddDate(0, 0, -14).Unix()
	cut30d := now.AddDate(0, 0, -30).Unix()

	s.mu.RLock()
	defer s.mu.RUnlock()

	// Firefox-style frecency: each play contributes a recency-bucketed weight
	// (<=4d: 100, <=14d: 70, <=30d: 50, older: 20) summed per album so both
	// recent spins and sustained favorites surface naturally.
	rows, err := s.db.QueryContext(ctx, `
SELECT a.id, a.source_type, a.provider, a.external_id, a.title, a.artist, a.genre, a.year,
       a.track_count, a.duration_sec, a.artwork_url, a.primary_format,
       a.max_bit_depth, a.max_sample_rate, a.version_group_id, a.is_primary,
       a.is_partial, a.available, a.added_at,
       COUNT(pe.id) AS recent_plays,
       MAX(pe.played_at) AS last_played,
       SUM(CASE
         WHEN pe.played_at >= ? THEN 100
         WHEN pe.played_at >= ? THEN 70
         WHEN pe.played_at >= ? THEN 50
         ELSE 20
       END) AS frecency_score
FROM play_events pe
JOIN albums a ON a.id = pe.album_id
WHERE pe.played_at >= ? AND a.available = 1
GROUP BY a.id
ORDER BY frecency_score DESC, last_played DESC
LIMIT ?`, cut4d, cut14d, cut30d, since, limit)
	if err != nil {
		return nil, err
	}
	defer func() { _ = rows.Close() }()

	out := make([]HeavyRotationAlbum, 0)
	for rows.Next() {
		var h HeavyRotationAlbum
		var isPrimary, isPartial, avail int
		var addedUnix, lastPlayedUnix int64
		var frecencyScore int
		if err := rows.Scan(
			&h.ID, &h.SourceType, &h.Provider, &h.ExternalID, &h.Title, &h.Artist, &h.Genre, &h.Year,
			&h.TrackCount, &h.DurationSec, &h.ArtworkURL, &h.PrimaryFormat,
			&h.MaxBit, &h.MaxRate, &h.VersionGroupID, &isPrimary,
			&isPartial, &avail, &addedUnix,
			&h.RecentPlays, &lastPlayedUnix, &frecencyScore,
		); err != nil {
			return nil, err
		}
		h.Source = Source(h.Provider)
		h.IsPrimary = isPrimary != 0
		h.IsPartial = isPartial != 0
		h.Available = avail != 0
		h.InLibrary = true
		if addedUnix > 0 {
			ts := time.Unix(addedUnix, 0).UTC()
			h.AddedAt = &ts
		}
		if lastPlayedUnix > 0 {
			lp := time.Unix(lastPlayedUnix, 0).UTC()
			h.LastPlayed = &lp
		}
		out = append(out, h)
	}
	return out, rows.Err()
}

func (s *libraryStore) queryAlbums(ctx context.Context, f AlbumFilter) ([]Album, int, error) {
	s.mu.RLock()
	defer s.mu.RUnlock()

	var where []string
	var args []any

	if !f.IncludeUnavailable {
		where = append(where, "a.available = 1")
	}
	if f.GroupVersions {
		where = append(where, "a.is_primary = 1")
	}
	switch strings.ToLower(strings.TrimSpace(f.SourceType)) {
	case SourceTypeLocal, SourceTypeRemote:
		where = append(where, "a.source_type = ?")
		args = append(args, strings.ToLower(strings.TrimSpace(f.SourceType)))
	}
	if p := strings.ToLower(strings.TrimSpace(f.Provider)); p != "" && p != "all" {
		where = append(where, "a.provider = ?")
		args = append(args, p)
	}
	if artist := strings.TrimSpace(f.Artist); artist != "" {
		where = append(where, "LOWER(a.artist) = LOWER(?)")
		args = append(args, artist)
	}
	if genre := strings.TrimSpace(f.Genre); genre != "" {
		where = append(where, "LOWER(a.genre) = LOWER(?)")
		args = append(args, genre)
	}
	if fts := buildFTSQuery(f.Query); fts != "" {
		where = append(where, "(a.id IN (SELECT album_id FROM albums_fts WHERE albums_fts MATCH ?) OR a.id IN (SELECT DISTINCT t.album_id FROM tracks t JOIN tracks_fts tf ON tf.track_id = t.id WHERE tracks_fts MATCH ?))")
		args = append(args, fts, fts)
	}

	whereSQL := ""
	if len(where) > 0 {
		whereSQL = "WHERE " + strings.Join(where, " AND ")
	}

	var total int
	if err := s.db.QueryRowContext(ctx, "SELECT COUNT(*) FROM albums a "+whereSQL, args...).Scan(&total); err != nil {
		return nil, 0, err
	}

	orderBy := "a.sort_artist ASC, a.sort_title ASC, a.id ASC"
	switch strings.ToLower(strings.TrimSpace(f.Sort)) {
	case "title":
		orderBy = "a.sort_title ASC, a.sort_artist ASC, a.id ASC"
	case "year":
		orderBy = "a.year DESC, a.sort_artist ASC, a.sort_title ASC, a.id ASC"
	case "added":
		orderBy = "a.added_at DESC, a.sort_title ASC, a.id ASC"
	case "plays":
		orderBy = "play_count DESC, a.sort_artist ASC, a.sort_title ASC, a.id ASC"
	}

	limit := f.Limit
	if limit <= 0 {
		limit = 200
	}
	if limit > 1000 {
		limit = 1000
	}
	offset := f.Offset
	if offset < 0 {
		offset = 0
	}

	querySQL := `
SELECT a.id, a.source_type, a.provider, a.external_id, a.title, a.artist, a.genre, a.year,
       a.track_count, a.duration_sec, a.artwork_url, a.primary_format,
       a.max_bit_depth, a.max_sample_rate, a.version_group_id, a.is_primary,
       a.is_partial, a.available, a.added_at,
       COALESCE((SELECT SUM(t.play_count) FROM tracks t WHERE t.album_id = a.id), 0) AS play_count,
       CASE WHEN a.version_group_id = '' THEN 1
            ELSE (SELECT COUNT(*) FROM albums av WHERE av.version_group_id = a.version_group_id AND av.available = 1)
       END AS version_count
FROM albums a ` + whereSQL + ` ORDER BY ` + orderBy + ` LIMIT ? OFFSET ?`

	qArgs := append(append([]any(nil), args...), limit, offset)
	rows, err := s.db.QueryContext(ctx, querySQL, qArgs...)
	if err != nil {
		return nil, 0, err
	}
	defer func() { _ = rows.Close() }()

	items := make([]Album, 0)
	for rows.Next() {
		var a Album
		var isPrimary, isPartial, avail int
		var addedUnix int64
		if err := rows.Scan(
			&a.ID, &a.SourceType, &a.Provider, &a.ExternalID, &a.Title, &a.Artist, &a.Genre, &a.Year,
			&a.TrackCount, &a.DurationSec, &a.ArtworkURL, &a.PrimaryFormat,
			&a.MaxBit, &a.MaxRate, &a.VersionGroupID, &isPrimary,
			&isPartial, &avail, &addedUnix, &a.PlayCount, &a.VersionCount,
		); err != nil {
			return nil, 0, err
		}
		a.Source = Source(a.Provider)
		a.IsPrimary = isPrimary != 0
		a.IsPartial = isPartial != 0
		a.Available = avail != 0
		a.InLibrary = true
		if addedUnix > 0 {
			ts := time.Unix(addedUnix, 0).UTC()
			a.AddedAt = &ts
		}
		items = append(items, a)
	}
	return items, total, rows.Err()
}

func (s *libraryStore) getAlbum(ctx context.Context, id string) (Album, bool, error) {
	s.mu.RLock()
	defer s.mu.RUnlock()

	var a Album
	var isPrimary, isPartial, avail int
	var addedUnix int64
	err := s.db.QueryRowContext(ctx, `
SELECT a.id, a.source_type, a.provider, a.external_id, a.title, a.artist, a.genre, a.year,
       a.track_count, a.duration_sec, a.artwork_url, a.primary_format,
       a.max_bit_depth, a.max_sample_rate, a.version_group_id, a.is_primary,
       a.is_partial, a.available, a.added_at,
       COALESCE((SELECT SUM(t.play_count) FROM tracks t WHERE t.album_id = a.id), 0) AS play_count
FROM albums a WHERE a.id = ?`, id).Scan(
		&a.ID, &a.SourceType, &a.Provider, &a.ExternalID, &a.Title, &a.Artist, &a.Genre, &a.Year,
		&a.TrackCount, &a.DurationSec, &a.ArtworkURL, &a.PrimaryFormat,
		&a.MaxBit, &a.MaxRate, &a.VersionGroupID, &isPrimary,
		&isPartial, &avail, &addedUnix, &a.PlayCount,
	)
	if errors.Is(err, sql.ErrNoRows) {
		return Album{}, false, nil
	}
	if err != nil {
		return Album{}, false, err
	}
	a.Source = Source(a.Provider)
	a.IsPrimary = isPrimary != 0
	a.IsPartial = isPartial != 0
	a.Available = avail != 0
	a.InLibrary = a.Available && !a.IsPartial
	if addedUnix > 0 {
		ts := time.Unix(addedUnix, 0).UTC()
		a.AddedAt = &ts
	}

	tracks, err := s.queryTracksByAlbumLocked(ctx, a.ID)
	if err != nil {
		return Album{}, false, err
	}
	a.Tracks = tracks

	if a.VersionGroupID != "" {
		vRows, err := s.db.QueryContext(ctx, `
SELECT id, source_type, provider, title, year, track_count, primary_format,
       max_bit_depth, max_sample_rate, is_primary, available
FROM albums
WHERE version_group_id = ?
ORDER BY is_primary DESC, max_bit_depth DESC, max_sample_rate DESC, id ASC`, a.VersionGroupID)
		if err != nil {
			return Album{}, false, err
		}
		for vRows.Next() {
			var v AlbumVersion
			var vp, va int
			if err := vRows.Scan(
				&v.ID, &v.SourceType, &v.Provider, &v.Title, &v.Year, &v.TrackCount, &v.PrimaryFormat,
				&v.MaxBit, &v.MaxRate, &vp, &va,
			); err != nil {
				_ = vRows.Close()
				return Album{}, false, err
			}
			v.Source = Source(v.Provider)
			v.IsPrimary = vp != 0
			v.Available = va != 0
			a.Versions = append(a.Versions, v)
		}
		_ = vRows.Close()
		a.VersionCount = len(a.Versions)
	} else {
		a.VersionCount = 1
	}

	return a, true, nil
}

func (s *libraryStore) queryTracksByAlbumLocked(ctx context.Context, albumID string) ([]Track, error) {
	rows, err := s.db.QueryContext(ctx, `
SELECT id, album_id, source_type, provider, external_id,
       title, artist, album_artist, album, genre,
       disc_number, track_number, year, duration_sec,
       sample_rate, bit_depth, channels, mime, ext, size_bytes,
       artwork_url, file_path, file_mtime_ns, folder_art, has_embed_art, qobuz_track_id,
       play_count, last_played_at, loved, available, added_at
FROM tracks
WHERE album_id = ? AND available = 1
ORDER BY disc_number ASC, track_number ASC, sort_title ASC`, albumID)
	if err != nil {
		return nil, err
	}
	defer func() { _ = rows.Close() }()
	return scanTracks(rows)
}

func (s *libraryStore) queryTracks(ctx context.Context, f TrackFilter) ([]Track, int, error) {
	s.mu.RLock()
	defer s.mu.RUnlock()

	var where []string
	var args []any

	if !f.IncludeUnavailable {
		where = append(where, "t.available = 1")
	}
	switch strings.ToLower(strings.TrimSpace(f.SourceType)) {
	case SourceTypeLocal, SourceTypeRemote:
		where = append(where, "t.source_type = ?")
		args = append(args, strings.ToLower(strings.TrimSpace(f.SourceType)))
	}
	if p := strings.ToLower(strings.TrimSpace(f.Provider)); p != "" && p != "all" {
		where = append(where, "t.provider = ?")
		args = append(args, p)
	}
	if artist := strings.TrimSpace(f.Artist); artist != "" {
		where = append(where, "(LOWER(t.artist) = LOWER(?) OR LOWER(t.album_artist) = LOWER(?))")
		args = append(args, artist, artist)
	}
	if albID := strings.TrimSpace(f.AlbumID); albID != "" {
		where = append(where, "t.album_id = ?")
		args = append(args, albID)
	}
	if alb := strings.TrimSpace(f.Album); alb != "" {
		where = append(where, "LOWER(t.album) = LOWER(?)")
		args = append(args, alb)
	}
	if genre := strings.TrimSpace(f.Genre); genre != "" {
		where = append(where, "LOWER(t.genre) = LOWER(?)")
		args = append(args, genre)
	}
	if fts := buildFTSQuery(f.Query); fts != "" {
		where = append(where, "t.id IN (SELECT track_id FROM tracks_fts WHERE tracks_fts MATCH ?)")
		args = append(args, fts)
	}

	whereSQL := ""
	if len(where) > 0 {
		whereSQL = "WHERE " + strings.Join(where, " AND ")
	}

	var total int
	if err := s.db.QueryRowContext(ctx, "SELECT COUNT(*) FROM tracks t "+whereSQL, args...).Scan(&total); err != nil {
		return nil, 0, err
	}

	dir := "ASC"
	if strings.EqualFold(strings.TrimSpace(f.SortDir), "desc") {
		dir = "DESC"
	}

	orderBy := "t.sort_artist " + dir + ", t.sort_album " + dir + ", t.disc_number ASC, t.track_number ASC, t.sort_title ASC"
	switch strings.ToLower(strings.TrimSpace(f.Sort)) {
	case "title":
		orderBy = "t.sort_title " + dir + ", t.sort_artist ASC"
	case "artist":
		orderBy = "t.sort_artist " + dir + ", t.sort_album ASC, t.disc_number ASC, t.track_number ASC"
	case "album":
		orderBy = "t.sort_album " + dir + ", t.disc_number ASC, t.track_number ASC, t.sort_title ASC"
	case "year":
		orderBy = "t.year " + dir + ", t.sort_artist ASC, t.sort_album ASC, t.disc_number ASC, t.track_number ASC"
	case "added":
		orderBy = "t.added_at " + dir + ", t.sort_album ASC, t.disc_number ASC, t.track_number ASC"
	case "plays":
		orderBy = "t.play_count " + dir + ", t.last_played_at DESC, t.sort_title ASC"
	case "duration":
		orderBy = "t.duration_sec " + dir + ", t.sort_title ASC"
	case "quality":
		orderBy = "t.bit_depth " + dir + ", t.sample_rate " + dir + ", t.sort_title ASC"
	}
	// Deterministic tie-breaker so random-access OFFSET/LIMIT pagination never
	// duplicates or skips rows when sort keys collide.
	orderBy += ", t.id ASC"

	limit := f.Limit
	if limit <= 0 {
		limit = 200
	}
	if limit > 1000 {
		limit = 1000
	}
	offset := f.Offset
	if offset < 0 {
		offset = 0
	}

	querySQL := `
SELECT t.id, t.album_id, t.source_type, t.provider, t.external_id,
       t.title, t.artist, t.album_artist, t.album, t.genre,
       t.disc_number, t.track_number, t.year, t.duration_sec,
       t.sample_rate, t.bit_depth, t.channels, t.mime, t.ext, t.size_bytes,
       t.artwork_url, t.file_path, t.file_mtime_ns, t.folder_art, t.has_embed_art, t.qobuz_track_id,
       t.play_count, t.last_played_at, t.loved, t.available, t.added_at
FROM tracks t ` + whereSQL + ` ORDER BY ` + orderBy + ` LIMIT ? OFFSET ?`

	qArgs := append(append([]any(nil), args...), limit, offset)
	rows, err := s.db.QueryContext(ctx, querySQL, qArgs...)
	if err != nil {
		return nil, 0, err
	}
	defer func() { _ = rows.Close() }()

	items, err := scanTracks(rows)
	return items, total, err
}

func (s *libraryStore) getTrack(ctx context.Context, id string) (Track, bool, error) {
	s.mu.RLock()
	defer s.mu.RUnlock()

	rows, err := s.db.QueryContext(ctx, `
SELECT id, album_id, source_type, provider, external_id,
       title, artist, album_artist, album, genre,
       disc_number, track_number, year, duration_sec,
       sample_rate, bit_depth, channels, mime, ext, size_bytes,
       artwork_url, file_path, file_mtime_ns, folder_art, has_embed_art, qobuz_track_id,
       play_count, last_played_at, loved, available, added_at
FROM tracks WHERE id = ?`, id)
	if err != nil {
		return Track{}, false, err
	}
	defer func() { _ = rows.Close() }()
	items, err := scanTracks(rows)
	if err != nil || len(items) == 0 {
		return Track{}, false, err
	}
	return items[0], true, nil
}

func (s *libraryStore) getTrackByArtworkURL(ctx context.Context, artworkURL string) (Track, bool, error) {
	s.mu.RLock()
	defer s.mu.RUnlock()

	rows, err := s.db.QueryContext(ctx, `
SELECT id, album_id, source_type, provider, external_id,
       title, artist, album_artist, album, genre,
       disc_number, track_number, year, duration_sec,
       sample_rate, bit_depth, channels, mime, ext, size_bytes,
       artwork_url, file_path, file_mtime_ns, folder_art, has_embed_art, qobuz_track_id,
       play_count, last_played_at, loved, available, added_at
FROM tracks WHERE artwork_url = ? ORDER BY available DESC LIMIT 1`, artworkURL)
	if err != nil {
		return Track{}, false, err
	}
	defer func() { _ = rows.Close() }()
	items, err := scanTracks(rows)
	if err != nil || len(items) == 0 {
		return Track{}, false, err
	}
	return items[0], true, nil
}

func (s *libraryStore) listLocalArtHashes(ctx context.Context) ([]string, error) {
	s.mu.RLock()
	defer s.mu.RUnlock()

	rows, err := s.db.QueryContext(ctx, `
SELECT DISTINCT artwork_url
FROM tracks
WHERE source_type = 'local'
  AND available = 1
  AND artwork_url LIKE '/media/art/%'`)
	if err != nil {
		return nil, err
	}
	defer func() { _ = rows.Close() }()

	var hashes []string
	for rows.Next() {
		var u string
		if err := rows.Scan(&u); err != nil {
			return nil, err
		}
		if h, ok := strings.CutPrefix(u, "/media/art/"); ok && len(h) == 16 {
			hashes = append(hashes, h)
		}
	}
	return hashes, rows.Err()
}

func (s *libraryStore) hasAlbum(ctx context.Context, id string) bool {
	s.mu.RLock()
	defer s.mu.RUnlock()
	var n int
	return s.db.QueryRowContext(ctx, `SELECT 1 FROM albums WHERE id = ? AND available = 1`, id).Scan(&n) == nil
}

func (s *libraryStore) listArtists(ctx context.Context, f FacetFilter) ([]ArtistSummary, int, error) {
	s.mu.RLock()
	defer s.mu.RUnlock()

	var where []string
	var args []any
	if !f.IncludeUnavailable {
		where = append(where, "a.available = 1")
	}
	switch strings.ToLower(strings.TrimSpace(f.SourceType)) {
	case SourceTypeLocal, SourceTypeRemote:
		where = append(where, "a.source_type = ?")
		args = append(args, strings.ToLower(strings.TrimSpace(f.SourceType)))
	}
	if p := strings.ToLower(strings.TrimSpace(f.Provider)); p != "" && p != "all" {
		where = append(where, "a.provider = ?")
		args = append(args, p)
	}
	if genre := strings.TrimSpace(f.Genre); genre != "" {
		where = append(where, "LOWER(a.genre) = LOWER(?)")
		args = append(args, genre)
	}
	if q := strings.TrimSpace(f.Query); q != "" {
		for _, tok := range strings.Fields(q) {
			where = append(where, "LOWER(a.artist) LIKE LOWER(?)")
			args = append(args, "%"+tok+"%")
		}
	}

	whereSQL := ""
	if len(where) > 0 {
		whereSQL = "WHERE " + strings.Join(where, " AND ")
	}

	var total int
	if err := s.db.QueryRowContext(ctx, "SELECT COUNT(DISTINCT LOWER(a.artist)) FROM albums a "+whereSQL, args...).Scan(&total); err != nil {
		return nil, 0, err
	}

	limit := f.Limit
	if limit <= 0 {
		limit = 500
	}
	if limit > 1000 {
		limit = 1000
	}
	offset := f.Offset
	if offset < 0 {
		offset = 0
	}

	qArgs := append(append([]any(nil), args...), limit, offset)
	rows, err := s.db.QueryContext(ctx, `
SELECT MIN(a.artist) AS name,
       COUNT(*) AS album_count,
       COALESCE(SUM(a.track_count), 0) AS track_count,
       COALESCE(MAX(CASE WHEN a.artwork_url != '' THEN a.artwork_url END), '') AS artwork_url,
       GROUP_CONCAT(DISTINCT a.provider) AS providers
FROM albums a `+whereSQL+`
GROUP BY LOWER(a.artist)
ORDER BY MIN(a.sort_artist) ASC, LOWER(a.artist) ASC
LIMIT ? OFFSET ?`, qArgs...)
	if err != nil {
		return nil, 0, err
	}
	defer func() { _ = rows.Close() }()

	out := make([]ArtistSummary, 0)
	for rows.Next() {
		var a ArtistSummary
		var provCSV string
		if err := rows.Scan(&a.Name, &a.AlbumCount, &a.TrackCount, &a.ArtworkURL, &provCSV); err != nil {
			return nil, 0, err
		}
		if provCSV != "" {
			a.Providers = strings.Split(provCSV, ",")
			sort.Strings(a.Providers)
		} else {
			a.Providers = make([]string, 0)
		}
		out = append(out, a)
	}
	return out, total, rows.Err()
}

func (s *libraryStore) listGenres(ctx context.Context, f FacetFilter) ([]GenreSummary, error) {
	s.mu.RLock()
	defer s.mu.RUnlock()

	where := []string{"a.genre != ''"}
	var args []any
	if !f.IncludeUnavailable {
		where = append(where, "a.available = 1")
	}
	switch strings.ToLower(strings.TrimSpace(f.SourceType)) {
	case SourceTypeLocal, SourceTypeRemote:
		where = append(where, "a.source_type = ?")
		args = append(args, strings.ToLower(strings.TrimSpace(f.SourceType)))
	}
	if p := strings.ToLower(strings.TrimSpace(f.Provider)); p != "" && p != "all" {
		where = append(where, "a.provider = ?")
		args = append(args, p)
	}

	//nolint:gosec // WHERE clauses are fixed static column predicates with ? parameters
	query := `
SELECT MIN(a.genre) AS name, COUNT(*) AS album_count, COALESCE(SUM(a.track_count), 0) AS track_count
FROM albums a
WHERE ` + strings.Join(where, " AND ") + `
GROUP BY LOWER(a.genre)
ORDER BY LOWER(MIN(a.genre)) ASC`
	rows, err := s.db.QueryContext(ctx, query, args...)
	if err != nil {
		return nil, err
	}
	defer func() { _ = rows.Close() }()

	out := make([]GenreSummary, 0)
	for rows.Next() {
		var g GenreSummary
		if err := rows.Scan(&g.Name, &g.AlbumCount, &g.TrackCount); err != nil {
			return nil, err
		}
		out = append(out, g)
	}
	return out, rows.Err()
}

type storeCounts struct {
	totalTracks   int
	totalAlbums   int
	localTracks   int
	localAlbums   int
	remoteTracks  int
	remoteAlbums  int
	missingTracks int
	missingAlbums int
	bootID        string
	gen           uint64
	states        map[string]ProviderSyncStatus
}

func (s *libraryStore) countsAndProviders(ctx context.Context) storeCounts {
	s.mu.RLock()
	defer s.mu.RUnlock()

	out := storeCounts{
		bootID: s.bootID,
		gen:    s.gen,
		states: make(map[string]ProviderSyncStatus),
	}

	_ = s.db.QueryRowContext(ctx, `SELECT COUNT(*) FROM tracks WHERE available = 1`).Scan(&out.totalTracks)
	_ = s.db.QueryRowContext(ctx, `SELECT COUNT(*) FROM albums WHERE available = 1`).Scan(&out.totalAlbums)
	_ = s.db.QueryRowContext(ctx, `SELECT COUNT(*) FROM tracks WHERE source_type = 'local' AND available = 1`).Scan(&out.localTracks)
	_ = s.db.QueryRowContext(ctx, `SELECT COUNT(*) FROM albums WHERE source_type = 'local' AND available = 1`).Scan(&out.localAlbums)
	_ = s.db.QueryRowContext(ctx, `SELECT COUNT(*) FROM tracks WHERE source_type = 'remote' AND available = 1`).Scan(&out.remoteTracks)
	_ = s.db.QueryRowContext(ctx, `SELECT COUNT(*) FROM albums WHERE source_type = 'remote' AND available = 1`).Scan(&out.remoteAlbums)
	_ = s.db.QueryRowContext(ctx, `SELECT COUNT(*) FROM tracks WHERE available = 0 AND missing_since > 0`).Scan(&out.missingTracks)
	_ = s.db.QueryRowContext(ctx, `SELECT COUNT(*) FROM albums WHERE available = 0 AND missing_since > 0`).Scan(&out.missingAlbums)

	if rows, err := s.db.QueryContext(ctx, `SELECT provider, source_type, last_sync_at, last_error FROM provider_state`); err == nil {
		for rows.Next() {
			var ps ProviderSyncStatus
			var syncUnix int64
			if rows.Scan(&ps.Provider, &ps.SourceType, &syncUnix, &ps.LastError) == nil {
				if syncUnix > 0 {
					ts := time.Unix(syncUnix, 0).UTC()
					ps.LastSyncAt = &ts
				}
				out.states[ps.Provider] = ps
			}
		}
		_ = rows.Close()
	}

	if rows, err := s.db.QueryContext(ctx, `SELECT provider, COUNT(*) FROM albums WHERE available = 1 GROUP BY provider`); err == nil {
		for rows.Next() {
			var p string
			var n int
			if rows.Scan(&p, &n) == nil {
				ps := out.states[p]
				ps.Provider = p
				ps.Albums = n
				out.states[p] = ps
			}
		}
		_ = rows.Close()
	}

	if rows, err := s.db.QueryContext(ctx, `SELECT provider, COUNT(*) FROM tracks WHERE available = 1 GROUP BY provider`); err == nil {
		for rows.Next() {
			var p string
			var n int
			if rows.Scan(&p, &n) == nil {
				ps := out.states[p]
				ps.Provider = p
				ps.Tracks = n
				out.states[p] = ps
			}
		}
		_ = rows.Close()
	}

	return out
}

func scanTracks(rows *sql.Rows) ([]Track, error) {
	items := make([]Track, 0)
	for rows.Next() {
		var t Track
		var hasEmbed, loved, avail int
		var lastPlayedUnix, addedUnix int64
		if err := rows.Scan(
			&t.ID, &t.AlbumID, &t.SourceType, &t.Provider, &t.ExternalID,
			&t.Title, &t.Artist, &t.AlbumArtist, &t.Album, &t.Genre,
			&t.DiscNumber, &t.TrackNumber, &t.Year, &t.DurationSec,
			&t.SampleRate, &t.BitDepth, &t.Channels, &t.MIME, &t.Ext, &t.Size,
			&t.ArtworkURL, &t.FilePath, &t.FileMtimeNS, &t.FolderArt, &hasEmbed, &t.QobuzTrackID,
			&t.PlayCount, &lastPlayedUnix, &loved, &avail, &addedUnix,
		); err != nil {
			return nil, err
		}
		t.Source = Source(t.Provider)
		t.HasEmbedArt = hasEmbed != 0
		t.Loved = loved != 0
		t.Available = avail != 0
		t.InLibrary = true
		if lastPlayedUnix > 0 {
			ts := time.Unix(lastPlayedUnix, 0).UTC()
			t.LastPlayedAt = &ts
		}
		if addedUnix > 0 {
			ts := time.Unix(addedUnix, 0).UTC()
			t.AddedAt = &ts
		}
		items = append(items, t)
	}
	return items, rows.Err()
}

// buildFTSQuery tokenizes user input into safe FTS5 prefix queries ("word1"* "word2"*)
// so arbitrary punctuation never triggers FTS5 syntax errors.
func buildFTSQuery(raw string) string {
	raw = strings.TrimSpace(raw)
	if raw == "" {
		return ""
	}
	var tokens []string
	var cur strings.Builder
	flush := func() {
		if cur.Len() > 0 {
			tokens = append(tokens, strconv.Quote(cur.String())+"*")
			cur.Reset()
		}
	}
	for _, r := range raw {
		if unicode.IsLetter(r) || unicode.IsDigit(r) {
			cur.WriteRune(unicode.ToLower(r))
		} else {
			flush()
		}
	}
	flush()
	return strings.Join(tokens, " ")
}

func boolToInt(b bool) int {
	if b {
		return 1
	}
	return 0
}

// unifiedSearch queries artists, albums (version-grouped), and tracks in SQLite
// and ranks results with intent scoring so exact/prefix matches surface the right
// entity type (artist, album, or track) as TopResult.
func (s *libraryStore) unifiedSearch(ctx context.Context, rawQuery string, limit int) (UnifiedSearchResult, error) {
	q := strings.TrimSpace(rawQuery)
	if limit <= 0 {
		limit = 8
	}
	if limit > 50 {
		limit = 50
	}
	res := UnifiedSearchResult{
		Query:   q,
		Artists: make([]ArtistSummary, 0),
		Albums:  make([]Album, 0),
		Tracks:  make([]Track, 0),
	}
	if q == "" {
		return res, nil
	}

	candidateLimit := max(limit*4, 32)
	artists, _, err := s.listArtists(ctx, FacetFilter{
		Query: q,
		Limit: candidateLimit,
	})
	if err != nil {
		return res, err
	}
	albums, _, err := s.queryAlbums(ctx, AlbumFilter{
		Query:         q,
		GroupVersions: true,
		Limit:         candidateLimit,
	})
	if err != nil {
		return res, err
	}
	tracks, _, err := s.queryTracks(ctx, TrackFilter{
		Query: q,
		Limit: candidateLimit,
	})
	if err != nil {
		return res, err
	}

	normQ := normalizeSearchText(q)
	qTokens := strings.Fields(normQ)

	type scoredArtist struct {
		item  ArtistSummary
		score int
	}
	scoredArtists := make([]scoredArtist, 0, len(artists))
	for _, a := range artists {
		sc := scoreSearchText(a.Name, normQ, qTokens)
		if sc > 0 {
			// Exact artist match gets a small boost over an eponymous track/album.
			if normalizeSearchText(a.Name) == normQ {
				sc += 40
			}
			sc += min(a.AlbumCount*5, 30)
		}
		scoredArtists = append(scoredArtists, scoredArtist{item: a, score: sc})
	}
	sort.SliceStable(scoredArtists, func(i, j int) bool {
		if scoredArtists[i].score != scoredArtists[j].score {
			return scoredArtists[i].score > scoredArtists[j].score
		}
		if scoredArtists[i].item.AlbumCount != scoredArtists[j].item.AlbumCount {
			return scoredArtists[i].item.AlbumCount > scoredArtists[j].item.AlbumCount
		}
		return strings.ToLower(scoredArtists[i].item.Name) < strings.ToLower(scoredArtists[j].item.Name)
	})

	type scoredAlbum struct {
		item  Album
		score int
	}
	scoredAlbums := make([]scoredAlbum, 0, len(albums))
	for _, a := range albums {
		titleScore := scoreSearchText(a.Title, normQ, qTokens)
		comboScore := scoreSearchText(a.Artist+" "+a.Title, normQ, qTokens)
		// Artist-only hits still appear in Albums, but score lower than direct title matches
		// so searching an artist name surfaces the Artist as TopResult instead of a random album.
		artistScore := scoreSearchText(a.Artist, normQ, qTokens) / 3
		sc := max(titleScore, comboScore, artistScore)
		if titleScore >= 500 {
			sc += min(a.PlayCount*3, 25)
		}
		scoredAlbums = append(scoredAlbums, scoredAlbum{item: a, score: sc})
	}
	sort.SliceStable(scoredAlbums, func(i, j int) bool {
		if scoredAlbums[i].score != scoredAlbums[j].score {
			return scoredAlbums[i].score > scoredAlbums[j].score
		}
		if scoredAlbums[i].item.PlayCount != scoredAlbums[j].item.PlayCount {
			return scoredAlbums[i].item.PlayCount > scoredAlbums[j].item.PlayCount
		}
		return strings.ToLower(scoredAlbums[i].item.Title) < strings.ToLower(scoredAlbums[j].item.Title)
	})

	type scoredTrack struct {
		item  Track
		score int
	}
	scoredTracks := make([]scoredTrack, 0, len(tracks))
	for _, t := range tracks {
		titleScore := scoreSearchText(t.Title, normQ, qTokens)
		comboScore := scoreSearchText(t.Artist+" "+t.Title, normQ, qTokens)
		metaScore := max(scoreSearchText(t.Artist, normQ, qTokens), scoreSearchText(t.Album, normQ, qTokens)) / 4
		sc := max(titleScore, comboScore, metaScore)
		if titleScore >= 500 {
			sc += min(t.PlayCount*3, 25)
		}
		scoredTracks = append(scoredTracks, scoredTrack{item: t, score: sc})
	}
	sort.SliceStable(scoredTracks, func(i, j int) bool {
		if scoredTracks[i].score != scoredTracks[j].score {
			return scoredTracks[i].score > scoredTracks[j].score
		}
		if scoredTracks[i].item.PlayCount != scoredTracks[j].item.PlayCount {
			return scoredTracks[i].item.PlayCount > scoredTracks[j].item.PlayCount
		}
		return strings.ToLower(scoredTracks[i].item.Title) < strings.ToLower(scoredTracks[j].item.Title)
	})

	for i := 0; i < len(scoredArtists) && i < limit; i++ {
		res.Artists = append(res.Artists, scoredArtists[i].item)
	}
	for i := 0; i < len(scoredAlbums) && i < limit; i++ {
		res.Albums = append(res.Albums, scoredAlbums[i].item)
	}
	for i := 0; i < len(scoredTracks) && i < limit; i++ {
		res.Tracks = append(res.Tracks, scoredTracks[i].item)
	}

	var best *TopSearchResult
	if len(scoredArtists) > 0 {
		a := scoredArtists[0].item
		best = &TopSearchResult{Type: "artist", Score: scoredArtists[0].score, Artist: &a}
	}
	if len(scoredAlbums) > 0 && (best == nil || scoredAlbums[0].score > best.Score) {
		a := scoredAlbums[0].item
		best = &TopSearchResult{Type: "album", Score: scoredAlbums[0].score, Album: &a}
	}
	if len(scoredTracks) > 0 && (best == nil || scoredTracks[0].score > best.Score) {
		t := scoredTracks[0].item
		best = &TopSearchResult{Type: "track", Score: scoredTracks[0].score, Track: &t}
	}
	res.TopResult = best
	return res, nil
}

func normalizeSearchText(s string) string {
	var b strings.Builder
	b.Grow(len(s))
	lastSpace := true
	for _, r := range strings.ToLower(s) {
		if unicode.IsLetter(r) || unicode.IsDigit(r) {
			b.WriteRune(r)
			lastSpace = false
		} else if !lastSpace {
			b.WriteByte(' ')
			lastSpace = true
		}
	}
	return strings.TrimSpace(b.String())
}

func scoreSearchText(candidate, normQ string, qTokens []string) int {
	if normQ == "" {
		return 0
	}
	cand := normalizeSearchText(candidate)
	if cand == "" {
		return 0
	}
	if cand == normQ {
		return 1000
	}
	// Match ignoring leading "the " (e.g. "beatles" matches "The Beatles").
	trimmedCand := strings.TrimPrefix(cand, "the ")
	if trimmedCand != cand && trimmedCand == normQ {
		return 960
	}
	if strings.HasPrefix(cand, normQ) {
		return 820 - min(len(cand)-len(normQ), 120)
	}
	if trimmedCand != cand && strings.HasPrefix(trimmedCand, normQ) {
		return 790 - min(len(trimmedCand)-len(normQ), 120)
	}
	words := strings.Fields(cand)
	if len(qTokens) > 0 {
		allPrefix := true
		for _, qt := range qTokens {
			matched := false
			for _, w := range words {
				if strings.HasPrefix(w, qt) {
					matched = true
					break
				}
			}
			if !matched {
				allPrefix = false
				break
			}
		}
		if allPrefix {
			return 620 - min(len(cand)-len(normQ), 120)
		}
	}
	if strings.Contains(cand, normQ) {
		return 420 - min(len(cand)-len(normQ), 120)
	}
	if len(qTokens) > 0 {
		matched := 0
		for _, qt := range qTokens {
			if strings.Contains(cand, qt) {
				matched++
			}
		}
		if matched == len(qTokens) {
			return 260
		}
	}
	return 100
}

func (s *libraryStore) getPref(ctx context.Context, key string) (string, bool, error) {
	key = strings.TrimSpace(key)
	if key == "" {
		return "", false, ErrInvalidArgument
	}
	s.mu.RLock()
	defer s.mu.RUnlock()
	var val string
	err := s.db.QueryRowContext(ctx, `SELECT value FROM app_prefs WHERE key = ?`, key).Scan(&val)
	if errors.Is(err, sql.ErrNoRows) {
		return "", false, nil
	}
	if err != nil {
		return "", false, err
	}
	return val, true, nil
}

func (s *libraryStore) setPref(ctx context.Context, key, value string) error {
	key = strings.TrimSpace(key)
	if key == "" {
		return ErrInvalidArgument
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	_, err := s.db.ExecContext(ctx, `
INSERT INTO app_prefs(key, value, updated_at) VALUES (?, ?, ?)
ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at`,
		key, value, s.now().Unix())
	return err
}

func (s *libraryStore) deletePref(ctx context.Context, key string) error {
	key = strings.TrimSpace(key)
	if key == "" {
		return ErrInvalidArgument
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	_, err := s.db.ExecContext(ctx, `DELETE FROM app_prefs WHERE key = ?`, key)
	return err
}

func (s *libraryStore) listPrefs(ctx context.Context) (map[string]string, error) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	rows, err := s.db.QueryContext(ctx, `SELECT key, value FROM app_prefs ORDER BY key ASC`)
	if err != nil {
		return nil, err
	}
	defer func() { _ = rows.Close() }()
	out := make(map[string]string)
	for rows.Next() {
		var k, v string
		if err := rows.Scan(&k, &v); err != nil {
			return nil, err
		}
		out[k] = v
	}
	return out, rows.Err()
}

func (s *libraryStore) getProviderCredentials(ctx context.Context, provider string, dst any) (bool, error) {
	provider = strings.ToLower(strings.TrimSpace(provider))
	if provider == "" {
		return false, ErrInvalidArgument
	}
	s.mu.RLock()
	defer s.mu.RUnlock()
	var raw string
	err := s.db.QueryRowContext(ctx, `SELECT data_json FROM provider_credentials WHERE provider = ?`, provider).Scan(&raw)
	if errors.Is(err, sql.ErrNoRows) {
		return false, nil
	}
	if err != nil {
		return false, err
	}
	if err := json.Unmarshal([]byte(raw), dst); err != nil {
		return false, fmt.Errorf("decode credentials for %q: %w", provider, err)
	}
	return true, nil
}

func (s *libraryStore) setProviderCredentials(ctx context.Context, provider string, src any) error {
	provider = strings.ToLower(strings.TrimSpace(provider))
	if provider == "" {
		return ErrInvalidArgument
	}
	raw, err := json.Marshal(src)
	if err != nil {
		return fmt.Errorf("encode credentials for %q: %w", provider, err)
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	_, err = s.db.ExecContext(ctx, `
INSERT INTO provider_credentials(provider, data_json, updated_at) VALUES (?, ?, ?)
ON CONFLICT(provider) DO UPDATE SET data_json = excluded.data_json, updated_at = excluded.updated_at`,
		provider, string(raw), s.now().Unix())
	return err
}

func (s *libraryStore) deleteProviderCredentials(ctx context.Context, provider string) error {
	provider = strings.ToLower(strings.TrimSpace(provider))
	if provider == "" {
		return ErrInvalidArgument
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	_, err := s.db.ExecContext(ctx, `DELETE FROM provider_credentials WHERE provider = ?`, provider)
	return err
}
