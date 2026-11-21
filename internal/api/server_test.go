package api

import (
	"bytes"
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
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

func (s *stubController) Info() upnp.RendererInfo                    { return s.info }
func (s *stubController) LocalIP() string                            { return "192.168.1.9" }
func (s *stubController) SetURI(_ context.Context, m upnp.MediaItem) error { s.lastURI = m.URL; return nil }
func (s *stubController) SetNextURI(_ context.Context, _ upnp.MediaItem) error { return nil }
func (s *stubController) Play(_ context.Context) error               { s.state = "PLAYING"; return nil }
func (s *stubController) Pause(_ context.Context) error              { s.state = "PAUSED_PLAYBACK"; return nil }
func (s *stubController) Stop(_ context.Context) error               { s.state = "STOPPED"; return nil }
func (s *stubController) Seek(_ context.Context, _ time.Duration) error { return nil }
func (s *stubController) SetVolume(_ context.Context, v int) error   { s.vol = v; return nil }
func (s *stubController) SetMute(_ context.Context, _ bool) error    { return nil }
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
