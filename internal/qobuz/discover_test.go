package qobuz

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"sync/atomic"
	"testing"
)

func TestDiscoverFeedAndCaching(t *testing.T) {
	var indexCalls atomic.Int32
	var genreCalls atomic.Int32

	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/genre/list":
			genreCalls.Add(1)
			_ = json.NewEncoder(w).Encode(map[string]any{
				"genres": map[string]any{
					"items": []map[string]any{
						{"id": 80, "name": "Jazz"},
						{"id": 10, "name": "Classical"},
					},
				},
			})
		case "/discover/index":
			indexCalls.Add(1)
			if got := r.URL.Query().Get("genre_ids"); got != "10,80" {
				t.Errorf("genre_ids = %q, want 10,80", got)
			}
			_ = json.NewEncoder(w).Encode(map[string]any{
				"containers": map[string]any{
					"new_releases": map[string]any{
						"data": map[string]any{
							"has_more": true,
							"items": []map[string]any{
								{
									"id":          "alb1",
									"title":       "Blue Train",
									"track_count": 5,
									"image":       map[string]string{"large": "https://img/1.jpg"},
									"artists":     []map[string]any{{"id": 1, "name": "John Coltrane", "roles": []string{"main-artist"}}},
									"genre":       map[string]any{"id": 80, "name": "Jazz"},
									"dates":       map[string]string{"original": "1957-09-15"},
									"audio_info":  map[string]any{"maximum_sampling_rate": 192, "maximum_bit_depth": 24},
									"rights":      map[string]any{"streamable": true},
								},
								{
									"id":           "alb-ai",
									"title":        "AI Slop",
									"ai_generated": true,
									"rights":       map[string]any{"streamable": true},
								},
								{
									"id":     "alb-unstreamable",
									"title":  "Unavailable",
									"rights": map[string]any{"streamable": false},
								},
							},
						},
					},
					"playlists": map[string]any{
						"data": map[string]any{
							"has_more": true,
							"items": []map[string]any{
								{"id": 123456, "name": "Hi-Res Jazz"},
							},
						},
					},
					"qobuzissims": map[string]any{
						"data": map[string]any{
							"has_more": false,
							"items": []map[string]any{
								{
									"id":          "alb2",
									"title":       "Kind of Blue",
									"track_count": 5,
									"image":       map[string]string{"large": "https://img/2.jpg"},
									"artists":     []map[string]any{{"id": 2, "name": "Miles Davis", "roles": []string{"main-artist"}}},
									"genre":       map[string]any{"id": 80, "name": "Jazz"},
									"dates":       map[string]string{"original": "1959-08-17"},
									"audio_info":  map[string]any{"maximum_sampling_rate": 96, "maximum_bit_depth": 24},
									"rights":      map[string]any{"streamable": true},
								},
							},
						},
					},
				},
			})
		default:
			http.NotFound(w, r)
		}
	}))
	defer srv.Close()

	c := NewClient(Config{BaseURL: srv.URL, AppID: "app", AppSecret: "sec", AuthToken: "tok"})

	feed, err := c.Discover(context.Background(), []int64{80, 10, 80})
	if err != nil {
		t.Fatalf("Discover: %v", err)
	}
	if len(feed.Genres) != 2 {
		t.Fatalf("genres len = %d, want 2", len(feed.Genres))
	}
	if len(feed.Shelves) != 2 {
		t.Fatalf("shelves len = %d, want 2", len(feed.Shelves))
	}
	if feed.Shelves[0].Slug != "new-releases" || len(feed.Shelves[0].Items) != 1 {
		t.Fatalf("unexpected first shelf: %+v", feed.Shelves[0])
	}
	alb := feed.Shelves[0].Items[0]
	if alb.ID != "qobuz:alb1" || alb.Artist != "John Coltrane" || alb.Year != 1957 || alb.MaxBit != 24 || alb.MaxRate != 192000 {
		t.Fatalf("unexpected album conversion: %+v", alb)
	}

	// Second call with reordered genres should hit cache.
	if _, err := c.Discover(context.Background(), []int64{10, 80}); err != nil {
		t.Fatalf("Discover cached: %v", err)
	}
	if indexCalls.Load() != 1 || genreCalls.Load() != 1 {
		t.Fatalf("expected cached calls: index=%d genres=%d", indexCalls.Load(), genreCalls.Load())
	}
}

func TestDiscoverShelfAndSimilar(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/discover/newReleases":
			_ = json.NewEncoder(w).Encode(map[string]any{
				"has_more": true,
				"items": []map[string]any{
					{
						"id":         "nr1",
						"title":      "Fresh LP",
						"artists":    []map[string]any{{"id": 7, "name": "Artist A", "roles": []string{"main-artist"}}},
						"audio_info": map[string]any{"maximum_sampling_rate": 44.1, "maximum_bit_depth": 16},
						"rights":     map[string]any{"streamable": true},
					},
				},
			})
		case "/album/getFeatured":
			if got := r.URL.Query().Get("type"); got != "qobuzissims" {
				t.Errorf("featured type = %q, want qobuzissims", got)
			}
			_ = json.NewEncoder(w).Encode(map[string]any{
				"albums": map[string]any{
					"total": 40,
					"items": []map[string]any{
						{
							"id":                    "q1",
							"title":                 "Hi-Res Gem",
							"artist":                map[string]any{"id": 9, "name": "Artist B"},
							"maximum_bit_depth":     24,
							"maximum_sampling_rate": 96,
						},
					},
				},
			})
		case "/album/suggest":
			if got := r.URL.Query().Get("album_id"); got != "alb1" {
				t.Errorf("album_id = %q, want alb1", got)
			}
			_ = json.NewEncoder(w).Encode(map[string]any{
				"albums": map[string]any{
					"items": []map[string]any{
						{
							"id":      "sim1",
							"title":   "Similar LP",
							"artists": []map[string]any{{"id": 11, "name": "Similar Artist", "roles": []string{"main-artist"}}},
							"rights":  map[string]any{"streamable": true},
						},
					},
				},
			})
		case "/artist/getSimilarArtists":
			if got := r.URL.Query().Get("artist_id"); got != "42" {
				t.Errorf("artist_id = %q, want 42", got)
			}
			_ = json.NewEncoder(w).Encode(map[string]any{
				"artists": map[string]any{
					"items": []map[string]any{
						{
							"id":           99,
							"name":         "Related Artist",
							"albums_count": 12,
							"image":        map[string]string{"large": "https://img/art.jpg"},
						},
					},
				},
			})
		default:
			http.NotFound(w, r)
		}
	}))
	defer srv.Close()

	c := NewClient(Config{BaseURL: srv.URL, AppID: "app", AppSecret: "sec", AuthToken: "tok"})

	page, err := c.DiscoverShelf(context.Background(), "new-releases", nil, 24, 0)
	if err != nil || len(page.Items) != 1 || !page.HasMore {
		t.Fatalf("DiscoverShelf(new-releases) = %+v, err=%v", page, err)
	}

	feat, err := c.DiscoverShelf(context.Background(), "qobuzissims", nil, 24, 0)
	if err != nil || len(feat.Items) != 1 || feat.Total != 40 || !feat.HasMore {
		t.Fatalf("DiscoverShelf(qobuzissims) = %+v, err=%v", feat, err)
	}

	if _, err = c.DiscoverShelf(context.Background(), "nope", nil, 24, 0); !errors.Is(err, ErrNotFound) {
		t.Fatalf("unknown shelf err = %v, want ErrNotFound", err)
	}

	simAlb, err := c.SimilarAlbums(context.Background(), "qobuz:alb1", 10)
	if err != nil || len(simAlb) != 1 || simAlb[0].ID != "qobuz:sim1" {
		t.Fatalf("SimilarAlbums = %+v, err=%v", simAlb, err)
	}

	simArt, err := c.SimilarArtists(context.Background(), 42, 10)
	if err != nil || len(simArt) != 1 || simArt[0].Name != "Related Artist" || simArt[0].ArtworkURL != "https://img/art.jpg" {
		t.Fatalf("SimilarArtists = %+v, err=%v", simArt, err)
	}
}
