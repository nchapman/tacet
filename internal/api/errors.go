package api

import (
	"errors"
	"log"
	"net/http"

	"tacet/internal/library"
	"tacet/internal/player"
	"tacet/internal/qobuz"
	"tacet/internal/queue"
	"tacet/internal/upnp"
)

// ErrorBody is the structured error object returned in all JSON API error responses.
type ErrorBody struct {
	Code      string         `json:"code"`
	Message   string         `json:"message"`
	Retryable bool           `json:"retryable"`
	Details   map[string]any `json:"details,omitempty"`
}

// ErrorEnvelope wraps ErrorBody under the top-level "error" key.
type ErrorEnvelope struct {
	Error ErrorBody `json:"error"`
}

// Domain errors scoped to HTTP API resolution.
var (
	ErrTrackNotFound    = errors.New("track not found")
	ErrAlbumNotFound    = errors.New("album not found")
	ErrQobuzUnavailable = errors.New("qobuz is not configured")
)

func writeAPIError(w http.ResponseWriter, status int, code, message string, retryable bool) {
	writeJSON(w, status, ErrorEnvelope{
		Error: ErrorBody{
			Code:      code,
			Message:   message,
			Retryable: retryable,
		},
	})
}

func writeDomainError(w http.ResponseWriter, err error) {
	if err == nil {
		return
	}
	switch {
	case errors.Is(err, queue.ErrZoneNotFound):
		writeAPIError(w, http.StatusNotFound, "ZONE_NOT_FOUND", err.Error(), false)
	case errors.Is(err, queue.ErrQueueItemNotFound):
		writeAPIError(w, http.StatusNotFound, "QUEUE_ITEM_NOT_FOUND", err.Error(), false)
	case errors.Is(err, ErrTrackNotFound):
		writeAPIError(w, http.StatusNotFound, "TRACK_NOT_FOUND", err.Error(), false)
	case errors.Is(err, ErrAlbumNotFound), errors.Is(err, library.ErrNotFound):
		writeAPIError(w, http.StatusNotFound, "ALBUM_NOT_FOUND", err.Error(), false)
	case errors.Is(err, library.ErrProviderNotFound):
		writeAPIError(w, http.StatusNotFound, "PROVIDER_NOT_FOUND", err.Error(), false)
	case errors.Is(err, library.ErrProviderOffline):
		writeAPIError(w, http.StatusUnauthorized, "PROVIDER_OFFLINE", err.Error(), false)
	case errors.Is(err, qobuz.ErrNotFound):
		writeAPIError(w, http.StatusNotFound, "NOT_FOUND", err.Error(), false)
	case errors.Is(err, queue.ErrQueueEmpty):
		writeAPIError(w, http.StatusConflict, "QUEUE_EMPTY", err.Error(), false)
	case errors.Is(err, queue.ErrEndOfQueue):
		writeAPIError(w, http.StatusConflict, "END_OF_QUEUE", err.Error(), false)
	case errors.Is(err, queue.ErrInvalidArgument), errors.Is(err, library.ErrInvalidArgument), errors.Is(err, qobuz.ErrInvalidQuality), errors.Is(err, upnp.ErrInvalidRendererURL):
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
	case errors.Is(err, queue.ErrCapabilityUnsupported):
		writeAPIError(w, http.StatusUnprocessableEntity, "CAPABILITY_UNSUPPORTED", err.Error(), false)
	case errors.Is(err, qobuz.ErrUnauthenticated):
		writeAPIError(w, http.StatusUnauthorized, "QOBUZ_UNAUTHENTICATED", err.Error(), false)
	case errors.Is(err, ErrQobuzUnavailable):
		writeAPIError(w, http.StatusServiceUnavailable, "QOBUZ_UNAVAILABLE", err.Error(), false)
	case errors.Is(err, player.ErrPlaybackBlocked):
		writeAPIError(w, http.StatusConflict, "PLAYBACK_BLOCKED", err.Error(), false)
	case errors.Is(err, player.ErrPlayerNotFound):
		writeAPIError(w, http.StatusNotFound, "PLAYER_NOT_FOUND", err.Error(), false)
	case errors.Is(err, player.ErrUnauthorized):
		writeAPIError(w, http.StatusUnauthorized, "PLAYER_UNAUTHORIZED", err.Error(), false)
	case errors.Is(err, player.ErrInvalidPlayer):
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
	case errors.Is(err, player.ErrCommandTimeout):
		writeAPIError(w, http.StatusGatewayTimeout, "PLAYER_TIMEOUT", "player endpoint timed out acknowledging command", true)
	case errors.Is(err, player.ErrPlayerDisconnected):
		writeAPIError(w, http.StatusBadGateway, "RENDERER_UNREACHABLE", "player endpoint is not connected", true)
	case errors.Is(err, queue.ErrRendererUnreachable):
		log.Printf("tacet: renderer unreachable: %v", err)
		writeAPIError(w, http.StatusBadGateway, "RENDERER_UNREACHABLE", "renderer is unreachable or failed to respond", true)
	default:
		log.Printf("tacet: upstream error: %v", err)
		writeAPIError(w, http.StatusBadGateway, "UPSTREAM_ERROR", "upstream request failed", true)
	}
}
