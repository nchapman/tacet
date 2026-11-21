package api

import (
	"context"
	"embed"
	"encoding/json"
	"fmt"
	"io/fs"
	"net/http"
	"sync"
	"time"

	"tacet/internal/library"
	"tacet/internal/media"
	"tacet/internal/qobuz"
	"tacet/internal/queue"
	"tacet/internal/upnp"
)

//go:embed web/*
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

	// Embedded Web UI
	sub, err := fs.Sub(webFS, "web")
	if err == nil {
		s.mux.Handle("/", http.FileServer(http.FS(sub)))
	}
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
