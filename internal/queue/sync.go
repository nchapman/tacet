package queue

import (
	"context"
	"errors"
	"fmt"
	"log"
	"strconv"
	"strings"
	"time"

	"tacet/internal/library"
	"tacet/internal/upnp"
)

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
		// via Status() or GENA, otherwise brief STOPPED states during buffering
		// trigger cascading auto-advances across the queue.
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
		if st.State != "PLAYING" || st.Position != 0 || st.TrackURI != "" || z.status.Position <= 0 {
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
