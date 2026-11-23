package api

import (
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"tacet/internal/library"
	"tacet/internal/queue"
)

func TestDesktopCORS(t *testing.T) {
	cat, err := library.NewCatalogWithDB(":memory:")
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = cat.Close() }()
	mgr := queue.NewManager(4320, "", nil)
	defer mgr.Close()
	srv := NewServer(cat, nil, mgr)
	defer srv.Close()
	h := srv.Handler()
	request := func(method, path, origin string) *httptest.ResponseRecorder {
		req := httptest.NewRequest(method, path, strings.NewReader(`{"streamMode":"relay"}`))
		req.Header.Set("Origin", origin)
		switch method {
		case http.MethodPatch:
			req.Header.Set("Content-Type", "application/json")
		case http.MethodOptions:
			req.Header.Set("Access-Control-Request-Method", "GET")
			req.Header.Set("Access-Control-Request-Headers", "range")
		}
		rec := httptest.NewRecorder()
		h.ServeHTTP(rec, req)
		return rec
	}

	for _, origin := range []string{"tauri://localhost", "http://tauri.localhost"} {
		t.Run(origin, func(t *testing.T) {
			for _, tc := range []struct {
				method, path string
				status       int
			}{
				{http.MethodGet, "/api/v1/info", http.StatusOK},
				{http.MethodPatch, "/api/v1/settings", http.StatusOK},
				{http.MethodOptions, "/media/local/test", http.StatusNoContent},
			} {
				rec := request(tc.method, tc.path, origin)
				if rec.Code != tc.status || rec.Header().Get("Access-Control-Allow-Origin") != origin || !strings.Contains(rec.Header().Get("Vary"), "Origin") {
					t.Errorf("%s %s: status %d, headers %v", tc.method, tc.path, rec.Code, rec.Header())
				}
			}
		})
	}
	for _, origin := range []string{"https://example.org", "null", "http://localhost:5173", "http://tauri.localhost:4320"} {
		rec := request(http.MethodPatch, "/api/v1/settings", origin)
		if rec.Code != http.StatusForbidden || rec.Header().Get("Access-Control-Allow-Origin") != "" {
			t.Errorf("unexpected access from %q: status %d, headers %v", origin, rec.Code, rec.Header())
		}
	}
	srv.SetCORSOrigin("http://localhost:5173")
	if rec := request(http.MethodPatch, "/api/v1/settings", "tauri://localhost"); rec.Code != http.StatusForbidden {
		t.Errorf("explicit origin should replace defaults, got %d", rec.Code)
	}
	if rec := request(http.MethodPatch, "/api/v1/settings", "http://localhost:5173"); rec.Code != http.StatusOK || rec.Header().Get("Access-Control-Allow-Origin") != "http://localhost:5173" {
		t.Errorf("configured client: status %d, headers %v", rec.Code, rec.Header())
	}
}
