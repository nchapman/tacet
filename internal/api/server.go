package api

import (
	"context"
	"embed"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"net/http"
	"strings"
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
type DiscoverFunc func(ctx context.Context) ([]*upnp.UPnPRenderer, error)

// ConnectFunc connects to a renderer by its UPnP device description URL.
type ConnectFunc func(ctx context.Context, rawURL string) (*upnp.UPnPRenderer, error)

// Server wires HTTP API endpoints, media endpoints, SSE events, GENA callbacks, and web UI.
type Server struct {
	catalog  *library.Catalog
	qobuz    *qobuz.Client
	manager  *queue.Manager
	media    *media.Handler
	gena     *upnp.GENAClient
	discover DiscoverFunc
	connect  ConnectFunc
	mux      *http.ServeMux
}

// NewServer constructs the HTTP API and web remote server.
func NewServer(cat *library.Catalog, qb *qobuz.Client, mgr *queue.Manager) *Server {
	s := &Server{
		catalog:  cat,
		qobuz:    qb,
		manager:  mgr,
		media:    media.NewHandler(cat, qb),
		discover: upnp.Discover,
		connect:  upnp.ConnectByURL,
		mux:      http.NewServeMux(),
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

func (s *Server) handleListRenderers(w http.ResponseWriter, r *http.Request) {
	writeJSON(w, http.StatusOK, map[string]any{
		"renderers":  s.manager.Renderers(),
		"activeId":   s.manager.ActiveRendererID(),
		"streamMode": s.manager.StreamMode(),
	})
}

func (s *Server) handleDiscoverRenderers(w http.ResponseWriter, r *http.Request) {
	ctx, cancel := context.WithTimeout(r.Context(), 5*time.Second)
	defer cancel()
	found, err := s.discover(ctx)
	if err != nil {
		writeError(w, http.StatusInternalServerError, err)
		return
	}
	for _, dev := range found {
		s.manager.RegisterRenderer(dev)
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"discovered": len(found),
		"renderers":  s.manager.Renderers(),
		"activeId":   s.manager.ActiveRendererID(),
	})
}

func (s *Server) handleConnectRenderer(w http.ResponseWriter, r *http.Request) {
	var body struct {
		URL string `json:"url"`
	}
	if err := decodeJSON(w, r, &body); err != nil {
		writeError(w, http.StatusBadRequest, err)
		return
	}
	ctx, cancel := context.WithTimeout(r.Context(), 5*time.Second)
	defer cancel()
	dev, err := s.connect(ctx, body.URL)
	if err != nil {
		writeError(w, http.StatusBadGateway, err)
		return
	}
	s.manager.RegisterRenderer(dev)
	_ = s.manager.SelectRenderer(dev.Info().ID)
	writeJSON(w, http.StatusOK, map[string]any{
		"renderer":  dev.Info(),
		"renderers": s.manager.Renderers(),
		"activeId":  s.manager.ActiveRendererID(),
	})
}

func (s *Server) handleSelectRenderer(w http.ResponseWriter, r *http.Request) {
	var body struct {
		ID string `json:"id"`
	}
	if err := decodeJSON(w, r, &body); err != nil {
		writeError(w, http.StatusBadRequest, err)
		return
	}
	if err := s.manager.SelectRenderer(body.ID); err != nil {
		writeError(w, http.StatusNotFound, err)
		return
	}
	snap, _ := s.manager.Snapshot(body.ID)
	writeJSON(w, http.StatusOK, snap)
}

func (s *Server) handleStreamMode(w http.ResponseWriter, r *http.Request) {
	var body struct {
		Mode queue.StreamMode `json:"mode"`
	}
	if err := decodeJSON(w, r, &body); err != nil {
		writeError(w, http.StatusBadRequest, err)
		return
	}
	if err := s.manager.SetStreamMode(body.Mode); err != nil {
		writeError(w, http.StatusBadRequest, err)
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"streamMode": s.manager.StreamMode()})
}

func (s *Server) handleState(w http.ResponseWriter, r *http.Request) {
	id := r.URL.Query().Get("renderer")
	snap, ok := s.manager.Snapshot(id)
	if !ok {
		writeJSON(w, http.StatusOK, map[string]any{
			"renderer":   nil,
			"state":      "IDLE",
			"queue":      []any{},
			"streamMode": s.manager.StreamMode(),
		})
		return
	}
	writeJSON(w, http.StatusOK, snap)
}

func (s *Server) handleEvents(w http.ResponseWriter, r *http.Request) {
	flusher, ok := w.(http.Flusher)
	if !ok {
		http.Error(w, "streaming unsupported", http.StatusInternalServerError)
		return
	}

	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	w.Header().Set("Connection", "keep-alive")

	ch, unsub := s.manager.Subscribe()
	defer unsub()

	send := func(ev queue.Event) {
		data, err := json.Marshal(ev.Data)
		if err != nil {
			return
		}
		fmt.Fprintf(w, "event: %s\ndata: %s\n\n", ev.Type, data)
		flusher.Flush()
	}

	// Send initial snapshot right away.
	send(queue.Event{Type: "renderers", Data: s.manager.Renderers()})
	if snap, ok := s.manager.Snapshot(""); ok {
		send(queue.Event{Type: "state", Data: snap})
	}

	keepalive := time.NewTicker(20 * time.Second)
	defer keepalive.Stop()

	for {
		select {
		case <-r.Context().Done():
			return
		case ev := <-ch:
			send(ev)
		case <-keepalive.C:
			fmt.Fprint(w, ": ping\n\n")
			flusher.Flush()
		}
	}
}

type queueTrackRequest struct {
	RendererID   string          `json:"rendererId"`
	Tracks       []library.Track `json:"tracks"`
	LocalTrackID string          `json:"localTrackId"`
	LocalAlbumID string          `json:"localAlbumId"`
	QobuzAlbumID string          `json:"qobuzAlbumId"`
	StartIndex   int             `json:"startIndex"`
}

func (s *Server) resolveRequestTracks(ctx context.Context, req queueTrackRequest) ([]library.Track, error) {
	if len(req.Tracks) > 0 {
		// Re-hydrate local track file paths from catalog when clients post JSON tracks.
		resolved := make([]library.Track, 0, len(req.Tracks))
		for _, t := range req.Tracks {
			if t.Source == library.SourceLocal {
				if catTrack, ok := s.catalog.Track(t.ID); ok {
					resolved = append(resolved, catTrack)
					continue
				}
			}
			resolved = append(resolved, t)
		}
		return resolved, nil
	}
	if req.LocalTrackID != "" {
		t, ok := s.catalog.Track(req.LocalTrackID)
		if !ok {
			return nil, fmt.Errorf("local track %q not found", req.LocalTrackID)
		}
		return []library.Track{t}, nil
	}
	if req.LocalAlbumID != "" {
		alb, ok := s.catalog.Album(req.LocalAlbumID)
		if !ok {
			return nil, fmt.Errorf("local album %q not found", req.LocalAlbumID)
		}
		return alb.Tracks, nil
	}
	if req.QobuzAlbumID != "" {
		if s.qobuz == nil {
			return nil, errors.New("qobuz is not configured")
		}
		rawID := strings.TrimPrefix(req.QobuzAlbumID, "qobuz:")
		alb, err := s.qobuz.GetAlbum(ctx, rawID)
		if err != nil {
			return nil, err
		}
		return alb.Tracks, nil
	}
	return nil, errors.New("specify tracks, localTrackId, localAlbumId, or qobuzAlbumId")
}

func (s *Server) handleQueuePlay(w http.ResponseWriter, r *http.Request) {
	var req queueTrackRequest
	if err := decodeJSON(w, r, &req); err != nil {
		writeError(w, http.StatusBadRequest, err)
		return
	}
	tracks, err := s.resolveRequestTracks(r.Context(), req)
	if err != nil {
		writeError(w, http.StatusBadRequest, err)
		return
	}
	if err := s.manager.PlayTracks(r.Context(), req.RendererID, tracks, req.StartIndex); err != nil {
		writeError(w, http.StatusBadGateway, err)
		return
	}
	snap, _ := s.manager.Snapshot(req.RendererID)
	writeJSON(w, http.StatusOK, snap)
}

func (s *Server) handleQueueEnqueue(w http.ResponseWriter, r *http.Request) {
	var req queueTrackRequest
	if err := decodeJSON(w, r, &req); err != nil {
		writeError(w, http.StatusBadRequest, err)
		return
	}
	tracks, err := s.resolveRequestTracks(r.Context(), req)
	if err != nil {
		writeError(w, http.StatusBadRequest, err)
		return
	}
	if err := s.manager.Enqueue(r.Context(), req.RendererID, tracks); err != nil {
		writeError(w, http.StatusBadGateway, err)
		return
	}
	snap, _ := s.manager.Snapshot(req.RendererID)
	writeJSON(w, http.StatusOK, snap)
}

func (s *Server) handleQueueJump(w http.ResponseWriter, r *http.Request) {
	var req struct {
		RendererID string `json:"rendererId"`
		Index      int    `json:"index"`
	}
	if err := decodeJSON(w, r, &req); err != nil {
		writeError(w, http.StatusBadRequest, err)
		return
	}
	if err := s.manager.PlayIndex(r.Context(), req.RendererID, req.Index); err != nil {
		writeError(w, http.StatusBadRequest, err)
		return
	}
	snap, _ := s.manager.Snapshot(req.RendererID)
	writeJSON(w, http.StatusOK, snap)
}

func (s *Server) handleQueueRemove(w http.ResponseWriter, r *http.Request) {
	var req struct {
		RendererID string `json:"rendererId"`
		Index      int    `json:"index"`
	}
	if err := decodeJSON(w, r, &req); err != nil {
		writeError(w, http.StatusBadRequest, err)
		return
	}
	if err := s.manager.RemoveAt(r.Context(), req.RendererID, req.Index); err != nil {
		writeError(w, http.StatusBadRequest, err)
		return
	}
	snap, _ := s.manager.Snapshot(req.RendererID)
	writeJSON(w, http.StatusOK, snap)
}

func (s *Server) handleQueueClear(w http.ResponseWriter, r *http.Request) {
	rendererID := r.URL.Query().Get("renderer")
	if err := s.manager.ClearQueue(r.Context(), rendererID); err != nil {
		writeError(w, http.StatusBadGateway, err)
		return
	}
	snap, _ := s.manager.Snapshot(rendererID)
	writeJSON(w, http.StatusOK, snap)
}

func (s *Server) handleTransportAction(fn func(ctx context.Context, id string) error) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		rendererID := r.URL.Query().Get("renderer")
		if err := fn(r.Context(), rendererID); err != nil {
			writeError(w, http.StatusBadGateway, err)
			return
		}
		snap, _ := s.manager.Snapshot(rendererID)
		writeJSON(w, http.StatusOK, snap)
	}
}

func (s *Server) handleSeek(w http.ResponseWriter, r *http.Request) {
	var req struct {
		RendererID  string `json:"rendererId"`
		PositionSec int    `json:"positionSec"`
	}
	if err := decodeJSON(w, r, &req); err != nil {
		writeError(w, http.StatusBadRequest, err)
		return
	}
	if err := s.manager.Seek(r.Context(), req.RendererID, req.PositionSec); err != nil {
		writeError(w, http.StatusBadGateway, err)
		return
	}
	snap, _ := s.manager.Snapshot(req.RendererID)
	writeJSON(w, http.StatusOK, snap)
}

func (s *Server) handleVolume(w http.ResponseWriter, r *http.Request) {
	var req struct {
		RendererID string `json:"rendererId"`
		Volume     int    `json:"volume"`
	}
	if err := decodeJSON(w, r, &req); err != nil {
		writeError(w, http.StatusBadRequest, err)
		return
	}
	if err := s.manager.SetVolume(r.Context(), req.RendererID, req.Volume); err != nil {
		writeError(w, http.StatusBadGateway, err)
		return
	}
	snap, _ := s.manager.Snapshot(req.RendererID)
	writeJSON(w, http.StatusOK, snap)
}

func (s *Server) handleMute(w http.ResponseWriter, r *http.Request) {
	var req struct {
		RendererID string `json:"rendererId"`
		Muted      bool   `json:"muted"`
	}
	if err := decodeJSON(w, r, &req); err != nil {
		writeError(w, http.StatusBadRequest, err)
		return
	}
	if err := s.manager.SetMute(r.Context(), req.RendererID, req.Muted); err != nil {
		writeError(w, http.StatusBadGateway, err)
		return
	}
	snap, _ := s.manager.Snapshot(req.RendererID)
	writeJSON(w, http.StatusOK, snap)
}

func (s *Server) handleLocalAlbums(w http.ResponseWriter, r *http.Request) {
	writeJSON(w, http.StatusOK, map[string]any{
		"roots":  s.catalog.Roots(),
		"albums": s.catalog.Albums(),
	})
}

func (s *Server) handleLocalAlbum(w http.ResponseWriter, r *http.Request) {
	id := r.PathValue("id")
	alb, ok := s.catalog.Album(id)
	if !ok {
		writeError(w, http.StatusNotFound, fmt.Errorf("album %q not found", id))
		return
	}
	writeJSON(w, http.StatusOK, alb)
}

func (s *Server) handleLocalTracks(w http.ResponseWriter, r *http.Request) {
	q := strings.TrimSpace(r.URL.Query().Get("q"))
	var tracks []library.Track
	if q != "" {
		tracks = s.catalog.Search(q, 100)
	} else {
		tracks = s.catalog.Tracks()
	}
	writeJSON(w, http.StatusOK, map[string]any{"tracks": tracks})
}

func (s *Server) handleLocalRescan(w http.ResponseWriter, r *http.Request) {
	count, err := s.catalog.Scan()
	if err != nil {
		writeError(w, http.StatusInternalServerError, err)
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"tracks": count,
		"albums": len(s.catalog.Albums()),
	})
}

func (s *Server) handleQobuzStatus(w http.ResponseWriter, r *http.Request) {
	if s.qobuz == nil {
		writeJSON(w, http.StatusOK, qobuz.Status{})
		return
	}
	writeJSON(w, http.StatusOK, s.qobuz.Status())
}

func (s *Server) handleQobuzOAuthStart(w http.ResponseWriter, r *http.Request) {
	if s.qobuz == nil {
		http.Error(w, "qobuz client unavailable", http.StatusServiceUnavailable)
		return
	}
	scheme := "http"
	if r.TLS != nil {
		scheme = "https"
	}
	callbackURL := fmt.Sprintf("%s://%s/api/qobuz/oauth/callback", scheme, r.Host)
	authURL, err := s.qobuz.OAuthURL(r.Context(), callbackURL)
	if err != nil {
		http.Error(w, "failed to prepare Qobuz OAuth: "+err.Error(), http.StatusBadGateway)
		return
	}
	http.Redirect(w, r, authURL, http.StatusFound)
}

func (s *Server) handleQobuzOAuthCallback(w http.ResponseWriter, r *http.Request) {
	if s.qobuz == nil {
		http.Error(w, "qobuz client unavailable", http.StatusServiceUnavailable)
		return
	}
	code := strings.TrimSpace(r.URL.Query().Get("code_autorisation"))
	if code == "" {
		code = strings.TrimSpace(r.URL.Query().Get("code"))
	}
	if code == "" {
		http.Error(w, "missing Qobuz OAuth code in callback URL", http.StatusBadRequest)
		return
	}
	if _, err := s.qobuz.ExchangeOAuthCode(r.Context(), code); err != nil {
		http.Error(w, "Qobuz sign-in failed: "+err.Error(), http.StatusUnauthorized)
		return
	}
	http.Redirect(w, r, "/?tab=qobuz", http.StatusFound)
}

func (s *Server) handleQobuzLogin(w http.ResponseWriter, r *http.Request) {
	if s.qobuz == nil {
		writeError(w, http.StatusServiceUnavailable, errors.New("qobuz client unavailable"))
		return
	}
	var req struct {
		Email     string `json:"email"`
		Password  string `json:"password"`
		AppID     string `json:"appId"`
		AppSecret string `json:"appSecret"`
		AuthToken string `json:"authToken"`
	}
	if err := decodeJSON(w, r, &req); err != nil {
		writeError(w, http.StatusBadRequest, err)
		return
	}

	if req.AppID != "" || req.AppSecret != "" {
		s.qobuz.SetCredentials(req.AppID, req.AppSecret, "")
	}
	if strings.TrimSpace(req.AuthToken) != "" {
		st, err := s.qobuz.LoginWithToken(r.Context(), req.AuthToken)
		if err != nil {
			writeError(w, http.StatusUnauthorized, err)
			return
		}
		writeJSON(w, http.StatusOK, st)
		return
	}
	if req.Email != "" && req.Password != "" {
		st, err := s.qobuz.Login(r.Context(), req.Email, req.Password)
		if err != nil {
			writeError(w, http.StatusUnauthorized, err)
			return
		}
		writeJSON(w, http.StatusOK, st)
		return
	}
	st := s.qobuz.Status()
	if !st.Authenticated {
		writeError(w, http.StatusBadRequest, errors.New("sign in via Qobuz OAuth or provide a user_auth_token"))
		return
	}
	writeJSON(w, http.StatusOK, st)
}

func (s *Server) handleQobuzSearch(w http.ResponseWriter, r *http.Request) {
	if s.qobuz == nil {
		writeError(w, http.StatusServiceUnavailable, errors.New("qobuz unavailable"))
		return
	}
	q := strings.TrimSpace(r.URL.Query().Get("q"))
	res, err := s.qobuz.Search(r.Context(), q, 30)
	if err != nil {
		writeError(w, http.StatusBadGateway, err)
		return
	}
	writeJSON(w, http.StatusOK, res)
}

func (s *Server) handleQobuzAlbum(w http.ResponseWriter, r *http.Request) {
	if s.qobuz == nil {
		writeError(w, http.StatusServiceUnavailable, errors.New("qobuz unavailable"))
		return
	}
	id := strings.TrimPrefix(r.PathValue("id"), "qobuz:")
	alb, err := s.qobuz.GetAlbum(r.Context(), id)
	if err != nil {
		writeError(w, http.StatusBadGateway, err)
		return
	}
	writeJSON(w, http.StatusOK, alb)
}

func decodeJSON(w http.ResponseWriter, r *http.Request, dst any) error {
	defer r.Body.Close()
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
