package player

import (
	"context"
	"crypto/rand"
	"crypto/subtle"
	"encoding/hex"
	"errors"
	"fmt"
	"math"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"tacet/internal/upnp"
)

const (
	defaultGracePeriod    = 20 * time.Second
	defaultCommandTimeout = 3 * time.Second
	maxNameLen            = 80
	maxModelLen           = 80
	maxPlayerIDLen        = 64
	maxReportedSeconds    = 24 * 60 * 60 // 24h cap prevents int64 time.Duration overflow
)

// Domain errors returned by the remote player registry and endpoint controller.
var (
	ErrPlayerNotFound     = errors.New("player endpoint not found")
	ErrUnauthorized       = errors.New("invalid player credentials")
	ErrPlayerDisconnected = errors.New("player endpoint is not connected")
	ErrCommandTimeout     = errors.New("player endpoint timed out acknowledging command")
	ErrPlaybackBlocked    = errors.New("player endpoint blocked playback — user interaction required on the device")
	ErrInvalidPlayer      = errors.New("invalid player registration")
)

// RegisterRequest configures any remote HTTP/SSE audio playback endpoint
// (web browser, mobile app, desktop app, or headless audio daemon).
type RegisterRequest struct {
	PlayerID            string   `json:"playerId,omitempty"`
	Name                string   `json:"name"`
	Model               string   `json:"model,omitempty"`
	Manufacturer        string   `json:"manufacturer,omitempty"`
	Codecs              []string `json:"codecs,omitempty"`
	HasVolume           *bool    `json:"hasVolume,omitempty"`
	SupportsSetNext     *bool    `json:"supportsSetNext,omitempty"`
	SupportsSeek        *bool    `json:"supportsSeek,omitempty"`
	PreferRelativeMedia *bool    `json:"preferRelativeMedia,omitempty"`
	Host                string   `json:"-"`
}

// Registration is returned after registering or refreshing a player endpoint.
type Registration struct {
	ZoneID string            `json:"zoneId"`
	Token  string            `json:"token"`
	Info   upnp.RendererInfo `json:"renderer"`
}

// MediaPayload describes a track pushed to a player endpoint via set_uri/set_next_uri.
type MediaPayload struct {
	ID          string `json:"id"`
	URL         string `json:"url"`
	Title       string `json:"title"`
	Artist      string `json:"artist"`
	Album       string `json:"album"`
	ArtworkURL  string `json:"artworkUrl,omitempty"`
	MIME        string `json:"mime"`
	DurationSec int    `json:"durationSec"`
	SampleRate  int    `json:"sampleRate,omitempty"`
	BitDepth    int    `json:"bitDepth,omitempty"`
}

// Command is pushed to a connected player endpoint over its SSE command stream.
type Command struct {
	ID          string        `json:"id"`
	Type        string        `json:"type"`
	Media       *MediaPayload `json:"media,omitempty"`
	PositionSec *int          `json:"positionSec,omitempty"`
	Volume      *int          `json:"volume,omitempty"`
	Muted       *bool         `json:"muted,omitempty"`
}

// StateReport is pushed by a player endpoint whenever playback/volume changes
// or to acknowledge a command.
type StateReport struct {
	CmdID        string   `json:"cmdId,omitempty"`
	OK           *bool    `json:"ok,omitempty"`
	ErrorCode    string   `json:"errorCode,omitempty"`
	ErrorMessage string   `json:"errorMessage,omitempty"`
	State        string   `json:"state,omitempty"`
	PositionSec  *float64 `json:"positionSec,omitempty"`
	DurationSec  *float64 `json:"durationSec,omitempty"`
	TrackURI     string   `json:"trackUri,omitempty"`
	Volume       *int     `json:"volume,omitempty"`
	Muted        *bool    `json:"muted,omitempty"`
}

// CommandStream represents a single active SSE command connection for a player.
type CommandStream struct {
	ID       uint64
	Commands <-chan Command
	Replaced <-chan struct{}
}

type cmdResult struct {
	ok           bool
	errorCode    string
	errorMessage string
}

// Registry manages active HTTP/SSE player endpoints registered by any client.
type Registry struct {
	mu             sync.RWMutex
	players        map[string]*RemotePlayer
	gracePeriod    time.Duration
	commandTimeout time.Duration
	onRegister     func(upnp.Controller)
	onUnregister   func(string)
	onEvent        func(upnp.DeviceEvent)
	closed         bool
}

// RegistryOption customizes Registry timing or hooks.
type RegistryOption func(*Registry)

// WithGracePeriod overrides how long a disconnected endpoint stays registered
// before automatic removal (allowing reconnects/reloads to preserve the queue).
func WithGracePeriod(d time.Duration) RegistryOption {
	return func(r *Registry) {
		if d > 0 {
			r.gracePeriod = d
		}
	}
}

// WithCommandTimeout overrides how long controller actions wait for an ack.
func WithCommandTimeout(d time.Duration) RegistryOption {
	return func(r *Registry) {
		if d > 0 {
			r.commandTimeout = d
		}
	}
}

// NewRegistry creates a player Registry wired into the queue manager callbacks.
func NewRegistry(
	onRegister func(upnp.Controller),
	onUnregister func(string),
	onEvent func(upnp.DeviceEvent),
	opts ...RegistryOption,
) *Registry {
	r := &Registry{
		players:        make(map[string]*RemotePlayer),
		gracePeriod:    defaultGracePeriod,
		commandTimeout: defaultCommandTimeout,
		onRegister:     onRegister,
		onUnregister:   onUnregister,
		onEvent:        onEvent,
	}
	for _, opt := range opts {
		opt(r)
	}
	return r
}

// Register creates or refreshes a remote player zone. If zoneID already exists,
// existingToken must match the current session token to reattach.
func (r *Registry) Register(req RegisterRequest, existingToken string) (Registration, error) {
	name := clampString(req.Name, maxNameLen, "Remote Player")
	model := clampString(req.Model, maxModelLen, "Remote Endpoint")
	manufacturer := clampString(req.Manufacturer, maxModelLen, "tacet")

	zoneID, err := normalizeZoneID(req.PlayerID)
	if err != nil {
		return Registration{}, err
	}

	codecs := normalizeCodecs(req.Codecs)
	host := strings.TrimSpace(req.Host)
	if host == "" {
		host = "remote"
	}

	hasVolume := true
	if req.HasVolume != nil {
		hasVolume = *req.HasVolume
	}
	supportsSetNext := true
	if req.SupportsSetNext != nil {
		supportsSetNext = *req.SupportsSetNext
	}
	supportsSeek := true
	if req.SupportsSeek != nil {
		supportsSeek = *req.SupportsSeek
	}
	preferRelative := true
	if req.PreferRelativeMedia != nil {
		preferRelative = *req.PreferRelativeMedia
	}

	r.mu.Lock()
	if r.closed {
		r.mu.Unlock()
		return Registration{}, ErrPlayerDisconnected
	}

	if existing, ok := r.players[zoneID]; ok {
		if !existing.verifyToken(existingToken) {
			r.mu.Unlock()
			return Registration{}, fmt.Errorf("%w: player id already active", ErrUnauthorized)
		}
		existing.updateMetadata(name, model, manufacturer, host, codecs, hasVolume, supportsSetNext, supportsSeek, preferRelative)
		existing.scheduleGraceLocked(r.gracePeriod, func() { r.reapIfDisconnected(zoneID, existing) })
		info := existing.Info()
		tok := existing.token
		r.mu.Unlock()

		if r.onRegister != nil {
			r.onRegister(existing)
		}
		return Registration{ZoneID: zoneID, Token: tok, Info: info}, nil
	}

	tok, err := randomHex(32)
	if err != nil {
		r.mu.Unlock()
		return Registration{}, fmt.Errorf("generate player token: %w", err)
	}

	rp := &RemotePlayer{
		token:          tok,
		commandTimeout: r.commandTimeout,
		onEvent:        r.onEvent,
		preferRelative: preferRelative,
		pending:        make(map[string]chan cmdResult),
		info: upnp.RendererInfo{
			ID:              zoneID,
			Kind:            "player",
			Name:            name,
			Manufacturer:    manufacturer,
			Model:           model,
			Location:        "tacet://" + zoneID,
			Host:            host,
			HasVolume:       hasVolume,
			SupportsSetNext: supportsSetNext,
			SupportsSeek:    supportsSeek,
			SupportsEvents:  true,
			SinkProtocols:   codecs,
		},
		status: upnp.TransportStatus{
			State:       "STOPPED",
			Volume:      100,
			HasVolValue: hasVolume,
			HasMuteVal:  hasVolume,
			HasVolume:   hasVolume,
		},
	}
	rp.scheduleGraceLocked(r.gracePeriod, func() { r.reapIfDisconnected(zoneID, rp) })
	r.players[zoneID] = rp
	info := rp.Info()
	r.mu.Unlock()

	if r.onRegister != nil {
		r.onRegister(rp)
	}
	return Registration{ZoneID: zoneID, Token: tok, Info: info}, nil
}

// Get returns an authenticated RemotePlayer for zoneID.
func (r *Registry) Get(zoneID, token string) (*RemotePlayer, error) {
	r.mu.RLock()
	rp, ok := r.players[zoneID]
	r.mu.RUnlock()
	if !ok {
		return nil, fmt.Errorf("%w: %s", ErrPlayerNotFound, zoneID)
	}
	if !rp.verifyToken(token) {
		return nil, ErrUnauthorized
	}
	return rp, nil
}

// Unregister explicitly removes an authenticated player endpoint immediately.
func (r *Registry) Unregister(zoneID, token string) error {
	r.mu.Lock()
	rp, ok := r.players[zoneID]
	if !ok {
		r.mu.Unlock()
		return fmt.Errorf("%w: %s", ErrPlayerNotFound, zoneID)
	}
	if !rp.verifyToken(token) {
		r.mu.Unlock()
		return ErrUnauthorized
	}
	delete(r.players, zoneID)
	r.mu.Unlock()

	rp.shutdown()
	if r.onUnregister != nil {
		r.onUnregister(zoneID)
	}
	return nil
}

// DetachStream marks the player's command SSE stream disconnected and starts
// the grace-period timer for automatic removal if it does not reconnect.
func (r *Registry) DetachStream(rp *RemotePlayer, streamID uint64) {
	if rp == nil {
		return
	}
	zoneID := rp.Info().ID
	r.mu.RLock()
	grace := r.gracePeriod
	closed := r.closed
	r.mu.RUnlock()

	detached := rp.detachStream(streamID)
	if !detached || closed {
		return
	}
	rp.scheduleGrace(grace, func() { r.reapIfDisconnected(zoneID, rp) })
}

// Close shuts down all active player streams and cancels pending timers.
func (r *Registry) Close() {
	r.mu.Lock()
	r.closed = true
	list := make([]*RemotePlayer, 0, len(r.players))
	for _, rp := range r.players {
		list = append(list, rp)
	}
	r.players = make(map[string]*RemotePlayer)
	r.mu.Unlock()

	for _, rp := range list {
		rp.shutdown()
	}
}

func (r *Registry) reapIfDisconnected(zoneID string, target *RemotePlayer) {
	r.mu.Lock()
	cur, ok := r.players[zoneID]
	if !ok || cur != target || cur.PushConnected() {
		r.mu.Unlock()
		return
	}
	delete(r.players, zoneID)
	r.mu.Unlock()

	cur.shutdown()
	if r.onUnregister != nil {
		r.onUnregister(zoneID)
	}
}

// RemotePlayer implements upnp.Controller, upnp.RelativeMediaController, and
// upnp.PushStatusController for any HTTP/SSE playback client.
type RemotePlayer struct {
	token          string
	commandTimeout time.Duration
	onEvent        func(upnp.DeviceEvent)
	cmdSeq         atomic.Uint64
	eventSeq       atomic.Uint32

	mu             sync.Mutex
	preferRelative bool
	info           upnp.RendererInfo
	status         upnp.TransportStatus
	streamID       uint64
	cmdCh          chan Command
	replacedCh     chan struct{}
	pending        map[string]chan cmdResult
	graceTimer     *time.Timer
}

// PreferRelativeMedia signals queue.Manager to emit server-relative /media/*
// URLs and relay Qobuz streams through /media/qobuz/ when requested by the client.
func (p *RemotePlayer) PreferRelativeMedia() bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.preferRelative
}

// PushConnected reports whether the endpoint currently has a live SSE command stream.
func (p *RemotePlayer) PushConnected() bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.cmdCh != nil
}

// Info returns the renderer metadata snapshot.
func (p *RemotePlayer) Info() upnp.RendererInfo {
	p.mu.Lock()
	defer p.mu.Unlock()
	cp := p.info
	if len(p.info.SinkProtocols) > 0 {
		cp.SinkProtocols = append([]string(nil), p.info.SinkProtocols...)
	}
	return cp
}

// LocalIP returns loopback as fallback when absolute URLs are constructed without PublicBaseURL.
func (p *RemotePlayer) LocalIP() string { return "127.0.0.1" }

// AttachStream attaches a new SSE command stream, replacing any older stream.
func (p *RemotePlayer) AttachStream() CommandStream {
	p.mu.Lock()
	defer p.mu.Unlock()

	if p.graceTimer != nil {
		p.graceTimer.Stop()
		p.graceTimer = nil
	}
	if p.replacedCh != nil {
		close(p.replacedCh)
	}

	p.streamID++
	ch := make(chan Command, 32)
	rep := make(chan struct{})
	p.cmdCh = ch
	p.replacedCh = rep

	return CommandStream{
		ID:       p.streamID,
		Commands: ch,
		Replaced: rep,
	}
}

func (p *RemotePlayer) detachStream(streamID uint64) bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.streamID != streamID || p.cmdCh == nil {
		return false
	}
	p.cmdCh = nil
	p.replacedCh = nil
	for id, ch := range p.pending {
		delete(p.pending, id)
		ch <- cmdResult{ok: false, errorCode: "DISCONNECTED", errorMessage: "player endpoint disconnected"}
	}
	return true
}

func (p *RemotePlayer) shutdown() {
	p.mu.Lock()
	if p.graceTimer != nil {
		p.graceTimer.Stop()
		p.graceTimer = nil
	}
	if p.replacedCh != nil {
		close(p.replacedCh)
		p.replacedCh = nil
	}
	p.cmdCh = nil
	for id, ch := range p.pending {
		delete(p.pending, id)
		ch <- cmdResult{ok: false, errorCode: "DISCONNECTED", errorMessage: "player endpoint closed"}
	}
	p.mu.Unlock()
}

func (p *RemotePlayer) scheduleGrace(d time.Duration, fn func()) {
	p.mu.Lock()
	p.scheduleGraceLocked(d, fn)
	p.mu.Unlock()
}

func (p *RemotePlayer) scheduleGraceLocked(d time.Duration, fn func()) {
	if p.graceTimer != nil {
		p.graceTimer.Stop()
	}
	p.graceTimer = time.AfterFunc(d, fn)
}

func (p *RemotePlayer) updateMetadata(name, model, manufacturer, host string, codecs []string, hasVol, supportsNext, supportsSeek, preferRel bool) {
	p.mu.Lock()
	p.info.Name = name
	p.info.Model = model
	p.info.Manufacturer = manufacturer
	p.info.Host = host
	p.info.HasVolume = hasVol
	p.info.SupportsSetNext = supportsNext
	p.info.SupportsSeek = supportsSeek
	p.preferRelative = preferRel
	p.status.HasVolume = hasVol
	if len(codecs) > 0 {
		p.info.SinkProtocols = codecs
	}
	p.mu.Unlock()
}

func (p *RemotePlayer) verifyToken(tok string) bool {
	if tok == "" || p.token == "" {
		return false
	}
	return subtle.ConstantTimeCompare([]byte(tok), []byte(p.token)) == 1
}

// ReportState applies an endpoint state update, resolves any matching command
// ack, and forwards transport/volume updates into queue.Manager.
func (p *RemotePlayer) ReportState(rep StateReport) {
	p.mu.Lock()
	zoneID := p.info.ID

	if rep.CmdID != "" {
		if ch, ok := p.pending[rep.CmdID]; ok {
			delete(p.pending, rep.CmdID)
			okVal := true
			if rep.OK != nil {
				okVal = *rep.OK
			}
			ch <- cmdResult{
				ok:           okVal,
				errorCode:    rep.ErrorCode,
				errorMessage: rep.ErrorMessage,
			}
		}
	}

	seq := p.eventSeq.Add(1)
	if seq == 0 {
		seq = p.eventSeq.Add(1)
	}
	ev := upnp.DeviceEvent{
		RendererID: zoneID,
		Service:    upnp.ServiceAVTransport,
		Seq:        seq,
	}

	if st := normalizeIncomingState(rep.State); st != "" {
		p.status.State = st
		ev.HasState = true
		ev.State = st
	}
	if sec, ok := validSeconds(rep.PositionSec); ok {
		pos := time.Duration(sec * float64(time.Second))
		p.status.Position = pos
		p.status.HasPosition = true
		ev.HasPosition = true
		ev.Position = pos
	}
	if sec, ok := validSeconds(rep.DurationSec); ok {
		dur := time.Duration(sec * float64(time.Second))
		p.status.Duration = dur
		ev.HasDuration = true
		ev.Duration = dur
	}
	if rep.TrackURI != "" {
		p.status.TrackURI = rep.TrackURI
		ev.HasTrackURI = true
		ev.TrackURI = rep.TrackURI
	}
	if rep.Volume != nil && *rep.Volume >= 0 && *rep.Volume <= 100 {
		p.status.Volume = *rep.Volume
		p.status.HasVolValue = true
		ev.HasVolume = true
		ev.Volume = *rep.Volume
	}
	if rep.Muted != nil {
		p.status.Muted = *rep.Muted
		p.status.HasMuteVal = true
		ev.HasMute = true
		ev.Muted = *rep.Muted
	}
	onEvent := p.onEvent
	p.mu.Unlock()

	if onEvent != nil && ev.HasAnyUpdate() {
		onEvent(ev)
	}
}

// SetURI loads a track into the player endpoint.
func (p *RemotePlayer) SetURI(ctx context.Context, item upnp.MediaItem) error {
	return p.sendCommand(ctx, Command{
		Type:  "set_uri",
		Media: mediaPayloadFromItem(item),
	})
}

// SetNextURI preloads the next queued track into the player endpoint.
func (p *RemotePlayer) SetNextURI(ctx context.Context, item upnp.MediaItem) error {
	return p.sendCommand(ctx, Command{
		Type:  "set_next_uri",
		Media: mediaPayloadFromItem(item),
	})
}

// Play starts or resumes playback on the player endpoint.
func (p *RemotePlayer) Play(ctx context.Context) error {
	return p.sendCommand(ctx, Command{Type: "play"})
}

// Pause pauses playback on the player endpoint.
func (p *RemotePlayer) Pause(ctx context.Context) error {
	return p.sendCommand(ctx, Command{Type: "pause"})
}

// Stop stops playback on the player endpoint.
func (p *RemotePlayer) Stop(ctx context.Context) error {
	return p.sendCommand(ctx, Command{Type: "stop"})
}

// Seek seeks playback on the player endpoint to pos.
func (p *RemotePlayer) Seek(ctx context.Context, pos time.Duration) error {
	sec := int(pos.Seconds())
	if sec < 0 {
		sec = 0
	}
	return p.sendCommand(ctx, Command{Type: "seek", PositionSec: &sec})
}

// SetVolume updates playback volume (0–100) on the player endpoint.
func (p *RemotePlayer) SetVolume(ctx context.Context, vol int) error {
	v := vol
	return p.sendCommand(ctx, Command{Type: "set_volume", Volume: &v})
}

// SetMute toggles mute on the player endpoint.
func (p *RemotePlayer) SetMute(ctx context.Context, muted bool) error {
	m := muted
	return p.sendCommand(ctx, Command{Type: "set_mute", Muted: &m})
}

// Status returns the last pushed transport/volume snapshot in O(1) memory.
func (p *RemotePlayer) Status(_ context.Context) (upnp.TransportStatus, error) {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.status, nil
}

func (p *RemotePlayer) sendCommand(ctx context.Context, cmd Command) error {
	cmd.ID = fmt.Sprintf("cmd_%d", p.cmdSeq.Add(1))
	ackCh := make(chan cmdResult, 1)

	p.mu.Lock()
	ch := p.cmdCh
	if ch == nil {
		p.mu.Unlock()
		return ErrPlayerDisconnected
	}
	p.pending[cmd.ID] = ackCh
	select {
	case ch <- cmd:
	default:
		delete(p.pending, cmd.ID)
		p.mu.Unlock()
		return ErrCommandTimeout
	}
	timeout := p.commandTimeout
	p.mu.Unlock()

	timer := time.NewTimer(timeout)
	defer timer.Stop()

	select {
	case res := <-ackCh:
		if res.ok {
			return nil
		}
		if res.errorCode == "PLAYBACK_BLOCKED" || res.errorCode == "AUTOPLAY_BLOCKED" {
			if res.errorMessage != "" {
				return fmt.Errorf("%w: %s", ErrPlaybackBlocked, res.errorMessage)
			}
			return ErrPlaybackBlocked
		}
		if res.errorCode == "DISCONNECTED" {
			return ErrPlayerDisconnected
		}
		if res.errorMessage != "" {
			return errors.New(res.errorMessage)
		}
		return errors.New("player endpoint rejected command")
	case <-timer.C:
		p.mu.Lock()
		delete(p.pending, cmd.ID)
		connected := p.cmdCh != nil
		p.mu.Unlock()
		if !connected {
			return ErrPlayerDisconnected
		}
		return ErrCommandTimeout
	case <-ctx.Done():
		p.mu.Lock()
		delete(p.pending, cmd.ID)
		p.mu.Unlock()
		return ctx.Err()
	}
}

func validSeconds(v *float64) (float64, bool) {
	if v == nil || math.IsNaN(*v) || math.IsInf(*v, 0) || *v < 0 || *v > maxReportedSeconds {
		return 0, false
	}
	return *v, true
}

func clampString(raw string, maxLen int, fallback string) string {
	s := strings.TrimSpace(raw)
	if s == "" {
		return fallback
	}
	if len(s) > maxLen {
		return s[:maxLen]
	}
	return s
}

func mediaPayloadFromItem(item upnp.MediaItem) *MediaPayload {
	return &MediaPayload{
		ID:          item.ID,
		URL:         item.URL,
		Title:       item.Title,
		Artist:      item.Artist,
		Album:       item.Album,
		ArtworkURL:  item.AlbumArtURL,
		MIME:        item.MIME,
		DurationSec: int(item.Duration.Seconds() + 0.5),
		SampleRate:  item.SampleRate,
		BitDepth:    item.BitDepth,
	}
}

func normalizeIncomingState(raw string) string {
	switch strings.ToUpper(strings.TrimSpace(raw)) {
	case "PLAYING":
		return "PLAYING"
	case "PAUSED", "PAUSED_PLAYBACK":
		return "PAUSED_PLAYBACK"
	case "BUFFERING", "TRANSITIONING":
		return "TRANSITIONING"
	case "STOPPED", "IDLE", "NO_MEDIA_PRESENT":
		return "STOPPED"
	default:
		return ""
	}
}

func normalizeZoneID(raw string) (string, error) {
	trimmed := strings.TrimSpace(raw)
	if trimmed == "" {
		suffix, err := randomHex(8)
		if err != nil {
			return "", fmt.Errorf("generate player id: %w", err)
		}
		return "player:" + suffix, nil
	}
	trimmed = strings.TrimPrefix(trimmed, "player:")
	if len(trimmed) == 0 || len(trimmed) > maxPlayerIDLen {
		return "", fmt.Errorf("%w: playerId length out of bounds", ErrInvalidPlayer)
	}
	for _, r := range trimmed {
		isOK := (r >= 'a' && r <= 'z') || (r >= 'A' && r <= 'Z') || (r >= '0' && r <= '9') || r == '-' || r == '_'
		if !isOK {
			return "", fmt.Errorf("%w: playerId must be alphanumeric, '-' or '_'", ErrInvalidPlayer)
		}
	}
	return "player:" + trimmed, nil
}

func normalizeCodecs(in []string) []string {
	if len(in) == 0 {
		return nil
	}
	out := make([]string, 0, len(in))
	seen := make(map[string]struct{}, len(in))
	for _, c := range in {
		c = strings.ToLower(strings.TrimSpace(c))
		if c == "" || len(c) > 80 {
			continue
		}
		if _, dup := seen[c]; dup {
			continue
		}
		seen[c] = struct{}{}
		out = append(out, c)
	}
	return out
}

func randomHex(nBytes int) (string, error) {
	buf := make([]byte, nBytes)
	if _, err := rand.Read(buf); err != nil {
		return "", err
	}
	return hex.EncodeToString(buf), nil
}
