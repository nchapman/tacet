package cast

import (
	"context"
	"fmt"
	"math"
	"net"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/vishen/go-chromecast/application"
	castproto "github.com/vishen/go-chromecast/cast"

	"tacet/internal/upnp"
)

// castSinkProtocols lists the audio formats natively supported by Google Cast
// Default Media Receiver (CC1AD845): FLAC (up to 24/96), WAV, Opus (Ogg/WebM),
// AAC, MP4/M4A, and MP3. DSD is intentionally omitted so tacet auto-decodes
// 1-bit DSD to 24-bit/88.2kHz WAV.
var castSinkProtocols = []string{
	"audio/flac",
	"audio/wav",
	"audio/mpeg",
	"audio/aac",
	"audio/mp4",
	`audio/ogg; codecs="opus"`,
	`audio/webm; codecs="opus"`,
}

type castMusicMetadata struct {
	MetadataType int               `json:"metadataType"`
	Title        string            `json:"title,omitempty"`
	Artist       string            `json:"artist,omitempty"`
	AlbumName    string            `json:"albumName,omitempty"`
	TrackNumber  int               `json:"trackNumber,omitempty"`
	Images       []castproto.Image `json:"images,omitempty"`
}

type castMediaInfo struct {
	ContentID   string            `json:"contentId"`
	StreamType  string            `json:"streamType"`
	ContentType string            `json:"contentType"`
	Duration    float64           `json:"duration,omitempty"`
	Metadata    castMusicMetadata `json:"metadata"`
}

type castLoadMediaPayload struct {
	castproto.PayloadHeader
	Media       castMediaInfo `json:"media"`
	CurrentTime int           `json:"currentTime"`
	Autoplay    bool          `json:"autoplay"`
}

const (
	castMediaNamespace = "urn:x-cast:com.google.cast.media"
	castDefaultSender  = "sender-0"
	castPreloadSec     = 10
)

type castQueueItem struct {
	Media       castMediaInfo `json:"media"`
	Autoplay    bool          `json:"autoplay"`
	PreloadTime float64       `json:"preloadTime,omitempty"`
}

type castQueueData struct {
	Items      []castQueueItem `json:"items"`
	StartIndex int             `json:"startIndex"`
	RepeatMode string          `json:"repeatMode"`
}

type castQueueLoadPayload struct {
	castproto.PayloadHeader
	Media       castMediaInfo `json:"media"`
	CurrentTime float32       `json:"currentTime"`
	Autoplay    bool          `json:"autoplay"`
	QueueData   castQueueData `json:"queueData"`
}

type castQueueInsertPayload struct {
	castproto.PayloadHeader
	MediaSessionID int             `json:"mediaSessionId"`
	Items          []castQueueItem `json:"items"`
}

type castQueueUpdatePayload struct {
	castproto.PayloadHeader
	MediaSessionID int    `json:"mediaSessionId"`
	Jump           int    `json:"jump,omitempty"`
	RepeatMode     string `json:"repeatMode,omitempty"`
}

// metadataConn wraps a go-chromecast Conn so outgoing LOAD commands include
// Google Cast MusicTrackMediaMetadata (metadataType: 3) — or a full QUEUE_LOAD
// payload — for Nest Hubs, Pixel Tablets, and TVs without modifying
// go-chromecast or leaking Cast concerns outside internal/cast.
type metadataConn struct {
	castproto.Conn
	mu           sync.Mutex
	pendingItem  *upnp.MediaItem
	pendingQueue *castQueueLoadPayload
	reqSeq       atomic.Int64
}

func (c *metadataConn) nextReqID() int {
	return int(c.reqSeq.Add(1) + (1 << 28))
}

func (c *metadataConn) setPendingItem(item *upnp.MediaItem) {
	c.mu.Lock()
	c.pendingQueue = nil
	if item == nil {
		c.pendingItem = nil
	} else {
		cp := *item
		c.pendingItem = &cp
	}
	c.mu.Unlock()
}

func (c *metadataConn) setPendingQueue(q *castQueueLoadPayload) {
	c.mu.Lock()
	c.pendingItem = nil
	c.pendingQueue = q
	c.mu.Unlock()
}

func (c *metadataConn) Send(requestID int, payload castproto.Payload, sourceID, destinationID, namespace string) error {
	if loadCmd, ok := payload.(*castproto.LoadMediaCommand); ok {
		c.mu.Lock()
		pq := c.pendingQueue
		c.pendingQueue = nil
		item := c.pendingItem
		c.pendingItem = nil
		c.mu.Unlock()
		if pq != nil {
			pq.SetRequestId(requestID)
			payload = pq
		} else if item != nil {
			lp := buildCastLoadPayload(loadCmd, *item)
			lp.SetRequestId(requestID)
			payload = lp
		}
	}
	payload.SetRequestId(requestID)
	return c.Conn.Send(requestID, payload, sourceID, destinationID, namespace)
}

func buildCastMediaInfo(item upnp.MediaItem) castMediaInfo {
	mime := item.MIME
	if mime == "" {
		mime = "audio/flac"
	}
	meta := castMusicMetadata{
		MetadataType: 3, // Google Cast MUSIC_TRACK
		Title:        strings.TrimSpace(item.Title),
		Artist:       strings.TrimSpace(item.Artist),
		AlbumName:    strings.TrimSpace(item.Album),
		TrackNumber:  item.TrackNumber,
	}
	if art := strings.TrimSpace(item.AlbumArtURL); art != "" {
		meta.Images = []castproto.Image{{URL: art}}
	}
	return castMediaInfo{
		ContentID:   item.URL,
		StreamType:  "BUFFERED",
		ContentType: mime,
		Duration:    item.Duration.Seconds(),
		Metadata:    meta,
	}
}

func buildCastQueueLoadPayload(items []upnp.MediaItem, startIndex int) *castQueueLoadPayload {
	qItems := make([]castQueueItem, len(items))
	for i, it := range items {
		qItems[i] = castQueueItem{
			Media:       buildCastMediaInfo(it),
			Autoplay:    true,
			PreloadTime: castPreloadSec,
		}
	}
	return &castQueueLoadPayload{
		PayloadHeader: castproto.LoadHeader,
		Media:         buildCastMediaInfo(items[startIndex]),
		CurrentTime:   0,
		Autoplay:      true,
		QueueData: castQueueData{
			Items:      qItems,
			StartIndex: startIndex,
			RepeatMode: "REPEAT_OFF",
		},
	}
}

func buildCastLoadPayload(cmd *castproto.LoadMediaCommand, item upnp.MediaItem) *castLoadMediaPayload {
	mi := buildCastMediaInfo(item)
	mi.ContentID = cmd.Media.ContentId
	mi.StreamType = cmd.Media.StreamType
	mi.ContentType = cmd.Media.ContentType
	return &castLoadMediaPayload{
		PayloadHeader: cmd.PayloadHeader,
		CurrentTime:   cmd.CurrentTime,
		Autoplay:      cmd.Autoplay,
		Media:         mi,
	}
}

// Renderer wraps a Google Cast device (or Cast speaker group) as a standard
// upnp.Controller + upnp.QueueController so queue.Manager treats it identically
// to Linkplay, UPnP, and browser zones.
type Renderer struct {
	mu         sync.Mutex
	info       upnp.RendererInfo
	host       string
	port       int
	localIP    string
	app        *application.Application
	metaConn   *metadataConn
	connected  bool
	lastURI    string
	queueItems []upnp.MediaItem
	curIdx     int
	queueDirty bool
	repeatMode string
}

// NewRenderer constructs a lazy-connecting Cast Renderer from discovery metadata.
// It does not open a TLS session or launch the Default Media Receiver until
// playback or volume control is requested, avoiding waking idle TVs/displays.
func NewRenderer(uuid, name, model, host string, port int) *Renderer {
	if port <= 0 {
		port = 8009
	}
	id := "cast:" + strings.TrimSpace(uuid)
	if name == "" {
		name = host
	}
	return &Renderer{
		info: upnp.RendererInfo{
			ID:              id,
			Kind:            "cast",
			Name:            name,
			Manufacturer:    "Google Cast",
			Model:           strings.TrimSpace(model),
			Location:        fmt.Sprintf("cast://%s:%d", host, port),
			Host:            host,
			HasVolume:       true,
			SupportsSetNext: false,
			SupportsSeek:    true,
			SupportsEvents:  false,
			SupportsQueue:   true,
			SinkProtocols:   append([]string(nil), castSinkProtocols...),
		},
		host:    host,
		port:    port,
		localIP: detectRouteIP(host, port),
	}
}

// Info returns the renderer capability descriptor.
func (r *Renderer) Info() upnp.RendererInfo {
	return r.info
}

// LocalIP returns the local LAN IP address routed toward this Cast device.
func (r *Renderer) LocalIP() string {
	return r.localIP
}

// Close closes any open Cast TLS connection without stopping other apps.
func (r *Renderer) Close() error {
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.app != nil {
		err := r.app.Close(false)
		r.app = nil
		r.connected = false
		return err
	}
	return nil
}

func (r *Renderer) ensureConnectedLocked() (*application.Application, error) {
	if r.connected && r.app != nil {
		return r.app, nil
	}
	mc := &metadataConn{Conn: castproto.NewConnection()}
	app := application.NewApplication(
		application.WithCacheDisabled(true),
		application.WithConnectionRetries(2),
	)
	app.SetConn(mc)
	if err := app.Start(r.host, r.port); err != nil {
		return nil, fmt.Errorf("connect cast %s (%s:%d): %w", r.info.Name, r.host, r.port, err)
	}
	r.app = app
	r.metaConn = mc
	r.connected = true
	return app, nil
}

func (r *Renderer) resetLocked() {
	if r.app != nil {
		_ = r.app.Close(false)
		r.app = nil
	}
	r.metaConn = nil
	r.connected = false
}

// runWithCtx bounds blocking go-chromecast calls by ctx. If ctx expires while
// the underlying Cast socket is hung, runWithCtx returns ctx.Err() promptly so
// callers (queue.Manager) never stall indefinitely.
func runWithCtx(ctx context.Context, fn func() error) error {
	if ctx == nil {
		return fn()
	}
	if err := ctx.Err(); err != nil {
		return err
	}
	done := make(chan error, 1)
	go func() {
		done <- fn()
	}()
	select {
	case err := <-done:
		return err
	case <-ctx.Done():
		return ctx.Err()
	}
}

// SetURI loads item onto the Cast device via Default Media Receiver (CC1AD845).
func (r *Renderer) SetURI(ctx context.Context, item upnp.MediaItem) error {
	return runWithCtx(ctx, func() error {
		r.mu.Lock()
		defer r.mu.Unlock()

		app, err := r.ensureConnectedLocked()
		if err != nil {
			return err
		}
		mime := item.MIME
		if mime == "" {
			mime = "audio/flac"
		}
		// detach=true, forceDetach=true returns immediately once LOAD is accepted.
		r.metaConn.setPendingItem(&item)
		if err := app.Load(item.URL, 0, mime, false, true, true); err != nil {
			// Retry once on a fresh Cast TLS session in case the previous socket went stale.
			r.resetLocked()
			app, connErr := r.ensureConnectedLocked()
			if connErr != nil {
				return fmt.Errorf("cast load %s: %w", r.info.Name, err)
			}
			r.metaConn.setPendingItem(&item)
			if retryErr := app.Load(item.URL, 0, mime, false, true, true); retryErr != nil {
				r.metaConn.setPendingItem(nil)
				return fmt.Errorf("cast load %s: %w", r.info.Name, retryErr)
			}
		}
		r.lastURI = item.URL
		return nil
	})
}

// SetNextURI is unused when QueueController is implemented.
func (r *Renderer) SetNextURI(_ context.Context, _ upnp.MediaItem) error {
	return fmt.Errorf("SetNextURI unsupported on Google Cast renderer")
}

// ReplaceQueue loads items into the Cast device's native media queue via
// QUEUE_LOAD so Nest Hubs, Pixel Tablets, and Google Home show Next/Previous
// controls and preload upcoming tracks.
func (r *Renderer) ReplaceQueue(ctx context.Context, items []upnp.MediaItem, startIndex int) error {
	if len(items) == 0 {
		return fmt.Errorf("cast ReplaceQueue: empty items")
	}
	if startIndex < 0 || startIndex >= len(items) {
		startIndex = 0
	}
	return runWithCtx(ctx, func() error {
		r.mu.Lock()
		defer r.mu.Unlock()
		return r.replaceQueueLocked(items, startIndex)
	})
}

func (r *Renderer) replaceQueueLocked(items []upnp.MediaItem, startIndex int) error {
	app, err := r.ensureConnectedLocked()
	if err != nil {
		return err
	}
	startItem := items[startIndex]
	mime := startItem.MIME
	if mime == "" {
		mime = "audio/flac"
	}
	payload := buildCastQueueLoadPayload(items, startIndex)
	if r.repeatMode != "" {
		payload.QueueData.RepeatMode = r.repeatMode
	}
	r.metaConn.setPendingQueue(payload)
	if err := app.Load(startItem.URL, 0, mime, false, true, true); err != nil {
		r.resetLocked()
		app, connErr := r.ensureConnectedLocked()
		if connErr != nil {
			return fmt.Errorf("cast queue load %s: %w", r.info.Name, err)
		}
		r.metaConn.setPendingQueue(payload)
		if retryErr := app.Load(startItem.URL, 0, mime, false, true, true); retryErr != nil {
			r.metaConn.setPendingQueue(nil)
			return fmt.Errorf("cast queue load %s: %w", r.info.Name, retryErr)
		}
	}
	r.queueItems = append([]upnp.MediaItem(nil), items...)
	r.curIdx = startIndex
	r.lastURI = startItem.URL
	r.queueDirty = false
	return nil
}

// AppendQueue appends items to the active Cast media queue via QUEUE_INSERT.
func (r *Renderer) AppendQueue(ctx context.Context, items []upnp.MediaItem) error {
	if len(items) == 0 {
		return nil
	}
	return runWithCtx(ctx, func() error {
		r.mu.Lock()
		defer r.mu.Unlock()
		r.queueItems = append(r.queueItems, items...)
		if !r.connected || r.app == nil || r.metaConn == nil {
			r.queueDirty = true
			return nil
		}
		castApp, media, _ := r.app.Status()
		if castApp == nil || castApp.TransportId == "" || media == nil || media.MediaSessionId == 0 {
			r.queueDirty = true
			return nil
		}
		qItems := make([]castQueueItem, len(items))
		for i, it := range items {
			qItems[i] = castQueueItem{
				Media:       buildCastMediaInfo(it),
				Autoplay:    true,
				PreloadTime: castPreloadSec,
			}
		}
		payload := &castQueueInsertPayload{
			PayloadHeader:  castproto.PayloadHeader{Type: "QUEUE_INSERT"},
			MediaSessionID: media.MediaSessionId,
			Items:          qItems,
		}
		if err := r.metaConn.Send(r.metaConn.nextReqID(), payload, castDefaultSender, castApp.TransportId, castMediaNamespace); err != nil {
			r.queueDirty = true
			return err
		}
		return nil
	})
}

// RemoveQueueRange removes 0-based inclusive indices [start..end] from the tracked
// queue and marks the Cast device queue dirty so the next PlayIndex resyncs it.
func (r *Renderer) RemoveQueueRange(_ context.Context, start, end int) error {
	r.mu.Lock()
	defer r.mu.Unlock()
	if start < 0 || end < start || start >= len(r.queueItems) {
		return nil
	}
	if end >= len(r.queueItems) {
		end = len(r.queueItems) - 1
	}
	r.queueItems = append(r.queueItems[:start], r.queueItems[end+1:]...)
	if r.curIdx > end {
		r.curIdx -= (end - start + 1)
	} else if r.curIdx >= len(r.queueItems) {
		r.curIdx = len(r.queueItems) - 1
	}
	r.queueDirty = true
	return nil
}

// MoveQueueItem reorders a tracked queue item from 0-based from to 0-based to,
// preserving curIdx and marking the Cast device queue dirty for next PlayIndex resync.
func (r *Renderer) MoveQueueItem(_ context.Context, from, to int) error {
	r.mu.Lock()
	defer r.mu.Unlock()
	if from < 0 || to < 0 || from >= len(r.queueItems) || to >= len(r.queueItems) || from == to {
		return nil
	}
	moved := r.queueItems[from]
	without := append(append([]upnp.MediaItem(nil), r.queueItems[:from]...), r.queueItems[from+1:]...)
	reordered := make([]upnp.MediaItem, 0, len(r.queueItems))
	reordered = append(reordered, without[:to]...)
	reordered = append(reordered, moved)
	reordered = append(reordered, without[to:]...)
	r.queueItems = reordered
	switch {
	case r.curIdx == from:
		r.curIdx = to
	case from < r.curIdx && to >= r.curIdx:
		r.curIdx--
	case from > r.curIdx && to <= r.curIdx:
		r.curIdx++
	}
	r.queueDirty = true
	return nil
}

// PlayIndex jumps playback to 0-based index in the Cast media queue.
func (r *Renderer) PlayIndex(ctx context.Context, index int) error {
	return runWithCtx(ctx, func() error {
		r.mu.Lock()
		defer r.mu.Unlock()
		if index < 0 || index >= len(r.queueItems) {
			return fmt.Errorf("cast PlayIndex %d out of range [0,%d)", index, len(r.queueItems))
		}
		if !r.queueDirty && r.connected && r.app != nil && r.metaConn != nil {
			castApp, media, _ := r.app.Status()
			if castApp != nil && castApp.TransportId != "" && media != nil && media.MediaSessionId != 0 && r.curIdx >= 0 {
				jump := index - r.curIdx
				if jump != 0 {
					payload := &castQueueUpdatePayload{
						PayloadHeader:  castproto.QueueUpdateHeader,
						MediaSessionID: media.MediaSessionId,
						Jump:           jump,
					}
					if err := r.metaConn.Send(r.metaConn.nextReqID(), payload, castDefaultSender, castApp.TransportId, castMediaNamespace); err == nil {
						r.curIdx = index
						r.lastURI = r.queueItems[index].URL
						return nil
					}
				}
			}
		}
		return r.replaceQueueLocked(r.queueItems, index)
	})
}

// Next advances 1 track in the active Cast media queue.
func (r *Renderer) Next(ctx context.Context) error {
	return r.queueJump(ctx, 1)
}

// Previous steps back 1 track in the active Cast media queue.
func (r *Renderer) Previous(ctx context.Context) error {
	return r.queueJump(ctx, -1)
}

func (r *Renderer) queueJump(ctx context.Context, delta int) error {
	return runWithCtx(ctx, func() error {
		r.mu.Lock()
		defer r.mu.Unlock()
		if !r.connected || r.app == nil || r.metaConn == nil {
			return fmt.Errorf("cast renderer %s is not connected", r.info.Name)
		}
		castApp, media, _ := r.app.Status()
		if castApp == nil || castApp.TransportId == "" || media == nil || media.MediaSessionId == 0 {
			return fmt.Errorf("cast renderer %s has no active media session", r.info.Name)
		}
		payload := &castQueueUpdatePayload{
			PayloadHeader:  castproto.QueueUpdateHeader,
			MediaSessionID: media.MediaSessionId,
			Jump:           delta,
		}
		if err := r.metaConn.Send(r.metaConn.nextReqID(), payload, castDefaultSender, castApp.TransportId, castMediaNamespace); err != nil {
			return err
		}
		nextIdx := r.curIdx + delta
		if nextIdx >= 0 && nextIdx < len(r.queueItems) {
			r.curIdx = nextIdx
			r.lastURI = r.queueItems[nextIdx].URL
		}
		return nil
	})
}

// SetLoopMode updates the Cast media queue repeatMode.
func (r *Renderer) SetLoopMode(ctx context.Context, repeat string, _ bool) error {
	mode := "REPEAT_OFF"
	switch strings.ToLower(strings.TrimSpace(repeat)) {
	case "all":
		mode = "REPEAT_ALL"
	case "one":
		mode = "REPEAT_SINGLE"
	}
	return runWithCtx(ctx, func() error {
		r.mu.Lock()
		defer r.mu.Unlock()
		r.repeatMode = mode
		if !r.connected || r.app == nil || r.metaConn == nil {
			return nil
		}
		castApp, media, _ := r.app.Status()
		if castApp == nil || castApp.TransportId == "" || media == nil || media.MediaSessionId == 0 {
			return nil
		}
		payload := &castQueueUpdatePayload{
			PayloadHeader:  castproto.QueueUpdateHeader,
			MediaSessionID: media.MediaSessionId,
			RepeatMode:     mode,
		}
		return r.metaConn.Send(r.metaConn.nextReqID(), payload, castDefaultSender, castApp.TransportId, castMediaNamespace)
	})
}

// Play resumes paused playback. Because Cast's LOAD command starts playback
// automatically, Play is a no-op when media is still initializing (media == nil),
// buffering, or already playing; it only sends Unpause when explicitly PAUSED.
func (r *Renderer) Play(ctx context.Context) error {
	return runWithCtx(ctx, func() error {
		r.mu.Lock()
		defer r.mu.Unlock()
		if !r.connected || r.app == nil {
			return nil
		}
		_, media, _ := r.app.Status()
		if media == nil {
			return nil
		}
		if strings.EqualFold(strings.TrimSpace(media.PlayerState), "PAUSED") {
			return r.app.Unpause()
		}
		return nil
	})
}

// Pause pauses current Cast media playback.
func (r *Renderer) Pause(ctx context.Context) error {
	return runWithCtx(ctx, func() error {
		r.mu.Lock()
		defer r.mu.Unlock()
		if !r.connected || r.app == nil {
			return nil
		}
		if _, media, _ := r.app.Status(); media == nil {
			return nil
		}
		return r.app.Pause()
	})
}

// Stop stops current Cast media playback.
func (r *Renderer) Stop(ctx context.Context) error {
	return runWithCtx(ctx, func() error {
		r.mu.Lock()
		defer r.mu.Unlock()
		if !r.connected || r.app == nil {
			return nil
		}
		if _, media, _ := r.app.Status(); media == nil {
			return nil
		}
		return r.app.StopMedia()
	})
}

// Seek seeks to pos within the active Cast media session.
func (r *Renderer) Seek(ctx context.Context, pos time.Duration) error {
	return runWithCtx(ctx, func() error {
		r.mu.Lock()
		defer r.mu.Unlock()
		if !r.connected || r.app == nil {
			return fmt.Errorf("cast renderer %s is not connected", r.info.Name)
		}
		if pos < 0 {
			pos = 0
		}
		return r.app.SeekToTime(float32(pos.Seconds()))
	})
}

// Status queries current playback and volume state from the Cast device.
// When idle (no active Cast connection), returns STOPPED without opening a socket.
func (r *Renderer) Status(ctx context.Context) (upnp.TransportStatus, error) {
	var out upnp.TransportStatus
	err := runWithCtx(ctx, func() error {
		if !r.mu.TryLock() {
			return fmt.Errorf("cast renderer %s busy", r.info.Name)
		}
		defer r.mu.Unlock()

		if !r.connected || r.app == nil {
			out = upnp.TransportStatus{
				State:     "STOPPED",
				HasVolume: true,
			}
			return nil
		}

		if err := r.app.Update(); err != nil {
			r.resetLocked()
			out = upnp.TransportStatus{
				State:     "STOPPED",
				HasVolume: true,
			}
			return err
		}

		castApp, media, vol := r.app.Status()
		// Ignore media from external Cast apps (Spotify, YouTube, etc.) so they
		// never hijack tacet's playback state or record false plays.
		if castApp == nil || castApp.AppId != "CC1AD845" {
			out = mapCastStatus(nil, vol, "")
			return nil
		}
		out = mapCastStatus(media, vol, r.lastURI)
		if media != nil && media.Media.ContentId != "" && len(r.queueItems) > 0 {
			if idx := r.matchQueueIndexLocked(media.Media.ContentId); idx >= 0 {
				out.QueueIndex = idx
				out.HasQueueIndex = true
				r.curIdx = idx
				r.lastURI = r.queueItems[idx].URL
			}
		}
		return nil
	})
	return out, err
}

// matchQueueIndexLocked matches contentID against r.queueItems preferring the
// current index and immediate neighbors so duplicate tracks don't snap backwards.
func (r *Renderer) matchQueueIndexLocked(contentID string) int {
	n := len(r.queueItems)
	if n == 0 {
		return -1
	}
	candidates := []int{r.curIdx, r.curIdx + 1, r.curIdx - 1}
	if r.curIdx == n-1 && n > 1 {
		candidates = append(candidates, 0)
	}
	for _, c := range candidates {
		if c >= 0 && c < n && r.queueItems[c].URL == contentID {
			return c
		}
	}
	for i, it := range r.queueItems {
		if it.URL == contentID {
			return i
		}
	}
	return -1
}

// SetVolume sets the Cast device's volume (0..100).
func (r *Renderer) SetVolume(ctx context.Context, vol int) error {
	return runWithCtx(ctx, func() error {
		r.mu.Lock()
		defer r.mu.Unlock()
		if vol < 0 {
			vol = 0
		}
		if vol > 100 {
			vol = 100
		}
		app, err := r.ensureConnectedLocked()
		if err != nil {
			return err
		}
		return app.SetVolume(float32(vol) / 100.0)
	})
}

// SetMute sets the Cast device's mute state.
func (r *Renderer) SetMute(ctx context.Context, muted bool) error {
	return runWithCtx(ctx, func() error {
		r.mu.Lock()
		defer r.mu.Unlock()
		app, err := r.ensureConnectedLocked()
		if err != nil {
			return err
		}
		return app.SetMuted(muted)
	})
}

func mapCastStatus(media *castproto.Media, vol *castproto.Volume, fallbackURI string) upnp.TransportStatus {
	st := upnp.TransportStatus{
		State:     "STOPPED",
		HasVolume: true,
	}
	if vol != nil {
		v := int(math.Round(float64(vol.Level) * 100))
		if v < 0 {
			v = 0
		}
		if v > 100 {
			v = 100
		}
		st.Volume = v
		st.HasVolValue = true
		st.Muted = vol.Muted
		st.HasMuteVal = true
	}
	if media != nil {
		switch strings.ToUpper(strings.TrimSpace(media.PlayerState)) {
		case "PLAYING":
			st.State = "PLAYING"
		case "BUFFERING":
			st.State = "TRANSITIONING"
		case "PAUSED":
			st.State = "PAUSED_PLAYBACK"
		default:
			st.State = "STOPPED"
		}
		if media.CurrentTime >= 0 {
			st.Position = time.Duration(float64(media.CurrentTime) * float64(time.Second))
			st.HasPosition = true
		}
		if media.Media.Duration > 0 {
			st.Duration = time.Duration(float64(media.Media.Duration) * float64(time.Second))
		}
		if media.Media.ContentId != "" {
			st.TrackURI = media.Media.ContentId
		} else {
			st.TrackURI = fallbackURI
		}
	}
	return st
}

func detectRouteIP(host string, port int) string {
	d := &net.Dialer{Timeout: 500 * time.Millisecond}
	conn, err := d.DialContext(context.Background(), "udp4", net.JoinHostPort(host, fmt.Sprintf("%d", port)))
	if err != nil {
		return "127.0.0.1"
	}
	defer func() { _ = conn.Close() }()
	if addr, ok := conn.LocalAddr().(*net.UDPAddr); ok && addr.IP != nil {
		return addr.IP.String()
	}
	return "127.0.0.1"
}
