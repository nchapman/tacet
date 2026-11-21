package upnp

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/huin/goupnp/dcps/av1"
	"github.com/huin/goupnp/soap"
)

// ErrActionUnsupported indicates the UPnP renderer permanently rejected an
// optional action (UPnP error 401 Invalid Action or 602 Optional Action Not
// Implemented). Callers should cache this capability and stop retrying it.
var ErrActionUnsupported = errors.New("UPnP action unsupported by renderer")

// IsActionUnsupported reports whether err is a permanent UPnP unsupported-action
// error (either wrapped ErrActionUnsupported or SOAP fault 401/602).
func IsActionUnsupported(err error) bool {
	if err == nil {
		return false
	}
	if errors.Is(err, ErrActionUnsupported) {
		return true
	}
	var fault *soap.SOAPFaultError
	if errors.As(err, &fault) {
		code := fault.Detail.UPnPError.Errorcode
		return code == 401 || code == 602
	}
	return false
}

// RendererInfo describes a discovered UPnP/DLNA MediaRenderer or remote player endpoint.
type RendererInfo struct {
	ID              string   `json:"id"`
	Kind            string   `json:"kind,omitempty"`
	Name            string   `json:"name"`
	Manufacturer    string   `json:"manufacturer,omitempty"`
	Model           string   `json:"model,omitempty"`
	Location        string   `json:"location"`
	Host            string   `json:"host"`
	HasVolume       bool     `json:"hasVolume"`
	SupportsSetNext bool     `json:"supportsSetNext"`
	SupportsSeek    bool     `json:"supportsSeek"`
	SupportsEvents  bool     `json:"supportsEvents"`
	SinkProtocols   []string `json:"sinkProtocols,omitempty"`
}

// TransportStatus is a live snapshot of a renderer's playback and volume state.
type TransportStatus struct {
	State       string        `json:"state"`
	Position    time.Duration `json:"position"`
	Duration    time.Duration `json:"duration"`
	TrackURI    string        `json:"trackUri,omitempty"`
	HasPosition bool          `json:"-"`
	Volume      int           `json:"volume"`
	HasVolValue bool          `json:"-"`
	Muted       bool          `json:"muted"`
	HasMuteVal  bool          `json:"-"`
	HasVolume   bool          `json:"hasVolume"`
}

// Controller is the transport/volume interface used by the queue manager.
type Controller interface {
	Info() RendererInfo
	LocalIP() string
	SetURI(ctx context.Context, item MediaItem) error
	SetNextURI(ctx context.Context, item MediaItem) error
	Play(ctx context.Context) error
	Pause(ctx context.Context) error
	Stop(ctx context.Context) error
	Seek(ctx context.Context, pos time.Duration) error
	Status(ctx context.Context) (TransportStatus, error)
	SetVolume(ctx context.Context, vol int) error
	SetMute(ctx context.Context, muted bool) error
}

// EventSource is optionally implemented by UPnP controllers that expose GENA
// event subscription endpoints.
type EventSource interface {
	EventURLs() (avTransportURL, renderingControlURL string)
}

// RelativeMediaController is optionally implemented by player endpoints that
// load media via same-origin relative /media/* URLs and prefer server-relayed
// Qobuz streams over direct CDN URLs.
type RelativeMediaController interface {
	PreferRelativeMedia() bool
}

// PushStatusController is optionally implemented by endpoints that push live
// transport state directly to the server over a persistent session.
type PushStatusController interface {
	PushConnected() bool
}

// Renderer wraps huin/goupnp AV1 service clients for a single UPnP/DLNA device.
type Renderer struct {
	capMu           sync.RWMutex
	info            RendererInfo
	supportsSetNext bool
	supportsSeek    bool

	localIP    string
	avEventURL string
	rcEventURL string

	soapMu sync.Mutex // Serializes SOAP calls so embedded libupnp servers (e.g. WiiM) don't drop concurrent requests.
	av     *av1.AVTransport1
	rc     *av1.RenderingControl1
	cm     *av1.ConnectionManager1
}

func newSOAPHTTPClient() http.Client {
	return http.Client{
		Timeout: 5 * time.Second,
		CheckRedirect: func(*http.Request, []*http.Request) error {
			return http.ErrUseLastResponse
		},
		Transport: &http.Transport{
			// Many UPnP renderers (LinkPlay/WiiM libupnp) close TCP sockets right
			// after each SOAP response without Connection: close, causing Go's
			// pooled keep-alive POSTs to fail with EOF.
			DisableKeepAlives: true,
			DialContext: (&net.Dialer{
				Timeout: 3 * time.Second,
			}).DialContext,
		},
	}
}

func isTransientConnErr(err error) bool {
	if err == nil {
		return false
	}
	if errors.Is(err, io.EOF) || errors.Is(err, io.ErrUnexpectedEOF) || errors.Is(err, syscall.ECONNRESET) {
		return true
	}
	msg := err.Error()
	return strings.Contains(msg, "EOF") || strings.Contains(msg, "connection reset by peer")
}

func (r *Renderer) withSOAP(ctx context.Context, fn func() error) error {
	r.soapMu.Lock()
	defer r.soapMu.Unlock()

	err := fn()
	if err != nil && isTransientConnErr(err) && ctx.Err() == nil {
		select {
		case <-time.After(120 * time.Millisecond):
		case <-ctx.Done():
			return ctx.Err()
		}
		err = fn()
	}
	return err
}

// Info returns metadata and learned capabilities for this renderer.
func (r *Renderer) Info() RendererInfo {
	r.capMu.RLock()
	info := r.info
	info.SupportsSetNext = r.supportsSetNext
	info.SupportsSeek = r.supportsSeek
	r.capMu.RUnlock()
	return info
}

// EventURLs returns the validated GENA subscription URLs for AVTransport and
// RenderingControl (empty string if unavailable).
func (r *Renderer) EventURLs() (string, string) {
	return r.avEventURL, r.rcEventURL
}

// LocalIP returns the local LAN IP address on the interface that reaches this renderer.
func (r *Renderer) LocalIP() string {
	return r.localIP
}

// SetURI calls AVTransport.SetAVTransportURI with negotiated DIDL-Lite metadata.
func (r *Renderer) SetURI(ctx context.Context, item MediaItem) error {
	meta := BuildDIDL(item, r.info.SinkProtocols)
	return r.withSOAP(ctx, func() error {
		return r.av.SetAVTransportURICtx(ctx, 0, item.URL, meta)
	})
}

// SetNextURI calls AVTransport.SetNextAVTransportURI for gapless track preloading.
// If the renderer's SCPD or a prior 401/602 SOAP fault indicates SetNextAVTransportURI
// is unsupported, it returns ErrActionUnsupported immediately without a network call.
func (r *Renderer) SetNextURI(ctx context.Context, item MediaItem) error {
	r.capMu.RLock()
	supported := r.supportsSetNext
	r.capMu.RUnlock()
	if !supported {
		return ErrActionUnsupported
	}

	meta := BuildDIDL(item, r.info.SinkProtocols)
	err := r.withSOAP(ctx, func() error {
		return r.av.SetNextAVTransportURICtx(ctx, 0, item.URL, meta)
	})
	if err != nil && IsActionUnsupported(err) {
		r.capMu.Lock()
		r.supportsSetNext = false
		r.capMu.Unlock()
		return fmt.Errorf("%w: %w", ErrActionUnsupported, err)
	}
	return err
}

// Play starts or resumes playback at 1x speed.
func (r *Renderer) Play(ctx context.Context) error {
	return r.withSOAP(ctx, func() error {
		return r.av.PlayCtx(ctx, 0, "1")
	})
}

// Pause pauses playback.
func (r *Renderer) Pause(ctx context.Context) error {
	return r.withSOAP(ctx, func() error {
		return r.av.PauseCtx(ctx, 0)
	})
}

// Stop stops playback.
func (r *Renderer) Stop(ctx context.Context) error {
	return r.withSOAP(ctx, func() error {
		return r.av.StopCtx(ctx, 0)
	})
}

// Seek seeks to an absolute position within the current track via REL_TIME.
func (r *Renderer) Seek(ctx context.Context, pos time.Duration) error {
	target := FormatUPnPDuration(pos)
	return r.withSOAP(ctx, func() error {
		return r.av.SeekCtx(ctx, 0, "REL_TIME", target)
	})
}

// Status fetches transport state, track position/duration/URI, and volume/mute.
func (r *Renderer) Status(ctx context.Context) (TransportStatus, error) {
	r.soapMu.Lock()
	defer r.soapMu.Unlock()

	state, _, _, err := r.av.GetTransportInfoCtx(ctx, 0)
	if err != nil {
		return TransportStatus{}, fmt.Errorf("GetTransportInfo: %w", err)
	}

	status := TransportStatus{
		State:     strings.TrimSpace(state),
		HasVolume: r.rc != nil,
	}

	_, durStr, _, trackURI, relStr, _, _, _, posErr := r.av.GetPositionInfoCtx(ctx, 0)
	if posErr == nil {
		status.Duration = ParseUPnPDuration(durStr)
		status.Position = ParseUPnPDuration(relStr)
		status.TrackURI = strings.TrimSpace(trackURI)
		status.HasPosition = true
	}

	if r.rc != nil {
		if vol, err := r.rc.GetVolumeCtx(ctx, 0, "Master"); err == nil {
			status.Volume = int(vol)
			status.HasVolValue = true
		}
		if muted, err := r.rc.GetMuteCtx(ctx, 0, "Master"); err == nil {
			status.Muted = muted
			status.HasMuteVal = true
		}
	}

	return status, nil
}

// SetVolume sets Master volume clamped to 0..100.
func (r *Renderer) SetVolume(ctx context.Context, vol int) error {
	if r.rc == nil {
		return errors.New("renderer does not expose RenderingControl")
	}
	clamped := clampVolumeUint16(vol)
	return r.withSOAP(ctx, func() error {
		return r.rc.SetVolumeCtx(ctx, 0, "Master", clamped)
	})
}

func clampVolumeUint16(vol int) uint16 {
	if vol <= 0 {
		return 0
	}
	if vol >= 100 {
		return 100
	}
	return uint16(vol)
}

// SetMute sets Master mute state.
func (r *Renderer) SetMute(ctx context.Context, muted bool) error {
	if r.rc == nil {
		return errors.New("renderer does not expose RenderingControl")
	}
	return r.withSOAP(ctx, func() error {
		return r.rc.SetMuteCtx(ctx, 0, "Master", muted)
	})
}

// outboundIPForHost asks the OS routing table which local IP routes to targetHost
// without transmitting packets.
func outboundIPForHost(ctx context.Context, loc *url.URL, fallback net.IP) string {
	if loc != nil && loc.Host != "" {
		host := loc.Hostname()
		port := loc.Port()
		if port == "" {
			port = "80"
		}
		d := net.Dialer{Timeout: 500 * time.Millisecond}
		if conn, err := d.DialContext(ctx, "udp4", net.JoinHostPort(host, port)); err == nil {
			defer func() { _ = conn.Close() }()
			if udpAddr, ok := conn.LocalAddr().(*net.UDPAddr); ok && udpAddr.IP != nil {
				if ip4 := udpAddr.IP.To4(); ip4 != nil && !ip4.IsLoopback() {
					return ip4.String()
				}
			}
		}
	}
	if fallback != nil {
		if ip4 := fallback.To4(); ip4 != nil && !ip4.IsLoopback() {
			return ip4.String()
		}
	}
	return firstLANIPv4()
}

func firstLANIPv4() string {
	ifaces, err := net.Interfaces()
	if err != nil {
		return "127.0.0.1"
	}
	for _, iface := range ifaces {
		if iface.Flags&net.FlagUp == 0 || iface.Flags&net.FlagLoopback != 0 {
			continue
		}
		addrs, err := iface.Addrs()
		if err != nil {
			continue
		}
		for _, a := range addrs {
			ipNet, ok := a.(*net.IPNet)
			if !ok || ipNet.IP == nil {
				continue
			}
			if ip4 := ipNet.IP.To4(); ip4 != nil && !ip4.IsLoopback() {
				return ip4.String()
			}
		}
	}
	return "127.0.0.1"
}
