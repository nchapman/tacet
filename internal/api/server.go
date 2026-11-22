package api

import (
	"context"
	"embed"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"log"
	"mime"
	"net/http"
	"net/url"
	"path"
	"strings"
	"sync"
	"time"

	"tacet/internal/library"
	"tacet/internal/media"
	"tacet/internal/player"
	"tacet/internal/qobuz"
	"tacet/internal/queue"
	"tacet/internal/upnp"
)

// ServerVersion identifies the running tacet build in /api/v1/info.
const ServerVersion = "0.2.0"

//go:embed all:web/dist
var webFS embed.FS

//go:embed openapi.yaml
var openAPISpec []byte

// DiscoverFunc discovers UPnP renderers on the LAN.
type DiscoverFunc func(ctx context.Context) ([]*upnp.Renderer, error)

// ConnectFunc connects to a renderer by its UPnP device description URL.
type ConnectFunc func(ctx context.Context, rawURL string) (*upnp.Renderer, error)

// Settings represents global server playback preferences.
type Settings struct {
	StreamMode         queue.StreamMode `json:"streamMode"`
	QobuzMaxQuality    string           `json:"qobuzMaxQuality"`
	TranscodeAvailable bool             `json:"transcodeAvailable"`
}

// Server wires HTTP API endpoints, media endpoints, SSE events, GENA callbacks, and web UI.
type Server struct {
	catalog    *library.Catalog
	qobuz      *qobuz.Client
	manager    *queue.Manager
	players    *player.Registry
	media      *media.Handler
	gena       *upnp.GENAClient
	discover   DiscoverFunc
	connect    ConnectFunc
	corsOrigin string
	mux        *http.ServeMux
	apiMux     *http.ServeMux
	oauthMu    sync.Mutex
	oauthState map[string]time.Time
}

// NewServer constructs the HTTP API and web remote server.
func NewServer(cat *library.Catalog, qb *qobuz.Client, mgr *queue.Manager) *Server {
	s := &Server{
		catalog: cat,
		qobuz:   qb,
		manager: mgr,
		players: player.NewRegistry(
			mgr.RegisterRenderer,
			func(id string) { mgr.UnregisterRenderer(id) },
			mgr.HandleDeviceEvent,
		),
		media:      media.NewHandler(cat, qb),
		discover:   upnp.Discover,
		connect:    upnp.ConnectByURL,
		mux:        http.NewServeMux(),
		apiMux:     http.NewServeMux(),
		oauthState: make(map[string]time.Time),
	}
	mgr.SetErrorClassifier(func(err error) (string, bool, bool) {
		switch {
		case errors.Is(err, player.ErrPlaybackBlocked):
			return "PLAYBACK_BLOCKED", false, true
		case errors.Is(err, player.ErrCommandTimeout):
			return "PLAYER_TIMEOUT", true, true
		case errors.Is(err, player.ErrPlayerDisconnected):
			return "RENDERER_UNREACHABLE", true, true
		default:
			return "", false, false
		}
	})

	cat.SetOnChange(func(st library.Status) {
		mgr.Publish("library.updated", st)
	})

	s.routes()
	return s
}

// Close releases active player endpoint sessions and background timers.
func (s *Server) Close() {
	if s.players != nil {
		s.players.Close()
	}
}

// SetArtCacheDir configures the on-disk directory used to cache resized cover art.
func (s *Server) SetArtCacheDir(dir string) {
	s.media.SetArtCacheDir(dir)
}

// SetGENA attaches the UPnP GENA event receiver so renderers can deliver NOTIFY
// callbacks to /upnp/event/{token}.
func (s *Server) SetGENA(g *upnp.GENAClient) {
	s.gena = g
}

// SetCORSOrigin enables CORS headers for the specified origin (or "*" for LAN apps).
func (s *Server) SetCORSOrigin(origin string) {
	s.corsOrigin = strings.TrimSpace(origin)
}

// Handler returns the root HTTP handler with optional CORS and JSON 405 handling for /api/*.
func (s *Server) Handler() http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		isAPI := r.URL.Path == "/api" || strings.HasPrefix(r.URL.Path, "/api/")

		if s.corsOrigin != "" && (isAPI || strings.HasPrefix(r.URL.Path, "/media/")) {
			w.Header().Set("Access-Control-Allow-Origin", s.corsOrigin)
			w.Header().Set("Access-Control-Allow-Methods", "GET, POST, PUT, PATCH, DELETE, OPTIONS")
			w.Header().Set("Access-Control-Allow-Headers", "Content-Type, Authorization, If-None-Match, Last-Event-ID, Range")
			w.Header().Set("Access-Control-Expose-Headers", "ETag, Allow, Content-Length, Content-Range, Accept-Ranges")
			if s.corsOrigin != "*" {
				w.Header().Set("Access-Control-Allow-Credentials", "true")
				w.Header().Add("Vary", "Origin")
			}
			if r.Method == http.MethodOptions {
				w.WriteHeader(http.StatusNoContent)
				return
			}
		}

		if isAPI {
			if isMutatingMethod(r.Method) && !s.isAllowedOrigin(r) {
				writeAPIError(w, http.StatusForbidden, "CROSS_SITE_BLOCKED", "cross-site mutating request rejected", false)
				return
			}
			h, pattern := s.apiMux.Handler(r)
			if pattern != "" {
				s.apiMux.ServeHTTP(w, r)
				return
			}
			probe := &statusProbe{header: make(http.Header)}
			h.ServeHTTP(probe, r)
			if probe.status == http.StatusMethodNotAllowed {
				if allow := probe.header.Get("Allow"); allow != "" {
					w.Header().Set("Allow", allow)
				}
				writeAPIError(w, http.StatusMethodNotAllowed, "METHOD_NOT_ALLOWED", "method not allowed for this endpoint", false)
				return
			}
			writeAPIError(w, http.StatusNotFound, "NOT_FOUND", "API endpoint not found", false)
			return
		}
		s.mux.ServeHTTP(w, r)
	})
}

type statusProbe struct {
	header http.Header
	status int
}

func (p *statusProbe) Header() http.Header         { return p.header }
func (p *statusProbe) WriteHeader(code int)        { p.status = code }
func (p *statusProbe) Write(b []byte) (int, error) { return len(b), nil }

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

	// Health, server metadata, OpenAPI spec, and global settings
	s.apiMux.HandleFunc("GET /api/health", func(w http.ResponseWriter, _ *http.Request) {
		writeJSON(w, http.StatusOK, map[string]string{"status": "ok"})
	})
	s.apiMux.HandleFunc("GET /api/v1/info", s.handleInfo)
	s.apiMux.HandleFunc("GET /api/v1/openapi.yaml", s.handleOpenAPI)
	s.apiMux.HandleFunc("GET /api/v1/settings", s.handleGetSettings)
	s.apiMux.HandleFunc("PATCH /api/v1/settings", s.handlePatchSettings)

	// Live SSE stream (multi-zone, optional ?zone= filter)
	s.apiMux.HandleFunc("GET /api/v1/events", s.handleEvents)

	// Zones & discovery
	s.apiMux.HandleFunc("GET /api/v1/zones", s.handleListZones)
	s.apiMux.HandleFunc("POST /api/v1/zones/discover", s.handleDiscoverZones)
	s.apiMux.HandleFunc("POST /api/v1/zones/connect", s.handleConnectZone)
	s.apiMux.HandleFunc("GET /api/v1/zones/{zoneId}", s.handleGetZone)
	s.apiMux.HandleFunc("DELETE /api/v1/zones/{zoneId}", s.handleDeleteZone)

	// Zone queue management
	s.apiMux.HandleFunc("PUT /api/v1/zones/{zoneId}/queue", s.handleReplaceQueue)
	s.apiMux.HandleFunc("DELETE /api/v1/zones/{zoneId}/queue", s.handleClearQueue)
	s.apiMux.HandleFunc("POST /api/v1/zones/{zoneId}/queue/items", s.handleAddQueueItems)
	s.apiMux.HandleFunc("POST /api/v1/zones/{zoneId}/queue/jump", s.handleJumpQueue)
	s.apiMux.HandleFunc("DELETE /api/v1/zones/{zoneId}/queue/items/{itemId}", s.handleRemoveQueueItem)
	s.apiMux.HandleFunc("POST /api/v1/zones/{zoneId}/queue/items/{itemId}/move", s.handleMoveQueueItem)

	// Zone transport & volume
	s.apiMux.HandleFunc("POST /api/v1/zones/{zoneId}/transport/play", s.handleZoneAction(s.manager.Play))
	s.apiMux.HandleFunc("POST /api/v1/zones/{zoneId}/transport/pause", s.handleZoneAction(s.manager.Pause))
	s.apiMux.HandleFunc("POST /api/v1/zones/{zoneId}/transport/stop", s.handleZoneAction(s.manager.Stop))
	s.apiMux.HandleFunc("POST /api/v1/zones/{zoneId}/transport/next", s.handleZoneAction(s.manager.Next))
	s.apiMux.HandleFunc("POST /api/v1/zones/{zoneId}/transport/prev", s.handleZoneAction(s.manager.Prev))
	s.apiMux.HandleFunc("POST /api/v1/zones/{zoneId}/transport/seek", s.handleSeek)
	s.apiMux.HandleFunc("PUT /api/v1/zones/{zoneId}/volume", s.handleVolume)
	s.apiMux.HandleFunc("PUT /api/v1/zones/{zoneId}/mute", s.handleMute)
	s.apiMux.HandleFunc("PUT /api/v1/zones/{zoneId}/quality", s.handleZoneQuality)
	s.apiMux.HandleFunc("PUT /api/v1/zones/{zoneId}/shuffle", s.handleZoneShuffle)
	s.apiMux.HandleFunc("PUT /api/v1/zones/{zoneId}/repeat", s.handleZoneRepeat)

	// Remote HTTP/SSE audio player endpoints (web, mobile, desktop, CLI/daemon)
	s.apiMux.HandleFunc("POST /api/v1/players", s.handleRegisterPlayer)
	s.apiMux.HandleFunc("PATCH /api/v1/players/{zoneId}", s.handleUpdatePlayer)
	s.apiMux.HandleFunc("GET /api/v1/players/{zoneId}/commands", s.handlePlayerCommands)
	s.apiMux.HandleFunc("POST /api/v1/players/{zoneId}/state", s.handlePlayerState)
	s.apiMux.HandleFunc("DELETE /api/v1/players/{zoneId}", s.handleUnregisterPlayer)

	// Unified multi-source library (local NAS + synced remote sources)
	s.apiMux.HandleFunc("GET /api/v1/library/status", s.handleLibraryStatus)
	s.apiMux.HandleFunc("GET /api/v1/library/albums", s.handleLibraryAlbums)
	s.apiMux.HandleFunc("GET /api/v1/library/albums/{id}", s.handleLibraryAlbum)
	s.apiMux.HandleFunc("POST /api/v1/library/albums/{id}/primary", s.handleSetPrimaryVersion)
	s.apiMux.HandleFunc("DELETE /api/v1/library/albums/{id}/version-group", s.handleUngroupVersion)
	s.apiMux.HandleFunc("POST /api/v1/library/versions/group", s.handleGroupVersions)
	s.apiMux.HandleFunc("GET /api/v1/library/tracks", s.handleLibraryTracks)
	s.apiMux.HandleFunc("GET /api/v1/library/artists", s.handleLibraryArtists)
	s.apiMux.HandleFunc("GET /api/v1/library/genres", s.handleLibraryGenres)
	s.apiMux.HandleFunc("GET /api/v1/library/heavy-rotation", s.handleHeavyRotation)
	s.apiMux.HandleFunc("GET /api/v1/library/search", s.handleLibrarySearch)
	s.apiMux.HandleFunc("POST /api/v1/library/rescan", s.handleLibraryRescan)
	s.apiMux.HandleFunc("POST /api/v1/library/sync", s.handleLibrarySync)
	s.apiMux.HandleFunc("POST /api/v1/library/remote/albums", s.handleAddRemoteAlbum)
	s.apiMux.HandleFunc("DELETE /api/v1/library/remote/albums/{id}", s.handleRemoveRemoteAlbum)
	s.apiMux.HandleFunc("POST /api/v1/library/remote/tracks", s.handleAddRemoteTrack)
	s.apiMux.HandleFunc("DELETE /api/v1/library/remote/tracks/{id}", s.handleRemoveRemoteTrack)

	// Qobuz
	s.apiMux.HandleFunc("GET /api/v1/qobuz/status", s.handleQobuzStatus)
	s.apiMux.HandleFunc("GET /api/v1/qobuz/oauth/start", s.handleQobuzOAuthStart)
	s.apiMux.HandleFunc("GET /api/v1/qobuz/oauth/callback", s.handleQobuzOAuthCallback)
	s.apiMux.HandleFunc("POST /api/v1/qobuz/login", s.handleQobuzLogin)
	s.apiMux.HandleFunc("DELETE /api/v1/qobuz/session", s.handleQobuzLogout)
	s.apiMux.HandleFunc("GET /api/v1/qobuz/search", s.handleQobuzSearch)
	s.apiMux.HandleFunc("GET /api/v1/qobuz/artist", s.handleQobuzArtist)
	s.apiMux.HandleFunc("GET /api/v1/qobuz/albums/{id}", s.handleQobuzAlbum)

	// Embedded React/Vite Web UI
	if sub, err := fs.Sub(webFS, "web/dist"); err == nil {
		s.mux.Handle("/", newSPAHandler(sub))
	}
}

func (s *Server) currentSettings() Settings {
	maxQ := qobuz.QualityLabel(qobuz.FormatFLAC192)
	if s.qobuz != nil {
		maxQ = s.qobuz.MaxQuality()
	}
	return Settings{
		StreamMode:         s.manager.StreamMode(),
		QobuzMaxQuality:    maxQ,
		TranscodeAvailable: s.manager.TranscodeAvailable(),
	}
}

func (s *Server) handleInfo(w http.ResponseWriter, _ *http.Request) {
	writeJSON(w, http.StatusOK, map[string]any{
		"name":       "tacet",
		"version":    ServerVersion,
		"apiVersion": 1,
		"capabilities": map[string]bool{
			"localLibrary": len(s.catalog.Roots()) > 0,
			"qobuz":        s.qobuz != nil,
			"gena":         s.gena != nil,
		},
	})
}

func (s *Server) handleOpenAPI(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Content-Type", "application/yaml; charset=utf-8")
	w.Header().Set("Cache-Control", "no-cache")
	if r.Method == http.MethodHead {
		return
	}
	_, _ = w.Write(openAPISpec)
}

func (s *Server) handleGetSettings(w http.ResponseWriter, _ *http.Request) {
	writeJSON(w, http.StatusOK, s.currentSettings())
}

func (s *Server) handlePatchSettings(w http.ResponseWriter, r *http.Request) {
	var body struct {
		StreamMode      *queue.StreamMode `json:"streamMode"`
		QobuzMaxQuality *string           `json:"qobuzMaxQuality"`
	}
	if err := decodeJSON(w, r, &body); err != nil {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
		return
	}
	if body.StreamMode != nil {
		if err := s.manager.SetStreamMode(*body.StreamMode); err != nil {
			writeDomainError(w, err)
			return
		}
		if s.catalog != nil {
			if err := s.catalog.SetPref(r.Context(), library.PrefStreamMode, string(*body.StreamMode)); err != nil {
				log.Printf("warning: failed to persist stream_mode preference: %v", err)
			}
		}
	}
	if body.QobuzMaxQuality != nil {
		if s.qobuz == nil {
			writeDomainError(w, ErrQobuzUnavailable)
			return
		}
		if err := s.qobuz.SetMaxQuality(*body.QobuzMaxQuality); err != nil {
			writeDomainError(w, err)
			return
		}
		if s.catalog != nil {
			if err := s.catalog.SetPref(r.Context(), library.PrefQobuzMaxQuality, s.qobuz.MaxQuality()); err != nil {
				log.Printf("warning: failed to persist qobuz_max_quality preference: %v", err)
			}
		}
		s.manager.Publish("qobuz.status", s.qobuz.Status())
	}
	settings := s.currentSettings()
	s.manager.Publish("settings.updated", settings)
	writeJSON(w, http.StatusOK, settings)
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

func isMutatingMethod(method string) bool {
	return method == http.MethodPost || method == http.MethodPut || method == http.MethodPatch || method == http.MethodDelete
}

// isAllowedOrigin defends LAN/localhost endpoints against drive-by browser CSRF
// while allowing same-origin browsers, non-browser native/CLI clients (no Origin
// / Sec-Fetch-Site headers), and explicitly configured --cors-origin origins.
func (s *Server) isAllowedOrigin(r *http.Request) bool {
	origin := strings.TrimSpace(r.Header.Get("Origin"))
	if origin != "" {
		if s.corsOrigin == "*" || (s.corsOrigin != "" && origin == s.corsOrigin) {
			return true
		}
		u, err := url.Parse(origin)
		if err != nil || u.Host == "" {
			return false
		}
		return strings.EqualFold(u.Host, r.Host)
	}
	if sfs := strings.ToLower(strings.TrimSpace(r.Header.Get("Sec-Fetch-Site"))); sfs == "cross-site" {
		return s.corsOrigin == "*"
	}
	return true
}

func decodeJSON(w http.ResponseWriter, r *http.Request, dst any) error {
	defer func() { _ = r.Body.Close() }()
	if ct := strings.TrimSpace(r.Header.Get("Content-Type")); ct != "" {
		mt, _, err := mime.ParseMediaType(ct)
		if err != nil || !strings.EqualFold(mt, "application/json") {
			return errors.New("Content-Type must be application/json")
		}
	} else if r.Header.Get("Origin") != "" || r.Header.Get("Sec-Fetch-Site") != "" {
		return errors.New("Content-Type must be application/json")
	}
	dec := json.NewDecoder(http.MaxBytesReader(w, r.Body, 2<<20))
	dec.DisallowUnknownFields()
	if err := dec.Decode(dst); err != nil {
		return fmt.Errorf("invalid JSON: %w", err)
	}
	if err := dec.Decode(&struct{}{}); !errors.Is(err, io.EOF) {
		return errors.New("invalid JSON: unexpected trailing data")
	}
	return nil
}

func writeJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(v)
}
