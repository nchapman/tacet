package queue

import (
	"context"
	"errors"
	"fmt"
	"io"
	"slices"
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

// RepeatMode controls queue repeat behavior on a zone.
type RepeatMode string

// Supported repeat modes.
const (
	RepeatOff RepeatMode = "off"
	RepeatAll RepeatMode = "all"
	RepeatOne RepeatMode = "one"
)

// ParseRepeatMode validates and normalizes a repeat mode string.
func ParseRepeatMode(raw string) (RepeatMode, bool) {
	switch strings.ToLower(strings.TrimSpace(raw)) {
	case "", "off":
		return RepeatOff, true
	case "all":
		return RepeatAll, true
	case "one":
		return RepeatOne, true
	default:
		return "", false
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
	AudioQuality      AudioQuality      `json:"audioQuality"`
	ActiveQuality     AudioQuality      `json:"activeQuality,omitempty"`
	Shuffle           bool              `json:"shuffle"`
	Repeat            RepeatMode        `json:"repeat"`
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
	ServerTime        time.Time      `json:"serverTime"`
	DurationSec       int            `json:"durationSec"`
	CurrentIndex      int            `json:"currentIndex"`
	CurrentItemID     string         `json:"currentItemId,omitempty"`
	CurrentTrack      *library.Track `json:"currentTrack,omitempty"`
	AudioQuality      AudioQuality   `json:"audioQuality"`
	ActiveQuality     AudioQuality   `json:"activeQuality,omitempty"`
	Shuffle           bool           `json:"shuffle"`
	Repeat            RepeatMode     `json:"repeat"`
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
	opMu               sync.Mutex
	ctrl               upnp.Controller
	queue              []Item
	originalQueue      []Item
	currentIndex       int
	shuffle            bool
	repeat             RepeatMode
	revision           uint64
	status             upnp.TransportStatus
	positionUpdatedAt  time.Time
	lastPlaybackEmit   time.Time
	awaitingAudio      bool
	currentURI         string
	nextURI            string
	nextIndex          int
	audioQuality       AudioQuality
	activeQuality      AudioQuality
	nextQuality        AudioQuality
	nextPreloaded      bool
	nextPreloading     bool
	nextFailed         bool
	setNextUnsupported bool
	polling            bool
	lastPoll           time.Time
	wasPlaying         bool
	userStopped        bool
	loadedAt           time.Time
	playRecordedItemID string
	generation         uint64
	preloadGen         uint64
}

// invalidatePreloadLocked resets all gapless preload state and bumps preloadGen
// so any concurrent in-flight preloadNext() discards its stale SetNextURI.
// Returns true if a previously preloaded URI should be cleared on the renderer.
func invalidatePreloadLocked(z *zone) bool {
	wasPreloaded := z.nextPreloaded
	z.preloadGen++
	z.nextPreloaded = false
	z.nextPreloading = false
	z.nextFailed = false
	z.nextURI = ""
	z.nextQuality = ""
	z.nextIndex = -1
	return wasPreloaded
}

// ErrorClassifier maps external controller errors onto structured zone.error
// SSE codes and retryable flags.
type ErrorClassifier func(err error) (code string, retryable bool, matched bool)

// SavedZoneQueue is the persisted per-zone queue snapshot stored in SQLite so
// queues survive server restarts and resync with live speaker playback.
type SavedZoneQueue struct {
	Tracks       []library.Track `json:"tracks"`
	CurrentIndex int             `json:"currentIndex"`
	Repeat       RepeatMode      `json:"repeat,omitempty"`
	Shuffle      bool            `json:"shuffle,omitempty"`
}

const maxPersistedZoneTracks = 500

// Manager coordinates discovered renderers, per-zone queues, gapless preloading,
// GENA event updates, and split SSE broadcasts to connected remote clients.
type Manager struct {
	mu                 sync.RWMutex
	zones              map[string]*zone
	zoneOrder          []string
	savedQualities     map[string]AudioQuality
	savedQueues        map[string]SavedZoneQueue
	transcodeAvailable bool
	streamMode         StreamMode
	httpPort           int
	publicBaseURL      string
	qobuz              *qobuz.Client
	gena               *upnp.GENAClient
	errorClassifier    ErrorClassifier
	onTrackPlayed      func(library.Track)
	onQueuePersist     func(map[string]SavedZoneQueue)
	persistMu          sync.Mutex
	persistWriteMu     sync.Mutex
	persistTimer       *time.Timer
	persistDirty       bool
	persistClosed      bool
	runCtx             context.Context

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
		zones:          make(map[string]*zone),
		savedQualities: make(map[string]AudioQuality),
		savedQueues:    make(map[string]SavedZoneQueue),
		streamMode:     StreamModeDirect,
		httpPort:       httpPort,
		publicBaseURL:  strings.TrimRight(strings.TrimSpace(publicBaseURL), "/"),
		qobuz:          qb,
		subs:           make(map[*subscriber]struct{}),
		stopCh:         make(chan struct{}),
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

// SetErrorClassifier installs a hook used to map external controller errors
// onto zone.error SSE codes.
func (m *Manager) SetErrorClassifier(fn ErrorClassifier) {
	m.mu.Lock()
	m.errorClassifier = fn
	m.mu.Unlock()
}

// SetOnTrackPlayed installs a hook invoked once per queue item after the track
// has actively played past the scrobble threshold (30s or 50% duration).
func (m *Manager) SetOnTrackPlayed(fn func(library.Track)) {
	m.mu.Lock()
	m.onTrackPlayed = fn
	m.mu.Unlock()
}

// Done returns a channel closed when Manager.Close is called.
func (m *Manager) Done() <-chan struct{} {
	return m.stopCh
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
	evSrc, ok := ctrl.(upnp.EventSource)
	if !ok {
		return
	}
	avURL, rcURL := evSrc.EventURLs()
	if avURL == "" && rcURL == "" {
		return
	}
	gena.WatchRenderer(ctx, ctrl.Info().ID, m.baseURLFor(ctrl), avURL, rcURL)
}

// SetStreamMode switches between Handoff (direct Qobuz URL) and Relay mode.
func (m *Manager) SetStreamMode(mode StreamMode) error {
	if mode != StreamModeDirect && mode != StreamModeRelay {
		return fmt.Errorf("%w: unsupported stream mode %q (use direct or relay)", ErrInvalidArgument, mode)
	}
	m.mu.Lock()
	m.streamMode = mode
	var changed []PlaybackEvent
	var toClear []upnp.Controller
	for _, z := range m.zones {
		if z.nextPreloaded || z.nextFailed {
			if z.nextPreloaded {
				toClear = append(toClear, z.ctrl)
			}
			z.nextPreloaded = false
			z.nextFailed = false
			z.nextURI = ""
			z.revision++
			changed = append(changed, m.playbackEventLocked(z))
		}
	}
	m.mu.Unlock()

	for _, ctrl := range toClear {
		m.clearNextPreloadAsync(ctrl)
	}
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

// SetTranscodeAvailable records whether server-side ffmpeg+libopus transcoding is available.
func (m *Manager) SetTranscodeAvailable(available bool) {
	m.mu.Lock()
	m.transcodeAvailable = available
	m.mu.Unlock()
}

// TranscodeAvailable reports whether server-side transcoding is available.
func (m *Manager) TranscodeAvailable() bool {
	m.mu.RLock()
	defer m.mu.RUnlock()
	return m.transcodeAvailable
}

// LoadZoneQualities seeds saved per-endpoint quality settings restored from persistence.
func (m *Manager) LoadZoneQualities(saved map[string]AudioQuality) {
	m.mu.Lock()
	defer m.mu.Unlock()
	for id, raw := range saved {
		if q, ok := NormalizeAudioQuality(string(raw)); ok {
			m.savedQualities[id] = q
			if z, exists := m.zones[id]; exists {
				z.audioQuality = q
			}
		}
	}
}

// LoadZoneQueues pre-populates persisted per-zone queues loaded from SQLite
// before renderers are discovered on startup.
func (m *Manager) LoadZoneQueues(saved map[string]SavedZoneQueue) {
	m.mu.Lock()
	defer m.mu.Unlock()
	for id, sq := range saved {
		if len(sq.Tracks) == 0 {
			continue
		}
		cp := SavedZoneQueue{
			Tracks:       append([]library.Track(nil), sq.Tracks...),
			CurrentIndex: sq.CurrentIndex,
			Repeat:       sq.Repeat,
			Shuffle:      sq.Shuffle,
		}
		m.savedQueues[id] = cp
		if z, ok := m.zones[id]; ok && len(z.queue) == 0 {
			m.hydrateZoneQueueLocked(z, cp)
		}
	}
}

func (m *Manager) hydrateZoneQueueLocked(z *zone, sq SavedZoneQueue) {
	items := make([]Item, len(sq.Tracks))
	for i, tr := range sq.Tracks {
		items[i] = Item{ItemID: m.nextItemID(), Track: tr}
	}
	idx := sq.CurrentIndex
	if idx < 0 || idx >= len(items) {
		idx = 0
	}
	z.queue = items
	z.currentIndex = idx
	if rep, ok := ParseRepeatMode(string(sq.Repeat)); ok {
		z.repeat = rep
	}
	z.shuffle = sq.Shuffle
	// Zero loadedAt so the very first Status() poll snaps currentIndex to live playback.
	z.loadedAt = time.Time{}
}

// SavedZoneQueues returns a snapshot of all non-empty zone queues for persistence.
func (m *Manager) SavedZoneQueues() map[string]SavedZoneQueue {
	m.mu.RLock()
	defer m.mu.RUnlock()
	out := make(map[string]SavedZoneQueue, len(m.savedQueues)+len(m.zones))
	for id, sq := range m.savedQueues {
		if len(sq.Tracks) > 0 {
			out[id] = sq
		}
	}
	for id, z := range m.zones {
		// Ephemeral browser players are not persisted across restarts.
		if z.ctrl.Info().Kind == "player" {
			delete(out, id)
			continue
		}
		if sq, ok := snapshotSavedQueueLocked(z); ok {
			out[id] = sq
		} else {
			delete(out, id)
		}
	}
	return out
}

func snapshotSavedQueueLocked(z *zone) (SavedZoneQueue, bool) {
	if len(z.queue) == 0 {
		return SavedZoneQueue{}, false
	}
	n := len(z.queue)
	if n > maxPersistedZoneTracks {
		n = maxPersistedZoneTracks
	}
	tracks := make([]library.Track, n)
	for i := 0; i < n; i++ {
		tracks[i] = z.queue[i].Track
	}
	idx := z.currentIndex
	if idx < 0 {
		idx = 0
	}
	if idx >= n {
		idx = n - 1
	}
	return SavedZoneQueue{
		Tracks:       tracks,
		CurrentIndex: idx,
		Repeat:       z.repeat,
		Shuffle:      z.shuffle,
	}, true
}

// SetQueuePersistFunc registers a callback invoked (debounced) whenever a zone's
// queue, currentIndex, repeat, or shuffle changes.
func (m *Manager) SetQueuePersistFunc(fn func(map[string]SavedZoneQueue)) {
	m.mu.Lock()
	m.onQueuePersist = fn
	m.mu.Unlock()
}

func (m *Manager) scheduleQueuePersist() {
	m.mu.RLock()
	hasFn := m.onQueuePersist != nil
	m.mu.RUnlock()
	if !hasFn {
		return
	}
	m.persistMu.Lock()
	if m.persistClosed {
		m.persistMu.Unlock()
		return
	}
	m.persistDirty = true
	if m.persistTimer == nil {
		m.persistTimer = time.AfterFunc(500*time.Millisecond, func() {
			m.flushQueuePersist(false)
		})
	}
	m.persistMu.Unlock()
}

// FlushQueuePersist immediately persists any pending zone queue changes and
// waits for any in-flight persistence write to finish. Safe to call on shutdown
// before closing SQLite.
func (m *Manager) FlushQueuePersist() {
	m.flushQueuePersist(true)
}

func (m *Manager) flushQueuePersist(closing bool) {
	m.persistWriteMu.Lock()
	defer m.persistWriteMu.Unlock()

	m.persistMu.Lock()
	if closing {
		m.persistClosed = true
	}
	if m.persistTimer != nil {
		m.persistTimer.Stop()
		m.persistTimer = nil
	}
	dirty := m.persistDirty
	m.persistDirty = false
	m.persistMu.Unlock()
	if !dirty {
		return
	}
	m.mu.RLock()
	fn := m.onQueuePersist
	m.mu.RUnlock()
	if fn != nil {
		fn(m.SavedZoneQueues())
	}
}

// SavedZoneQualities returns a copy of all non-default per-endpoint quality settings for persistence.
func (m *Manager) SavedZoneQualities() map[string]AudioQuality {
	m.mu.RLock()
	defer m.mu.RUnlock()
	out := make(map[string]AudioQuality, len(m.savedQualities))
	for id, q := range m.savedQualities {
		if q != "" && q != AudioQualityOriginal {
			out[id] = q
		}
	}
	return out
}

// SetZoneQuality updates the audio quality delivered to a specific zone (applies on the next track).
func (m *Manager) SetZoneQuality(zoneID string, quality AudioQuality) (ZoneSnapshot, error) {
	q, ok := NormalizeAudioQuality(string(quality))
	if !ok {
		return ZoneSnapshot{}, fmt.Errorf("%w: unsupported audio quality %q", ErrInvalidArgument, quality)
	}
	m.mu.Lock()
	z, exists := m.zones[zoneID]
	if !exists {
		m.mu.Unlock()
		return ZoneSnapshot{}, fmt.Errorf("%w: %s", ErrZoneNotFound, zoneID)
	}
	if q.NeedsTranscode() && !m.transcodeAvailable {
		m.mu.Unlock()
		return ZoneSnapshot{}, fmt.Errorf("%w: ffmpeg with libopus is required for transcoding", ErrCapabilityUnsupported)
	}
	if q.IsOpus() && !RendererSupportsLossy(z.ctrl.Info()) {
		m.mu.Unlock()
		return ZoneSnapshot{}, fmt.Errorf("%w: %s does not advertise Opus or AAC support", ErrCapabilityUnsupported, z.ctrl.Info().Name)
	}
	z.audioQuality = q
	if q == AudioQualityOriginal {
		delete(m.savedQualities, zoneID)
	} else {
		m.savedQualities[zoneID] = q
	}
	var toClear upnp.Controller
	if z.nextPreloaded || z.nextPreloading || z.nextFailed {
		if invalidatePreloadLocked(z) {
			toClear = z.ctrl
		}
	}
	z.revision++
	snap := m.snapshotLocked(z)
	ev := m.playbackEventLocked(z)
	m.mu.Unlock()

	if toClear != nil {
		m.clearNextPreloadAsync(toClear)
	}
	m.emit("zone.playback", zoneID, ev)
	return snap, nil
}

// RegisterRenderer adds or updates a discovered UPnP renderer zone.
// Re-registering an unchanged UPnP renderer (same Location, Name, and Host)
// quietly refreshes its controller without spamming zones.updated or GENA.
func (m *Manager) RegisterRenderer(ctrl upnp.Controller) {
	info := ctrl.Info()
	m.mu.Lock()
	unchanged := false
	if existing, ok := m.zones[info.ID]; ok {
		prev := existing.ctrl.Info()
		if prev.Kind != "player" && info.Kind != "player" &&
			prev.Location != "" &&
			prev.Kind == info.Kind &&
			prev.Location == info.Location &&
			prev.Name == info.Name &&
			prev.Host == info.Host &&
			prev.HasVolume == info.HasVolume &&
			prev.SupportsSetNext == info.SupportsSetNext &&
			prev.SupportsSeek == info.SupportsSeek &&
			prev.SupportsEvents == info.SupportsEvents &&
			prev.SupportsQueue == info.SupportsQueue &&
			slices.Equal(prev.SinkProtocols, info.SinkProtocols) {
			unchanged = true
		}
		if !unchanged {
			existing.ctrl = ctrl
			existing.setNextUnsupported = !info.SupportsSetNext
			existing.revision++
		}
	} else {
		savedQ := m.savedQualities[info.ID]
		if savedQ == "" {
			savedQ = AudioQualityOriginal
		}
		z := &zone{
			ctrl:               ctrl,
			currentIndex:       -1,
			revision:           1,
			audioQuality:       savedQ,
			positionUpdatedAt:  time.Now().UTC(),
			setNextUnsupported: !info.SupportsSetNext,
			status: upnp.TransportStatus{
				State:     "STOPPED",
				HasVolume: info.HasVolume,
			},
		}
		if sq, ok := m.savedQueues[info.ID]; ok && len(sq.Tracks) > 0 {
			m.hydrateZoneQueueLocked(z, sq)
		}
		m.zones[info.ID] = z
		m.zoneOrder = append(m.zoneOrder, info.ID)
	}
	if unchanged {
		m.mu.Unlock()
		return
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

// UPnPRenderers returns info snapshots for all currently registered UPnP renderers
// (excluding HTTP/SSE browser players and Google Cast endpoints).
func (m *Manager) UPnPRenderers() []upnp.RendererInfo {
	m.mu.RLock()
	defer m.mu.RUnlock()
	out := make([]upnp.RendererInfo, 0, len(m.zoneOrder))
	for _, id := range m.zoneOrder {
		if z, ok := m.zones[id]; ok {
			info := z.ctrl.Info()
			if info.Kind != "player" && info.Kind != "cast" {
				out = append(out, info)
			}
		}
	}
	return out
}

// CastRenderers returns info snapshots for all currently registered Google Cast endpoints.
func (m *Manager) CastRenderers() []upnp.RendererInfo {
	m.mu.RLock()
	defer m.mu.RUnlock()
	out := make([]upnp.RendererInfo, 0, len(m.zoneOrder))
	for _, id := range m.zoneOrder {
		if z, ok := m.zones[id]; ok {
			info := z.ctrl.Info()
			if info.Kind == "cast" {
				out = append(out, info)
			}
		}
	}
	return out
}

// UPnPHosts returns the set of IP hosts currently served by UPnP renderers.
func (m *Manager) UPnPHosts() map[string]struct{} {
	m.mu.RLock()
	defer m.mu.RUnlock()
	hosts := make(map[string]struct{}, len(m.zoneOrder))
	for _, id := range m.zoneOrder {
		if z, ok := m.zones[id]; ok {
			info := z.ctrl.Info()
			if info.Kind != "player" && info.Kind != "cast" && info.Host != "" {
				hosts[info.Host] = struct{}{}
			}
		}
	}
	return hosts
}

// IsZoneActive reports whether zoneID is currently playing or buffering.
func (m *Manager) IsZoneActive(zoneID string) bool {
	m.mu.RLock()
	defer m.mu.RUnlock()
	z, ok := m.zones[zoneID]
	if !ok {
		return false
	}
	st := strings.ToUpper(strings.TrimSpace(z.status.State))
	return st == "PLAYING" || st == "TRANSITIONING"
}

// UnregisterRenderer removes a renderer zone and broadcasts zones.updated.
func (m *Manager) UnregisterRenderer(zoneID string) bool {
	m.mu.Lock()
	removed, ok := m.zones[zoneID]
	if !ok {
		m.mu.Unlock()
		return false
	}
	if removed.ctrl.Info().Kind != "player" {
		if sq, ok := snapshotSavedQueueLocked(removed); ok {
			m.savedQueues[zoneID] = sq
		} else {
			delete(m.savedQueues, zoneID)
		}
	}
	delete(m.zones, zoneID)
	order := make([]string, 0, len(m.zoneOrder))
	for _, id := range m.zoneOrder {
		if id != zoneID {
			order = append(order, id)
		}
	}
	m.zoneOrder = order
	zones := m.zonesLocked()
	m.mu.Unlock()

	if closer, ok := removed.ctrl.(io.Closer); ok {
		_ = closer.Close()
	}
	m.emit("zones.updated", "", map[string]any{"zones": zones})
	return true
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
	if !eventsActive {
		if ps, ok := z.ctrl.(upnp.PushStatusController); ok && ps.PushConnected() {
			eventsActive = true
		}
	}
	updatedAt := z.positionUpdatedAt
	if updatedAt.IsZero() {
		updatedAt = time.Now().UTC()
	}

	quality := z.audioQuality
	if quality == "" {
		quality = AudioQualityOriginal
	}

	repeat := z.repeat
	if repeat == "" {
		repeat = RepeatOff
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
		AudioQuality:      quality,
		ActiveQuality:     z.activeQuality,
		Shuffle:           z.shuffle,
		Repeat:            repeat,
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
		ServerTime:        time.Now().UTC(),
		DurationSec:       snap.DurationSec,
		CurrentIndex:      snap.CurrentIndex,
		CurrentItemID:     snap.CurrentItemID,
		CurrentTrack:      snap.CurrentTrack,
		AudioQuality:      snap.AudioQuality,
		ActiveQuality:     snap.ActiveQuality,
		Shuffle:           snap.Shuffle,
		Repeat:            snap.Repeat,
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
	if eventType == "zone.queue" {
		m.scheduleQueuePersist()
	}
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
	m.mu.RLock()
	classifier := m.errorClassifier
	m.mu.RUnlock()

	code := "RENDERER_UNREACHABLE"
	retryable := true
	if classifier != nil {
		if c, r, ok := classifier(err); ok {
			code = c
			retryable = r
		}
	}
	if code == "RENDERER_UNREACHABLE" {
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
