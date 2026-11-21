package api

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"tacet/internal/library"
	"tacet/internal/qobuz"
	"tacet/internal/queue"
	"tacet/internal/upnp"
)

type stubController struct {
	info    upnp.RendererInfo
	lastURI string
	state   string
	vol     int
}

func (s *stubController) Info() upnp.RendererInfo { return s.info }
func (s *stubController) LocalIP() string         { return "192.168.1.9" }
func (s *stubController) SetURI(_ context.Context, m upnp.MediaItem) error {
	s.lastURI = m.URL
	return nil
}
func (s *stubController) SetNextURI(_ context.Context, _ upnp.MediaItem) error { return nil }
func (s *stubController) Play(_ context.Context) error                         { s.state = "PLAYING"; return nil }
func (s *stubController) Pause(_ context.Context) error                        { s.state = "PAUSED_PLAYBACK"; return nil }
func (s *stubController) Stop(_ context.Context) error                         { s.state = "STOPPED"; return nil }
func (s *stubController) Seek(_ context.Context, _ time.Duration) error        { return nil }
func (s *stubController) SetVolume(_ context.Context, v int) error             { s.vol = v; return nil }
func (s *stubController) SetMute(_ context.Context, _ bool) error              { return nil }
func (s *stubController) Status(_ context.Context) (upnp.TransportStatus, error) {
	return upnp.TransportStatus{State: s.state, Volume: s.vol, HasVolume: true}, nil
}

func TestAPI_PlayLocalTrackAndSwitchStreamMode(t *testing.T) {
	root := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "track.flac"), []byte("fLaC1234"), 0o600); err != nil {
		t.Fatal(err)
	}
	cat := library.NewCatalog(root)
	if _, err := cat.Scan(); err != nil {
		t.Fatal(err)
	}
	track := cat.Tracks()[0]

	ctrl := &stubController{
		info:  upnp.RendererInfo{ID: "uuid:wiim-ultra", Name: "WiiM Ultra", HasVolume: true},
		state: "STOPPED",
	}
	mgr := queue.NewManager(8080, "", qobuz.NewClient(qobuz.Config{}))
	mgr.RegisterRenderer(ctrl)

	srv := NewServer(cat, qobuz.NewClient(qobuz.Config{}), mgr)
	ts := httptest.NewServer(srv.Handler())
	defer ts.Close()

	// Switch to Quiet Relay mode.
	resp, err := http.Post(ts.URL+"/api/settings/stream-mode", "application/json", bytes.NewBufferString(`{"mode":"relay"}`))
	if err != nil || resp.StatusCode != http.StatusOK {
		t.Fatalf("stream-mode failed: err=%v status=%v", err, resp)
	}
	resp.Body.Close()
	if mgr.StreamMode() != queue.StreamModeRelay {
		t.Fatalf("expected relay mode, got %q", mgr.StreamMode())
	}

	// Play the local track.
	body, _ := json.Marshal(map[string]any{"localTrackId": track.ID})
	resp, err = http.Post(ts.URL+"/api/queue/play", "application/json", bytes.NewReader(body))
	if err != nil || resp.StatusCode != http.StatusOK {
		t.Fatalf("queue/play failed: err=%v status=%v", err, resp)
	}
	resp.Body.Close()

	wantURI := "http://192.168.1.9:8080/media/local/" + track.ID + ".flac"
	if ctrl.lastURI != wantURI {
		t.Fatalf("expected SetURI %q, got %q", wantURI, ctrl.lastURI)
	}
	if ctrl.state != "PLAYING" {
		t.Fatalf("expected PLAYING, got %q", ctrl.state)
	}

	// Embedded UI serves index.html.
	uiResp, err := http.Get(ts.URL + "/")
	if err != nil || uiResp.StatusCode != http.StatusOK {
		t.Fatalf("GET / failed: err=%v status=%v", err, uiResp)
	}
	uiResp.Body.Close()
}

func TestAPI_QueueTransportLibraryAndValidation(t *testing.T) {
	root := t.TempDir()
	albDir := filepath.Join(root, "Album One")
	if err := os.MkdirAll(albDir, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(albDir, "01 Alpha.flac"), []byte("fLaC1111"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(albDir, "02 Beta.flac"), []byte("fLaC2222"), 0o600); err != nil {
		t.Fatal(err)
	}

	cat := library.NewCatalog(root)
	if _, err := cat.Scan(); err != nil {
		t.Fatal(err)
	}
	alb := cat.Albums()[0]

	ctrl := &stubController{
		info:  upnp.RendererInfo{ID: "uuid:wiim", Name: "WiiM Ultra", HasVolume: true, SupportsSetNext: true},
		state: "STOPPED",
	}
	mgr := queue.NewManager(8080, "", nil)
	mgr.RegisterRenderer(ctrl)

	srv := NewServer(cat, nil, mgr)
	ts := httptest.NewServer(srv.Handler())
	defer ts.Close()

	// List renderers & select unknown returns 404.
	resp, err := http.Get(ts.URL + "/api/renderers")
	if err != nil || resp.StatusCode != http.StatusOK {
		t.Fatalf("GET /api/renderers: err=%v status=%v", err, resp)
	}
	resp.Body.Close()

	resp, _ = http.Post(ts.URL+"/api/renderers/select", "application/json", bytes.NewBufferString(`{"id":"missing"}`))
	if resp.StatusCode != http.StatusNotFound {
		t.Fatalf("expected 404 selecting unknown renderer, got %d", resp.StatusCode)
	}
	resp.Body.Close()

	// Local library endpoints: albums, album by ID, 404 album, search, rescan.
	resp, _ = http.Get(ts.URL + "/api/library/albums/" + alb.ID)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("GET album failed: %d", resp.StatusCode)
	}
	resp.Body.Close()

	resp, _ = http.Get(ts.URL + "/api/library/albums/does-not-exist")
	if resp.StatusCode != http.StatusNotFound {
		t.Fatalf("expected 404 for unknown album, got %d", resp.StatusCode)
	}
	resp.Body.Close()

	resp, _ = http.Get(ts.URL + "/api/library/tracks?q=Alpha")
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("search tracks failed: %d", resp.StatusCode)
	}
	var searchRes struct {
		Tracks []library.Track `json:"tracks"`
	}
	_ = json.NewDecoder(resp.Body).Decode(&searchRes)
	resp.Body.Close()
	if len(searchRes.Tracks) != 1 {
		t.Fatalf("expected 1 search hit for Alpha, got %d", len(searchRes.Tracks))
	}

	resp, _ = http.Post(ts.URL+"/api/library/rescan", "application/json", nil)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("rescan failed: %d", resp.StatusCode)
	}
	resp.Body.Close()

	// Play album, jump, enqueue, remove, pause, play, seek, volume, mute, stop, clear.
	body, _ := json.Marshal(map[string]any{"localAlbumId": alb.ID})
	resp, _ = http.Post(ts.URL+"/api/queue/play", "application/json", bytes.NewReader(body))
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("queue/play album failed: %d", resp.StatusCode)
	}
	resp.Body.Close()

	for _, tc := range []struct {
		path string
		body string
	}{
		{"/api/queue/jump", `{"index":1}`},
		{"/api/queue/enqueue", `{"localTrackId":"` + cat.Tracks()[0].ID + `"}`},
		{"/api/queue/remove", `{"index":2}`},
		{"/api/transport/pause", ``},
		{"/api/transport/play", ``},
		{"/api/transport/seek", `{"positionSec":30}`},
		{"/api/transport/volume", `{"volume":65}`},
		{"/api/transport/mute", `{"muted":true}`},
		{"/api/transport/prev", ``},
		{"/api/transport/stop", ``},
		{"/api/queue/clear", ``},
	} {
		r2, err2 := http.Post(ts.URL+tc.path, "application/json", strings.NewReader(tc.body))
		if err2 != nil || r2.StatusCode != http.StatusOK {
			t.Fatalf("POST %s failed: err=%v status=%v", tc.path, err2, r2)
		}
		r2.Body.Close()
	}

	// Oversized JSON body (>2 MiB) must fail cleanly with 400 (not panic).
	oversized := `{"id":"` + strings.Repeat("a", 2<<20+1024) + `"}`
	resp, err = http.Post(ts.URL+"/api/renderers/select", "application/json", strings.NewReader(oversized))
	if err != nil {
		t.Fatalf("oversized body request errored: %v", err)
	}
	if resp.StatusCode != http.StatusBadRequest {
		t.Fatalf("expected 400 on oversized JSON body, got %d", resp.StatusCode)
	}
	resp.Body.Close()

	// Invalid stream mode returns 400.
	resp, _ = http.Post(ts.URL+"/api/settings/stream-mode", "application/json", strings.NewReader(`{"mode":"bogus"}`))
	if resp.StatusCode != http.StatusBadRequest {
		t.Fatalf("expected 400 on invalid stream mode, got %d", resp.StatusCode)
	}
	resp.Body.Close()

	// GENA callback endpoint returns 503 when disabled.
	req, _ := http.NewRequest("NOTIFY", ts.URL+"/upnp/event/abc", nil)
	resp, err = http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	if resp.StatusCode != http.StatusServiceUnavailable {
		t.Fatalf("expected 503 when GENA disabled, got %d", resp.StatusCode)
	}
	resp.Body.Close()
}

func TestAPI_SSEInitialEventsAndQobuzEndpoints(t *testing.T) {
	qbSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/user/login":
			_ = json.NewEncoder(w).Encode(map[string]any{
				"user":            map[string]any{"display_name": "Tester", "subscription": map[string]any{"offer": "studio"}},
				"user_auth_token": "valid-tok",
			})
		case "/catalog/search":
			_ = json.NewEncoder(w).Encode(map[string]any{"tracks": map[string]any{"items": []any{}}, "albums": map[string]any{"items": []any{}}})
		default:
			http.NotFound(w, r)
		}
	}))
	defer qbSrv.Close()

	qb := qobuz.NewClient(qobuz.Config{BaseURL: qbSrv.URL, AppID: "app1", AppSecret: "sec1"})
	mgr := queue.NewManager(8080, "", qb)
	mgr.RegisterRenderer(&stubController{info: upnp.RendererInfo{ID: "uuid:r1", Name: "R1"}, state: "STOPPED"})

	srv := NewServer(library.NewCatalog(), qb, mgr)
	ts := httptest.NewServer(srv.Handler())
	defer ts.Close()

	// Qobuz token login + status + search.
	resp, err := http.Post(ts.URL+"/api/qobuz/login", "application/json", strings.NewReader(`{"authToken":"valid-tok"}`))
	if err != nil || resp.StatusCode != http.StatusOK {
		t.Fatalf("qobuz login failed: err=%v status=%v", err, resp)
	}
	resp.Body.Close()

	resp, _ = http.Get(ts.URL + "/api/qobuz/status")
	var st qobuz.Status
	_ = json.NewDecoder(resp.Body).Decode(&st)
	resp.Body.Close()
	if !st.Authenticated || st.UserName != "Tester" {
		t.Fatalf("unexpected qobuz status: %+v", st)
	}

	resp, _ = http.Get(ts.URL + "/api/qobuz/search?q=miles")
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("qobuz search failed: %d", resp.StatusCode)
	}
	resp.Body.Close()

	// OAuth start rejects untrusted Host headers (code theft prevention).
	noRedirectClient := &http.Client{
		CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse },
	}
	evilReq, _ := http.NewRequest(http.MethodGet, ts.URL+"/api/qobuz/oauth/start", nil)
	evilReq.Host = "evil.example:8080"
	resp, err = noRedirectClient.Do(evilReq)
	if err != nil {
		t.Fatal(err)
	}
	if resp.StatusCode != http.StatusBadRequest {
		t.Fatalf("expected 400 for untrusted OAuth Host header, got %d", resp.StatusCode)
	}
	resp.Body.Close()

	// OAuth start from loopback succeeds, sets HttpOnly state cookie, and redirects to www.qobuz.com.
	okStartResp, err := noRedirectClient.Get(ts.URL + "/api/qobuz/oauth/start")
	if err != nil {
		t.Fatal(err)
	}
	if okStartResp.StatusCode != http.StatusFound || !strings.HasPrefix(okStartResp.Header.Get("Location"), "https://www.qobuz.com/signin/oauth?") {
		t.Fatalf("expected 302 to Qobuz OAuth, got status=%d loc=%q", okStartResp.StatusCode, okStartResp.Header.Get("Location"))
	}
	var stateCookie *http.Cookie
	for _, c := range okStartResp.Cookies() {
		if c.Name == oauthStateCookieName {
			stateCookie = c
		}
	}
	okStartResp.Body.Close()
	if stateCookie == nil || !stateCookie.HttpOnly {
		t.Fatalf("expected HttpOnly %s cookie from oauth/start", oauthStateCookieName)
	}

	// OAuth callback without valid state cookie returns 403.
	resp, _ = http.Get(ts.URL + "/api/qobuz/oauth/callback?code_autorisation=abc")
	if resp.StatusCode != http.StatusForbidden {
		t.Fatalf("expected 403 for callback without state cookie, got %d", resp.StatusCode)
	}
	resp.Body.Close()

	// OAuth callback with valid state cookie but missing code returns 400 (and consumes state).
	cbReq, _ := http.NewRequest(http.MethodGet, ts.URL+"/api/qobuz/oauth/callback", nil)
	cbReq.AddCookie(stateCookie)
	resp, err = noRedirectClient.Do(cbReq)
	if err != nil {
		t.Fatal(err)
	}
	if resp.StatusCode != http.StatusBadRequest {
		t.Fatalf("expected 400 for missing oauth code with valid cookie, got %d", resp.StatusCode)
	}
	resp.Body.Close()

	// Replaying the same state cookie returns 403.
	cbReplay, _ := http.NewRequest(http.MethodGet, ts.URL+"/api/qobuz/oauth/callback?code_autorisation=abc", nil)
	cbReplay.AddCookie(stateCookie)
	resp, err = noRedirectClient.Do(cbReplay)
	if err != nil {
		t.Fatal(err)
	}
	if resp.StatusCode != http.StatusForbidden {
		t.Fatalf("expected 403 on replayed OAuth state cookie, got %d", resp.StatusCode)
	}
	resp.Body.Close()

	// SSE stream emits initial renderers + state events immediately.
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	sseReq, _ := http.NewRequestWithContext(ctx, http.MethodGet, ts.URL+"/api/events", nil)
	sseResp, err := http.DefaultClient.Do(sseReq)
	if err != nil {
		t.Fatal(err)
	}
	defer sseResp.Body.Close()

	scanner := bufio.NewScanner(sseResp.Body)
	var seenRenderers, seenState bool
	for scanner.Scan() {
		line := scanner.Text()
		if line == "event: renderers" {
			seenRenderers = true
		}
		if line == "event: state" {
			seenState = true
		}
		if seenRenderers && seenState {
			break
		}
	}
	if !seenRenderers || !seenState {
		t.Fatalf("expected initial SSE renderers and state events, got renderers=%v state=%v", seenRenderers, seenState)
	}
}
