package cast

import (
	"context"
	"fmt"
	"math"
	"net"
	"strings"
	"sync"
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

// metadataConn wraps a go-chromecast Conn so outgoing LOAD commands include
// Google Cast MusicTrackMediaMetadata (metadataType: 3) for Nest Hubs, Pixel
// Tablets, and TVs without modifying go-chromecast or leaking Cast concerns
// outside internal/cast.
type metadataConn struct {
	castproto.Conn
	mu          sync.Mutex
	pendingItem *upnp.MediaItem
}

func (c *metadataConn) setPendingItem(item *upnp.MediaItem) {
	c.mu.Lock()
	if item == nil {
		c.pendingItem = nil
	} else {
		cp := *item
		c.pendingItem = &cp
	}
	c.mu.Unlock()
}

func (c *metadataConn) Send(requestID int, payload castproto.Payload, sourceID, destinationID, namespace string) error {
	if loadCmd, ok := payload.(*castproto.LoadMediaCommand); ok {
		c.mu.Lock()
		item := c.pendingItem
		c.pendingItem = nil
		c.mu.Unlock()
		if item != nil {
			payload = buildCastLoadPayload(loadCmd, *item)
		}
	}
	return c.Conn.Send(requestID, payload, sourceID, destinationID, namespace)
}

func buildCastLoadPayload(cmd *castproto.LoadMediaCommand, item upnp.MediaItem) *castLoadMediaPayload {
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
	return &castLoadMediaPayload{
		PayloadHeader: cmd.PayloadHeader,
		CurrentTime:   cmd.CurrentTime,
		Autoplay:      cmd.Autoplay,
		Media: castMediaInfo{
			ContentID:   cmd.Media.ContentId,
			StreamType:  cmd.Media.StreamType,
			ContentType: cmd.Media.ContentType,
			Duration:    item.Duration.Seconds(),
			Metadata:    meta,
		},
	}
}

// Renderer wraps a Google Cast device (or Cast speaker group) as a standard
// upnp.Controller so queue.Manager treats it identically to UPnP and browser zones.
type Renderer struct {
	mu        sync.Mutex
	info      upnp.RendererInfo
	host      string
	port      int
	localIP   string
	app       *application.Application
	metaConn  *metadataConn
	connected bool
	lastURI   string
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

// SetNextURI is currently unsupported on Cast renderers (queue.Manager falls back
// cleanly to auto-advance on STOPPED).
func (r *Renderer) SetNextURI(_ context.Context, _ upnp.MediaItem) error {
	return fmt.Errorf("SetNextURI unsupported on Google Cast renderer")
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
		r.mu.Lock()
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

		_, media, vol := r.app.Status()
		out = mapCastStatus(media, vol, r.lastURI)
		return nil
	})
	return out, err
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
