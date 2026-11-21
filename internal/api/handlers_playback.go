package api

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"strconv"
	"strings"
	"time"

	"tacet/internal/library"
	"tacet/internal/qobuz"
	"tacet/internal/queue"
)

func (s *Server) handleListZones(w http.ResponseWriter, _ *http.Request) {
	writeJSON(w, http.StatusOK, map[string]any{
		"zones": s.manager.Zones(),
	})
}

func (s *Server) handleGetZone(w http.ResponseWriter, r *http.Request) {
	zoneID := r.PathValue("zoneId")
	snap, ok := s.manager.Zone(zoneID)
	if !ok {
		writeDomainError(w, fmt.Errorf("%w: %q", queue.ErrZoneNotFound, zoneID))
		return
	}
	writeJSON(w, http.StatusOK, snap)
}

func (s *Server) handleDiscoverZones(w http.ResponseWriter, r *http.Request) {
	ctx, cancel := context.WithTimeout(r.Context(), 5*time.Second)
	defer cancel()
	found, err := s.discover(ctx)
	if err != nil {
		writeAPIError(w, http.StatusInternalServerError, "DISCOVERY_FAILED", err.Error(), true)
		return
	}
	for _, dev := range found {
		s.manager.RegisterRenderer(dev)
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"discovered": len(found),
		"zones":      s.manager.Zones(),
	})
}

func (s *Server) handleConnectZone(w http.ResponseWriter, r *http.Request) {
	var body struct {
		URL string `json:"url"`
	}
	if err := decodeJSON(w, r, &body); err != nil {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
		return
	}
	if strings.TrimSpace(body.URL) == "" {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", "url is required", false)
		return
	}
	ctx, cancel := context.WithTimeout(r.Context(), 5*time.Second)
	defer cancel()
	dev, err := s.connect(ctx, body.URL)
	if err != nil {
		writeDomainError(w, err)
		return
	}
	s.manager.RegisterRenderer(dev)
	zone, _ := s.manager.Zone(dev.Info().ID)
	writeJSON(w, http.StatusOK, map[string]any{
		"zone":  zone,
		"zones": s.manager.Zones(),
	})
}

func (s *Server) handleDeleteZone(w http.ResponseWriter, r *http.Request) {
	zoneID := r.PathValue("zoneId")
	snap, ok := s.manager.Zone(zoneID)
	if !ok {
		writeAPIError(w, http.StatusNotFound, "ZONE_NOT_FOUND", fmt.Sprintf("zone %q not found", zoneID), false)
		return
	}
	if snap.Renderer.Kind == "player" {
		writeAPIError(w, http.StatusConflict, "PLAYER_MANAGED_ZONE", "use DELETE /api/v1/players/{zoneId} to unregister a remote player endpoint", false)
		return
	}
	s.manager.UnregisterRenderer(zoneID)
	writeJSON(w, http.StatusOK, map[string]any{
		"removed": true,
		"zoneId":  zoneID,
	})
}

func (s *Server) handleEvents(w http.ResponseWriter, r *http.Request) {
	flusher, ok := w.(http.Flusher)
	if !ok {
		writeAPIError(w, http.StatusInternalServerError, "STREAMING_UNSUPPORTED", "SSE streaming unsupported", false)
		return
	}

	zoneFilter := make(map[string]struct{})
	for _, z := range r.URL.Query()["zone"] {
		if z = strings.TrimSpace(z); z != "" {
			zoneFilter[z] = struct{}{}
		}
	}

	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	w.Header().Set("Connection", "keep-alive")
	w.Header().Set("X-Accel-Buffering", "no")

	ch, unsub := s.manager.Subscribe()
	defer unsub()

	sendFrame := func(id uint64, evType string, payload any) bool {
		data, err := json.Marshal(payload)
		if err != nil {
			return true
		}
		if id > 0 {
			if _, err := fmt.Fprintf(w, "id: %d\nevent: %s\ndata: %s\n\n", id, evType, data); err != nil {
				return false
			}
		} else {
			if _, err := fmt.Fprintf(w, "event: %s\ndata: %s\n\n", evType, data); err != nil {
				return false
			}
		}
		flusher.Flush()
		return true
	}

	if _, err := fmt.Fprint(w, "retry: 3000\n\n"); err != nil {
		return
	}
	flusher.Flush()

	// Send initial multi-zone snapshot right away.
	allZones := s.manager.Zones()
	zones := make([]queue.ZoneSnapshot, 0, len(allZones))
	for _, z := range allZones {
		if len(zoneFilter) > 0 {
			if _, match := zoneFilter[z.ZoneID]; !match {
				continue
			}
		}
		zones = append(zones, z)
	}
	qbStatus := qobuz.Status{MaxFormatID: qobuz.FormatFLAC192, MaxQuality: qobuz.QualityLabel(qobuz.FormatFLAC192)}
	if s.qobuz != nil {
		qbStatus = s.qobuz.Status()
	}
	if !sendFrame(0, "snapshot", map[string]any{
		"serverTime": time.Now().UTC(),
		"zones":      zones,
		"settings":   s.currentSettings(),
		"qobuz":      qbStatus,
		"library":    s.catalog.Status(),
	}) {
		return
	}

	keepalive := time.NewTicker(20 * time.Second)
	defer keepalive.Stop()

	for {
		select {
		case <-r.Context().Done():
			return
		case <-s.manager.Done():
			return
		case ev, open := <-ch:
			if !open {
				// Subscriber lagged past channel capacity; notify client to resync cleanly.
				_ = sendFrame(0, "resync", map[string]string{"reason": "subscriber_lagged"})
				return
			}
			if ev.ZoneID != "" && len(zoneFilter) > 0 {
				if _, match := zoneFilter[ev.ZoneID]; !match {
					continue
				}
			}
			data := ev.Data
			if ev.Type == "zones.updated" && len(zoneFilter) > 0 {
				if m, ok := ev.Data.(map[string]any); ok {
					if rawZones, ok := m["zones"].([]queue.ZoneSnapshot); ok {
						filtered := make([]queue.ZoneSnapshot, 0, len(rawZones))
						for _, z := range rawZones {
							if _, match := zoneFilter[z.ZoneID]; match {
								filtered = append(filtered, z)
							}
						}
						data = map[string]any{"zones": filtered}
					}
				}
			}
			if !sendFrame(ev.ID, ev.Type, data) {
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

type queueSource struct {
	Type     string   `json:"type"`
	AlbumID  string   `json:"albumId,omitempty"`
	TrackIDs []string `json:"trackIds,omitempty"`
}

func (s *Server) resolveSource(ctx context.Context, src queueSource) ([]library.Track, error) {
	switch src.Type {
	case "album":
		id := strings.TrimSpace(src.AlbumID)
		if id == "" {
			return nil, fmt.Errorf("%w: source.albumId is required", queue.ErrInvalidArgument)
		}
		if alb, ok := s.catalog.Album(id); ok && len(alb.Tracks) > 0 && !alb.IsPartial {
			return alb.Tracks, nil
		}
		if strings.HasPrefix(id, "qobuz:") {
			if s.qobuz == nil {
				return nil, ErrQobuzUnavailable
			}
			alb, err := s.qobuz.GetAlbum(ctx, id)
			if err != nil {
				return nil, err
			}
			return alb.Tracks, nil
		}
		alb, ok := s.catalog.Album(id)
		if !ok {
			return nil, fmt.Errorf("%w: %q", ErrAlbumNotFound, id)
		}
		return alb.Tracks, nil

	case "tracks":
		const maxBatchTracks = 500
		if len(src.TrackIDs) == 0 {
			return nil, fmt.Errorf("%w: source.trackIds must not be empty", queue.ErrInvalidArgument)
		}
		if len(src.TrackIDs) > maxBatchTracks {
			return nil, fmt.Errorf("%w: source.trackIds exceeds maximum of %d tracks", queue.ErrInvalidArgument, maxBatchTracks)
		}
		resolved := make([]library.Track, 0, len(src.TrackIDs))
		for _, rawID := range src.TrackIDs {
			id := strings.TrimSpace(rawID)
			if tr, ok := s.catalog.Track(id); ok {
				resolved = append(resolved, tr)
				continue
			}
			if strings.HasPrefix(id, "qobuz:") {
				if s.qobuz == nil {
					return nil, ErrQobuzUnavailable
				}
				numID, err := strconv.ParseInt(strings.TrimPrefix(id, "qobuz:"), 10, 64)
				if err != nil || numID <= 0 {
					return nil, fmt.Errorf("%w: invalid qobuz trackId %q", queue.ErrInvalidArgument, id)
				}
				tr, err := s.qobuz.ResolveTrack(ctx, numID)
				if err != nil {
					return nil, err
				}
				resolved = append(resolved, tr)
				continue
			}
			return nil, fmt.Errorf("%w: %q", ErrTrackNotFound, id)
		}
		return resolved, nil

	default:
		return nil, fmt.Errorf("%w: source.type must be 'album' or 'tracks'", queue.ErrInvalidArgument)
	}
}

func (s *Server) handleReplaceQueue(w http.ResponseWriter, r *http.Request) {
	zoneID := r.PathValue("zoneId")
	var req struct {
		Source     queueSource `json:"source"`
		StartIndex int         `json:"startIndex"`
	}
	if err := decodeJSON(w, r, &req); err != nil {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
		return
	}
	tracks, err := s.resolveSource(r.Context(), req.Source)
	if err != nil {
		writeDomainError(w, err)
		return
	}
	snap, err := s.manager.ReplaceQueue(r.Context(), zoneID, tracks, req.StartIndex)
	if err != nil {
		writeDomainError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, snap)
}

func (s *Server) handleAddQueueItems(w http.ResponseWriter, r *http.Request) {
	zoneID := r.PathValue("zoneId")
	var req struct {
		Source   queueSource    `json:"source"`
		Position queue.Position `json:"position"`
		Autoplay *bool          `json:"autoplay"`
	}
	if err := decodeJSON(w, r, &req); err != nil {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
		return
	}
	tracks, err := s.resolveSource(r.Context(), req.Source)
	if err != nil {
		writeDomainError(w, err)
		return
	}
	autoplay := true
	if req.Autoplay != nil {
		autoplay = *req.Autoplay
	}
	snap, err := s.manager.AddQueueItems(r.Context(), zoneID, tracks, req.Position, autoplay)
	if err != nil {
		writeDomainError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, snap)
}

func (s *Server) handleJumpQueue(w http.ResponseWriter, r *http.Request) {
	zoneID := r.PathValue("zoneId")
	var req struct {
		ItemID string `json:"itemId"`
	}
	if err := decodeJSON(w, r, &req); err != nil {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
		return
	}
	snap, err := s.manager.JumpToItem(r.Context(), zoneID, req.ItemID)
	if err != nil {
		writeDomainError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, snap)
}

func (s *Server) handleRemoveQueueItem(w http.ResponseWriter, r *http.Request) {
	zoneID := r.PathValue("zoneId")
	itemID := r.PathValue("itemId")
	snap, err := s.manager.RemoveItem(r.Context(), zoneID, itemID)
	if err != nil {
		writeDomainError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, snap)
}

func (s *Server) handleMoveQueueItem(w http.ResponseWriter, r *http.Request) {
	zoneID := r.PathValue("zoneId")
	itemID := r.PathValue("itemId")
	var req struct {
		ToIndex *int `json:"toIndex"`
	}
	if err := decodeJSON(w, r, &req); err != nil {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
		return
	}
	if req.ToIndex == nil {
		writeDomainError(w, fmt.Errorf("%w: toIndex is required", queue.ErrInvalidArgument))
		return
	}
	snap, err := s.manager.MoveItem(zoneID, itemID, *req.ToIndex)
	if err != nil {
		writeDomainError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, snap)
}

func (s *Server) handleClearQueue(w http.ResponseWriter, r *http.Request) {
	zoneID := r.PathValue("zoneId")
	snap, err := s.manager.ClearQueue(r.Context(), zoneID)
	if err != nil {
		writeDomainError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, snap)
}

func (s *Server) handleZoneAction(fn func(ctx context.Context, zoneID string) (queue.ZoneSnapshot, error)) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		zoneID := r.PathValue("zoneId")
		snap, err := fn(r.Context(), zoneID)
		if err != nil {
			writeDomainError(w, err)
			return
		}
		writeJSON(w, http.StatusOK, snap)
	}
}

func (s *Server) handleSeek(w http.ResponseWriter, r *http.Request) {
	zoneID := r.PathValue("zoneId")
	var req struct {
		PositionSec *int `json:"positionSec"`
	}
	if err := decodeJSON(w, r, &req); err != nil {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
		return
	}
	if req.PositionSec == nil {
		writeDomainError(w, fmt.Errorf("%w: positionSec is required", queue.ErrInvalidArgument))
		return
	}
	snap, err := s.manager.Seek(r.Context(), zoneID, *req.PositionSec)
	if err != nil {
		writeDomainError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, snap)
}

func (s *Server) handleVolume(w http.ResponseWriter, r *http.Request) {
	zoneID := r.PathValue("zoneId")
	var req struct {
		Volume *int `json:"volume"`
	}
	if err := decodeJSON(w, r, &req); err != nil {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
		return
	}
	if req.Volume == nil {
		writeDomainError(w, fmt.Errorf("%w: volume is required", queue.ErrInvalidArgument))
		return
	}
	snap, err := s.manager.SetVolume(r.Context(), zoneID, *req.Volume)
	if err != nil {
		writeDomainError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, snap)
}

func (s *Server) handleMute(w http.ResponseWriter, r *http.Request) {
	zoneID := r.PathValue("zoneId")
	var req struct {
		Muted *bool `json:"muted"`
	}
	if err := decodeJSON(w, r, &req); err != nil {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
		return
	}
	if req.Muted == nil {
		writeDomainError(w, fmt.Errorf("%w: muted is required", queue.ErrInvalidArgument))
		return
	}
	snap, err := s.manager.SetMute(r.Context(), zoneID, *req.Muted)
	if err != nil {
		writeDomainError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, snap)
}

func parsePageParams(r *http.Request, defaultLimit, maxLimit int) (int, int, error) {
	limit := defaultLimit
	offset := 0
	if raw := strings.TrimSpace(r.URL.Query().Get("limit")); raw != "" {
		v, err := strconv.Atoi(raw)
		if err != nil || v <= 0 {
			return 0, 0, errors.New("limit must be a positive integer")
		}
		if v > maxLimit {
			v = maxLimit
		}
		limit = v
	}
	if raw := strings.TrimSpace(r.URL.Query().Get("offset")); raw != "" {
		v, err := strconv.Atoi(raw)
		if err != nil || v < 0 {
			return 0, 0, errors.New("offset must be a non-negative integer")
		}
		offset = v
	}
	return limit, offset, nil
}
