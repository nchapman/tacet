package api

import (
	"encoding/json"
	"fmt"
	"net"
	"net/http"
	"net/url"
	"strings"
	"time"

	"tacet/internal/player"
)

const playerSessionCookieName = "tacet_player_session"

func extractPlayerToken(r *http.Request) string {
	if auth := strings.TrimSpace(r.Header.Get("Authorization")); len(auth) > 7 && strings.EqualFold(auth[:7], "Bearer ") {
		return strings.TrimSpace(auth[7:])
	}
	if c, err := r.Cookie(playerSessionCookieName); err == nil {
		return strings.TrimSpace(c.Value)
	}
	return ""
}

const legacyPlayerCookiePath = "/api/v1/players"

func playerCookiePath(zoneID string) string {
	return legacyPlayerCookiePath + "/" + url.PathEscape(zoneID)
}

func clearLegacyPlayerTokenCookie(w http.ResponseWriter, r *http.Request) {
	// Expire any pre-existing broad-path cookie from older builds so browsers
	// never send both a stale /api/v1/players cookie and a scoped zone cookie.
	//nolint:gosec // Secure mirrors r.TLS because tacet commonly runs over plain LAN HTTP.
	http.SetCookie(w, &http.Cookie{
		Name:     playerSessionCookieName,
		Value:    "",
		Path:     legacyPlayerCookiePath,
		HttpOnly: true,
		SameSite: http.SameSiteLaxMode,
		Secure:   r.TLS != nil,
		MaxAge:   -1,
	})
}

func setPlayerTokenCookie(w http.ResponseWriter, r *http.Request, zoneID, token string) {
	clearLegacyPlayerTokenCookie(w, r)
	// Scope cookie per zone so multiple player tabs on the same host never overwrite each other.
	//nolint:gosec // Secure mirrors r.TLS because tacet commonly runs over plain LAN HTTP.
	http.SetCookie(w, &http.Cookie{
		Name:     playerSessionCookieName,
		Value:    token,
		Path:     playerCookiePath(zoneID),
		HttpOnly: true,
		SameSite: http.SameSiteLaxMode,
		Secure:   r.TLS != nil,
		MaxAge:   86400 * 30,
	})
}

func clearPlayerTokenCookie(w http.ResponseWriter, r *http.Request, zoneID string) {
	clearLegacyPlayerTokenCookie(w, r)
	//nolint:gosec // Secure mirrors r.TLS because tacet commonly runs over plain LAN HTTP.
	http.SetCookie(w, &http.Cookie{
		Name:     playerSessionCookieName,
		Value:    "",
		Path:     playerCookiePath(zoneID),
		HttpOnly: true,
		SameSite: http.SameSiteLaxMode,
		Secure:   r.TLS != nil,
		MaxAge:   -1,
	})
}

func remoteHost(r *http.Request) string {
	host, _, err := net.SplitHostPort(strings.TrimSpace(r.RemoteAddr))
	if err != nil || host == "" {
		return "remote"
	}
	return host
}

func (s *Server) handleRegisterPlayer(w http.ResponseWriter, r *http.Request) {
	var req player.RegisterRequest
	if err := decodeJSON(w, r, &req); err != nil {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
		return
	}
	req.Host = remoteHost(r)

	reg, err := s.players.Register(req, extractPlayerToken(r))
	if err != nil {
		writeDomainError(w, err)
		return
	}
	setPlayerTokenCookie(w, r, reg.ZoneID, reg.Token)
	writeJSON(w, http.StatusOK, reg)
}

func (s *Server) handleUpdatePlayer(w http.ResponseWriter, r *http.Request) {
	zoneID := r.PathValue("zoneId")
	var req player.UpdateRequest
	if err := decodeJSON(w, r, &req); err != nil {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
		return
	}
	info, err := s.players.Update(zoneID, extractPlayerToken(r), req)
	if err != nil {
		writeDomainError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"zoneId":   zoneID,
		"renderer": info,
	})
}

func (s *Server) handlePlayerCommands(w http.ResponseWriter, r *http.Request) {
	zoneID := r.PathValue("zoneId")
	rp, err := s.players.Get(zoneID, extractPlayerToken(r))
	if err != nil {
		writeDomainError(w, err)
		return
	}

	flusher, ok := w.(http.Flusher)
	if !ok {
		writeAPIError(w, http.StatusInternalServerError, "STREAMING_UNSUPPORTED", "streaming unsupported", false)
		return
	}

	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	w.Header().Set("Connection", "keep-alive")
	w.Header().Set("X-Accel-Buffering", "no")

	stream := rp.AttachStream()
	s.manager.RegisterRenderer(rp)
	defer func() {
		if s.players.DetachStream(rp, stream.ID) {
			if _, exists := s.manager.Zone(zoneID); exists {
				s.manager.RegisterRenderer(rp)
			}
		}
	}()

	if _, werr := fmt.Fprint(w, "retry: 3000\n\n"); werr != nil {
		return
	}
	connectedPayload := map[string]any{
		"zoneId":     zoneID,
		"streamId":   stream.ID,
		"serverTime": time.Now().UTC(),
	}
	if snap, exists := s.manager.Zone(zoneID); exists {
		connectedPayload["zone"] = snap
	}
	if werr := writePlayerSSE(w, flusher, "", "connected", connectedPayload); werr != nil {
		return
	}

	heartbeat := time.NewTicker(20 * time.Second)
	defer heartbeat.Stop()

	for {
		select {
		case <-r.Context().Done():
			return
		case <-s.manager.Done():
			return
		case <-stream.Replaced:
			_ = writePlayerSSE(w, flusher, "", "replaced", map[string]string{
				"reason": "player session taken over by another connection",
			})
			return
		case cmd := <-stream.Commands:
			if werr := writePlayerSSE(w, flusher, cmd.ID, "command", cmd); werr != nil {
				return
			}
		case <-heartbeat.C:
			if _, werr := fmt.Fprint(w, ": keepalive\n\n"); werr != nil {
				return
			}
			flusher.Flush()
		}
	}
}

func writePlayerSSE(w http.ResponseWriter, flusher http.Flusher, id, event string, payload any) error {
	data, err := json.Marshal(payload)
	if err != nil {
		return err
	}
	if id != "" {
		if _, err := fmt.Fprintf(w, "id: %s\n", id); err != nil {
			return err
		}
	}
	if _, err := fmt.Fprintf(w, "event: %s\ndata: %s\n\n", event, data); err != nil {
		return err
	}
	flusher.Flush()
	return nil
}

func (s *Server) handlePlayerState(w http.ResponseWriter, r *http.Request) {
	zoneID := r.PathValue("zoneId")
	rp, err := s.players.Get(zoneID, extractPlayerToken(r))
	if err != nil {
		writeDomainError(w, err)
		return
	}

	var rep player.StateReport
	if err := decodeJSON(w, r, &rep); err != nil {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
		return
	}
	rp.ReportState(rep)
	w.WriteHeader(http.StatusNoContent)
}

func (s *Server) handleUnregisterPlayer(w http.ResponseWriter, r *http.Request) {
	zoneID := r.PathValue("zoneId")
	if err := s.players.Unregister(zoneID, extractPlayerToken(r)); err != nil {
		writeDomainError(w, err)
		return
	}
	clearPlayerTokenCookie(w, r, zoneID)
	writeJSON(w, http.StatusOK, map[string]any{
		"unregistered": true,
		"zoneId":       zoneID,
	})
}
