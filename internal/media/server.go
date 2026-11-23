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
	"runtime"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"tacet/internal/library"
	"tacet/internal/qobuz"
	"tacet/internal/upnp"
)

const relayReadIdleTimeout = 30 * time.Second

var relayBufPool = sync.Pool{
	New: func() any {
		b := make([]byte, 128*1024)
		return &b
	},
}

// Handler serves local NAS audio files, cover art, and Qobuz HTTP relay
// streams over plain HTTP with DLNA streaming headers and Range support.
type Handler struct {
	catalog       *library.Catalog
	qobuz         *qobuz.Client
	art           *artCache
	transcoder    *TranscoderInfo
	relayHTTP     *http.Client
	prewarmMu     sync.Mutex
	prewarmCancel context.CancelFunc
	prewarmClosed bool
	prewarmWG     sync.WaitGroup
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
				ForceAttemptHTTP2:     true,
				DisableCompression:    true,
				TLSHandshakeTimeout:   10 * time.Second,
				ResponseHeaderTimeout: 15 * time.Second,
				IdleConnTimeout:       90 * time.Second,
				MaxIdleConns:          32,
				MaxIdleConnsPerHost:   8,
				ReadBufferSize:        128 * 1024,
				WriteBufferSize:       64 * 1024,
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

// Close cancels any background cover-art prewarming and waits for workers to exit.
func (h *Handler) Close() {
	h.prewarmMu.Lock()
	h.prewarmClosed = true
	cancel := h.prewarmCancel
	h.prewarmCancel = nil
	h.prewarmMu.Unlock()
	if cancel != nil {
		cancel()
	}
	h.prewarmWG.Wait()
}

// PrewarmArt asynchronously pre-encodes uncached 400px thumbnails (WebP when
// cwebp is available, otherwise JPEG) for all local library covers using low
// bounded concurrency. Calling it again cancels any prior in-flight run.
func (h *Handler) PrewarmArt(ctx context.Context) {
	if h.art == nil || h.art.dir == "" || h.catalog == nil {
		return
	}
	if ctx == nil {
		ctx = context.Background()
	}
	h.prewarmMu.Lock()
	if h.prewarmClosed {
		h.prewarmMu.Unlock()
		return
	}
	runCtx, cancel := context.WithCancel(ctx)
	prev := h.prewarmCancel
	h.prewarmCancel = cancel
	h.prewarmWG.Add(1)
	h.prewarmMu.Unlock()
	if prev != nil {
		prev()
	}

	go func() {
		defer h.prewarmWG.Done()
		defer cancel()
		h.prewarmArtSync(runCtx)
	}()
}

// prewarmArtSync synchronously prewarms uncached 400px local cover art and
// returns the number of newly encoded thumbnails.
func (h *Handler) prewarmArtSync(ctx context.Context) int {
	art := h.art
	if art == nil || art.dir == "" || h.catalog == nil {
		return 0
	}
	hashes, err := h.catalog.LocalArtHashes(ctx)
	if err != nil || len(hashes) == 0 {
		return 0
	}

	format := art.preferredFormat()
	pending := make([]string, 0, len(hashes))
	for _, hash := range hashes {
		if !art.existsDisk(hash, artSizeSmall, format) {
			pending = append(pending, hash)
		}
	}
	if len(pending) == 0 {
		return 0
	}

	start := time.Now()
	workers := min(2, runtime.NumCPU(), len(pending))
	jobs := make(chan string, len(pending))
	for _, hash := range pending {
		jobs <- hash
	}
	close(jobs)

	var built atomic.Int64
	var wg sync.WaitGroup
	wg.Add(workers)
	for range workers {
		go func() {
			defer wg.Done()
			for hash := range jobs {
				if ctx.Err() != nil {
					return
				}
				if _, _, ok := art.getOrBuild(ctx, hash, artSizeSmall, format, func() ([]byte, string, bool) {
					return h.catalog.CoverArtByHash(hash)
				}); ok {
					built.Add(1)
				}
			}
		}()
	}
	wg.Wait()

	if n := int(built.Load()); n > 0 {
		log.Printf("prewarmed %d/%d cover art thumbnails (%s, %dpx) in %s",
			n, len(pending), format.ext, artSizeSmall, time.Since(start).Round(time.Millisecond))
		return n
	}
	return 0
}

func (h *Handler) transcoderInfo() TranscoderInfo {
	if h.transcoder != nil {
		return *h.transcoder
	}
	return DetectTranscoder()
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

	if qParam := r.URL.Query().Get("q"); qParam != "" {
		if _, err := os.Stat(track.FilePath); err != nil {
			http.Error(w, "audio file unavailable", http.StatusNotFound)
			return
		}
		var totalSamples uint64
		if strings.EqualFold(strings.TrimSpace(qParam), "wav") {
			totalSamples = library.ProbeAudio(track.FilePath).TotalSamples
		}
		if spec, transcode := parseTranscodeSpec(qParam, r.URL.Query().Get("container"), track, totalSamples, h.transcoderInfo().HasLibSoxr); transcode {
			h.streamTranscode(w, r, spec, track.FilePath, "", nil, nil)
			return
		}
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
	format := h.art.negotiateFormat(r.Header.Get("Accept"))
	hashed := isArtHash(id)

	w.Header().Add("Vary", "Accept")

	var (
		data     []byte
		mimeType string
		ok       bool
	)
	if hashed {
		expectedETag := fmt.Sprintf(`"art-%s-%d-%s"`, id, bucket, format.ext)
		w.Header().Set("Cache-Control", "public, max-age=31536000, immutable")
		if etagMatches(r.Header.Get("If-None-Match"), expectedETag) {
			w.Header().Set("ETag", expectedETag)
			w.WriteHeader(http.StatusNotModified)
			return
		}
		data, mimeType, ok = h.art.getOrBuild(r.Context(), id, bucket, format, func() ([]byte, string, bool) {
			return h.catalog.CoverArtByHash(id)
		})
		if ok {
			var actualExt string
			switch mimeType {
			case artFormatWebP.mime:
				actualExt = artFormatWebP.ext
			case artFormatJPEG.mime:
				actualExt = artFormatJPEG.ext
			default:
				actualExt = "raw"
			}
			actualETag := fmt.Sprintf(`"art-%s-%d-%s"`, id, bucket, actualExt)
			w.Header().Set("ETag", actualETag)
			if actualETag != expectedETag && etagMatches(r.Header.Get("If-None-Match"), actualETag) {
				w.WriteHeader(http.StatusNotModified)
				return
			}
		}
	} else {
		w.Header().Set("Cache-Control", "public, max-age=86400")
		data, mimeType, ok = h.art.getOrBuild(r.Context(), "", bucket, format, func() ([]byte, string, bool) {
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
// over plain HTTP to the renderer (Relay mode), preserving Range requests.
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

	qTrack := library.Track{
		SampleRate: stream.SampleRate,
		BitDepth:   stream.BitDepth,
		Channels:   2,
		MIME:       stream.MIME,
	}
	spec, transcode := parseTranscodeSpec(r.URL.Query().Get("q"), r.URL.Query().Get("container"), qTrack, 0, h.transcoderInfo().HasLibSoxr)
	if transcode && r.Method == http.MethodHead {
		h.streamTranscode(w, r, spec, "", "", nil, nil)
		return
	}

	upReq, err := http.NewRequestWithContext(r.Context(), r.Method, stream.URL, nil)
	if err != nil {
		http.Error(w, "bad upstream url", http.StatusBadGateway)
		return
	}
	if !transcode {
		if rng := r.Header.Get("Range"); rng != "" {
			upReq.Header.Set("Range", rng)
		}
	}

	upResp, err := h.relayHTTP.Do(upReq)
	if err != nil {
		http.Error(w, fmt.Sprintf("upstream qobuz fetch failed: %v", err), http.StatusBadGateway)
		return
	}
	defer func() { _ = upResp.Body.Close() }()

	if transcode {
		if upResp.StatusCode != http.StatusOK {
			http.Error(w, fmt.Sprintf("upstream qobuz returned HTTP %d", upResp.StatusCode), http.StatusBadGateway)
			return
		}
		inFmt := ""
		lowerMIME := strings.ToLower(stream.MIME)
		switch {
		case strings.Contains(lowerMIME, "flac"):
			inFmt = "flac"
		case strings.Contains(lowerMIME, "mpeg"), strings.Contains(lowerMIME, "mp3"):
			inFmt = "mp3"
		}
		body := newIdleTimeoutReader(upResp.Body, relayReadIdleTimeout)
		defer body.Close()
		h.streamTranscode(w, r, spec, "", inFmt, body, func() { _ = upResp.Body.Close() })
		return
	}

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

	body := newIdleTimeoutReader(upResp.Body, relayReadIdleTimeout)
	defer body.Close()
	bufPtr := relayBufPool.Get().(*[]byte)
	_, copyErr := io.CopyBuffer(w, body, *bufPtr)
	relayBufPool.Put(bufPtr)
	if copyErr != nil && !errors.Is(copyErr, context.Canceled) && r.Context().Err() == nil {
		log.Printf("qobuz relay stream %d interrupted: %v", trackID, copyErr)
	}
}

// idleTimeoutReader closes the underlying ReadCloser if a single Read stalls
// longer than timeout, reusing a single timer to avoid per-Read allocations.
type idleTimeoutReader struct {
	rc      io.ReadCloser
	timeout time.Duration
	timer   *time.Timer
}

func newIdleTimeoutReader(rc io.ReadCloser, timeout time.Duration) *idleTimeoutReader {
	t := time.AfterFunc(timeout, func() { _ = rc.Close() })
	t.Stop()
	return &idleTimeoutReader{rc: rc, timeout: timeout, timer: t}
}

func (r *idleTimeoutReader) Read(p []byte) (int, error) {
	r.timer.Reset(r.timeout)
	n, err := r.rc.Read(p)
	r.timer.Stop()
	return n, err
}

func (r *idleTimeoutReader) Close() {
	r.timer.Stop()
}
