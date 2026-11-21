package api

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"strings"
	"time"

	"tacet/internal/library"
	"tacet/internal/queue"
)

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

	send := func(ev queue.Event) bool {
		data, err := json.Marshal(ev.Data)
		if err != nil {
			return true
		}
		if _, err := fmt.Fprintf(w, "event: %s\ndata: %s\n\n", ev.Type, data); err != nil {
			return false
		}
		flusher.Flush()
		return true
	}

	// Send initial snapshot right away.
	if !send(queue.Event{Type: "renderers", Data: s.manager.Renderers()}) {
		return
	}
	if snap, ok := s.manager.Snapshot(""); ok {
		if !send(queue.Event{Type: "state", Data: snap}) {
			return
		}
	}

	keepalive := time.NewTicker(20 * time.Second)
	defer keepalive.Stop()

	for {
		select {
		case <-r.Context().Done():
			return
		case ev := <-ch:
			if !send(ev) {
				return
			}
		case <-keepalive.C:
			if _, err := fmt.Fprint(w, ": ping\n\n"); err != nil {
				return
			}
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
