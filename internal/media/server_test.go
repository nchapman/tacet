package media

import (
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"testing"

	"tacet/internal/library"
	"tacet/internal/qobuz"
)

func TestServeLocal_RangeAndDLNAHeaders(t *testing.T) {
	root := t.TempDir()
	payload := []byte("fLaC0123456789abcdefghijklmnopqrstuvwxyz")
	if err := os.WriteFile(filepath.Join(root, "song.flac"), payload, 0o600); err != nil {
		t.Fatal(err)
	}

	cat := library.NewCatalog(root)
	if _, err := cat.Scan(); err != nil {
		t.Fatal(err)
	}
	track := cat.Tracks()[0]

	mux := http.NewServeMux()
	NewHandler(cat, nil).Register(mux)
	srv := httptest.NewServer(mux)
	defer srv.Close()

	req, _ := http.NewRequest(http.MethodGet, srv.URL+"/media/local/"+track.ID+".flac", nil)
	req.Header.Set("Range", "bytes=4-13")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusPartialContent {
		t.Fatalf("expected 206 Partial Content, got %d", resp.StatusCode)
	}
	if got := resp.Header.Get("Content-Type"); got != "audio/flac" {
		t.Fatalf("expected audio/flac, got %q", got)
	}
	if got := resp.Header.Get("transferMode.dlna.org"); got != "Streaming" {
		t.Fatalf("missing transferMode.dlna.org header, got %q", got)
	}
	body, _ := io.ReadAll(resp.Body)
	if string(body) != "0123456789" {
		t.Fatalf("unexpected range body %q", string(body))
	}
}

func TestServeQobuzRelay_ForwardsRangeUntouched(t *testing.T) {
	cdnPayload := []byte("FLAC_RAW_MASTER_BYTES_24_192")
	cdnSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "audio/flac")
		w.Header().Set("Accept-Ranges", "bytes")
		if r.Header.Get("Range") == "bytes=0-3" {
			w.Header().Set("Content-Range", "bytes 0-3/28")
			w.WriteHeader(http.StatusPartialContent)
			_, _ = w.Write(cdnPayload[:4])
			return
		}
		_, _ = w.Write(cdnPayload)
	}))
	defer cdnSrv.Close()

	apiSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_ = json.NewEncoder(w).Encode(map[string]any{
			"url":           cdnSrv.URL + "/master.flac",
			"mime_type":     "audio/flac",
			"bit_depth":     24,
			"sampling_rate": 192.0,
			"duration":      200,
			"format_id":     27,
		})
	}))
	defer apiSrv.Close()

	qb := qobuz.NewClient(qobuz.Config{
		BaseURL:   apiSrv.URL,
		AppID:     "app",
		AppSecret: "sec",
		AuthToken: "tok",
	})

	mux := http.NewServeMux()
	NewHandler(library.NewCatalog(), qb).Register(mux)
	srv := httptest.NewServer(mux)
	defer srv.Close()

	req, _ := http.NewRequest(http.MethodGet, srv.URL+"/media/qobuz/42.flac", nil)
	req.Header.Set("Range", "bytes=0-3")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusPartialContent {
		t.Fatalf("expected 206, got %d", resp.StatusCode)
	}
	body, _ := io.ReadAll(resp.Body)
	if string(body) != "FLAC" {
		t.Fatalf("expected FLAC, got %q", string(body))
	}
}

func TestServeQobuzRelay_RejectsUpstreamErrors(t *testing.T) {
	cdnSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		http.Error(w, "expired", http.StatusForbidden)
	}))
	defer cdnSrv.Close()

	apiSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_ = json.NewEncoder(w).Encode(map[string]any{
			"url":       cdnSrv.URL + "/master.flac",
			"mime_type": "audio/flac",
		})
	}))
	defer apiSrv.Close()

	qb := qobuz.NewClient(qobuz.Config{BaseURL: apiSrv.URL, AppID: "app", AppSecret: "sec", AuthToken: "tok"})
	mux := http.NewServeMux()
	NewHandler(library.NewCatalog(), qb).Register(mux)
	srv := httptest.NewServer(mux)
	defer srv.Close()

	resp, err := http.Get(srv.URL + "/media/qobuz/42.flac")
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusBadGateway {
		t.Fatalf("expected 502 on upstream 403, got %d", resp.StatusCode)
	}
	if resp.Header.Get("transferMode.dlna.org") != "" {
		t.Fatal("expected no DLNA streaming header on upstream error")
	}
}

func TestServeCoverArt_HeadersAndErrors(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "song.flac"), []byte("fLaC1234"), 0o600); err != nil {
		t.Fatal(err)
	}
	pngBytes := []byte{0x89, 'P', 'N', 'G', '\r', '\n', 0x1a, '\n'}
	if err := os.WriteFile(filepath.Join(root, "cover.png"), pngBytes, 0o600); err != nil {
		t.Fatal(err)
	}
	cat := library.NewCatalog(root)
	if _, err := cat.Scan(); err != nil {
		t.Fatal(err)
	}
	tr := cat.Tracks()[0]

	mux := http.NewServeMux()
	NewHandler(cat, nil).Register(mux)
	srv := httptest.NewServer(mux)
	defer srv.Close()

	resp, err := http.Get(srv.URL + "/media/art/" + tr.ID)
	if err != nil {
		t.Fatal(err)
	}
	body, _ := io.ReadAll(resp.Body)
	resp.Body.Close()
	if resp.StatusCode != http.StatusOK || resp.Header.Get("Content-Type") != "image/png" || resp.Header.Get("X-Content-Type-Options") != "nosniff" {
		t.Fatalf("unexpected art response: status=%d ct=%q nosniff=%q", resp.StatusCode, resp.Header.Get("Content-Type"), resp.Header.Get("X-Content-Type-Options"))
	}
	if string(body) != string(pngBytes) {
		t.Fatalf("unexpected art body len=%d", len(body))
	}

	// HEAD returns headers with empty body.
	headResp, err := http.Head(srv.URL + "/media/art/" + tr.ID)
	if err != nil {
		t.Fatal(err)
	}
	headBody, _ := io.ReadAll(headResp.Body)
	headResp.Body.Close()
	if headResp.StatusCode != http.StatusOK || len(headBody) != 0 {
		t.Fatalf("expected 200 empty HEAD body, got status=%d len=%d", headResp.StatusCode, len(headBody))
	}

	// Unknown art / unknown local / unconfigured Qobuz / bad Qobuz ID.
	for _, tc := range []struct {
		path string
		want int
	}{
		{"/media/art/missing", http.StatusNotFound},
		{"/media/local/missing.flac", http.StatusNotFound},
		{"/media/qobuz/123.flac", http.StatusServiceUnavailable},
	} {
		r, err := http.Get(srv.URL + tc.path)
		if err != nil {
			t.Fatal(err)
		}
		r.Body.Close()
		if r.StatusCode != tc.want {
			t.Fatalf("GET %s: got %d, want %d", tc.path, r.StatusCode, tc.want)
		}
	}
}
