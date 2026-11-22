package qobuz

import (
	"bufio"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"os"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"sync"
	"time"

	"tacet/internal/library"
)

const (
	diskCacheVersion = 1

	albumCacheTTL          = 7 * 24 * time.Hour
	artistCacheTTL         = 24 * time.Hour
	artistNameCacheTTL     = 7 * 24 * time.Hour
	artistNameMissCacheTTL = 24 * time.Hour
	searchCacheTTL         = time.Hour

	maxCachedAlbums      = 1000
	maxCachedArtists     = 500
	maxCachedArtistNames = 1000
	maxCachedSearches    = 250

	defaultCacheFlushDelay = 15 * time.Second
)

type cacheEntry[T any] struct {
	Value      T         `json:"value"`
	ExpiresAt  time.Time `json:"expiresAt"`
	LastAccess time.Time `json:"lastAccess"`
}

type diskCacheSnapshot struct {
	Version     int                                  `json:"version"`
	Albums      map[string]cacheEntry[library.Album] `json:"albums,omitempty"`
	Artists     map[string]cacheEntry[ArtistDetail]  `json:"artists,omitempty"`
	ArtistNames map[string]cacheEntry[int64]         `json:"artistNames,omitempty"`
	Searches    map[string]cacheEntry[SearchResult]  `json:"searches,omitempty"`
}

// catalogCache is a bounded TTL/LRU cache for slow-changing Qobuz catalog metadata
// backed by an optional atomic JSON file on disk.
type catalogCache struct {
	mu         sync.Mutex
	saveMu     sync.Mutex
	path       string
	now        func() time.Time
	flushDelay time.Duration
	timer      *time.Timer
	dirty      bool
	closed     bool

	albums      map[string]cacheEntry[library.Album]
	artists     map[string]cacheEntry[ArtistDetail]
	artistNames map[string]cacheEntry[int64]
	searches    map[string]cacheEntry[SearchResult]
}

func newCatalogCache(path string) *catalogCache {
	cc := &catalogCache{
		path:        strings.TrimSpace(path),
		now:         time.Now,
		flushDelay:  defaultCacheFlushDelay,
		albums:      make(map[string]cacheEntry[library.Album]),
		artists:     make(map[string]cacheEntry[ArtistDetail]),
		artistNames: make(map[string]cacheEntry[int64]),
		searches:    make(map[string]cacheEntry[SearchResult]),
	}
	if cc.path != "" {
		cleanOrphanedTempFiles(cc.path)
		cc.load()
	}
	return cc
}

func (cc *catalogCache) getAlbum(rawID string) (library.Album, bool) {
	if rawID == "" {
		return library.Album{}, false
	}
	cc.mu.Lock()
	defer cc.mu.Unlock()
	now := cc.now()
	e, ok := cc.albums[rawID]
	if !ok {
		return library.Album{}, false
	}
	if !now.Before(e.ExpiresAt) {
		delete(cc.albums, rawID)
		cc.scheduleSaveLocked()
		return library.Album{}, false
	}
	e.LastAccess = now
	cc.albums[rawID] = e
	return cloneAlbum(e.Value), true
}

func (cc *catalogCache) putAlbum(rawID string, alb library.Album) {
	if rawID == "" {
		return
	}
	cc.mu.Lock()
	defer cc.mu.Unlock()
	now := cc.now()
	cc.albums[rawID] = cacheEntry[library.Album]{
		Value:      cloneAlbum(alb),
		ExpiresAt:  now.Add(albumCacheTTL),
		LastAccess: now,
	}
	evictMap(cc.albums, maxCachedAlbums, now)
	cc.scheduleSaveLocked()
}

func artistCacheKey(artistID int64, limit int) string {
	return fmt.Sprintf("%d:%d", artistID, limit)
}

func (cc *catalogCache) getArtist(artistID int64, limit int) (ArtistDetail, bool) {
	if artistID <= 0 {
		return ArtistDetail{}, false
	}
	key := artistCacheKey(artistID, limit)
	cc.mu.Lock()
	defer cc.mu.Unlock()
	now := cc.now()
	e, ok := cc.artists[key]
	if !ok {
		return ArtistDetail{}, false
	}
	if !now.Before(e.ExpiresAt) {
		delete(cc.artists, key)
		cc.scheduleSaveLocked()
		return ArtistDetail{}, false
	}
	e.LastAccess = now
	cc.artists[key] = e
	return cloneArtistDetail(e.Value), true
}

func (cc *catalogCache) putArtist(artistID int64, limit int, detail ArtistDetail) {
	if artistID <= 0 {
		return
	}
	key := artistCacheKey(artistID, limit)
	cc.mu.Lock()
	defer cc.mu.Unlock()
	now := cc.now()
	cc.artists[key] = cacheEntry[ArtistDetail]{
		Value:      cloneArtistDetail(detail),
		ExpiresAt:  now.Add(artistCacheTTL),
		LastAccess: now,
	}
	evictMap(cc.artists, maxCachedArtists, now)
	cc.scheduleSaveLocked()
}

// getArtistName returns (artistID, found). A cached negative miss returns (0, true).
func (cc *catalogCache) getArtistName(name string) (int64, bool) {
	key := strings.ToLower(strings.TrimSpace(name))
	if key == "" {
		return 0, false
	}
	cc.mu.Lock()
	defer cc.mu.Unlock()
	now := cc.now()
	e, ok := cc.artistNames[key]
	if !ok {
		return 0, false
	}
	if !now.Before(e.ExpiresAt) {
		delete(cc.artistNames, key)
		cc.scheduleSaveLocked()
		return 0, false
	}
	e.LastAccess = now
	cc.artistNames[key] = e
	return e.Value, true
}

// putArtistName caches an artist name -> Qobuz ID mapping (id == 0 records a negative miss).
// When overwrite is false, an existing unexpired positive ID is preserved so opportunistic
// search caching cannot replace a primary artist with a less-popular homonymous match.
func (cc *catalogCache) putArtistName(name string, id int64, overwrite bool) {
	key := strings.ToLower(strings.TrimSpace(name))
	if key == "" || id < 0 {
		return
	}
	ttl := artistNameCacheTTL
	if id == 0 {
		ttl = artistNameMissCacheTTL
	}
	cc.mu.Lock()
	defer cc.mu.Unlock()
	now := cc.now()
	if existing, ok := cc.artistNames[key]; ok && now.Before(existing.ExpiresAt) {
		if existing.Value == id || (!overwrite && existing.Value > 0) {
			// Avoid dirtying disk cache on identical repeat hits or lower-priority search overwrites.
			existing.LastAccess = now
			cc.artistNames[key] = existing
			return
		}
	}
	cc.artistNames[key] = cacheEntry[int64]{
		Value:      id,
		ExpiresAt:  now.Add(ttl),
		LastAccess: now,
	}
	evictMap(cc.artistNames, maxCachedArtistNames, now)
	cc.scheduleSaveLocked()
}

func searchCacheKey(query string, limit, offset int) string {
	return fmt.Sprintf("%s\x00%d:%d", strings.ToLower(strings.TrimSpace(query)), limit, offset)
}

func (cc *catalogCache) getSearch(query string, limit, offset int) (SearchResult, bool) {
	key := searchCacheKey(query, limit, offset)
	cc.mu.Lock()
	defer cc.mu.Unlock()
	now := cc.now()
	e, ok := cc.searches[key]
	if !ok {
		return SearchResult{}, false
	}
	if !now.Before(e.ExpiresAt) {
		delete(cc.searches, key)
		cc.scheduleSaveLocked()
		return SearchResult{}, false
	}
	e.LastAccess = now
	cc.searches[key] = e
	return cloneSearchResult(e.Value), true
}

func (cc *catalogCache) putSearch(query string, limit, offset int, res SearchResult) {
	key := searchCacheKey(query, limit, offset)
	cc.mu.Lock()
	defer cc.mu.Unlock()
	now := cc.now()
	cc.searches[key] = cacheEntry[SearchResult]{
		Value:      cloneSearchResult(res),
		ExpiresAt:  now.Add(searchCacheTTL),
		LastAccess: now,
	}
	evictMap(cc.searches, maxCachedSearches, now)
	cc.scheduleSaveLocked()
}

func (cc *catalogCache) clear() {
	cc.mu.Lock()
	clear(cc.albums)
	clear(cc.artists)
	clear(cc.artistNames)
	clear(cc.searches)
	if cc.timer != nil {
		cc.timer.Stop()
		cc.timer = nil
	}
	cc.dirty = true
	path := cc.path
	cc.mu.Unlock()

	if path != "" {
		_ = cc.Flush()
	}
}

func (cc *catalogCache) stats() (albums, artists, artistNames, searches int) {
	cc.mu.Lock()
	defer cc.mu.Unlock()
	return len(cc.albums), len(cc.artists), len(cc.artistNames), len(cc.searches)
}

func (cc *catalogCache) scheduleSaveLocked() {
	if cc.path == "" || cc.closed {
		return
	}
	cc.dirty = true
	if cc.timer == nil {
		cc.timer = time.AfterFunc(cc.flushDelay, func() {
			if err := cc.Flush(); err != nil {
				log.Printf("warning: failed to persist Qobuz cache: %v", err)
			}
		})
	}
}

// Flush persists pending cache changes atomically to disk.
func (cc *catalogCache) Flush() error {
	cc.saveMu.Lock()
	defer cc.saveMu.Unlock()

	cc.mu.Lock()
	if cc.path == "" || !cc.dirty {
		cc.mu.Unlock()
		return nil
	}
	if cc.timer != nil {
		cc.timer.Stop()
		cc.timer = nil
	}
	now := cc.now()
	pruneExpired(cc.albums, now)
	pruneExpired(cc.artists, now)
	pruneExpired(cc.artistNames, now)
	pruneExpired(cc.searches, now)

	snap := diskCacheSnapshot{
		Version:     diskCacheVersion,
		Albums:      make(map[string]cacheEntry[library.Album], len(cc.albums)),
		Artists:     make(map[string]cacheEntry[ArtistDetail], len(cc.artists)),
		ArtistNames: make(map[string]cacheEntry[int64], len(cc.artistNames)),
		Searches:    make(map[string]cacheEntry[SearchResult], len(cc.searches)),
	}
	for k, v := range cc.albums {
		v.Value = cloneAlbum(v.Value)
		snap.Albums[k] = v
	}
	for k, v := range cc.artists {
		v.Value = cloneArtistDetail(v.Value)
		snap.Artists[k] = v
	}
	for k, v := range cc.artistNames {
		snap.ArtistNames[k] = v
	}
	for k, v := range cc.searches {
		v.Value = cloneSearchResult(v.Value)
		snap.Searches[k] = v
	}
	path := cc.path
	cc.dirty = false
	cc.mu.Unlock()

	if err := writeJSONAtomic(path, snap); err != nil {
		cc.mu.Lock()
		cc.dirty = true
		cc.mu.Unlock()
		return err
	}
	return nil
}

// Close stops background flush timers and writes any pending changes to disk.
func (cc *catalogCache) Close() error {
	cc.mu.Lock()
	cc.closed = true
	if cc.timer != nil {
		cc.timer.Stop()
		cc.timer = nil
	}
	cc.mu.Unlock()
	return cc.Flush()
}

func (cc *catalogCache) load() {
	raw, err := os.ReadFile(cc.path)
	if err != nil {
		if !errors.Is(err, os.ErrNotExist) {
			log.Printf("warning: failed to read Qobuz cache %s: %v", cc.path, err)
		}
		return
	}
	var snap diskCacheSnapshot
	if err := json.Unmarshal(raw, &snap); err != nil {
		log.Printf("warning: ignoring corrupt Qobuz cache %s: %v", cc.path, err)
		return
	}
	if snap.Version != diskCacheVersion {
		return
	}
	now := cc.now()
	cc.mu.Lock()
	defer cc.mu.Unlock()
	for k, v := range snap.Albums {
		if now.Before(v.ExpiresAt) {
			rehydrateTracksQobuzIDs(v.Value.Tracks)
			cc.albums[k] = v
		}
	}
	for k, v := range snap.Artists {
		if now.Before(v.ExpiresAt) {
			for i := range v.Value.Albums {
				rehydrateTracksQobuzIDs(v.Value.Albums[i].Tracks)
			}
			cc.artists[k] = v
		}
	}
	for k, v := range snap.ArtistNames {
		if now.Before(v.ExpiresAt) {
			cc.artistNames[k] = v
		}
	}
	for k, v := range snap.Searches {
		if now.Before(v.ExpiresAt) {
			rehydrateTracksQobuzIDs(v.Value.Tracks)
			cc.searches[k] = v
		}
	}
	evictMap(cc.albums, maxCachedAlbums, now)
	evictMap(cc.artists, maxCachedArtists, now)
	evictMap(cc.artistNames, maxCachedArtistNames, now)
	evictMap(cc.searches, maxCachedSearches, now)
}

func rehydrateTrackQobuzID(tr *library.Track) {
	if tr.QobuzTrackID > 0 {
		return
	}
	if tr.Source != library.SourceQobuz && !strings.HasPrefix(tr.ID, "qobuz:") {
		return
	}
	raw := strings.TrimSpace(tr.ExternalID)
	if raw == "" {
		raw = strings.TrimPrefix(strings.TrimSpace(tr.ID), "qobuz:")
	}
	if id, err := strconv.ParseInt(raw, 10, 64); err == nil && id > 0 {
		tr.QobuzTrackID = id
	}
}

func rehydrateTracksQobuzIDs(tracks []library.Track) {
	for i := range tracks {
		rehydrateTrackQobuzID(&tracks[i])
	}
}

func cleanOrphanedTempFiles(cachePath string) {
	dir := filepath.Dir(cachePath)
	matches, err := filepath.Glob(filepath.Join(dir, ".qobuz-cache-*.tmp"))
	if err != nil {
		return
	}
	for _, m := range matches {
		_ = os.Remove(m)
	}
}

func writeJSONAtomic(path string, v any) error {
	dir := filepath.Dir(path)
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return fmt.Errorf("create cache dir: %w", err)
	}
	tmp, err := os.CreateTemp(dir, ".qobuz-cache-*.tmp")
	if err != nil {
		return fmt.Errorf("create temp cache file: %w", err)
	}
	tmpPath := tmp.Name()
	cleanup := func() { _ = os.Remove(tmpPath) }

	// os.CreateTemp already sets 0600 on POSIX; ignore chmod errors on ACL/CIFS bind mounts.
	_ = tmp.Chmod(0o600)

	bw := bufio.NewWriterSize(tmp, 64*1024)
	enc := json.NewEncoder(bw)
	if err := enc.Encode(v); err != nil {
		_ = tmp.Close()
		cleanup()
		return fmt.Errorf("encode cache: %w", err)
	}
	if err := bw.Flush(); err != nil {
		_ = tmp.Close()
		cleanup()
		return fmt.Errorf("flush temp cache file: %w", err)
	}
	if err := tmp.Close(); err != nil {
		cleanup()
		return fmt.Errorf("close temp cache file: %w", err)
	}
	if err := os.Rename(tmpPath, path); err != nil {
		cleanup()
		return fmt.Errorf("rename cache file: %w", err)
	}
	return nil
}

func pruneExpired[T any](m map[string]cacheEntry[T], now time.Time) {
	for k, e := range m {
		if !now.Before(e.ExpiresAt) {
			delete(m, k)
		}
	}
}

func evictMap[T any](m map[string]cacheEntry[T], maxEntries int, now time.Time) {
	if len(m) <= maxEntries {
		return
	}
	pruneExpired(m, now)
	for len(m) > maxEntries {
		var oldestKey string
		var oldestTime time.Time
		first := true
		for k, e := range m {
			if first || e.LastAccess.Before(oldestTime) {
				oldestKey = k
				oldestTime = e.LastAccess
				first = false
			}
		}
		if first {
			break
		}
		delete(m, oldestKey)
	}
}

func cloneAlbum(a library.Album) library.Album {
	a.Tracks = slices.Clone(a.Tracks)
	a.Versions = slices.Clone(a.Versions)
	return a
}

func cloneArtistDetail(d ArtistDetail) ArtistDetail {
	if d.Albums != nil {
		albums := make([]library.Album, len(d.Albums))
		for i := range d.Albums {
			albums[i] = cloneAlbum(d.Albums[i])
		}
		d.Albums = albums
	}
	d.Tracks = slices.Clone(d.Tracks)
	return d
}

func cloneSearchResult(r SearchResult) SearchResult {
	r.Artists = slices.Clone(r.Artists)
	r.Tracks = slices.Clone(r.Tracks)
	if r.Albums != nil {
		albums := make([]library.Album, len(r.Albums))
		for i := range r.Albums {
			albums[i] = cloneAlbum(r.Albums[i])
		}
		r.Albums = albums
	}
	return r
}
