package api

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"testing"

	"tacet/internal/library"
	"tacet/internal/qobuz"
	"tacet/internal/queue"
)

func TestDiscoverAPIHandlers(t *testing.T) {
	qSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/genre/list":
			_ = json.NewEncoder(w).Encode(map[string]any{
				"genres": map[string]any{
					"items": []map[string]any{{"id": 80, "name": "Jazz"}},
				},
			})
		case "/discover/index":
			_ = json.NewEncoder(w).Encode(map[string]any{
				"containers": map[string]any{
					"new_releases": map[string]any{
						"data": map[string]any{
							"has_more": true,
							"items": []map[string]any{
								{
									"id":      "d1",
									"title":   "Discovered LP",
									"artists": []map[string]any{{"id": 1, "name": "Jazz Trio", "roles": []string{"main-artist"}}},
									"rights":  map[string]any{"streamable": true},
								},
							},
						},
					},
				},
			})
		case "/discover/newReleases":
			_ = json.NewEncoder(w).Encode(map[string]any{
				"has_more": false,
				"items": []map[string]any{
					{
						"id":      "d1",
						"title":   "Discovered LP",
						"artists": []map[string]any{{"id": 1, "name": "Jazz Trio", "roles": []string{"main-artist"}}},
						"rights":  map[string]any{"streamable": true},
					},
				},
			})
		case "/album/suggest":
			_ = json.NewEncoder(w).Encode(map[string]any{
				"albums": map[string]any{
					"items": []map[string]any{
						{
							"id":      "s1",
							"title":   "Similar LP",
							"artists": []map[string]any{{"id": 2, "name": "Other Trio", "roles": []string{"main-artist"}}},
							"rights":  map[string]any{"streamable": true},
						},
					},
				},
			})
		case "/artist/getSimilarArtists":
			_ = json.NewEncoder(w).Encode(map[string]any{
				"artists": map[string]any{
					"items": []map[string]any{
						{"id": 77, "name": "Bill Evans", "albums_count": 50},
					},
				},
			})
		default:
			http.NotFound(w, r)
		}
	}))
	defer qSrv.Close()

	cat := library.NewCatalog("")
	qc := qobuz.NewClient(qobuz.Config{BaseURL: qSrv.URL, AppID: "app", AppSecret: "sec", AuthToken: "tok"})
	mgr := queue.NewManager(0, "", qc)
	srv := NewServer(cat, qc, mgr)
	h := srv.Handler()

	// 1. GET /api/v1/discover
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/api/v1/discover", nil))
	if rec.Code != http.StatusOK {
		t.Fatalf("GET /api/v1/discover = %d: %s", rec.Code, rec.Body.String())
	}
	var feed struct {
		Connected bool                  `json:"connected"`
		Genres    []qobuz.DiscoverGenre `json:"genres"`
		Shelves   []qobuz.DiscoverShelf `json:"shelves"`
	}
	if err := json.Unmarshal(rec.Body.Bytes(), &feed); err != nil {
		t.Fatalf("decode discover: %v", err)
	}
	if !feed.Connected || len(feed.Genres) != 1 || len(feed.Shelves) != 1 || feed.Shelves[0].Slug != "new-releases" {
		t.Fatalf("unexpected discover feed: %+v", feed)
	}

	// 2. GET /api/v1/discover/shelves/new-releases
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/api/v1/discover/shelves/new-releases?limit=12&offset=0", nil))
	if rec.Code != http.StatusOK {
		t.Fatalf("GET /api/v1/discover/shelves/new-releases = %d: %s", rec.Code, rec.Body.String())
	}

	// 3. Unknown shelf returns 404.
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/api/v1/discover/shelves/unknown", nil))
	if rec.Code != http.StatusNotFound {
		t.Fatalf("unknown shelf status = %d, want 404", rec.Code)
	}

	// 4. GET /api/v1/albums/qobuz:d1/similar
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/api/v1/albums/qobuz:d1/similar", nil))
	if rec.Code != http.StatusOK {
		t.Fatalf("GET /api/v1/albums/qobuz:d1/similar = %d: %s", rec.Code, rec.Body.String())
	}
	var simAlb struct {
		Items []library.Album `json:"items"`
	}
	if err := json.Unmarshal(rec.Body.Bytes(), &simAlb); err != nil || len(simAlb.Items) != 1 || simAlb.Items[0].ID != "qobuz:s1" {
		t.Fatalf("unexpected similar albums: %+v, err=%v", simAlb, err)
	}

	// 5. GET /api/v1/artists/1/similar
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/api/v1/artists/1/similar", nil))
	if rec.Code != http.StatusOK {
		t.Fatalf("GET /api/v1/artists/1/similar = %d: %s", rec.Code, rec.Body.String())
	}
	var simArt struct {
		Items []qobuz.Artist `json:"items"`
	}
	if err := json.Unmarshal(rec.Body.Bytes(), &simArt); err != nil || len(simArt.Items) != 1 || simArt.Items[0].Name != "Bill Evans" {
		t.Fatalf("unexpected similar artists: %+v, err=%v", simArt, err)
	}

	// 6. Invalid query / path params return 400.
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/api/v1/discover/shelves/new-releases?limit=-1", nil))
	if rec.Code != http.StatusBadRequest {
		t.Fatalf("invalid limit status = %d, want 400", rec.Code)
	}
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/api/v1/artists/notanumber/similar", nil))
	if rec.Code != http.StatusBadRequest {
		t.Fatalf("invalid artist id status = %d, want 400", rec.Code)
	}

	// 7. Disconnected Qobuz degrades gracefully on /discover, /albums/{id}/similar, and /artists/{id}/similar.
	qc.Logout()
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/api/v1/discover", nil))
	if rec.Code != http.StatusOK {
		t.Fatalf("disconnected /discover = %d, want 200", rec.Code)
	}
	if err := json.Unmarshal(rec.Body.Bytes(), &feed); err != nil || feed.Connected || len(feed.Shelves) != 0 {
		t.Fatalf("unexpected disconnected feed: %+v", feed)
	}
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/api/v1/albums/qobuz:d1/similar", nil))
	if rec.Code != http.StatusOK {
		t.Fatalf("disconnected similar albums = %d, want 200", rec.Code)
	}
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/api/v1/artists/1/similar", nil))
	if rec.Code != http.StatusOK {
		t.Fatalf("disconnected similar artists = %d, want 200", rec.Code)
	}
}
