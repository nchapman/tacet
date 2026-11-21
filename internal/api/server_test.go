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

func doReq(t *testing.T, method, url, body string) *http.Response {
	t.Helper()
	var rdr *strings.Reader
	if body != "" {
		rdr = strings.NewReader(body)
	} else {
		rdr = strings.NewReader("")
	}
	req, err := http.NewRequest(method, url, rdr)
	if err != nil {
		t.Fatalf("new request %s %s: %v", method, url, err)
	}
	if body != "" {
		req.Header.Set("Content-Type", "application/json")
	}
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatalf("%s %s failed: %v", method, url, err)
	}
	return resp
}

func assertErrorCode(t *testing.T, resp *http.Response, wantStatus int, wantCode string) {
	t.Helper()
	defer resp.Body.Close()
	if resp.StatusCode != wantStatus {
		t.Fatalf("expected HTTP %d, got %d", wantStatus, resp.StatusCode)
	}
	var env ErrorEnvelope
	if err := json.NewDecoder(resp.Body).Decode(&env); err != nil {
		t.Fatalf("expected structured JSON error envelope, decode failed: %v", err)
	}
	if env.Error.Code != wantCode {
		t.Fatalf("expected error code %q, got %+v", wantCode, env.Error)
	}
}

func TestAPIv1_SystemSettingsZonesQueueAndErrors(t *testing.T) {
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
	tracks := cat.Tracks()

	ctrl := &stubController{
		info: upnp.RendererInfo{
			ID:              "uuid:wiim-ultra",
			Name:            "WiiM Ultra",
			HasVolume:       true,
			SupportsSetNext: true,
			SupportsSeek:    true,
		},
		state: "STOPPED",
	}
	qb := qobuz.NewClient(qobuz.Config{})
	mgr := queue.NewManager(8080, "", qb)
	mgr.RegisterRenderer(ctrl)

	srv := NewServer(cat, qb, mgr)
	srv.SetCORSOrigin("http://localhost:5173")
	ts := httptest.NewServer(srv.Handler())
	defer ts.Close()

	// Health, info, openapi.yaml, CORS preflight.
	resp := doReq(t, http.MethodGet, ts.URL+"/api/health", "")
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("GET /api/health status=%d", resp.StatusCode)
	}
	resp.Body.Close()

	resp = doReq(t, http.MethodGet, ts.URL+"/api/v1/info", "")
	if resp.StatusCode != http.StatusOK || resp.Header.Get("Access-Control-Allow-Origin") != "http://localhost:5173" {
		t.Fatalf("GET /api/v1/info unexpected status=%d cors=%q", resp.StatusCode, resp.Header.Get("Access-Control-Allow-Origin"))
	}
	resp.Body.Close()

	resp = doReq(t, http.MethodGet, ts.URL+"/api/v1/openapi.yaml", "")
	if resp.StatusCode != http.StatusOK || !strings.Contains(resp.Header.Get("Content-Type"), "yaml") {
		t.Fatalf("GET /api/v1/openapi.yaml status=%d ct=%q", resp.StatusCode, resp.Header.Get("Content-Type"))
	}
	resp.Body.Close()

	// JSON 404 on /api and /api/unknown; JSON 405 on wrong method.
	assertErrorCode(t, doReq(t, http.MethodGet, ts.URL+"/api", ""), http.StatusNotFound, "NOT_FOUND")
	assertErrorCode(t, doReq(t, http.MethodGet, ts.URL+"/api/v1/nope", ""), http.StatusNotFound, "NOT_FOUND")
	assertErrorCode(t, doReq(t, http.MethodDelete, ts.URL+"/api/v1/info", ""), http.StatusMethodNotAllowed, "METHOD_NOT_ALLOWED")

	// Settings PATCH + strict unknown-field rejection.
	assertErrorCode(t, doReq(t, http.MethodPatch, ts.URL+"/api/v1/settings", `{"unknownField":true}`), http.StatusBadRequest, "INVALID_REQUEST")
	resp = doReq(t, http.MethodPatch, ts.URL+"/api/v1/settings", `{"streamMode":"relay","qobuzMaxQuality":"flac_24_96"}`)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("PATCH /api/v1/settings status=%d", resp.StatusCode)
	}
	resp.Body.Close()
	if mgr.StreamMode() != queue.StreamModeRelay || qb.MaxQuality() != "flac_24_96" {
		t.Fatalf("settings did not apply: mode=%q maxQ=%q", mgr.StreamMode(), qb.MaxQuality())
	}

	// Unknown zone returns 404 ZONE_NOT_FOUND.
	assertErrorCode(t, doReq(t, http.MethodGet, ts.URL+"/api/v1/zones/uuid:missing", ""), http.StatusNotFound, "ZONE_NOT_FOUND")

	// Library pagination, ETag 304, non-null empty search array, and sync/async rescan.
	resp = doReq(t, http.MethodGet, ts.URL+"/api/v1/library/albums?limit=10&offset=0", "")
	etag := resp.Header.Get("ETag")
	if resp.StatusCode != http.StatusOK || etag == "" {
		t.Fatalf("GET albums status=%d etag=%q", resp.StatusCode, etag)
	}
	resp.Body.Close()

	req304, _ := http.NewRequest(http.MethodGet, ts.URL+"/api/v1/library/albums?limit=10&offset=0", nil)
	req304.Header.Set("If-None-Match", etag)
	resp304, err := http.DefaultClient.Do(req304)
	if err != nil {
		t.Fatal(err)
	}
	if resp304.StatusCode != http.StatusNotModified {
		t.Fatalf("expected 304 Not Modified for matching ETag + params, got %d", resp304.StatusCode)
	}
	resp304.Body.Close()

	// Different limit with old ETag must NOT return 304.
	reqDiffLimit, _ := http.NewRequest(http.MethodGet, ts.URL+"/api/v1/library/albums?limit=200&offset=0", nil)
	reqDiffLimit.Header.Set("If-None-Match", etag)
	respDiff, err := http.DefaultClient.Do(reqDiffLimit)
	if err != nil {
		t.Fatal(err)
	}
	if respDiff.StatusCode != http.StatusOK {
		t.Fatalf("expected 200 OK for different limit with stale ETag, got %d", respDiff.StatusCode)
	}
	respDiff.Body.Close()

	resp = doReq(t, http.MethodGet, ts.URL+"/api/v1/library/tracks?q=zzzznotfound", "")
	var rawSearch bytes.Buffer
	_, _ = rawSearch.ReadFrom(resp.Body)
	resp.Body.Close()
	if !strings.Contains(rawSearch.String(), `"items":[]`) {
		t.Fatalf("expected non-null empty array in tracks search, got %s", rawSearch.String())
	}

	resp = doReq(t, http.MethodPost, ts.URL+"/api/v1/library/rescan?wait=true", "")
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("sync rescan status=%d", resp.StatusCode)
	}
	resp.Body.Close()

	// Replace queue with local album and verify SetURI + QueueItem IDs.
	zoneBase := ts.URL + "/api/v1/zones/uuid:wiim-ultra"
	body, _ := json.Marshal(map[string]any{
		"source":     map[string]any{"type": "album", "albumId": alb.ID},
		"startIndex": 0,
	})
	resp = doReq(t, http.MethodPut, zoneBase+"/queue", string(body))
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("PUT queue failed: %d", resp.StatusCode)
	}
	var snap queue.ZoneSnapshot
	_ = json.NewDecoder(resp.Body).Decode(&snap)
	resp.Body.Close()

	wantURI := "http://192.168.1.9:8080/media/local/" + tracks[0].ID + ".flac"
	if ctrl.lastURI != wantURI || len(snap.Queue) != 2 || snap.State != queue.StateBuffering {
		t.Fatalf("unexpected play result: uri=%q len=%d state=%q", ctrl.lastURI, len(snap.Queue), snap.State)
	}

	// Jump by itemId, add Play Next, move, idempotent delete (200 then 404), transport, volume, seek, clear.
	resp = doReq(t, http.MethodPost, zoneBase+"/queue/jump", `{"itemId":"`+snap.Queue[1].ItemID+`"}`)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("jump failed: %d", resp.StatusCode)
	}
	resp.Body.Close()

	// Next at end of queue returns 409 END_OF_QUEUE.
	assertErrorCode(t, doReq(t, http.MethodPost, zoneBase+"/transport/next", ""), http.StatusConflict, "END_OF_QUEUE")

	// Volume out of range returns 400 INVALID_REQUEST; valid volume succeeds.
	assertErrorCode(t, doReq(t, http.MethodPut, zoneBase+"/volume", `{"volume":150}`), http.StatusBadRequest, "INVALID_REQUEST")
	resp = doReq(t, http.MethodPut, zoneBase+"/volume", `{"volume":65}`)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("PUT volume failed: %d", resp.StatusCode)
	}
	resp.Body.Close()

	resp = doReq(t, http.MethodPut, zoneBase+"/mute", `{"muted":true}`)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("PUT mute failed: %d", resp.StatusCode)
	}
	resp.Body.Close()

	// Idempotent queue item delete.
	delURL := zoneBase + "/queue/items/" + snap.Queue[0].ItemID
	resp = doReq(t, http.MethodDelete, delURL, "")
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("first DELETE queue item failed: %d", resp.StatusCode)
	}
	resp.Body.Close()
	assertErrorCode(t, doReq(t, http.MethodDelete, delURL, ""), http.StatusNotFound, "QUEUE_ITEM_NOT_FOUND")

	// Clear queue.
	resp = doReq(t, http.MethodDelete, zoneBase+"/queue", "")
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("DELETE queue failed: %d", resp.StatusCode)
	}
	resp.Body.Close()

	// Embedded UI serves index.html at /.
	uiResp := doReq(t, http.MethodGet, ts.URL+"/", "")
	if uiResp.StatusCode != http.StatusOK {
		t.Fatalf("GET / status=%d", uiResp.StatusCode)
	}
	uiResp.Body.Close()
}

func TestAPIv1_SSEMultiZoneFilterAndQobuzLifecycle(t *testing.T) {
	qbSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/user/login":
			_ = json.NewEncoder(w).Encode(map[string]any{
				"user":            map[string]any{"display_name": "Tester", "subscription": map[string]any{"offer": "studio"}},
				"user_auth_token": "valid-tok",
			})
		case "/catalog/search":
			_ = json.NewEncoder(w).Encode(map[string]any{
				"tracks": map[string]any{"items": []any{}},
				"albums": map[string]any{"items": []any{}},
			})
		default:
			http.NotFound(w, r)
		}
	}))
	defer qbSrv.Close()

	qb := qobuz.NewClient(qobuz.Config{BaseURL: qbSrv.URL, AppID: "app1", AppSecret: "sec1"})
	mgr := queue.NewManager(8080, "", qb)
	mgr.RegisterRenderer(&stubController{info: upnp.RendererInfo{ID: "uuid:r1", Name: "R1", HasVolume: true}, state: "STOPPED"})
	mgr.RegisterRenderer(&stubController{info: upnp.RendererInfo{ID: "uuid:r2", Name: "R2", HasVolume: true}, state: "STOPPED"})

	srv := NewServer(library.NewCatalog(), qb, mgr)
	ts := httptest.NewServer(srv.Handler())
	defer ts.Close()

	// Unauthenticated Qobuz search returns 401 QOBUZ_UNAUTHENTICATED; empty query returns 400.
	assertErrorCode(t, doReq(t, http.MethodGet, ts.URL+"/api/v1/qobuz/search", ""), http.StatusBadRequest, "INVALID_REQUEST")
	assertErrorCode(t, doReq(t, http.MethodGet, ts.URL+"/api/v1/qobuz/search?q=miles", ""), http.StatusUnauthorized, "QOBUZ_UNAUTHENTICATED")

	// Token login -> search -> logout.
	resp := doReq(t, http.MethodPost, ts.URL+"/api/v1/qobuz/login", `{"authToken":"valid-tok"}`)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("qobuz login status=%d", resp.StatusCode)
	}
	resp.Body.Close()

	resp = doReq(t, http.MethodGet, ts.URL+"/api/v1/qobuz/search?q=miles", "")
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("qobuz search status=%d", resp.StatusCode)
	}
	resp.Body.Close()

	resp = doReq(t, http.MethodDelete, ts.URL+"/api/v1/qobuz/session", "")
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("qobuz logout status=%d", resp.StatusCode)
	}
	resp.Body.Close()
	if qb.Status().Authenticated {
		t.Fatal("expected qobuz to be logged out")
	}

	// OAuth start rejects untrusted Host header and sets HttpOnly state cookie on loopback.
	noRedirectClient := &http.Client{
		CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse },
	}
	evilReq, _ := http.NewRequest(http.MethodGet, ts.URL+"/api/v1/qobuz/oauth/start", nil)
	evilReq.Host = "evil.example:8080"
	evilResp, err := noRedirectClient.Do(evilReq)
	if err != nil {
		t.Fatal(err)
	}
	assertErrorCode(t, evilResp, http.StatusBadRequest, "INVALID_REQUEST")

	okStartResp, err := noRedirectClient.Get(ts.URL + "/api/v1/qobuz/oauth/start")
	if err != nil {
		t.Fatal(err)
	}
	if okStartResp.StatusCode != http.StatusFound {
		t.Fatalf("expected 302 from oauth/start, got %d", okStartResp.StatusCode)
	}
	var stateCookie *http.Cookie
	for _, c := range okStartResp.Cookies() {
		if c.Name == oauthStateCookieName {
			stateCookie = c
		}
	}
	okStartResp.Body.Close()
	if stateCookie == nil || !stateCookie.HttpOnly {
		t.Fatalf("expected HttpOnly %s cookie", oauthStateCookieName)
	}

	// OAuth callback without cookie -> 403; with cookie & missing code -> 400; replay -> 403.
	assertErrorCode(t, doReq(t, http.MethodGet, ts.URL+"/api/v1/qobuz/oauth/callback?code_autorisation=abc", ""), http.StatusForbidden, "OAUTH_STATE_INVALID")

	cbReq, _ := http.NewRequest(http.MethodGet, ts.URL+"/api/v1/qobuz/oauth/callback", nil)
	cbReq.AddCookie(stateCookie)
	cbResp, err := noRedirectClient.Do(cbReq)
	if err != nil {
		t.Fatal(err)
	}
	assertErrorCode(t, cbResp, http.StatusBadRequest, "INVALID_REQUEST")

	cbReplay, _ := http.NewRequest(http.MethodGet, ts.URL+"/api/v1/qobuz/oauth/callback?code_autorisation=abc", nil)
	cbReplay.AddCookie(stateCookie)
	replayResp, err := noRedirectClient.Do(cbReplay)
	if err != nil {
		t.Fatal(err)
	}
	assertErrorCode(t, replayResp, http.StatusForbidden, "OAUTH_STATE_INVALID")

	// Multi-zone SSE with ?zone=uuid:r1 emits X-Accel-Buffering: no, retry: 3000, snapshot, and filtered zone.volume.
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	sseReq, _ := http.NewRequestWithContext(ctx, http.MethodGet, ts.URL+"/api/v1/events?zone=uuid:r1", nil)
	sseResp, err := http.DefaultClient.Do(sseReq)
	if err != nil {
		t.Fatal(err)
	}
	defer sseResp.Body.Close()

	if sseResp.Header.Get("X-Accel-Buffering") != "no" {
		t.Fatalf("expected X-Accel-Buffering: no, got %q", sseResp.Header.Get("X-Accel-Buffering"))
	}

	go func() {
		time.Sleep(50 * time.Millisecond)
		_, _ = mgr.SetVolume(context.Background(), "uuid:r2", 80) // filtered out
		_, _ = mgr.SetVolume(context.Background(), "uuid:r1", 42) // delivered
	}()

	scanner := bufio.NewScanner(sseResp.Body)
	var seenRetry, seenSnapshot, seenVolumeID, sawR2 bool
	for scanner.Scan() {
		line := scanner.Text()
		if line == "retry: 3000" {
			seenRetry = true
		}
		if line == "event: snapshot" {
			seenSnapshot = true
		}
		if strings.Contains(line, `"uuid:r2"`) {
			sawR2 = true
		}
		if line == "event: zone.volume" {
			seenVolumeID = true
			break
		}
	}
	if !seenRetry || !seenSnapshot || !seenVolumeID || sawR2 {
		t.Fatalf("SSE check failed: retry=%v snapshot=%v volume=%v leakedR2=%v", seenRetry, seenSnapshot, seenVolumeID, sawR2)
	}
}
