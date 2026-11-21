package queue

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"tacet/internal/library"
	"tacet/internal/qobuz"
	"tacet/internal/upnp"
)

// StreamMode controls how Qobuz streams are handed to UPnP renderers.
type StreamMode string

const (
	// StreamModeDirect hands the Qobuz CDN HTTPS URL directly to the renderer.
	StreamModeDirect StreamMode = "direct"
	// StreamModeRelay terminates TLS on the NAS and serves untouched bytes over plain HTTP.
	StreamModeRelay StreamMode = "relay"
)

// PlaybackState is the normalized transport state exposed over the API.
type PlaybackState string

// Normalized playback states.
const (
	StateIdle      PlaybackState = "idle"
	StatePlaying   PlaybackState = "playing"
	StatePaused    PlaybackState = "paused"
	StateBuffering PlaybackState = "buffering"
	StateStopped   PlaybackState = "stopped"
)

// NormalizePlaybackState maps UPnP AVTransport wire states into the closed API enum.
func NormalizePlaybackState(raw string) PlaybackState {
	switch strings.ToUpper(strings.TrimSpace(raw)) {
	case "PLAYING":
		return StatePlaying
	case "PAUSED_PLAYBACK", "PAUSED_RECORDING", "PAUSED":
		return StatePaused
	case "TRANSITIONING", "BUFFERING":
		return StateBuffering
	case "STOPPED":
		return StateStopped
	default:
		return StateIdle
	}
}

// Item binds a track in a zone queue to a stable server-assigned itemId.
type Item struct {
	ItemID string        `json:"itemId"`
	Track  library.Track `json:"track"`
}

// ZoneSnapshot is the JSON-serializable state of a single renderer zone.
type ZoneSnapshot struct {
	ZoneID            string            `json:"zoneId"`
	Renderer          upnp.RendererInfo `json:"renderer"`
	Revision          uint64            `json:"revision"`
	State             PlaybackState     `json:"state"`
	PositionSec       int               `json:"positionSec"`
	PositionUpdatedAt time.Time         `json:"positionUpdatedAt"`
	DurationSec       int               `json:"durationSec"`
	Volume            int               `json:"volume"`
	Muted             bool              `json:"muted"`
	Queue             []Item            `json:"queue"`
	CurrentIndex      int               `json:"currentIndex"`
	CurrentItemID     string            `json:"currentItemId,omitempty"`
	CurrentTrack      *library.Track    `json:"currentTrack,omitempty"`
	GaplessArmed      bool              `json:"gaplessArmed"`
	EventsActive      bool              `json:"eventsActive"`
}

// PlaybackEvent is the lean SSE payload emitted when transport/progress state changes.
type PlaybackEvent struct {
	ZoneID            string         `json:"zoneId"`
	Revision          uint64         `json:"revision"`
	State             PlaybackState  `json:"state"`
	PositionSec       int            `json:"positionSec"`
	PositionUpdatedAt time.Time      `json:"positionUpdatedAt"`
	DurationSec       int            `json:"durationSec"`
	CurrentIndex      int            `json:"currentIndex"`
	CurrentItemID     string         `json:"currentItemId,omitempty"`
	CurrentTrack      *library.Track `json:"currentTrack,omitempty"`
	GaplessArmed      bool           `json:"gaplessArmed"`
	EventsActive      bool           `json:"eventsActive"`
}

// VolumeEvent is the lean SSE payload emitted when volume or mute changes.
type VolumeEvent struct {
	ZoneID   string `json:"zoneId"`
	Revision uint64 `json:"revision"`
	Volume   int    `json:"volume"`
	Muted    bool   `json:"muted"`
}

// ItemsEvent is the SSE payload emitted only when queue contents or order change.
type ItemsEvent struct {
	ZoneID        string         `json:"zoneId"`
	Revision      uint64         `json:"revision"`
	CurrentIndex  int            `json:"currentIndex"`
	CurrentItemID string         `json:"currentItemId,omitempty"`
	CurrentTrack  *library.Track `json:"currentTrack,omitempty"`
	Items         []Item         `json:"items"`
}

// ZoneErrorPayload describes a structured asynchronous error on a zone.
type ZoneErrorPayload struct {
	Code      string `json:"code"`
	Message   string `json:"message"`
	Retryable bool   `json:"retryable"`
}

// ZoneErrorEvent is pushed over SSE when an async playback/preload error occurs.
type ZoneErrorEvent struct {
	ID         string           `json:"id"`
	ZoneID     string           `json:"zoneId"`
	Error      ZoneErrorPayload `json:"error"`
	OccurredAt time.Time        `json:"occurredAt"`
}

// Event is pushed to SSE subscribers whenever zone or server state changes.
type Event struct {
	ID     uint64 `json:"id"`
	Type   string `json:"type"`
	ZoneID string `json:"-"`
	Data   any    `json:"data"`
}

type subscriber struct {
	ch     chan Event
	lagged atomic.Bool
}

type zone struct {
	ctrl               upnp.Controller
	queue              []Item
	currentIndex       int
	revision           uint64
	status             upnp.TransportStatus
	positionUpdatedAt  time.Time
	lastPlaybackEmit   time.Time
	currentURI         string
	nextURI            string
	nextPreloaded      bool
	nextPreloading     bool
	nextFailed         bool
	setNextUnsupported bool
	polling            bool
	lastPoll           time.Time
	wasPlaying         bool
	userStopped        bool
	loadedAt           time.Time
	generation         uint64
}

// Manager coordinates discovered renderers, per-zone queues, gapless preloading,
// GENA event updates, and split SSE broadcasts to connected remote clients.
type Manager struct {
	mu            sync.RWMutex
	zones         map[string]*zone
	zoneOrder     []string
	streamMode    StreamMode
	httpPort      int
	publicBaseURL string
	qobuz         *qobuz.Client
	gena          *upnp.GENAClient
	runCtx        context.Context

	itemSeq  atomic.Uint64
	eventSeq atomic.Uint64

	subMu sync.Mutex
	subs  map[*subscriber]struct{}

	stopCh   chan struct{}
	stopOnce sync.Once
}

// NewManager creates a multi-zone playback/queue manager.
func NewManager(httpPort int, publicBaseURL string, qb *qobuz.Client) *Manager {
	return &Manager{
		zones:         make(map[string]*zone),
		streamMode:    StreamModeDirect,
		httpPort:      httpPort,
		publicBaseURL: strings.TrimRight(strings.TrimSpace(publicBaseURL), "/"),
		qobuz:         qb,
		subs:          make(map[*subscriber]struct{}),
		stopCh:        make(chan struct{}),
	}
}

func (m *Manager) nextItemID() string {
	return fmt.Sprintf("qi_%d", m.itemSeq.Add(1))
}

// PublicBaseURL returns the configured --public-url override (if any).
func (m *Manager) PublicBaseURL() string {
	return m.publicBaseURL
}

// SetGENA attaches a UPnP GENA subscription client so registered renderers
// automatically receive push event subscriptions.
func (m *Manager) SetGENA(gena *upnp.GENAClient) {
	m.mu.Lock()
	m.gena = gena
	ctx := m.runCtx
	var ctrls []upnp.Controller
	if ctx != nil && gena != nil {
		for _, id := range m.zoneOrder {
			if z, ok := m.zones[id]; ok {
				ctrls = append(ctrls, z.ctrl)
			}
		}
	}
	m.mu.Unlock()

	for _, ctrl := range ctrls {
		m.subscribeRendererGENA(ctx, gena, ctrl)
	}
}

// Start launches the background status-polling and gapless watchdog loop.
func (m *Manager) Start(ctx context.Context) {
	m.mu.Lock()
	m.runCtx = ctx
	gena := m.gena
	ctrls := make([]upnp.Controller, 0, len(m.zoneOrder))
	for _, id := range m.zoneOrder {
		if z, ok := m.zones[id]; ok {
			ctrls = append(ctrls, z.ctrl)
		}
	}
	m.mu.Unlock()

	if gena != nil {
		for _, ctrl := range ctrls {
			m.subscribeRendererGENA(ctx, gena, ctrl)
		}
	}
	go m.pollLoop(ctx)
}

// Close stops background polling and unsubscribes active GENA subscriptions.
func (m *Manager) Close() {
	m.stopOnce.Do(func() {
		close(m.stopCh)
		m.mu.RLock()
		gena := m.gena
		m.mu.RUnlock()
		if gena != nil {
			ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
			gena.Close(ctx)
			cancel()
		}
	})
}

func (m *Manager) subscribeRendererGENA(ctx context.Context, gena *upnp.GENAClient, ctrl upnp.Controller) {
	dev, ok := ctrl.(*upnp.Renderer)
	if !ok {
		return
	}
	avURL, rcURL := dev.EventURLs()
	if avURL == "" && rcURL == "" {
		return
	}
	gena.WatchRenderer(ctx, dev.Info().ID, m.baseURLFor(ctrl), avURL, rcURL)
}

// SetStreamMode switches between Direct Qobuz handoff and Quiet Relay.
func (m *Manager) SetStreamMode(mode StreamMode) error {
	if mode != StreamModeDirect && mode != StreamModeRelay {
		return fmt.Errorf("%w: unsupported stream mode %q (use direct or relay)", ErrInvalidArgument, mode)
	}
	m.mu.Lock()
	m.streamMode = mode
	var changed []PlaybackEvent
	for _, z := range m.zones {
		if z.nextPreloaded || z.nextFailed {
			z.nextPreloaded = false
			z.nextFailed = false
			z.nextURI = ""
			z.revision++
			changed = append(changed, m.playbackEventLocked(z))
		}
	}
	m.mu.Unlock()

	for _, ev := range changed {
		m.emit("zone.playback", ev.ZoneID, ev)
	}
	return nil
}

// StreamMode returns the current Qobuz streaming mode.
func (m *Manager) StreamMode() StreamMode {
	m.mu.RLock()
	defer m.mu.RUnlock()
	return m.streamMode
}

// RegisterRenderer adds or updates a discovered UPnP renderer zone.
func (m *Manager) RegisterRenderer(ctrl upnp.Controller) {
	info := ctrl.Info()
	m.mu.Lock()
	if existing, ok := m.zones[info.ID]; ok {
		existing.ctrl = ctrl
		existing.setNextUnsupported = !info.SupportsSetNext
		existing.revision++
	} else {
		m.zones[info.ID] = &zone{
			ctrl:               ctrl,
			currentIndex:       -1,
			revision:           1,
			positionUpdatedAt:  time.Now().UTC(),
			setNextUnsupported: !info.SupportsSetNext,
			status: upnp.TransportStatus{
				State:     "STOPPED",
				HasVolume: info.HasVolume,
			},
		}
		m.zoneOrder = append(m.zoneOrder, info.ID)
	}
	ctx := m.runCtx
	gena := m.gena
	zones := m.zonesLocked()
	m.mu.Unlock()

	if ctx != nil && gena != nil {
		m.subscribeRendererGENA(ctx, gena, ctrl)
	}
	m.emit("zones.updated", "", map[string]any{"zones": zones})
}

// Renderers returns metadata for all registered renderers in stable order.
func (m *Manager) Renderers() []upnp.RendererInfo {
	m.mu.RLock()
	defer m.mu.RUnlock()
	out := make([]upnp.RendererInfo, 0, len(m.zoneOrder))
	for _, id := range m.zoneOrder {
		if z, ok := m.zones[id]; ok {
			out = append(out, z.ctrl.Info())
		}
	}
	return out
}

// Zones returns full snapshots for all registered zones in registration order.
func (m *Manager) Zones() []ZoneSnapshot {
	m.mu.RLock()
	defer m.mu.RUnlock()
	return m.zonesLocked()
}

func (m *Manager) zonesLocked() []ZoneSnapshot {
	out := make([]ZoneSnapshot, 0, len(m.zoneOrder))
	for _, id := range m.zoneOrder {
		if z, ok := m.zones[id]; ok {
			out = append(out, m.snapshotLocked(z))
		}
	}
	return out
}

// Zone returns a point-in-time snapshot of the specified zone.
func (m *Manager) Zone(zoneID string) (ZoneSnapshot, bool) {
	m.mu.RLock()
	defer m.mu.RUnlock()
	z, ok := m.zones[zoneID]
	if !ok {
		return ZoneSnapshot{}, false
	}
	return m.snapshotLocked(z), true
}

func (m *Manager) snapshotLocked(z *zone) ZoneSnapshot {
	info := z.ctrl.Info()
	qCopy := make([]Item, len(z.queue))
	copy(qCopy, z.queue)

	var cur *library.Track
	curItemID := ""
	if z.currentIndex >= 0 && z.currentIndex < len(z.queue) {
		tr := z.queue[z.currentIndex].Track
		cur = &tr
		curItemID = z.queue[z.currentIndex].ItemID
	}

	durSec := int(z.status.Duration.Seconds() + 0.5)
	if durSec == 0 && cur != nil {
		durSec = cur.DurationSec
	}

	eventsActive := m.gena != nil && m.gena.IsSubscribed(info.ID)
	updatedAt := z.positionUpdatedAt
	if updatedAt.IsZero() {
		updatedAt = time.Now().UTC()
	}

	return ZoneSnapshot{
		ZoneID:            info.ID,
		Renderer:          info,
		Revision:          z.revision,
		State:             NormalizePlaybackState(z.status.State),
		PositionSec:       int(z.status.Position.Seconds()),
		PositionUpdatedAt: updatedAt,
		DurationSec:       durSec,
		Volume:            z.status.Volume,
		Muted:             z.status.Muted,
		Queue:             qCopy,
		CurrentIndex:      z.currentIndex,
		CurrentItemID:     curItemID,
		CurrentTrack:      cur,
		GaplessArmed:      z.nextPreloaded,
		EventsActive:      eventsActive,
	}
}

func (m *Manager) playbackEventLocked(z *zone) PlaybackEvent {
	snap := m.snapshotLocked(z)
	return PlaybackEvent{
		ZoneID:            snap.ZoneID,
		Revision:          snap.Revision,
		State:             snap.State,
		PositionSec:       snap.PositionSec,
		PositionUpdatedAt: snap.PositionUpdatedAt,
		DurationSec:       snap.DurationSec,
		CurrentIndex:      snap.CurrentIndex,
		CurrentItemID:     snap.CurrentItemID,
		CurrentTrack:      snap.CurrentTrack,
		GaplessArmed:      snap.GaplessArmed,
		EventsActive:      snap.EventsActive,
	}
}

func (m *Manager) volumeEventLocked(z *zone) VolumeEvent {
	return VolumeEvent{
		ZoneID:   z.ctrl.Info().ID,
		Revision: z.revision,
		Volume:   z.status.Volume,
		Muted:    z.status.Muted,
	}
}

func (m *Manager) queueEventLocked(z *zone) ItemsEvent {
	snap := m.snapshotLocked(z)
	return ItemsEvent{
		ZoneID:        snap.ZoneID,
		Revision:      snap.Revision,
		CurrentIndex:  snap.CurrentIndex,
		CurrentItemID: snap.CurrentItemID,
		CurrentTrack:  snap.CurrentTrack,
		Items:         snap.Queue,
	}
}

func (m *Manager) lookupZone(zoneID string) (*zone, error) {
	m.mu.RLock()
	defer m.mu.RUnlock()
	if strings.TrimSpace(zoneID) == "" {
		return nil, fmt.Errorf("%w: zoneId is required", ErrInvalidArgument)
	}
	z, ok := m.zones[zoneID]
	if !ok {
		return nil, fmt.Errorf("%w: %q", ErrZoneNotFound, zoneID)
	}
	return z, nil
}

// Subscribe registers an SSE subscriber channel. Call the returned func on disconnect.
// When a slow subscriber's buffer overflows, its channel is closed so the SSE handler
// can notify the client to resync cleanly.
func (m *Manager) Subscribe() (<-chan Event, func()) {
	sub := &subscriber{ch: make(chan Event, 64)}
	m.subMu.Lock()
	m.subs[sub] = struct{}{}
	m.subMu.Unlock()

	unsub := func() {
		m.subMu.Lock()
		if _, exists := m.subs[sub]; exists {
			delete(m.subs, sub)
			close(sub.ch)
		}
		m.subMu.Unlock()
	}
	return sub.ch, unsub
}

// Publish broadcasts a global (non-zone-scoped) event such as settings.updated,
// qobuz.status, or library.updated to all SSE subscribers.
func (m *Manager) Publish(eventType string, data any) {
	m.emit(eventType, "", data)
}

func (m *Manager) emit(eventType, zoneID string, data any) {
	ev := Event{
		ID:     m.eventSeq.Add(1),
		Type:   eventType,
		ZoneID: zoneID,
		Data:   data,
	}
	m.subMu.Lock()
	for sub := range m.subs {
		select {
		case sub.ch <- ev:
		default:
			// Slow consumer lagged past buffer; close channel so the SSE handler
			// emits a resync event and reconnects cleanly instead of silently dropping.
			if sub.lagged.CompareAndSwap(false, true) {
				delete(m.subs, sub)
				close(sub.ch)
			}
		}
	}
	m.subMu.Unlock()
}

func (m *Manager) emitZoneError(zoneID string, err error) {
	code := "RENDERER_UNREACHABLE"
	retryable := true
	switch {
	case errors.Is(err, qobuz.ErrUnauthenticated):
		code = "QOBUZ_UNAUTHENTICATED"
		retryable = false
	case errors.Is(err, ErrCapabilityUnsupported):
		code = "CAPABILITY_UNSUPPORTED"
		retryable = false
	case errors.Is(err, ErrInvalidArgument):
		code = "INVALID_REQUEST"
		retryable = false
	}
	ev := ZoneErrorEvent{
		ID:     fmt.Sprintf("err_%d", m.eventSeq.Add(1)),
		ZoneID: zoneID,
		Error: ZoneErrorPayload{
			Code:      code,
			Message:   err.Error(),
			Retryable: retryable,
		},
		OccurredAt: time.Now().UTC(),
	}
	m.emit("zone.error", zoneID, ev)
}
