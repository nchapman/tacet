package qobuz

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"sync"
	"testing"
)

func TestRemoteSource_FavoritesSyncAndMutations(t *testing.T) {
	var mu sync.Mutex
	var createdAlbums, deletedAlbums, createdTracks, deletedTracks []string

	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/favorite/getUserFavorites":
			favType := r.URL.Query().Get("type")
			if favType == "albums" {
				_ = json.NewEncoder(w).Encode(map[string]any{
					"albums": map[string]any{
						"total": 1,
						"items": []map[string]any{
							{
								"id":                    "alb-fav-1",
								"title":                 "Kind of Blue",
								"tracks_count":          1,
								"released_at":           -327456000,
								"maximum_bit_depth":     24,
								"maximum_sampling_rate": 192.0,
								"artist":                map[string]any{"name": "Miles Davis"},
								"genre":                 map[string]any{"name": "Jazz"},
								"image":                 map[string]any{"large": "https://img.example/kob.jpg"},
							},
						},
					},
				})
				return
			}
			_ = json.NewEncoder(w).Encode(map[string]any{
				"tracks": map[string]any{
					"total": 1,
					"items": []map[string]any{
						{
							"id":                    7001,
							"title":                 "Nardis",
							"track_number":          1,
							"duration":              340,
							"maximum_bit_depth":     24,
							"maximum_sampling_rate": 96.0,
							"performer":             map[string]any{"name": "Bill Evans"},
							"album": map[string]any{
								"id":     "alb-loose-1",
								"title":  "Explorations",
								"artist": map[string]any{"name": "Bill Evans"},
								"genre":  map[string]any{"name": "Jazz"},
							},
						},
					},
				},
			})
		case "/album/get":
			albID := r.URL.Query().Get("album_id")
			_ = json.NewEncoder(w).Encode(map[string]any{
				"id":                    albID,
				"title":                 "Kind of Blue",
				"tracks_count":          1,
				"maximum_bit_depth":     24,
				"maximum_sampling_rate": 192.0,
				"artist":                map[string]any{"name": "Miles Davis"},
				"genre":                 map[string]any{"name": "Jazz"},
				"tracks": map[string]any{
					"items": []map[string]any{
						{
							"id":                    5001,
							"title":                 "So What",
							"track_number":          1,
							"duration":              562,
							"maximum_bit_depth":     24,
							"maximum_sampling_rate": 192.0,
							"performer":             map[string]any{"name": "Miles Davis"},
						},
					},
				},
			})
		case "/track/get":
			_ = json.NewEncoder(w).Encode(map[string]any{
				"id":                    8888,
				"title":                 "Flamenco Sketches",
				"track_number":          5,
				"duration":              566,
				"maximum_bit_depth":     24,
				"maximum_sampling_rate": 192.0,
				"performer":             map[string]any{"name": "Miles Davis"},
				"album": map[string]any{
					"id":     "alb-fav-1",
					"title":  "Kind of Blue",
					"artist": map[string]any{"name": "Miles Davis"},
					"genre":  map[string]any{"name": "Jazz"},
				},
			})
		case "/favorite/create":
			_ = r.ParseForm()
			mu.Lock()
			if v := r.FormValue("album_ids"); v != "" {
				createdAlbums = append(createdAlbums, v)
			}
			if v := r.FormValue("track_ids"); v != "" {
				createdTracks = append(createdTracks, v)
			}
			mu.Unlock()
			_ = json.NewEncoder(w).Encode(map[string]any{"status": "success"})
		case "/favorite/delete":
			_ = r.ParseForm()
			mu.Lock()
			if v := r.FormValue("album_ids"); v != "" {
				deletedAlbums = append(deletedAlbums, v)
			}
			if v := r.FormValue("track_ids"); v != "" {
				deletedTracks = append(deletedTracks, v)
			}
			mu.Unlock()
			_ = json.NewEncoder(w).Encode(map[string]any{"status": "success"})
		default:
			http.NotFound(w, r)
		}
	}))
	defer srv.Close()

	client := NewClient(Config{
		BaseURL:   srv.URL,
		AppID:     "app123",
		AppSecret: "sec456",
		AuthToken: "tok789",
	})
	src := client.AsRemoteSource()

	col, err := src.SyncCollection(context.Background())
	if err != nil {
		t.Fatalf("SyncCollection: %v", err)
	}
	if len(col.Albums) != 1 || len(col.Albums[0].Tracks) != 1 {
		t.Fatalf("col.Albums = %+v, want 1 album with 1 track", col.Albums)
	}
	if len(col.Tracks) != 1 || col.Tracks[0].Title != "Nardis" {
		t.Fatalf("col.Tracks = %+v, want 1 loose favorite track", col.Tracks)
	}

	alb, err := src.GetAlbum(context.Background(), "alb-fav-1")
	if err != nil || alb.ID != "qobuz:alb-fav-1" || len(alb.Tracks) != 1 {
		t.Fatalf("GetAlbum = (%+v, %v)", alb, err)
	}
	if favErr := src.AddFavorite(context.Background(), "album", "alb-fav-1"); favErr != nil {
		t.Fatalf("AddFavorite(album): %v", favErr)
	}
	if favErr := src.RemoveFavorite(context.Background(), "album", "alb-fav-1"); favErr != nil {
		t.Fatalf("RemoveFavorite(album): %v", favErr)
	}
	tr, err := src.GetTrack(context.Background(), "8888")
	if err != nil || tr.ID != "qobuz:8888" {
		t.Fatalf("GetTrack = (%+v, %v)", tr, err)
	}
	if err := src.AddFavorite(context.Background(), "track", "8888"); err != nil {
		t.Fatalf("AddFavorite(track): %v", err)
	}
	if err := src.RemoveFavorite(context.Background(), "track", "8888"); err != nil {
		t.Fatalf("RemoveFavorite(track): %v", err)
	}

	mu.Lock()
	defer mu.Unlock()
	if len(createdAlbums) != 1 || createdAlbums[0] != "alb-fav-1" {
		t.Fatalf("createdAlbums = %v, want [alb-fav-1]", createdAlbums)
	}
	if len(deletedAlbums) != 1 || deletedAlbums[0] != "alb-fav-1" {
		t.Fatalf("deletedAlbums = %v, want [alb-fav-1]", deletedAlbums)
	}
	if len(createdTracks) != 1 || createdTracks[0] != "8888" {
		t.Fatalf("createdTracks = %v, want [8888]", createdTracks)
	}
	if len(deletedTracks) != 1 || deletedTracks[0] != "8888" {
		t.Fatalf("deletedTracks = %v, want [8888]", deletedTracks)
	}
}

func TestSearchArtistsAndGetArtistByName(t *testing.T) {
	var artistSearchCalls int
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/catalog/search":
			_ = json.NewEncoder(w).Encode(map[string]any{
				"artists": map[string]any{
					"items": []map[string]any{
						{
							"id":           4201,
							"name":         "Al Green",
							"albums_count": 18,
							"image":        map[string]any{"large": "https://img/al-large.jpg"},
						},
					},
				},
				"albums": map[string]any{
					"items": []map[string]any{
						{
							"id":                    "q-ok",
							"title":                 "Call Me",
							"artist":                map[string]any{"id": 4201, "name": "Al Green"},
							"image":                 map[string]any{"large": "https://img/callme.jpg"},
							"tracks_count":          9,
							"release_date_original": "1973-04-29",
							"maximum_bit_depth":     24,
							"maximum_sampling_rate": 96.0,
							"streamable":            true,
						},
						{
							"id":         "q-unstreamable",
							"title":      "Blocked Album",
							"artist":     map[string]any{"id": 4201, "name": "Al Green"},
							"streamable": false,
						},
					},
				},
				"tracks": map[string]any{"items": []any{}},
			})
		case "/artist/search":
			artistSearchCalls++
			_ = json.NewEncoder(w).Encode(map[string]any{
				"artists": map[string]any{
					"items": []map[string]any{
						{"id": 9999, "name": "Al Green Tribute Band"},
						{"id": 4201, "name": "Al Green", "albums_count": 18},
					},
				},
			})
		case "/artist/get":
			if r.URL.Query().Get("artist_id") != "4201" || r.URL.Query().Get("extra") != "albums" {
				http.NotFound(w, r)
				return
			}
			_ = json.NewEncoder(w).Encode(map[string]any{
				"id":           4201,
				"name":         "Al Green",
				"albums_count": 18,
				"picture":      "https://img/al-pic.jpg",
				"albums": map[string]any{
					"items": []map[string]any{
						{
							"id":                    "q-call-me",
							"title":                 "Call Me",
							"artist":                map[string]any{"id": 4201, "name": "Al Green"},
							"image":                 map[string]any{"large": "https://img/callme.jpg"},
							"tracks_count":          9,
							"release_date_original": "1973-04-29",
							"maximum_bit_depth":     24,
							"maximum_sampling_rate": 96.0,
						},
					},
				},
			})
		default:
			http.NotFound(w, r)
		}
	}))
	defer srv.Close()

	client := NewClient(Config{BaseURL: srv.URL, AppID: "app", AppSecret: "sec", AuthToken: "tok"})

	// Resolve by name first -> hits /artist/search + /artist/get, skipping partial name match.
	det, err := client.GetArtistByName(context.Background(), "al green", 50)
	if err != nil {
		t.Fatalf("GetArtistByName: %v", err)
	}
	if det.Artist.ID != 4201 || det.Artist.ArtworkURL != "https://img/al-pic.jpg" || len(det.Albums) != 1 {
		t.Fatalf("detail = %+v", det)
	}
	// Second lookup should use cached artist ID without calling /artist/search again.
	if _, cachedErr := client.GetArtistByName(context.Background(), "Al Green", 50); cachedErr != nil {
		t.Fatalf("GetArtistByName cached: %v", cachedErr)
	}
	if artistSearchCalls != 1 {
		t.Fatalf("artistSearchCalls = %d, want 1 (cached)", artistSearchCalls)
	}

	// Catalog search decodes artists and filters streamable=false albums.
	res, err := client.Search(context.Background(), "Al Green", 10)
	if err != nil {
		t.Fatalf("Search: %v", err)
	}
	if len(res.Artists) != 1 || res.Artists[0].ID != 4201 || res.Artists[0].ArtworkURL != "https://img/al-large.jpg" {
		t.Fatalf("res.Artists = %+v", res.Artists)
	}
	if len(res.Albums) != 1 || res.Albums[0].ID != "qobuz:q-ok" {
		t.Fatalf("res.Albums = %+v, want only streamable q-ok", res.Albums)
	}
}
