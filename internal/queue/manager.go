package queue

import (
	"context"
	"errors"
	"fmt"
	"strings"
	"sync"
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

// ZoneSnapshot is the JSON-serializable state of a single renderer zone.
type ZoneSnapshot struct {
	Renderer     upnp.RendererInfo `json:"renderer"`
	State        string            `json:"state"`
	PositionSec  int               `json:"positionSec"`
	DurationSec  int               `json:"durationSec"`
	Volume       int               `json:"volume"`
	Muted        bool              `json:"muted"`
	HasVolume    bool              `json:"hasVolume"`
	Queue        []library.Track   `json:"queue"`
	CurrentIndex int               `json:"currentIndex"`
	CurrentTrack *library.Track    `json:"currentTrack,omitempty"`
	GaplessArmed bool              `json:"gaplessArmed"`
	EventsActive bool              `json:"eventsActive"`
	StreamMode   StreamMode        `json:"streamMode"`
	ActiveURL    string            `json:"activeUrl,omitempty"`
	LastError    string            `json:"lastError,omitempty"`
}

// Event is pushed to SSE subscribers whenever renderer/queue state changes.
type Event struct {
	Type string `json:"type"`
	Data any    `json:"data"`
}

type zone struct {
	ctrl               upnp.Controller
	queue              []library.Track
	currentIndex       int
	status             upnp.TransportStatus
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
	lastError          string
}

// Manager coordinates discovered renderers, per-zone queues, gapless preloading,
// GENA event updates, and state broadcasts to connected remote clients.
type Manager struct {
	mu            sync.RWMutex
	zones         map[string]*zone
	zoneOrder     []string
	activeID      string
	streamMode    StreamMode
	httpPort      int
	publicBaseURL string
	qobuz         *qobuz.Client
	gena          *upnp.GENAClient
	runCtx        context.Context

	subMu sync.Mutex
	subs  map[chan Event]struct{}

	stopCh   chan struct{}
	stopOnce sync.Once
}

// NewManager creates a playback/queue manager.
func NewManager(httpPort int, publicBaseURL string, qb *qobuz.Client) *Manager {
	return &Manager{
		zones:         make(map[string]*zone),
		streamMode:    StreamModeDirect,
		httpPort:      httpPort,
		publicBaseURL: strings.TrimRight(strings.TrimSpace(publicBaseURL), "/"),
		qobuz:         qb,
		subs:          make(map[chan Event]struct{}),
		stopCh:        make(chan struct{}),
	}
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

	for _, c := range ctrls {
		m.watchZoneGENA(ctx, c)
	}
}

// Start begins background transport polling and GENA event subscriptions for renderers.
func (m *Manager) Start(ctx context.Context) {
	m.mu.Lock()
	m.runCtx = ctx
	gena := m.gena
	var ctrls []upnp.Controller
	if gena != nil {
		for _, id := range m.zoneOrder {
			if z, ok := m.zones[id]; ok {
				ctrls = append(ctrls, z.ctrl)
			}
		}
	}
	m.mu.Unlock()

	for _, c := range ctrls {
		m.watchZoneGENA(ctx, c)
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

// SetStreamMode switches Qobuz handoff between "direct" and "relay".
func (m *Manager) SetStreamMode(mode StreamMode) error {
	if mode != StreamModeDirect && mode != StreamModeRelay {
		return fmt.Errorf("invalid stream mode %q (want direct or relay)", mode)
	}
	m.mu.Lock()
	m.streamMode = mode
	for _, z := range m.zones {
		// Invalidate preloaded next URI so it re-resolves with the new mode.
		z.nextPreloaded = false
		z.nextURI = ""
		z.nextFailed = false
	}
	m.mu.Unlock()
	m.broadcastActive()
	return nil
}

// StreamMode returns the current Qobuz delivery mode.
func (m *Manager) StreamMode() StreamMode {
	m.mu.RLock()
	defer m.mu.RUnlock()
	return m.streamMode
}

// RegisterRenderer adds or updates a discovered UPnP renderer.
func (m *Manager) RegisterRenderer(ctrl upnp.Controller) {
	info := ctrl.Info()
	m.mu.Lock()
	if existing, ok := m.zones[info.ID]; ok {
		existing.ctrl = ctrl
		if !info.SupportsSetNext {
			existing.setNextUnsupported = true
		}
	} else {
		m.zones[info.ID] = &zone{
			ctrl:               ctrl,
			currentIndex:       -1,
			setNextUnsupported: !info.SupportsSetNext,
			status: upnp.TransportStatus{
				State:     "STOPPED",
				HasVolume: info.HasVolume,
			},
		}
		m.zoneOrder = append(m.zoneOrder, info.ID)
	}
	if m.activeID == "" {
		m.activeID = info.ID
	}
	runCtx := m.runCtx
	m.mu.Unlock()

	if runCtx != nil {
		m.watchZoneGENA(runCtx, ctrl)
	}
	m.broadcast(Event{Type: "renderers", Data: m.Renderers()})
}

func (m *Manager) watchZoneGENA(ctx context.Context, ctrl upnp.Controller) {
	m.mu.RLock()
	gena := m.gena
	m.mu.RUnlock()
	if gena == nil {
		return
	}
	es, ok := ctrl.(upnp.EventSource)
	if !ok {
		return
	}
	avURL, rcURL := es.EventURLs()
	if avURL == "" && rcURL == "" {
		return
	}
	gena.WatchRenderer(ctx, ctrl.Info().ID, m.baseURLFor(ctrl), avURL, rcURL)
}

// Renderers lists all registered renderers in stable order.
func (m *Manager) Renderers() []upnp.RendererInfo {
	m.mu.RLock()
	defer m.mu.RUnlock()
	out := make([]upnp.RendererInfo, 0, len(m.zoneOrder))
	for _, id := range m.zoneOrder {
		if z, ok := m.zones[id]; ok {
			info := z.ctrl.Info()
			if z.setNextUnsupported {
				info.SupportsSetNext = false
			}
			out = append(out, info)
		}
	}
	return out
}

// ActiveRendererID returns the currently selected renderer UDN.
func (m *Manager) ActiveRendererID() string {
	m.mu.RLock()
	defer m.mu.RUnlock()
	return m.activeID
}

// SelectRenderer sets the active renderer by UDN.
func (m *Manager) SelectRenderer(id string) error {
	m.mu.Lock()
	if _, ok := m.zones[id]; !ok {
		m.mu.Unlock()
		return fmt.Errorf("renderer %q not found", id)
	}
	m.activeID = id
	runCtx := m.runCtx
	m.mu.Unlock()

	go func() {
		baseCtx := runCtx
		if baseCtx == nil {
			baseCtx = context.Background()
		}
		ctx, cancel := context.WithTimeout(baseCtx, 3*time.Second)
		defer cancel()
		m.pollOne(ctx, id)
	}()
	m.broadcastActive()
	return nil
}

// Snapshot returns the current state of the specified renderer (or the active
// renderer when id is empty).
func (m *Manager) Snapshot(id string) (ZoneSnapshot, bool) {
	m.mu.RLock()
	defer m.mu.RUnlock()
	if id == "" {
		id = m.activeID
	}
	z, ok := m.zones[id]
	if !ok {
		return ZoneSnapshot{}, false
	}
	return m.snapshotLocked(z), true
}

func (m *Manager) snapshotLocked(z *zone) ZoneSnapshot {
	q := make([]library.Track, len(z.queue))
	copy(q, z.queue)
	var cur *library.Track
	if z.currentIndex >= 0 && z.currentIndex < len(z.queue) {
		t := z.queue[z.currentIndex]
		cur = &t
	}
	durSec := int(z.status.Duration.Seconds() + 0.5)
	if durSec == 0 && cur != nil {
		durSec = cur.DurationSec
	}
	eventsActive := false
	if m.gena != nil {
		eventsActive = m.gena.IsSubscribed(z.ctrl.Info().ID)
	}
	info := z.ctrl.Info()
	if z.setNextUnsupported {
		info.SupportsSetNext = false
	}
	return ZoneSnapshot{
		Renderer:     info,
		State:        z.status.State,
		PositionSec:  int(z.status.Position.Seconds() + 0.5),
		DurationSec:  durSec,
		Volume:       z.status.Volume,
		Muted:        z.status.Muted,
		HasVolume:    info.HasVolume,
		Queue:        q,
		CurrentIndex: z.currentIndex,
		CurrentTrack: cur,
		GaplessArmed: z.nextPreloaded,
		EventsActive: eventsActive,
		StreamMode:   m.streamMode,
		ActiveURL:    z.currentURI,
		LastError:    z.lastError,
	}
}

// Subscribe registers a channel to receive live state events.
func (m *Manager) Subscribe() (<-chan Event, func()) {
	ch := make(chan Event, 32)
	m.subMu.Lock()
	m.subs[ch] = struct{}{}
	m.subMu.Unlock()
	return ch, func() {
		m.subMu.Lock()
		delete(m.subs, ch)
		m.subMu.Unlock()
	}
}

func (m *Manager) resolveZone(id string) (*zone, error) {
	m.mu.RLock()
	defer m.mu.RUnlock()
	if id == "" {
		id = m.activeID
	}
	z, ok := m.zones[id]
	if !ok {
		return nil, errors.New("no UPnP renderer selected or discovered")
	}
	return z, nil
}

func (m *Manager) recordZoneError(z *zone, err error) {
	m.mu.Lock()
	z.lastError = err.Error()
	m.mu.Unlock()
	m.broadcastZone(z)
}

func (m *Manager) broadcastActive() {
	if snap, ok := m.Snapshot(""); ok {
		m.broadcast(Event{Type: "state", Data: snap})
	}
}

func (m *Manager) broadcastZone(z *zone) {
	m.mu.RLock()
	id := z.ctrl.Info().ID
	isActive := id == m.activeID
	snap := m.snapshotLocked(z)
	m.mu.RUnlock()
	if isActive {
		m.broadcast(Event{Type: "state", Data: snap})
	}
}

func (m *Manager) broadcast(ev Event) {
	m.subMu.Lock()
	defer m.subMu.Unlock()
	for ch := range m.subs {
		select {
		case ch <- ev:
		default:
			// Drop slow consumer event to avoid blocking playback control.
		}
	}
}
