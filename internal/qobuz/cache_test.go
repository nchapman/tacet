package qobuz

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"sync/atomic"
	"testing"
	"time"

	"tacet/internal/cache"
	"tacet/internal/library"
)

func TestCatalogCache_DiskPersistenceAndHits(t *testing.T) {
	var albumCalls, artistCalls, artistSearchCalls, searchCalls atomic.Int32

	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/album/get":
			albumCalls.Add(1)
			_ = json.NewEncoder(w).Encode(map[string]any{
				"id":           "alb-42",
				"title":        "Kind of Blue",
				"artist":       map[string]any{"id": 100, "name": "Miles Davis"},
				"tracks_count": 1,
				"tracks": map[string]any{
					"items": []map[string]any{
						{
							"id":           9001,
							"title":        "So What",
							"track_number": 1,
							"duration":     562,
							"streamable":   true,
						},
					},
				},
			})
		case "/artist/search":
			artistSearchCalls.Add(1)
			q := r.URL.Query().Get("query")
			if q == "Unknown Local Band" {
				_ = json.NewEncoder(w).Encode(map[string]any{
					"artists": map[string]any{"items": []any{}},
				})
				return
			}
			_ = json.NewEncoder(w).Encode(map[string]any{
				"artists": map[string]any{
					"items": []map[string]any{
						{"id": 100, "name": "Miles Davis", "albums_count": 20},
					},
				},
			})
		case "/artist/get":
			artistCalls.Add(1)
			_ = json.NewEncoder(w).Encode(map[string]any{
				"id":           100,
				"name":         "Miles Davis",
				"albums_count": 20,
				"albums": map[string]any{
					"items": []map[string]any{
						{
							"id":         "alb-42",
							"title":      "Kind of Blue",
							"artist":     map[string]any{"id": 100, "name": "Miles Davis"},
							"streamable": true,
						},
					},
				},
			})
		case "/catalog/search":
			searchCalls.Add(1)
			_ = json.NewEncoder(w).Encode(map[string]any{
				"artists": map[string]any{
					"items": []map[string]any{{"id": 100, "name": "Miles Davis"}},
				},
				"albums": map[string]any{
					"items": []map[string]any{
						{"id": "alb-42", "title": "Kind of Blue", "artist": map[string]any{"id": 100, "name": "Miles Davis"}, "streamable": true},
					},
				},
				"tracks": map[string]any{
					"items": []map[string]any{
						{"id": 9001, "title": "So What", "streamable": true},
					},
				},
			})
		default:
			http.NotFound(w, r)
		}
	}))
	defer srv.Close()

	cachePath := filepath.Join(t.TempDir(), "cache.db")
	kv1, err := cache.Open(cachePath)
	if err != nil {
		t.Fatalf("cache.Open: %v", err)
	}
	cfg := Config{
		BaseURL:   srv.URL,
		AppID:     "app",
		AppSecret: "sec",
		AuthToken: "tok",
		Cache:     kv1,
	}

	c1 := NewClient(cfg)
	ctx := context.Background()

	// First calls hit the upstream test server.
	alb, err := c1.GetAlbum(ctx, "qobuz:alb-42")
	if err != nil {
		t.Fatalf("GetAlbum: %v", err)
	}
	if len(alb.Tracks) != 1 || alb.Tracks[0].Title != "So What" {
		t.Fatalf("unexpected album tracks: %+v", alb.Tracks)
	}
	// Mutating returned slice must not corrupt cached entry.
	alb.Tracks[0].InLibrary = true
	alb2, err := c1.GetAlbum(ctx, "alb-42")
	if err != nil {
		t.Fatalf("GetAlbum cached: %v", err)
	}
	if alb2.Tracks[0].InLibrary {
		t.Fatal("cached album track was mutated by caller")
	}

	if _, err = c1.GetArtistByName(ctx, "Miles Davis", 50); err != nil {
		t.Fatalf("GetArtistByName: %v", err)
	}
	// Negative artist miss should be cached too.
	if _, err = c1.GetArtistByName(ctx, "Unknown Local Band", 50); !errors.Is(err, ErrNotFound) {
		t.Fatalf("expected ErrNotFound, got %v", err)
	}
	if _, err = c1.GetArtistByName(ctx, "Unknown Local Band", 50); !errors.Is(err, ErrNotFound) {
		t.Fatalf("expected cached ErrNotFound, got %v", err)
	}
	if _, err = c1.Search(ctx, "Miles", 12); err != nil {
		t.Fatalf("Search: %v", err)
	}

	if err = c1.Close(); err != nil {
		t.Fatalf("Close client: %v", err)
	}
	if err = kv1.Close(); err != nil {
		t.Fatalf("Close cache: %v", err)
	}
	if _, statErr := os.Stat(cachePath); statErr != nil {
		t.Fatalf("expected cache db on disk: %v", statErr)
	}

	if albumCalls.Load() != 1 || artistCalls.Load() != 1 || artistSearchCalls.Load() != 2 || searchCalls.Load() != 1 {
		t.Fatalf("unexpected initial upstream call counts: album=%d artist=%d artistSearch=%d search=%d",
			albumCalls.Load(), artistCalls.Load(), artistSearchCalls.Load(), searchCalls.Load())
	}

	// Brand-new client + reopened SQLite cache serves every request with zero upstream calls.
	kv2, err := cache.Open(cachePath)
	if err != nil {
		t.Fatalf("reopen cache: %v", err)
	}
	defer func() { _ = kv2.Close() }()
	cfg.Cache = kv2
	c2 := NewClient(cfg)
	defer func() { _ = c2.Close() }()

	reloadedAlb, err := c2.GetAlbum(ctx, "qobuz:alb-42")
	if err != nil {
		t.Fatalf("c2 GetAlbum: %v", err)
	}
	if len(reloadedAlb.Tracks) != 1 || reloadedAlb.Tracks[0].QobuzTrackID != 9001 {
		t.Fatalf("expected QobuzTrackID 9001 to rehydrate from disk cache, got %+v", reloadedAlb.Tracks)
	}
	// Track cache should be warmed from reloaded album without calling /track/get.
	if resolved, rErr := c2.ResolveTrack(ctx, 9001); rErr != nil || resolved.QobuzTrackID != 9001 {
		t.Fatalf("expected ResolveTrack(9001) to hit trackCache after disk reload, got (%+v, %v)", resolved, rErr)
	}
	if _, err = c2.GetArtistByName(ctx, "Miles Davis", 50); err != nil {
		t.Fatalf("c2 GetArtistByName: %v", err)
	}
	if _, err = c2.GetArtistByName(ctx, "Unknown Local Band", 50); !errors.Is(err, ErrNotFound) {
		t.Fatalf("c2 negative miss expected ErrNotFound, got %v", err)
	}
	if _, err = c2.Search(ctx, "miles", 12); err != nil {
		t.Fatalf("c2 Search: %v", err)
	}

	if albumCalls.Load() != 1 || artistCalls.Load() != 1 || artistSearchCalls.Load() != 2 || searchCalls.Load() != 1 {
		t.Fatalf("expected zero additional upstream calls after disk reload: album=%d artist=%d artistSearch=%d search=%d",
			albumCalls.Load(), artistCalls.Load(), artistSearchCalls.Load(), searchCalls.Load())
	}

	// Logout clears catalog cache both in memory and on disk.
	c2.Logout()
	albs, arts, names, searches := c2.CacheStats()
	if albs+arts+names+searches != 0 {
		t.Fatalf("expected empty cache after logout, got %d/%d/%d/%d", albs, arts, names, searches)
	}
}

func TestCatalogCache_TTLExpiryAndCorruptPayload(t *testing.T) {
	now := time.Date(2026, 1, 1, 12, 0, 0, 0, time.UTC)
	kv, err := cache.Open(":memory:", cache.WithNow(func() time.Time { return now }))
	if err != nil {
		t.Fatalf("cache.Open: %v", err)
	}
	defer func() { _ = kv.Close() }()

	c := NewClient(Config{Cache: kv})
	defer func() { _ = c.Close() }()

	// Corrupt JSON payload in SQLite is treated as a cache miss and self-healed.
	_ = c.kv.Set("album:corrupt", []byte("{not valid json"), time.Hour)
	if _, ok := c.getCachedAlbum("corrupt"); ok {
		t.Fatal("expected corrupt JSON entry to be treated as miss")
	}

	c.putCachedSearch("miles", 10, 0, SearchResult{Artists: []Artist{{ID: 1, Name: "Miles"}}})
	c.putCachedArtistName("Local Only", 0, true)

	if _, ok := c.getCachedSearch("miles", 10, 0); !ok {
		t.Fatal("expected search hit before TTL")
	}
	if id, ok := c.getCachedArtistName("Local Only"); !ok || id != 0 {
		t.Fatalf("expected negative artist hit before TTL, got (%d, %v)", id, ok)
	}

	// Advance past search TTL (1h) but still inside negative artist miss TTL (24h).
	now = now.Add(2 * time.Hour)
	if _, ok := c.getCachedSearch("miles", 10, 0); ok {
		t.Fatal("expected search entry to expire after 2h")
	}
	if id, ok := c.getCachedArtistName("Local Only"); !ok || id != 0 {
		t.Fatalf("expected negative artist miss to still be valid at 2h, got (%d, %v)", id, ok)
	}

	// Advance past 24h negative miss TTL.
	now = now.Add(24 * time.Hour)
	if _, ok := c.getCachedArtistName("Local Only"); ok {
		t.Fatal("expected negative artist miss to expire after 26h")
	}

	// Put-if-absent (Search opportunistic caching) must not overwrite an existing positive artist ID,
	// but MUST allow upgrading a cached negative miss (ID 0) to a positive ID.
	c.putCachedArtistName("Nirvana", 1001, true)
	c.putCachedArtistName("Nirvana", 9999, false)
	if id, ok := c.getCachedArtistName("Nirvana"); !ok || id != 1001 {
		t.Fatalf("expected primary artist ID 1001 to be preserved, got (%d, %v)", id, ok)
	}
	c.putCachedArtistName("Formerly Missing", 0, true)
	c.putCachedArtistName("Formerly Missing", 4242, false)
	if id, ok := c.getCachedArtistName("Formerly Missing"); !ok || id != 4242 {
		t.Fatalf("expected cached miss to upgrade to 4242, got (%d, %v)", id, ok)
	}

	// Cached search tracks must rehydrate QobuzTrackID from ID ("qobuz:<id>").
	c.putCachedSearch("rehydrate", 10, 0, SearchResult{
		Tracks: []library.Track{{ID: "qobuz:55501", Title: "Blue in Green", QobuzTrackID: 55501}},
	})
	if res, ok := c.getCachedSearch("rehydrate", 10, 0); !ok || len(res.Tracks) != 1 || res.Tracks[0].QobuzTrackID != 55501 {
		t.Fatalf("expected cached search track QobuzTrackID 55501, got (%+v, %v)", res, ok)
	}
}

func TestCatalogCache_LogoutPreventsInflightRepopulation(t *testing.T) {
	started := make(chan struct{})
	release := make(chan struct{})

	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/album/get" {
			close(started)
			<-release
			_ = json.NewEncoder(w).Encode(map[string]any{
				"id":           "alb-race",
				"title":        "Race Album",
				"artist":       map[string]any{"id": 1, "name": "Artist"},
				"tracks_count": 1,
				"tracks": map[string]any{
					"items": []map[string]any{
						{"id": 777, "title": "Track 1", "track_number": 1, "duration": 200, "streamable": true},
					},
				},
			})
			return
		}
		http.NotFound(w, r)
	}))
	defer srv.Close()

	c := NewClient(Config{
		BaseURL:   srv.URL,
		AppID:     "app",
		AppSecret: "sec",
		AuthToken: "tok",
	})
	defer func() { _ = c.Close() }()

	done := make(chan error, 1)
	go func() {
		_, err := c.GetAlbum(context.Background(), "alb-race")
		done <- err
	}()

	<-started
	c.Logout()
	close(release)

	if err := <-done; err != nil {
		t.Fatalf("in-flight GetAlbum returned error: %v", err)
	}
	if albs, _, _, _ := c.CacheStats(); albs != 0 {
		t.Fatalf("expected catalog cache to remain empty after logout, got %d albums", albs)
	}
}
