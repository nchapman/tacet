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
		return fmt.Errorf("%w: track index out of range", ErrInvalidArgument)
	}
	track := z.queue[idx].Track
	ctrl := z.ctrl
	zoneID := ctrl.Info().ID
	quality := z.audioQuality
	m.mu.RUnlock()

	media, resolvedTrack, effQuality, err := m.buildMediaItem(ctx, ctrl, track, quality)
	if err != nil {
		m.emitZoneError(zoneID, err)
		return err
	}
	return m.playPreparedMedia(ctx, z, idx, gen, ctrl, media, resolvedTrack, effQuality)
}

func (m *Manager) playPreparedMedia(
	ctx context.Context,
	z *zone,
	idx int,
	gen uint64,
	ctrl upnp.Controller,
	media upnp.MediaItem,
	resolvedTrack library.Track,
	effQuality AudioQuality,
) error {
	zoneID := ctrl.Info().ID
	if err := ctrl.SetURI(ctx, media); err != nil {
		werr := wrapRendererErr("SetAVTransportURI", err)
		m.emitZoneError(zoneID, werr)
		return werr
	}
	// Record currentURI immediately after SetURI succeeds so even if Play times out
	// on a slow DAC and starts 300ms later, subsequent PLAYING reports are not treated
	// as stale tracks.
	m.mu.Lock()
	if z.generation == gen {
		if idx >= 0 && idx < len(z.queue) {
			z.queue[idx].Track = resolvedTrack
		}
		z.currentURI = media.URL
		z.activeQuality = effQuality
		z.status.Duration = media.Duration
		z.status.TrackURI = media.URL
	}
	m.mu.Unlock()

	if err := ctrl.Play(ctx); err != nil {
		werr := wrapRendererErr("Play", err)
		m.emitZoneError(zoneID, werr)
		return werr
	}

	m.mu.Lock()
	if z.generation == gen {
		if idx >= 0 && idx < len(z.queue) {
			z.queue[idx].Track = resolvedTrack
		}
		z.currentURI = media.URL
		z.activeQuality = effQuality
		z.status.State = "TRANSITIONING"
		z.status.Position = 0
		z.status.Duration = media.Duration
		z.status.TrackURI = media.URL
		z.positionUpdatedAt = time.Now().UTC()
		z.lastPlaybackEmit = z.positionUpdatedAt
		// Do NOT mark wasPlaying=true until the renderer actually confirms PLAYING
		// via Status() or GENA, otherwise brief STOPPED states during buffering
		// trigger cascading auto-advances across the queue.
		z.wasPlaying = false
		z.loadedAt = time.Now()
		z.revision++
	}
	pbEv := m.playbackEventLocked(z)
	m.mu.Unlock()

	m.emit("zone.playback", zoneID, pbEv)
	return nil
}

func (m *Manager) clearNextPreloadAsync(ctrl upnp.Controller) {
	if ctrl == nil || ctrl.Info().Kind != "player" {
		return
	}
	go func() {
		cctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
		defer cancel()
		_ = ctrl.SetNextURI(cctx, upnp.MediaItem{})
	}()
}

func (m *Manager) buildMediaItem(ctx context.Context, ctrl upnp.Controller, track library.Track, quality AudioQuality) (upnp.MediaItem, library.Track, AudioQuality, error) {
	info := ctrl.Info()

	effQuality, ok := NormalizeAudioQuality(string(quality))
	if !ok || (effQuality.NeedsTranscode() && !m.TranscodeAvailable()) {
		effQuality = AudioQualityOriginal
	}

	relativeMedia := false
	if rm, ok := ctrl.(upnp.RelativeMediaController); ok && rm.PreferRelativeMedia() {
		relativeMedia = true
	}
	baseURL := ""
	if !relativeMedia {
		baseURL = m.baseURLFor(ctrl)
	}

	// Determine transcoded output extension, MIME, and query params when active.
	transcodeExt, transcodeMIME, transcodeQuery := transcodeOutputFor(info, effQuality)

	switch track.Source {
	case library.SourceLocal:
		outMIME := track.MIME
		outExt := track.Ext
		outQuery := ""
		outSize := track.Size
		outRate := track.SampleRate
		outDepth := track.BitDepth
		outCh := track.Channels
		seekable := true
		transcoded := false
		if effQuality.NeedsTranscode() {
			outMIME = transcodeMIME
			outExt = transcodeExt
			outQuery = transcodeQuery
			outSize = 0
			outRate, outDepth, outCh = transcodedAudioParams(effQuality, track.SampleRate, track.BitDepth, track.Channels)
			seekable = false
			transcoded = true
		}
		if err := checkEndpointMIME(info, outMIME); err != nil {
			return upnp.MediaItem{}, track, effQuality, err
		}
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
			URL:         baseURL + "/media/local/" + track.ID + outExt + outQuery,
			Title:       track.Title,
			Artist:      track.Artist,
			Album:       track.Album,
			AlbumArtURL: artURL,
			TrackNumber: track.TrackNumber,
			MIME:        outMIME,
			Duration:    time.Duration(track.DurationSec) * time.Second,
			Size:        outSize,
			SampleRate:  outRate,
			BitDepth:    outDepth,
			Channels:    outCh,
			Seekable:    seekable,
			Transcoded:  transcoded,
		}, track, effQuality, nil

	case library.SourceQobuz:
		if m.qobuz == nil {
			return upnp.MediaItem{}, track, effQuality, errors.New("qobuz client is not configured")
		}
		qID := track.QobuzTrackID
		if qID <= 0 {
			if parsed, err := strconv.ParseInt(strings.TrimPrefix(track.ID, "qobuz:"), 10, 64); err == nil {
				qID = parsed
				track.QobuzTrackID = parsed
			}
		}
		stream, err := m.qobuz.StreamURL(ctx, qID)
		if err != nil {
			return upnp.MediaItem{}, track, effQuality, fmt.Errorf("qobuz stream URL: %w", err)
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

		outMIME := track.MIME
		outRate := track.SampleRate
		outDepth := track.BitDepth
		outCh := 2
		seekable := true
		transcoded := false
		if effQuality.NeedsTranscode() {
			outMIME = transcodeMIME
			outRate, outDepth, outCh = transcodedAudioParams(effQuality, track.SampleRate, track.BitDepth, 2)
			seekable = false
			transcoded = true
		}

		// Validate codec AFTER resolving the real Qobuz stream MIME (or transcoded output MIME).
		if err := checkEndpointMIME(info, outMIME); err != nil {
			return upnp.MediaItem{}, track, effQuality, err
		}

		useRelay := m.StreamMode() == StreamModeRelay || effQuality.NeedsTranscode()
		if !effQuality.NeedsTranscode() {
			if qdc, ok := ctrl.(upnp.QobuzDeliveryController); ok {
				switch strings.ToLower(strings.TrimSpace(qdc.QobuzDelivery())) {
				case "force":
					useRelay = true
				case "direct":
					useRelay = false
				}
			}
		}

		streamURL := stream.URL
		if useRelay {
			ext := ".flac"
			if strings.Contains(strings.ToLower(track.MIME), "mpeg") || strings.Contains(strings.ToLower(track.MIME), "mp3") {
				ext = ".mp3"
			}
			query := ""
			if effQuality.NeedsTranscode() {
				ext = transcodeExt
				query = transcodeQuery
			}
			streamURL = baseURL + "/media/qobuz/" + strconv.FormatInt(qID, 10) + ext + query
		}

		return upnp.MediaItem{
			ID:          track.ID,
			URL:         streamURL,
			Title:       track.Title,
			Artist:      track.Artist,
			Album:       track.Album,
			AlbumArtURL: track.ArtworkURL,
			TrackNumber: track.TrackNumber,
			MIME:        outMIME,
			Duration:    time.Duration(track.DurationSec) * time.Second,
			SampleRate:  outRate,
			BitDepth:    outDepth,
			Channels:    outCh,
			Seekable:    seekable,
			Transcoded:  transcoded,
		}, track, effQuality, nil

	default:
		return upnp.MediaItem{}, track, effQuality, fmt.Errorf("%w: unsupported track source %q", ErrInvalidArgument, track.Source)
	}
}

func transcodedAudioParams(q AudioQuality, srcRate, srcDepth, srcCh int) (rate, depth, ch int) {
	ch = srcCh
	if ch <= 0 {
		ch = 2
	}
	if q.IsOpus() {
		if ch > 2 {
			ch = 2
		}
		// Opus always encodes at 48 kHz and is lossy (bitsPerSample omitted in DIDL).
		return 48000, 0, ch
	}
	if q == AudioQualityWAV {
		if srcDepth == 1 {
			return 88200, 24, ch
		}
		depth = 24
		if srcDepth > 1 && srcDepth <= 16 {
			depth = 16
		}
		return srcRate, depth, ch
	}
	return srcRate, srcDepth, ch
}

func transcodeOutputFor(info upnp.RendererInfo, q AudioQuality) (ext, mime, query string) {
	if q == AudioQualityWAV {
		return ".wav", "audio/wav", "?q=wav"
	}
	if !q.IsOpus() {
		return "", "", ""
	}
	if info.Kind == "player" && len(info.SinkProtocols) > 0 {
		// Prefer explicit Opus codec capability matches in priority order:
		// Ogg/Opus -> WebM/Opus (Safari 17.4+) -> fragmented MP4/Opus (older Safari).
		if sinkHasExactCodec(info.SinkProtocols, "audio/ogg", "opus") {
			return ".ogg", "audio/ogg", "?q=" + string(q)
		}
		if sinkHasExactCodec(info.SinkProtocols, "audio/webm", "opus") {
			return ".webm", "audio/webm", "?q=" + string(q) + "&container=webm"
		}
		if sinkHasExactCodec(info.SinkProtocols, "audio/mp4", "opus") {
			return ".m4a", "audio/mp4", "?q=" + string(q) + "&container=mp4"
		}
		// Fallback for non-browser player clients that only advertise bare container MIMEs.
		if !sinkSupportsMIME(info.SinkProtocols, "audio/ogg") {
			if sinkSupportsMIME(info.SinkProtocols, "audio/webm") {
				return ".webm", "audio/webm", "?q=" + string(q) + "&container=webm"
			}
			if sinkSupportsMIME(info.SinkProtocols, "audio/mp4") {
				return ".m4a", "audio/mp4", "?q=" + string(q) + "&container=mp4"
			}
		}
	}
	if !RendererSupportsOpus(info) {
		return ".aac", "audio/aac", "?q=" + string(q) + "&container=adts"
	}
	return ".ogg", "audio/ogg", "?q=" + string(q)
}

func sinkHasExactCodec(protocols []string, containerMIME, codec string) bool {
	containerMIME = strings.ToLower(strings.TrimSpace(containerMIME))
	codec = strings.ToLower(strings.TrimSpace(codec))
	for _, proto := range protocols {
		p := strings.ToLower(strings.TrimSpace(proto))
		if strings.HasPrefix(p, containerMIME+";") && strings.Contains(p, codec) {
			return true
		}
	}
	return false
}

func sinkSupportsMIME(protocols []string, mime string) bool {
	mime = strings.ToLower(strings.TrimSpace(mime))
	for _, proto := range protocols {
		p := strings.ToLower(proto)
		if p == mime || strings.Contains(p, ":"+mime+":") || strings.HasPrefix(p, mime+";") {
			return true
		}
		if (mime == "audio/flac" || mime == "audio/x-flac") && (strings.Contains(p, "audio/flac") || strings.Contains(p, "audio/x-flac")) {
			return true
		}
		if (mime == "audio/mp4" || mime == "audio/x-m4a") && (strings.Contains(p, "audio/mp4") || strings.Contains(p, "audio/x-m4a") || strings.Contains(p, "audio/aac")) {
			return true
		}
	}
	return false
}

func checkEndpointMIME(info upnp.RendererInfo, mime string) error {
	if info.Kind != "player" || len(info.SinkProtocols) == 0 {
		return nil
	}
	mime = strings.ToLower(strings.TrimSpace(mime))
	if mime == "" {
		return nil
	}
	if sinkSupportsMIME(info.SinkProtocols, mime) {
		return nil
	}
	return fmt.Errorf("%w: player endpoint %q does not support %s", ErrCapabilityUnsupported, info.Name, mime)
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
	gena := m.gena
	ids := make([]string, 0, len(m.zones))
	for id, z := range m.zones {
		isPlaying := z.status.State == "PLAYING" || z.status.State == "TRANSITIONING"
		// Idle zones with live GENA push events only need a 10s self-healing poll;
		// idle zones without GENA poll every 5s; active/playing zones poll every 1s
		// so gapless preload and fallback track-end triggers remain tight.
		if !isPlaying {
			interval := 5 * time.Second
			pushConnected := gena != nil && gena.IsSubscribed(id)
			if !pushConnected {
				if ps, ok := z.ctrl.(upnp.PushStatusController); ok && ps.PushConnected() {
					pushConnected = true
				}
			}
			if pushConnected {
				interval = 10 * time.Second
			}
			if now.Sub(z.lastPoll) < interval {
				continue
			}
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

type updateResult struct {
	preloadTrack       *library.Track
	preloadGen         uint64
	playedTrack        *library.Track
	fallbackAdvanceIdx int
	gen                uint64
	emitPlayback       bool
	emitVolume         bool
	playbackEv         PlaybackEvent
	volumeEv           VolumeEvent
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
	runCtx := m.runCtx

	volChanged := false
	if ev.HasVolume && ev.Volume != z.status.Volume {
		z.status.Volume = ev.Volume
		volChanged = true
	}
	if ev.HasMute && ev.Muted != z.status.Muted {
		z.status.Muted = ev.Muted
		volChanged = true
	}

	var res updateResult
	if ev.HasState || ev.HasPosition || ev.HasTrackURI || ev.HasDuration {
		st := upnp.TransportStatus{
			State:       z.status.State,
			HasVolume:   z.status.HasVolume,
			HasPosition: ev.HasPosition,
			Position:    ev.Position,
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
		allowAutoAdvance := ev.Seq != 0
		res = m.applyTransportUpdateLocked(z, st, allowAutoAdvance)
	}
	if volChanged {
		z.revision++
		res.emitVolume = true
		res.volumeEv = m.volumeEventLocked(z)
	}
	m.mu.Unlock()

	m.finishUpdate(runCtx, z, ev.RendererID, res)
}

func (m *Manager) pollOne(ctx context.Context, zoneID string) {
	m.mu.Lock()
	z, ok := m.zones[zoneID]
	if !ok || z.polling {
		m.mu.Unlock()
		return
	}
	z.polling = true
	z.lastPoll = time.Now()
	gen := z.generation
	ctrl := z.ctrl
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
	res := m.applyTransportUpdateLocked(z, st, true)
	m.mu.Unlock()

	m.finishUpdate(ctx, z, zoneID, res)
}

func (m *Manager) finishUpdate(ctx context.Context, z *zone, zoneID string, res updateResult) {
	if ctx == nil {
		ctx = context.Background()
	}
	if res.emitVolume {
		m.emit("zone.volume", zoneID, res.volumeEv)
	}
	if res.emitPlayback {
		m.emit("zone.playback", zoneID, res.playbackEv)
	}
	if res.playedTrack != nil {
		m.mu.RLock()
		onPlayed := m.onTrackPlayed
		m.mu.RUnlock()
		if onPlayed != nil {
			go onPlayed(*res.playedTrack)
		}
	}
	if res.preloadTrack != nil {
		pctx, cancel := context.WithTimeout(ctx, 5*time.Second)
		m.preloadNext(pctx, z, *res.preloadTrack, res.gen, res.preloadGen)
		cancel()
	}
	if res.fallbackAdvanceIdx >= 0 {
		if err := m.loadAndPlay(ctx, z, res.fallbackAdvanceIdx, res.gen); err != nil {
			log.Printf("auto-advance failed on %s: %v", z.ctrl.Info().Name, err)
		}
	}
}

func (m *Manager) applyTransportUpdateLocked(z *zone, st upnp.TransportStatus, allowAutoAdvance bool) updateResult {
	now := time.Now().UTC()
	prevState := NormalizePlaybackState(z.status.State)
	prevIdx := z.currentIndex
	prevGapless := z.nextPreloaded
	prevPos := z.status.Position
	prevDur := z.status.Duration
	prevVol := z.status.Volume
	prevMute := z.status.Muted
	prevPosUpdated := z.positionUpdatedAt

	// Ignore stale track reports (URI belongs to neither the current nor preloaded track).
	staleTrack := z.currentURI != "" && st.TrackURI != "" &&
		st.TrackURI != z.currentURI &&
		(z.nextURI == "" || st.TrackURI != z.nextURI)

	// Preserve previous valid fields when partial status calls fail.
	z.status.State = st.State
	z.status.HasVolume = st.HasVolume
	if !staleTrack {
		if st.HasPosition {
			z.status.Position = st.Position
			if st.Duration > 0 {
				z.status.Duration = st.Duration
			}
			if st.TrackURI != "" {
				z.status.TrackURI = st.TrackURI
			}
			z.positionUpdatedAt = now
		} else {
			if st.Duration > 0 {
				z.status.Duration = st.Duration
			}
			if st.TrackURI != "" {
				z.status.TrackURI = st.TrackURI
			}
		}
	}
	if st.HasVolValue {
		z.status.Volume = st.Volume
	}
	if st.HasMuteVal {
		z.status.Muted = st.Muted
	}

	res := updateResult{
		fallbackAdvanceIdx: -1,
		gen:                z.generation,
	}

	// 1. Detect gapless transition: renderer URI switched to our preloaded nextURI.
	if z.nextPreloaded && z.nextURI != "" && z.status.TrackURI == z.nextURI {
		if z.nextIndex >= 0 && z.nextIndex < len(z.queue) {
			z.currentIndex = z.nextIndex
		} else {
			z.currentIndex++
		}
		z.currentURI = z.nextURI
		if z.nextQuality != "" {
			z.activeQuality = z.nextQuality
		}
		z.nextURI = ""
		z.nextIndex = -1
		z.nextQuality = ""
		z.nextPreloaded = false
		z.nextPreloading = false
		z.nextFailed = false
		z.loadedAt = time.Now()
		z.positionUpdatedAt = now
	}

	// 2. While PLAYING, preload the next track for gapless playback (if supported)
	// and record a library play event once the track passes the scrobble threshold.
	if st.State == "PLAYING" {
		z.wasPlaying = true
		if z.currentIndex >= 0 && z.currentIndex < len(z.queue) {
			curItem := z.queue[z.currentIndex]
			if curItem.ItemID != "" && z.playRecordedItemID != curItem.ItemID {
				dur := z.status.Duration
				if dur <= 0 && curItem.Track.DurationSec > 0 {
					dur = time.Duration(curItem.Track.DurationSec) * time.Second
				}
				threshold := 30 * time.Second
				if dur > 0 && dur/2 < threshold {
					threshold = dur / 2
				}
				if threshold < 5*time.Second {
					threshold = 5 * time.Second
				}
				if z.status.Position >= threshold {
					z.playRecordedItemID = curItem.ItemID
					tr := curItem.Track
					res.playedTrack = &tr
				}
			}
		}
		nextIdx := nextAutoIndexLocked(z)
		// Skip gapless preload when nextIdx == currentIndex (repeat one / 1-track repeat all)
		// because nextURI would match currentURI and trigger immediate transition detection;
		// fallback auto-advance handles single-track repeat cleanly at track end.
		if !z.setNextUnsupported && !z.nextPreloaded && !z.nextPreloading && !z.nextFailed &&
			nextIdx >= 0 && nextIdx != z.currentIndex {
			t := z.queue[nextIdx].Track
			res.preloadTrack = &t
			res.preloadGen = z.preloadGen
			z.nextIndex = nextIdx
			z.nextPreloading = true
		}
	}

	// 3. Fallback advance: if track finished and renderer became STOPPED/NO_MEDIA_PRESENT
	// without advancing gaplessly, explicitly load & play the next queued track.
	if allowAutoAdvance && (st.State == "STOPPED" || st.State == "NO_MEDIA_PRESENT") &&
		z.wasPlaying && !z.userStopped && time.Since(z.loadedAt) > 2*time.Second {
		z.wasPlaying = false
		if nextIdx := nextAutoIndexLocked(z); nextIdx >= 0 {
			z.currentIndex = nextIdx
			z.nextPreloaded = false
			z.nextPreloading = false
			z.nextFailed = false
			z.nextURI = ""
			z.nextIndex = -1
			z.generation++
			res.fallbackAdvanceIdx = z.currentIndex
			res.gen = z.generation
		}
	}

	newState := NormalizePlaybackState(z.status.State)
	volChanged := z.status.Volume != prevVol || z.status.Muted != prevMute

	// Emit zone.playback on any discrete state/track/duration/gapless change,
	// significant clock drift (>2s vs interpolated client clock), or periodic 5s sync
	// while playing. Never resend the full queue on poll ticks!
	discreteChange := newState != prevState ||
		z.currentIndex != prevIdx ||
		z.nextPreloaded != prevGapless ||
		z.status.Duration != prevDur

	driftDetected := false
	if st.HasPosition && newState == StatePlaying && !prevPosUpdated.IsZero() {
		expected := prevPos + now.Sub(prevPosUpdated)
		diff := z.status.Position - expected
		if diff < 0 {
			diff = -diff
		}
		driftDetected = diff > 2*time.Second
	} else if st.HasPosition && newState != StatePlaying && z.status.Position != prevPos {
		driftDetected = true
	}

	periodicSync := newState == StatePlaying && now.Sub(z.lastPlaybackEmit) >= 5*time.Second

	if volChanged {
		z.revision++
		res.emitVolume = true
		res.volumeEv = m.volumeEventLocked(z)
	}
	if discreteChange || driftDetected || periodicSync {
		z.revision++
		z.lastPlaybackEmit = now
		res.emitPlayback = true
		res.playbackEv = m.playbackEventLocked(z)
	}

	return res
}

func (m *Manager) preloadNext(ctx context.Context, z *zone, track library.Track, gen, pgen uint64) {
	m.mu.RLock()
	ctrl := z.ctrl
	zoneID := ctrl.Info().ID
	quality := z.audioQuality
	m.mu.RUnlock()

	media, resolved, effQuality, err := m.buildMediaItem(ctx, ctrl, track, quality)
	if err != nil {
		log.Printf("gapless preload skipped (resolve failed): %v", err)
		m.mu.Lock()
		if z.generation == gen && z.preloadGen == pgen {
			z.nextPreloading = false
			z.nextFailed = true
		}
		m.mu.Unlock()
		return
	}

	if err := ctrl.SetNextURI(ctx, media); err != nil {
		m.mu.Lock()
		if z.generation == gen && z.preloadGen == pgen {
			z.nextPreloading = false
			if upnp.IsActionUnsupported(err) {
				z.setNextUnsupported = true
			} else {
				z.nextFailed = true
			}
		}
		m.mu.Unlock()
		log.Printf("SetNextAVTransportURI unavailable on %s (will fall back to STOPPED advance): %v",
			ctrl.Info().Name, err)
		return
	}

	m.mu.Lock()
	var pbEv *PlaybackEvent
	stale := z.generation != gen || z.preloadGen != pgen
	if !stale {
		if z.nextIndex >= 0 && z.nextIndex < len(z.queue) && z.queue[z.nextIndex].Track.ID == resolved.ID {
			z.queue[z.nextIndex].Track = resolved
		}
		z.nextURI = media.URL
		z.nextQuality = effQuality
		z.nextPreloaded = true
		z.nextPreloading = false
		z.revision++
		ev := m.playbackEventLocked(z)
		pbEv = &ev
	}
	m.mu.Unlock()

	if stale {
		// Queue/shuffle/repeat/quality changed while SetNextURI was in flight;
		// clear the stale URI on the renderer so it does not gapless-advance to it.
		m.clearNextPreloadAsync(ctrl)
		return
	}

	if pbEv != nil {
		m.emit("zone.playback", zoneID, *pbEv)
	}
}
