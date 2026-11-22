package media

import (
	"context"
	"errors"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"tacet/internal/library"
	"tacet/internal/qobuz"
	"tacet/internal/upnp"
)

const relayReadIdleTimeout = 30 * time.Second

// Handler serves local NAS audio files, cover art, and Qobuz Quiet Relay
// streams over plain HTTP with DLNA streaming headers and Range support.
type Handler struct {
	catalog   *library.Catalog
	qobuz     *qobuz.Client
	art       *artCache
	relayHTTP *http.Client
}

// NewHandler creates a media HTTP handler.
func NewHandler(cat *library.Catalog, qb *qobuz.Client) *Handler {
	return &Handler{
		catalog: cat,
		qobuz:   qb,
		art:     newArtCache(""),
		relayHTTP: &http.Client{
			// No total request timeout because lossless tracks stream for minutes;
			// transport timeouts and per-read idle watchdog guard stalls.
			Transport: &http.Transport{
				DialContext: (&net.Dialer{
					Timeout:   10 * time.Second,
					KeepAlive: 30 * time.Second,
				}).DialContext,
				ResponseHeaderTimeout: 15 * time.Second,
				IdleConnTimeout:       60 * time.Second,
			},
			CheckRedirect: func(req *http.Request, via []*http.Request) error {
				if len(via) >= 5 {
					return errors.New("too many redirects")
				}
				if req.URL.Scheme != "https" {
					return errors.New("refusing non-HTTPS relay redirect")
				}
				if ip := net.ParseIP(req.URL.Hostname()); ip != nil && (ip.IsPrivate() || ip.IsLoopback() || ip.IsLinkLocalUnicast() || ip.IsUnspecified()) {
					return errors.New("refusing private relay redirect target")
				}
				return nil
			},
		},
	}
}

// SetArtCacheDir configures the on-disk directory used to cache resized JPEG cover art.
func (h *Handler) SetArtCacheDir(dir string) {
	h.art = newArtCache(dir)
}

// Register mounts media routes onto mux.
func (h *Handler) Register(mux *http.ServeMux) {
	mux.HandleFunc("/media/local/", h.serveLocal)
	mux.HandleFunc("/media/art/", h.serveArt)
	mux.HandleFunc("/media/qobuz/", h.serveQobuzRelay)
}

func (h *Handler) serveLocal(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodGet && r.Method != http.MethodHead {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}

	raw := strings.TrimPrefix(r.URL.Path, "/media/local/")
	if raw == "" || strings.Contains(raw, "/") {
		http.NotFound(w, r)
		return
	}
	id := strings.TrimSuffix(raw, filepath.Ext(raw))

	track, ok := h.catalog.Track(id)
	if !ok {
		http.NotFound(w, r)
		return
	}

	f, err := os.Open(track.FilePath)
	if err != nil {
		http.Error(w, "audio file unavailable", http.StatusNotFound)
		return
	}
	defer func() { _ = f.Close() }()

	fi, err := f.Stat()
	if err != nil {
		http.Error(w, "audio file stat failed", http.StatusInternalServerError)
		return
	}

	w.Header().Set("Content-Type", track.MIME)
	w.Header().Set("transferMode.dlna.org", "Streaming")
	w.Header().Set("contentFeatures.dlna.org", upnp.DLNAFeaturesSeekable)
	w.Header().Set("Accept-Ranges", "bytes")

	http.ServeContent(w, r, filepath.Base(track.FilePath), fi.ModTime(), f)
}

func (h *Handler) serveArt(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodGet && r.Method != http.MethodHead {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	id := strings.TrimPrefix(r.URL.Path, "/media/art/")
	if id == "" || strings.Contains(id, "/") {
		http.NotFound(w, r)
		return
	}

	bucket := parseArtBucket(r.URL.Query().Get("w"))
	hashed := isArtHash(id)

	var (
		data     []byte
		mimeType string
		ok       bool
	)
	if hashed {
		etag := fmt.Sprintf(`"art-%s-%d"`, id, bucket)
		w.Header().Set("Cache-Control", "public, max-age=31536000, immutable")
		w.Header().Set("ETag", etag)
		if r.Header.Get("If-None-Match") == etag {
			w.WriteHeader(http.StatusNotModified)
			return
		}
		data, mimeType, ok = h.art.getOrBuild(id, bucket, func() ([]byte, string, bool) {
			return h.catalog.CoverArtByHash(id)
		})
	} else {
		w.Header().Set("Cache-Control", "public, max-age=86400")
		data, mimeType, ok = h.art.getOrBuild("", bucket, func() ([]byte, string, bool) {
			return h.catalog.CoverArt(id)
		})
	}
	if !ok {
		w.Header().Del("Cache-Control")
		w.Header().Del("ETag")
		http.NotFound(w, r)
		return
	}
	w.Header().Set("Content-Type", safeImageMIME(mimeType))
	w.Header().Set("X-Content-Type-Options", "nosniff")
	w.Header().Set("Content-Length", strconv.Itoa(len(data)))
	if r.Method == http.MethodHead {
		return
	}
	_, _ = w.Write(data) //nolint:gosec // binary cover image with strict image/* Content-Type + nosniff
}

func safeImageMIME(raw string) string {
	switch strings.ToLower(strings.TrimSpace(raw)) {
	case "image/jpeg", "image/jpg":
		return "image/jpeg"
	case "image/png":
		return "image/png"
	case "image/webp":
		return "image/webp"
	case "image/gif":
		return "image/gif"
	default:
		return "application/octet-stream"
	}
}

// serveQobuzRelay terminates TLS on the NAS and forwards untouched FLAC bytes
// over plain HTTP to the renderer ("Quiet Relay" mode), preserving Range requests.
func (h *Handler) serveQobuzRelay(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodGet && r.Method != http.MethodHead {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	if h.qobuz == nil {
		http.Error(w, "qobuz not configured", http.StatusServiceUnavailable)
		return
	}

	raw := strings.TrimPrefix(r.URL.Path, "/media/qobuz/")
	raw = strings.TrimSuffix(raw, filepath.Ext(raw))
	trackID, err := strconv.ParseInt(raw, 10, 64)
	if err != nil || trackID <= 0 {
		http.NotFound(w, r)
		return
	}

	stream, err := h.qobuz.StreamURL(r.Context(), trackID)
	if err != nil {
		http.Error(w, fmt.Sprintf("resolve qobuz stream: %v", err), http.StatusBadGateway)
		return
	}

	upReq, err := http.NewRequestWithContext(r.Context(), r.Method, stream.URL, nil)
	if err != nil {
		http.Error(w, "bad upstream url", http.StatusBadGateway)
		return
	}
	if rng := r.Header.Get("Range"); rng != "" {
		upReq.Header.Set("Range", rng)
	}

	upResp, err := h.relayHTTP.Do(upReq)
	if err != nil {
		http.Error(w, fmt.Sprintf("upstream qobuz fetch failed: %v", err), http.StatusBadGateway)
		return
	}
	defer func() { _ = upResp.Body.Close() }()

	if upResp.StatusCode == http.StatusRequestedRangeNotSatisfiable {
		if cr := upResp.Header.Get("Content-Range"); cr != "" {
			w.Header().Set("Content-Range", cr)
		}
		http.Error(w, "range not satisfiable", http.StatusRequestedRangeNotSatisfiable)
		return
	}
	if upResp.StatusCode != http.StatusOK && upResp.StatusCode != http.StatusPartialContent {
		http.Error(w, fmt.Sprintf("upstream qobuz returned HTTP %d", upResp.StatusCode), http.StatusBadGateway)
		return
	}

	ct := stream.MIME
	if ct == "" {
		ct = upResp.Header.Get("Content-Type")
	}
	if ct == "" || ct == "audio/x-flac" {
		ct = "audio/flac"
	}
	w.Header().Set("Content-Type", ct)
	w.Header().Set("transferMode.dlna.org", "Streaming")
	w.Header().Set("contentFeatures.dlna.org", upnp.DLNAFeaturesSeekable)
	if ar := upResp.Header.Get("Accept-Ranges"); ar != "" {
		w.Header().Set("Accept-Ranges", ar)
	} else {
		w.Header().Set("Accept-Ranges", "bytes")
	}
	if cl := upResp.Header.Get("Content-Length"); cl != "" {
		w.Header().Set("Content-Length", cl)
	}
	if cr := upResp.Header.Get("Content-Range"); cr != "" {
		w.Header().Set("Content-Range", cr)
	}

	w.WriteHeader(upResp.StatusCode)
	if r.Method == http.MethodHead {
		return
	}

	body := &idleTimeoutReader{rc: upResp.Body, timeout: relayReadIdleTimeout}
	if _, err := io.Copy(w, body); err != nil && !errors.Is(err, context.Canceled) && r.Context().Err() == nil {
		log.Printf("qobuz relay stream %d interrupted: %v", trackID, err)
	}
}

// idleTimeoutReader closes the underlying ReadCloser if a single Read stalls
// longer than timeout, preventing leaked relay goroutines on dead upstreams.
type idleTimeoutReader struct {
	rc      io.ReadCloser
	timeout time.Duration
}

func (r *idleTimeoutReader) Read(p []byte) (int, error) {
	timer := time.AfterFunc(r.timeout, func() {
		_ = r.rc.Close()
	})
	defer timer.Stop()
	return r.rc.Read(p)
}
