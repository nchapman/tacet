package api

import (
	"encoding/json"
	"fmt"
	"net"
	"net/http"
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

func setPlayerTokenCookie(w http.ResponseWriter, r *http.Request, token string) {
	//nolint:gosec // Secure mirrors r.TLS because tacet commonly runs over plain LAN HTTP.
	http.SetCookie(w, &http.Cookie{
		Name:     playerSessionCookieName,
		Value:    token,
		Path:     "/api/v1/players",
		HttpOnly: true,
		SameSite: http.SameSiteLaxMode,
		Secure:   r.TLS != nil,
		MaxAge:   86400 * 30,
	})
}

func clearPlayerTokenCookie(w http.ResponseWriter, r *http.Request) {
	//nolint:gosec // Secure mirrors r.TLS because tacet commonly runs over plain LAN HTTP.
	http.SetCookie(w, &http.Cookie{
		Name:     playerSessionCookieName,
		Value:    "",
		Path:     "/api/v1/players",
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
	setPlayerTokenCookie(w, r, reg.Token)
	writeJSON(w, http.StatusOK, reg)
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
		writeAPIError(w, http.StatusInternalServerError, "SSE_UNSUPPORTED", "streaming unsupported", false)
		return
	}

	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	w.Header().Set("Connection", "keep-alive")
	w.Header().Set("X-Accel-Buffering", "no")

	stream := rp.AttachStream()
	s.manager.RegisterRenderer(rp)
	defer func() {
		s.players.DetachStream(rp, stream.ID)
		if _, exists := s.manager.Zone(zoneID); exists {
			s.manager.RegisterRenderer(rp)
		}
	}()

	_, _ = fmt.Fprint(w, "retry: 3000\n\n")
	writePlayerSSE(w, flusher, "connected", map[string]any{
		"zoneId":   zoneID,
		"streamId": stream.ID,
	})

	heartbeat := time.NewTicker(20 * time.Second)
	defer heartbeat.Stop()

	for {
		select {
		case <-r.Context().Done():
			return
		case <-stream.Replaced:
			writePlayerSSE(w, flusher, "replaced", map[string]string{
				"reason": "player session taken over by another connection",
			})
			return
		case cmd := <-stream.Commands:
			writePlayerSSE(w, flusher, "command", cmd)
		case <-heartbeat.C:
			_, _ = fmt.Fprint(w, ": keepalive\n\n")
			flusher.Flush()
		}
	}
}

func writePlayerSSE(w http.ResponseWriter, flusher http.Flusher, event string, payload any) {
	data, err := json.Marshal(payload)
	if err != nil {
		return
	}
	_, _ = fmt.Fprintf(w, "event: %s\ndata: %s\n\n", event, data)
	flusher.Flush()
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
	clearPlayerTokenCookie(w, r)
	writeJSON(w, http.StatusOK, map[string]any{
		"unregistered": true,
		"zoneId":       zoneID,
	})
}
