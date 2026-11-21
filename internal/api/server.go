package api

import (
	"context"
	"embed"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"net/http"
	"path"
	"strings"
	"sync"
	"time"

	"tacet/internal/library"
	"tacet/internal/media"
	"tacet/internal/qobuz"
	"tacet/internal/queue"
	"tacet/internal/upnp"
)

//go:embed all:web/dist
var webFS embed.FS

// DiscoverFunc discovers UPnP renderers on the LAN.
type DiscoverFunc func(ctx context.Context) ([]*upnp.Renderer, error)

// ConnectFunc connects to a renderer by its UPnP device description URL.
type ConnectFunc func(ctx context.Context, rawURL string) (*upnp.Renderer, error)

// Server wires HTTP API endpoints, media endpoints, SSE events, GENA callbacks, and web UI.
type Server struct {
	catalog    *library.Catalog
	qobuz      *qobuz.Client
	manager    *queue.Manager
	media      *media.Handler
	gena       *upnp.GENAClient
	discover   DiscoverFunc
	connect    ConnectFunc
	mux        *http.ServeMux
	oauthMu    sync.Mutex
	oauthState map[string]time.Time
}

// NewServer constructs the HTTP API and web remote server.
func NewServer(cat *library.Catalog, qb *qobuz.Client, mgr *queue.Manager) *Server {
	s := &Server{
		catalog:    cat,
		qobuz:      qb,
		manager:    mgr,
		media:      media.NewHandler(cat, qb),
		discover:   upnp.Discover,
		connect:    upnp.ConnectByURL,
		mux:        http.NewServeMux(),
		oauthState: make(map[string]time.Time),
	}
	s.routes()
	return s
}

// SetGENA attaches the UPnP GENA event receiver so renderers can deliver NOTIFY
// callbacks to /upnp/event/{token}.
func (s *Server) SetGENA(g *upnp.GENAClient) {
	s.gena = g
}

// Handler returns the root HTTP handler.
func (s *Server) Handler() http.Handler {
	return s.mux
}

func (s *Server) routes() {
	s.media.Register(s.mux)

	// UPnP GENA NOTIFY callback endpoint
	s.mux.HandleFunc("/upnp/event/", func(w http.ResponseWriter, r *http.Request) {
		if s.gena == nil {
			http.Error(w, "GENA events disabled", http.StatusServiceUnavailable)
			return
		}
		s.gena.Handler().ServeHTTP(w, r)
	})

	// Health check
	s.mux.HandleFunc("GET /api/health", func(w http.ResponseWriter, r *http.Request) {
		writeJSON(w, http.StatusOK, map[string]string{"status": "ok"})
	})

	// Renderers & settings
	s.mux.HandleFunc("GET /api/renderers", s.handleListRenderers)
	s.mux.HandleFunc("POST /api/renderers/discover", s.handleDiscoverRenderers)
	s.mux.HandleFunc("POST /api/renderers/connect", s.handleConnectRenderer)
	s.mux.HandleFunc("POST /api/renderers/select", s.handleSelectRenderer)
	s.mux.HandleFunc("POST /api/settings/stream-mode", s.handleStreamMode)

	// State & live SSE stream
	s.mux.HandleFunc("GET /api/state", s.handleState)
	s.mux.HandleFunc("GET /api/events", s.handleEvents)

	// Queue & transport
	s.mux.HandleFunc("POST /api/queue/play", s.handleQueuePlay)
	s.mux.HandleFunc("POST /api/queue/enqueue", s.handleQueueEnqueue)
	s.mux.HandleFunc("POST /api/queue/jump", s.handleQueueJump)
	s.mux.HandleFunc("POST /api/queue/remove", s.handleQueueRemove)
	s.mux.HandleFunc("POST /api/queue/clear", s.handleQueueClear)

	s.mux.HandleFunc("POST /api/transport/play", s.handleTransportAction(func(ctx context.Context, id string) error { return s.manager.Play(ctx, id) }))
	s.mux.HandleFunc("POST /api/transport/pause", s.handleTransportAction(func(ctx context.Context, id string) error { return s.manager.Pause(ctx, id) }))
	s.mux.HandleFunc("POST /api/transport/stop", s.handleTransportAction(func(ctx context.Context, id string) error { return s.manager.Stop(ctx, id) }))
	s.mux.HandleFunc("POST /api/transport/next", s.handleTransportAction(func(ctx context.Context, id string) error { return s.manager.Next(ctx, id) }))
	s.mux.HandleFunc("POST /api/transport/prev", s.handleTransportAction(func(ctx context.Context, id string) error { return s.manager.Prev(ctx, id) }))
	s.mux.HandleFunc("POST /api/transport/seek", s.handleSeek)
	s.mux.HandleFunc("POST /api/transport/volume", s.handleVolume)
	s.mux.HandleFunc("POST /api/transport/mute", s.handleMute)

	// Local NAS library
	s.mux.HandleFunc("GET /api/library/albums", s.handleLocalAlbums)
	s.mux.HandleFunc("GET /api/library/albums/{id}", s.handleLocalAlbum)
	s.mux.HandleFunc("GET /api/library/tracks", s.handleLocalTracks)
	s.mux.HandleFunc("POST /api/library/rescan", s.handleLocalRescan)

	// Qobuz
	s.mux.HandleFunc("GET /api/qobuz/status", s.handleQobuzStatus)
	s.mux.HandleFunc("GET /api/qobuz/oauth/start", s.handleQobuzOAuthStart)
	s.mux.HandleFunc("GET /api/qobuz/oauth/callback", s.handleQobuzOAuthCallback)
	s.mux.HandleFunc("POST /api/qobuz/login", s.handleQobuzLogin)
	s.mux.HandleFunc("GET /api/qobuz/search", s.handleQobuzSearch)
	s.mux.HandleFunc("GET /api/qobuz/albums/{id}", s.handleQobuzAlbum)

	// Return JSON 404 for unmatched /api or /api/* routes instead of serving SPA index.html.
	notFoundAPI := func(w http.ResponseWriter, _ *http.Request) {
		writeError(w, http.StatusNotFound, errors.New("API endpoint not found"))
	}
	s.mux.HandleFunc("/api", notFoundAPI)
	s.mux.HandleFunc("/api/", notFoundAPI)

	// Embedded React/Vite Web UI
	if sub, err := fs.Sub(webFS, "web/dist"); err == nil {
		s.mux.Handle("/", newSPAHandler(sub))
	}
}

func newSPAHandler(dist fs.FS) http.Handler {
	fileServer := http.FileServer(http.FS(dist))
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodGet && r.Method != http.MethodHead {
			http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
			return
		}
		clean := strings.TrimPrefix(path.Clean("/"+r.URL.Path), "/")
		if clean == "" {
			clean = "index.html"
		}

		if info, err := fs.Stat(dist, clean); err == nil && !info.IsDir() {
			if strings.HasPrefix(clean, "assets/") {
				w.Header().Set("Cache-Control", "public, max-age=31536000, immutable")
			} else if clean == "index.html" {
				w.Header().Set("Cache-Control", "no-cache")
			}
			fileServer.ServeHTTP(w, r)
			return
		}

		// Do not serve HTML fallback for missing static asset paths.
		if clean == "assets" || strings.HasPrefix(clean, "assets/") {
			http.NotFound(w, r)
			return
		}

		indexBytes, err := fs.ReadFile(dist, "index.html")
		if err != nil {
			http.Error(w, "web UI not built — run `make web-build`", http.StatusServiceUnavailable)
			return
		}
		w.Header().Set("Content-Type", "text/html; charset=utf-8")
		w.Header().Set("Cache-Control", "no-cache")
		if r.Method == http.MethodHead {
			return
		}
		_, _ = w.Write(indexBytes)
	})
}

func decodeJSON(w http.ResponseWriter, r *http.Request, dst any) error {
	defer func() { _ = r.Body.Close() }()
	dec := json.NewDecoder(http.MaxBytesReader(w, r.Body, 2<<20))
	if err := dec.Decode(dst); err != nil {
		return fmt.Errorf("invalid JSON: %w", err)
	}
	return nil
}

func writeJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(v)
}

func writeError(w http.ResponseWriter, status int, err error) {
	writeJSON(w, status, map[string]string{"error": err.Error()})
}
