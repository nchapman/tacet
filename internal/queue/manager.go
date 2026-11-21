package queue

import (
	"context"
	"errors"
	"fmt"
	"log"
	"strconv"
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
	m.mu.Unlock()

	go func() {
		ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
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

// PlayTracks replaces the renderer's queue and begins playing at startIndex.
func (m *Manager) PlayTracks(ctx context.Context, rendererID string, tracks []library.Track, startIndex int) error {
	if len(tracks) == 0 {
		return errors.New("no tracks provided")
	}
	if startIndex < 0 || startIndex >= len(tracks) {
		startIndex = 0
	}
	z, err := m.resolveZone(rendererID)
	if err != nil {
		return err
	}

	m.mu.Lock()
	z.queue = append([]library.Track(nil), tracks...)
	z.currentIndex = startIndex
	z.generation++
	gen := z.generation
	z.nextPreloaded = false
	z.nextURI = ""
	z.nextFailed = false
	z.wasPlaying = false
	z.userStopped = false
	z.lastError = ""
	m.mu.Unlock()

	return m.loadAndPlay(ctx, z, startIndex, gen)
}

// Enqueue appends tracks to the renderer's queue, starting playback if idle.
func (m *Manager) Enqueue(ctx context.Context, rendererID string, tracks []library.Track) error {
	if len(tracks) == 0 {
		return nil
	}
	z, err := m.resolveZone(rendererID)
	if err != nil {
		return err
	}

	m.mu.Lock()
	wasEmpty := len(z.queue) == 0 || z.currentIndex < 0
	z.queue = append(z.queue, tracks...)
	var startIdx int
	var gen uint64
	if wasEmpty {
		z.currentIndex = 0
		startIdx = 0
		z.generation++
		gen = z.generation
		z.userStopped = false
		z.wasPlaying = false
	} else {
		// Allow next-track preload to run if we were on the final track.
		if !z.nextPreloaded {
			z.nextFailed = false
		}
	}
	m.mu.Unlock()

	if wasEmpty {
		return m.loadAndPlay(ctx, z, startIdx, gen)
	}
	m.broadcastZone(z)
	return nil
}

// PlayIndex jumps directly to track index idx in the queue.
func (m *Manager) PlayIndex(ctx context.Context, rendererID string, idx int) error {
	z, err := m.resolveZone(rendererID)
	if err != nil {
		return err
	}
	m.mu.Lock()
	if idx < 0 || idx >= len(z.queue) {
		m.mu.Unlock()
		return fmt.Errorf("queue index %d out of range", idx)
	}
	z.currentIndex = idx
	z.generation++
	gen := z.generation
	z.nextPreloaded = false
	z.nextURI = ""
	z.nextFailed = false
	z.wasPlaying = false
	z.userStopped = false
	z.lastError = ""
	m.mu.Unlock()

	return m.loadAndPlay(ctx, z, idx, gen)
}

// RemoveAt deletes an entry from the queue.
func (m *Manager) RemoveAt(ctx context.Context, rendererID string, idx int) error {
	z, err := m.resolveZone(rendererID)
	if err != nil {
		return err
	}
	m.mu.Lock()
	if idx < 0 || idx >= len(z.queue) {
		m.mu.Unlock()
		return fmt.Errorf("queue index %d out of range", idx)
	}
	removingCurrent := idx == z.currentIndex
	removingNext := idx == z.currentIndex+1
	z.queue = append(z.queue[:idx], z.queue[idx+1:]...)

	if idx < z.currentIndex {
		z.currentIndex--
	}
	if removingNext {
		z.nextPreloaded = false
		z.nextURI = ""
		z.nextFailed = false
	}
	var shouldStop, shouldLoad bool
	var loadIdx int
	var gen uint64
	if removingCurrent {
		z.generation++
		gen = z.generation
		z.nextPreloaded = false
		z.nextURI = ""
		z.nextFailed = false
		if len(z.queue) == 0 {
			z.currentIndex = -1
			z.userStopped = true
			shouldStop = true
		} else {
			if z.currentIndex >= len(z.queue) {
				z.currentIndex = len(z.queue) - 1
			}
			loadIdx = z.currentIndex
			shouldLoad = true
		}
	}
	ctrl := z.ctrl
	m.mu.Unlock()

	if shouldStop {
		_ = ctrl.Stop(ctx)
	} else if shouldLoad {
		return m.loadAndPlay(ctx, z, loadIdx, gen)
	}
	m.broadcastZone(z)
	return nil
}

// ClearQueue stops playback and empties the queue.
func (m *Manager) ClearQueue(ctx context.Context, rendererID string) error {
	z, err := m.resolveZone(rendererID)
	if err != nil {
		return err
	}
	m.mu.Lock()
	z.queue = nil
	z.currentIndex = -1
	z.generation++
	z.currentURI = ""
	z.nextURI = ""
	z.nextPreloaded = false
	z.nextFailed = false
	z.wasPlaying = false
	z.userStopped = true
	z.status.State = "STOPPED"
	z.status.Position = 0
	ctrl := z.ctrl
	m.mu.Unlock()

	err = ctrl.Stop(ctx)
	m.broadcastZone(z)
	return err
}

// Play resumes playback (or starts track 0 if stopped with a populated queue).
func (m *Manager) Play(ctx context.Context, rendererID string) error {
	z, err := m.resolveZone(rendererID)
	if err != nil {
		return err
	}
	m.mu.Lock()
	needLoad := (z.status.State == "STOPPED" || z.status.State == "NO_MEDIA_PRESENT" || z.currentURI == "") && len(z.queue) > 0
	idx := z.currentIndex
	if idx < 0 && len(z.queue) > 0 {
		idx = 0
		z.currentIndex = 0
	}
	var gen uint64
	if needLoad {
		z.generation++
		gen = z.generation
		z.userStopped = false
	}
	ctrl := z.ctrl
	m.mu.Unlock()

	if needLoad {
		return m.loadAndPlay(ctx, z, idx, gen)
	}
	if err := ctrl.Play(ctx); err != nil {
		return err
	}
	m.mu.Lock()
	z.userStopped = false
	z.status.State = "PLAYING"
	m.mu.Unlock()
	m.broadcastZone(z)
	return nil
}

// Pause pauses playback on the renderer.
func (m *Manager) Pause(ctx context.Context, rendererID string) error {
	z, err := m.resolveZone(rendererID)
	if err != nil {
		return err
	}
	if err := z.ctrl.Pause(ctx); err != nil {
		return err
	}
	m.mu.Lock()
	z.status.State = "PAUSED_PLAYBACK"
	m.mu.Unlock()
	m.broadcastZone(z)
	return nil
}

// Stop halts playback without clearing the queue.
func (m *Manager) Stop(ctx context.Context, rendererID string) error {
	z, err := m.resolveZone(rendererID)
	if err != nil {
		return err
	}
	m.mu.Lock()
	z.userStopped = true
	z.wasPlaying = false
	z.nextPreloaded = false
	z.nextURI = ""
	z.generation++
	ctrl := z.ctrl
	m.mu.Unlock()

	if err := ctrl.Stop(ctx); err != nil {
		return err
	}
	m.mu.Lock()
	z.status.State = "STOPPED"
	z.status.Position = 0
	m.mu.Unlock()
	m.broadcastZone(z)
	return nil
}

// Next advances to the next track in the queue.
func (m *Manager) Next(ctx context.Context, rendererID string) error {
	z, err := m.resolveZone(rendererID)
	if err != nil {
		return err
	}
	m.mu.Lock()
	if z.currentIndex+1 >= len(z.queue) {
		m.mu.Unlock()
		return errors.New("already at end of queue")
	}
	z.currentIndex++
	idx := z.currentIndex
	z.generation++
	gen := z.generation
	z.nextPreloaded = false
	z.nextURI = ""
	z.nextFailed = false
	z.wasPlaying = false
	z.userStopped = false
	m.mu.Unlock()

	return m.loadAndPlay(ctx, z, idx, gen)
}

// Prev restarts the current track if past 5 seconds, otherwise jumps to the previous track.
func (m *Manager) Prev(ctx context.Context, rendererID string) error {
	z, err := m.resolveZone(rendererID)
	if err != nil {
		return err
	}
	m.mu.Lock()
	if z.status.Position > 5*time.Second && z.currentIndex >= 0 {
		ctrl := z.ctrl
		m.mu.Unlock()
		return ctrl.Seek(ctx, 0)
	}
	if z.currentIndex <= 0 {
		m.mu.Unlock()
		if len(z.queue) > 0 {
			return m.PlayIndex(ctx, rendererID, 0)
		}
		return errors.New("queue is empty")
	}
	z.currentIndex--
	idx := z.currentIndex
	z.generation++
	gen := z.generation
	z.nextPreloaded = false
	z.nextURI = ""
	z.nextFailed = false
	z.wasPlaying = false
	z.userStopped = false
	m.mu.Unlock()

	return m.loadAndPlay(ctx, z, idx, gen)
}

// Seek seeks to positionSec within the current track.
func (m *Manager) Seek(ctx context.Context, rendererID string, positionSec int) error {
	z, err := m.resolveZone(rendererID)
	if err != nil {
		return err
	}
	if positionSec < 0 {
		positionSec = 0
	}
	pos := time.Duration(positionSec) * time.Second
	if err := z.ctrl.Seek(ctx, pos); err != nil {
		return err
	}
	m.mu.Lock()
	z.status.Position = pos
	m.mu.Unlock()
	m.broadcastZone(z)
	return nil
}

// SetVolume updates renderer master volume (0-100).
func (m *Manager) SetVolume(ctx context.Context, rendererID string, vol int) error {
	z, err := m.resolveZone(rendererID)
	if err != nil {
		return err
	}
	if vol < 0 {
		vol = 0
	}
	if vol > 100 {
		vol = 100
	}
	if err := z.ctrl.SetVolume(ctx, vol); err != nil {
		return err
	}
	m.mu.Lock()
	z.status.Volume = vol
	m.mu.Unlock()
	m.broadcastZone(z)
	return nil
}

// SetMute updates renderer mute state.
func (m *Manager) SetMute(ctx context.Context, rendererID string, muted bool) error {
	z, err := m.resolveZone(rendererID)
	if err != nil {
		return err
	}
	if err := z.ctrl.SetMute(ctx, muted); err != nil {
		return err
	}
	m.mu.Lock()
	z.status.Muted = muted
	m.mu.Unlock()
	m.broadcastZone(z)
	return nil
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

func (m *Manager) loadAndPlay(ctx context.Context, z *zone, idx int, gen uint64) error {
	m.mu.RLock()
	if idx < 0 || idx >= len(z.queue) {
		m.mu.RUnlock()
		return errors.New("track index out of range")
	}
	track := z.queue[idx]
	ctrl := z.ctrl
	m.mu.RUnlock()

	media, resolvedTrack, err := m.buildMediaItem(ctx, ctrl, track)
	if err != nil {
		m.recordZoneError(z, err)
		return err
	}

	if err := ctrl.SetURI(ctx, media); err != nil {
		m.recordZoneError(z, fmt.Errorf("SetAVTransportURI: %w", err))
		return err
	}
	if err := ctrl.Play(ctx); err != nil {
		m.recordZoneError(z, fmt.Errorf("Play: %w", err))
		return err
	}

	m.mu.Lock()
	if z.generation == gen {
		if idx < len(z.queue) {
			z.queue[idx] = resolvedTrack
		}
		z.currentURI = media.URL
		z.status.State = "TRANSITIONING"
		z.status.Position = 0
		z.status.Duration = media.Duration
		z.status.TrackURI = media.URL
		// Do NOT mark wasPlaying=true until the renderer actually confirms PLAYING
		// via Status(), otherwise brief STOPPED states during buffering trigger
		// cascading auto-advances across the queue.
		z.wasPlaying = false
		z.loadedAt = time.Now()
		z.lastError = ""
	}
	m.mu.Unlock()

	m.broadcastZone(z)
	return nil
}

func (m *Manager) buildMediaItem(ctx context.Context, ctrl upnp.Controller, track library.Track) (upnp.MediaItem, library.Track, error) {
	baseURL := m.baseURLFor(ctrl)

	switch track.Source {
	case library.SourceLocal:
		artURL := ""
		if track.ArtworkURL != "" {
			if strings.HasPrefix(track.ArtworkURL, "http://") || strings.HasPrefix(track.ArtworkURL, "https://") {
				artURL = track.ArtworkURL
			} else {
				artURL = baseURL + track.ArtworkURL
			}
		}
		return upnp.MediaItem{
			ID:          track.ID,
			URL:         baseURL + "/media/local/" + track.ID + track.Ext,
			Title:       track.Title,
			Artist:      track.Artist,
			Album:       track.Album,
			AlbumArtURL: artURL,
			TrackNumber: track.TrackNumber,
			MIME:        track.MIME,
			Duration:    time.Duration(track.DurationSec) * time.Second,
			Size:        track.Size,
			SampleRate:  track.SampleRate,
			BitDepth:    track.BitDepth,
			Channels:    track.Channels,
			Seekable:    true,
		}, track, nil

	case library.SourceQobuz:
		if m.qobuz == nil {
			return upnp.MediaItem{}, track, errors.New("qobuz client is not configured")
		}
		stream, err := m.qobuz.StreamURL(ctx, track.QobuzTrackID)
		if err != nil {
			return upnp.MediaItem{}, track, fmt.Errorf("qobuz stream URL: %w", err)
		}
		if stream.SampleRate > 0 {
			track.SampleRate = stream.SampleRate
		}
		if stream.BitDepth > 0 {
			track.BitDepth = stream.BitDepth
		}
		if stream.Duration > 0 {
			track.DurationSec = int(stream.Duration.Seconds() + 0.5)
		}
		if stream.MIME != "" {
			track.MIME = stream.MIME
		}

		streamURL := stream.URL
		if m.StreamMode() == StreamModeRelay {
			streamURL = baseURL + "/media/qobuz/" + strconv.FormatInt(track.QobuzTrackID, 10) + ".flac"
		}

		return upnp.MediaItem{
			ID:          track.ID,
			URL:         streamURL,
			Title:       track.Title,
			Artist:      track.Artist,
			Album:       track.Album,
			AlbumArtURL: track.ArtworkURL,
			TrackNumber: track.TrackNumber,
			MIME:        track.MIME,
			Duration:    time.Duration(track.DurationSec) * time.Second,
			SampleRate:  track.SampleRate,
			BitDepth:    track.BitDepth,
			Channels:    2,
			Seekable:    true,
		}, track, nil

	default:
		return upnp.MediaItem{}, track, fmt.Errorf("unsupported track source %q", track.Source)
	}
}

func (m *Manager) baseURLFor(ctrl upnp.Controller) string {
	if m.publicBaseURL != "" {
		return m.publicBaseURL
	}
	return fmt.Sprintf("http://%s:%d", ctrl.LocalIP(), m.httpPort)
}

func (m *Manager) pollLoop(ctx context.Context) {
	m.pollTick(ctx)
	ticker := time.NewTicker(1 * time.Second)
	defer ticker.Stop()

	for {
		select {
		case <-ctx.Done():
			return
		case <-m.stopCh:
			return
		case <-ticker.C:
			m.pollTick(ctx)
		}
	}
}

func (m *Manager) pollTick(ctx context.Context) {
	now := time.Now()
	m.mu.RLock()
	active := m.activeID
	gena := m.gena
	ids := make([]string, 0, len(m.zones))
	for id, z := range m.zones {
		isPlaying := z.status.State == "PLAYING" || z.status.State == "TRANSITIONING"
		if id != active && !isPlaying {
			continue
		}
		// When GENA push events are active and the renderer is not actively
		// playing, slow background polling down to every 5s to keep idle SOAP
		// traffic minimal while still staying self-healing.
		if !isPlaying && gena != nil && gena.IsSubscribed(id) && now.Sub(z.lastPoll) < 5*time.Second {
			continue
		}
		ids = append(ids, id)
	}
	m.mu.RUnlock()

	for _, id := range ids {
		pctx, cancel := context.WithTimeout(ctx, 2500*time.Millisecond)
		m.pollOne(pctx, id)
		cancel()
	}
}

// HandleDeviceEvent applies a push UPnP GENA event to the matching zone,
// immediately updating state/volume and triggering gapless preloads or
// fallback track advances without waiting for the next poll tick.
func (m *Manager) HandleDeviceEvent(ev upnp.DeviceEvent) {
	if !ev.HasAnyUpdate() {
		return
	}

	m.mu.Lock()
	z, ok := m.zones[ev.RendererID]
	if !ok {
		m.mu.Unlock()
		return
	}
	gen := z.generation
	ctrl := z.ctrl
	runCtx := m.runCtx

	if ev.HasVolume {
		z.status.Volume = ev.Volume
	}
	if ev.HasMute {
		z.status.Muted = ev.Muted
	}

	var preloadTrack *library.Track
	fallbackAdvanceIdx := -1

	if ev.HasState || ev.HasTrackURI || ev.HasDuration {
		st := upnp.TransportStatus{
			State:       z.status.State,
			HasVolume:   z.status.HasVolume,
			HasPosition: false,
		}
		if ev.HasState {
			st.State = ev.State
		}
		if ev.HasTrackURI {
			st.TrackURI = ev.TrackURI
		}
		if ev.HasDuration {
			st.Duration = ev.Duration
		}
		// Seq==0 is the initial state dump emitted right upon SUBSCRIBE; apply
		// state/metadata without triggering a fallback auto-advance.
		allowAutoAdvance := ev.Seq != 0
		preloadTrack, fallbackAdvanceIdx, gen = m.applyTransportUpdateLocked(z, st, allowAutoAdvance)
	}
	m.mu.Unlock()

	baseCtx := runCtx
	if baseCtx == nil {
		baseCtx = context.Background()
	}
	if preloadTrack != nil {
		pctx, cancel := context.WithTimeout(baseCtx, 5*time.Second)
		m.preloadNext(pctx, z, *preloadTrack, gen)
		cancel()
	}
	if fallbackAdvanceIdx >= 0 {
		actx, cancel := context.WithTimeout(baseCtx, 5*time.Second)
		if err := m.loadAndPlay(actx, z, fallbackAdvanceIdx, gen); err != nil {
			log.Printf("auto-advance failed on %s: %v", ctrl.Info().Name, err)
		}
		cancel()
		return
	}

	m.broadcastZone(z)
}

// pollOne refreshes transport status for one zone and manages gapless preload
// and auto-advance transitions.
func (m *Manager) pollOne(ctx context.Context, id string) {
	m.mu.Lock()
	z, ok := m.zones[id]
	if !ok || z.polling {
		m.mu.Unlock()
		return
	}
	z.polling = true
	z.lastPoll = time.Now()
	ctrl := z.ctrl
	gen := z.generation
	m.mu.Unlock()

	defer func() {
		m.mu.Lock()
		z.polling = false
		m.mu.Unlock()
	}()

	st, err := ctrl.Status(ctx)
	if err != nil {
		return
	}

	m.mu.Lock()
	if z.generation != gen {
		m.mu.Unlock()
		return
	}
	if st.HasVolValue {
		z.status.Volume = st.Volume
	}
	if st.HasMuteVal {
		z.status.Muted = st.Muted
	}
	preloadTrack, fallbackAdvanceIdx, gen := m.applyTransportUpdateLocked(z, st, true)
	m.mu.Unlock()

	if preloadTrack != nil {
		m.preloadNext(ctx, z, *preloadTrack, gen)
	}
	if fallbackAdvanceIdx >= 0 {
		if err := m.loadAndPlay(ctx, z, fallbackAdvanceIdx, gen); err != nil {
			log.Printf("auto-advance failed on %s: %v", ctrl.Info().Name, err)
		}
		return
	}

	m.broadcastZone(z)
}

// applyTransportUpdateLocked merges polled or GENA-pushed transport state into
// z and returns any follow-up preload or fallback-advance work to perform after
// releasing m.mu.
func (m *Manager) applyTransportUpdateLocked(z *zone, st upnp.TransportStatus, allowAutoAdvance bool) (*library.Track, int, uint64) {
	// Merge fields carefully so a transient GetPositionInfo failure or a stale
	// URI from a prior track never resets UI position or duration back to zero.
	if st.State != "" {
		z.status.State = st.State
	}
	z.status.HasVolume = st.HasVolume

	staleTrack := st.TrackURI != "" && z.currentURI != "" && st.TrackURI != z.currentURI && st.TrackURI != z.nextURI
	if st.HasPosition && !staleTrack {
		// While actively playing the same track, ignore sporadic 0:00 glitches
		// reported by some UPnP renderers mid-track when TrackURI is blank.
		if !(st.State == "PLAYING" && st.Position == 0 && st.TrackURI == "" && z.status.Position > 0) {
			z.status.Position = st.Position
		}
		if st.Duration > 0 {
			z.status.Duration = st.Duration
		}
		if st.TrackURI != "" {
			z.status.TrackURI = st.TrackURI
		}
	} else if !staleTrack {
		// GENA events carry duration/TrackURI without position.
		if st.Duration > 0 {
			z.status.Duration = st.Duration
		}
		if st.TrackURI != "" {
			z.status.TrackURI = st.TrackURI
		}
	}

	// 1) Detect seamless gapless transition triggered by SetNextAVTransportURI.
	if z.nextPreloaded && z.nextURI != "" && st.TrackURI == z.nextURI {
		if z.currentIndex+1 < len(z.queue) {
			z.currentIndex++
		}
		z.currentURI = z.nextURI
		z.nextURI = ""
		z.nextPreloaded = false
		z.nextPreloading = false
		z.nextFailed = false
		z.status.Position = 0
		z.wasPlaying = true
		z.loadedAt = time.Now()
	}

	var preloadTrack *library.Track
	fallbackAdvanceIdx := -1
	gen := z.generation

	if z.status.State == "PLAYING" {
		// Only consider the current track confirmed PLAYING when the renderer's
		// URI matches (or if the renderer omits TrackURI).
		if st.TrackURI == "" || z.currentURI == "" || st.TrackURI == z.currentURI {
			z.wasPlaying = true
		}
		// 2) Preload next track via SetNextAVTransportURI for gapless playback
		// unless this renderer is known to not support SetNextAVTransportURI.
		if z.wasPlaying && !z.setNextUnsupported && !z.nextPreloaded && !z.nextPreloading && !z.nextFailed && z.currentIndex >= 0 && z.currentIndex+1 < len(z.queue) {
			t := z.queue[z.currentIndex+1]
			preloadTrack = &t
			z.nextPreloading = true
		}
	} else if allowAutoAdvance && z.wasPlaying && !z.userStopped && (z.status.State == "STOPPED" || z.status.State == "NO_MEDIA_PRESENT") {
		// 3) Fallback auto-advance if renderer finished track without gapless transition.
		z.wasPlaying = false
		if z.currentIndex >= 0 && z.currentIndex+1 < len(z.queue) {
			z.currentIndex++
			z.generation++
			gen = z.generation
			z.nextPreloaded = false
			z.nextPreloading = false
			z.nextURI = ""
			z.nextFailed = false
			fallbackAdvanceIdx = z.currentIndex
		}
	}

	return preloadTrack, fallbackAdvanceIdx, gen
}

func (m *Manager) preloadNext(ctx context.Context, z *zone, nextTrack library.Track, gen uint64) {
	defer func() {
		m.mu.Lock()
		z.nextPreloading = false
		m.mu.Unlock()
	}()

	media, resolvedTrack, err := m.buildMediaItem(ctx, z.ctrl, nextTrack)
	if err != nil {
		m.mu.Lock()
		if z.generation == gen {
			z.nextFailed = true
		}
		m.mu.Unlock()
		return
	}

	if err := z.ctrl.SetNextURI(ctx, media); err != nil {
		permanent := upnp.IsActionUnsupported(err)
		if permanent {
			log.Printf("SetNextAVTransportURI unsupported on %s — caching capability and using STOPPED auto-advance", z.ctrl.Info().Name)
		} else {
			log.Printf("SetNextAVTransportURI failed on %s (will fall back to STOPPED advance for this track): %v", z.ctrl.Info().Name, err)
		}
		m.mu.Lock()
		if permanent {
			z.setNextUnsupported = true
		}
		if z.generation == gen {
			z.nextFailed = true
		}
		m.mu.Unlock()
		if permanent {
			m.broadcast(Event{Type: "renderers", Data: m.Renderers()})
		}
		return
	}

	m.mu.Lock()
	if z.generation == gen {
		if z.currentIndex+1 < len(z.queue) && z.queue[z.currentIndex+1].ID == resolvedTrack.ID {
			z.queue[z.currentIndex+1] = resolvedTrack
		}
		z.nextPreloaded = true
		z.nextURI = media.URL
	}
	m.mu.Unlock()
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
