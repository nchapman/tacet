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
	"tacet/internal/qobuz"
	"tacet/internal/upnp"
)

func (m *Manager) replaceDeviceQueueAndPlay(
	ctx context.Context,
	z *zone,
	items []Item,
	startIndex int,
	gen uint64,
	ctrl upnp.Controller,
	qc upnp.QueueController,
	quality AudioQuality,
	repeat RepeatMode,
) error {
	zoneID := ctrl.Info().ID
	mediaItems, effQuality, err := m.buildQueueMediaItems(ctrl, items, startIndex, quality)
	if err != nil {
		m.emitZoneError(zoneID, err)
		return err
	}
	if err := qc.ReplaceQueue(ctx, mediaItems, startIndex); err != nil {
		werr := wrapRendererErr("ReplaceQueue", err)
		m.emitZoneError(zoneID, werr)
		return werr
	}
	// Keep shuffle=false on the device because tacet manages canonical queue order
	// so device indices always match z.queue 1:1.
	_ = qc.SetLoopMode(ctx, string(repeat), false)

	curMedia := mediaItems[startIndex]
	m.mu.Lock()
	if z.generation == gen {
		z.currentIndex = startIndex
		z.currentURI = curMedia.URL
		z.activeQuality = effQuality
		if effQuality == z.audioQuality {
			z.deviceQueueDirty = false
		}
		z.status.State = "TRANSITIONING"
		z.status.Position = 0
		z.status.Duration = curMedia.Duration
		z.status.TrackURI = curMedia.URL
		z.positionUpdatedAt = time.Now().UTC()
		z.lastPlaybackEmit = z.positionUpdatedAt
		z.wasPlaying = false
		z.loadedAt = time.Now()
		z.revision++
	}
	pbEv := m.playbackEventLocked(z)
	m.mu.Unlock()
	m.emit("zone.playback", zoneID, pbEv)
	return nil
}

func (m *Manager) buildQueueMediaItems(ctrl upnp.Controller, items []Item, startIndex int, quality AudioQuality) ([]upnp.MediaItem, AudioQuality, error) {
	out := make([]upnp.MediaItem, 0, len(items))
	effQuality := quality
	for i, it := range items {
		mi, q, err := m.buildLazyMediaItem(ctrl, it.Track, quality)
		if err != nil {
			return nil, quality, err
		}
		if i == startIndex {
			effQuality = q
		}
		out = append(out, mi)
	}
	return out, effQuality, nil
}

// buildLazyMediaItem constructs a MediaItem without making any blocking upstream
// Qobuz API calls. Qobuz tracks receive permanent /media/qobuz/{id} URLs
// (with ?handoff=1 in Direct mode so the media handler lazily issues a 302
// redirect to a freshly-signed Qobuz CDN URL only when the speaker requests it).
func (m *Manager) buildLazyMediaItem(ctrl upnp.Controller, track library.Track, quality AudioQuality) (upnp.MediaItem, AudioQuality, error) {
	mi, _, q, err := m.buildMediaItemInternal(context.Background(), ctrl, track, quality, true)
	return mi, q, err
}

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
	// Record currentURI and TRANSITIONING immediately after SetURI succeeds so:
	// 1. Even if Play times out on a slow DAC and starts 300ms later, subsequent
	//    PLAYING reports are not treated as stale tracks.
	// 2. If a fast push renderer (e.g. the browser player) confirms PLAYING
	//    during ctrl.Play(ctx), we can preserve that fresh PLAYING state instead
	//    of clobbering it back to TRANSITIONING after Play returns.
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
		z.wasPlaying = false
		z.loadedAt = time.Now()
	}
	m.mu.Unlock()

	if err := ctrl.Play(ctx); err != nil {
		werr := wrapRendererErr("Play", err)
		m.emitZoneError(zoneID, werr)
		return werr
	}

	m.mu.Lock()
	alreadyPlaying := false
	if z.generation == gen {
		if idx >= 0 && idx < len(z.queue) {
			z.queue[idx].Track = resolvedTrack
		}
		z.currentURI = media.URL
		z.activeQuality = effQuality
		alreadyPlaying = NormalizePlaybackState(z.status.State) == StatePlaying &&
			z.status.TrackURI == media.URL
		if !alreadyPlaying {
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
	}
	pbEv := m.playbackEventLocked(z)
	m.mu.Unlock()

	if !alreadyPlaying {
		m.emit("zone.playback", zoneID, pbEv)
	}
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
	return m.buildMediaItemInternal(ctx, ctrl, track, quality, false)
}

func (m *Manager) buildMediaItemInternal(ctx context.Context, ctrl upnp.Controller, track library.Track, quality AudioQuality, lazyQobuz bool) (upnp.MediaItem, library.Track, AudioQuality, error) {
	info := ctrl.Info()

	effQuality, ok := NormalizeAudioQuality(string(quality))
	if !ok || (effQuality.NeedsTranscode() && !m.TranscodeAvailable()) {
		effQuality = AudioQualityOriginal
	}
	// Automatically decode 1-bit DSD to uncompressed 24-bit/88.2kHz WAV for
	// endpoints that do not support native DSD (browsers, non-DSD UPnP speakers).
	if effQuality == AudioQualityOriginal && isDSDTrack(track) && !RendererSupportsDSD(info) && m.TranscodeAvailable() {
		effQuality = AudioQualityWAV
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
		if qID <= 0 {
			return upnp.MediaItem{}, track, effQuality, fmt.Errorf("%w: invalid qobuz track id %q", ErrInvalidArgument, track.ID)
		}
		var streamURL string
		if lazyQobuz {
			if !m.qobuz.IsConnected() {
				return upnp.MediaItem{}, track, effQuality, fmt.Errorf("qobuz stream URL: %w", qobuz.ErrUnauthenticated)
			}
		} else {
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
			streamURL = stream.URL
		}

		outMIME := track.MIME
		if outMIME == "" {
			outMIME = "audio/flac"
		}
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

		if useRelay || lazyQobuz {
			ext := ".flac"
			if strings.Contains(strings.ToLower(outMIME), "mpeg") || strings.Contains(strings.ToLower(outMIME), "mp3") {
				ext = ".mp3"
			}
			query := ""
			if effQuality.NeedsTranscode() {
				ext = transcodeExt
				query = transcodeQuery
			} else if !useRelay && lazyQobuz {
				query = "?handoff=1"
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

func isDSDTrack(t library.Track) bool {
	if t.BitDepth == 1 && t.SampleRate >= 2_000_000 {
		return true
	}
	ext := strings.ToLower(strings.TrimSpace(t.Ext))
	if ext == ".dsf" || ext == ".dff" {
		return true
	}
	mime := strings.ToLower(strings.TrimSpace(t.MIME))
	return strings.Contains(mime, "dsf") || strings.Contains(mime, "dsd")
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
		if z.polling {
			continue
		}
		info := z.ctrl.Info()
		// Browser players push all state over HTTP — never poll them.
		if info.Kind == "player" {
			continue
		}
		state := z.status.State
		isPlaying := state == "PLAYING"
		isTransitioning := state == "TRANSITIONING"
		// Lazy-disconnected Cast devices push MEDIA_STATUS once active; never poll while idle.
		if info.Kind == "cast" && !isPlaying && !isTransitioning {
			continue
		}

		pushConnected := gena != nil && gena.IsSubscribed(id)
		if !pushConnected {
			if ps, ok := z.ctrl.(upnp.PushStatusController); ok && ps.PushConnected() {
				pushConnected = true
			}
		}

		var interval time.Duration
		switch {
		case isPlaying:
			if pushConnected {
				// Live GENA/Cast push: poll every 15s for gentle clock sync and external
				// seeks, or every 2s near track end on plain UPnP to arm SetNextURI.
				interval = 15 * time.Second
				_, isNativeQueue := z.ctrl.(upnp.QueueController)
				if !isNativeQueue && !z.nextPreloaded && z.status.Duration > 0 {
					estPos := z.status.Position + now.Sub(z.positionUpdatedAt)
					if z.status.Duration-estPos <= 25*time.Second {
						interval = 2 * time.Second
					}
				}
			} else {
				interval = 3 * time.Second
			}
		case isTransitioning:
			if pushConnected {
				// GENA/Cast pushes PLAYING immediately on normal transitions (<2s);
				// keep a 10s self-healing poll in case a GENA packet is dropped on Wi-Fi.
				interval = 10 * time.Second
			} else {
				interval = 2 * time.Second
			}
		default:
			if pushConnected {
				interval = 60 * time.Second
			} else {
				interval = 10 * time.Second
			}
		}
		if now.Sub(z.lastPoll) < interval {
			continue
		}
		ids = append(ids, id)
	}
	m.mu.RUnlock()

	// Poll due zones concurrently so one slow or offline speaker never stalls another.
	for _, id := range ids {
		go func(zoneID string) {
			pctx, cancel := context.WithTimeout(ctx, 2500*time.Millisecond)
			m.pollOne(pctx, zoneID)
			cancel()
		}(id)
	}
}

type updateResult struct {
	preloadTrack       *library.Track
	preloadGen         uint64
	playedTrack        *library.Track
	fallbackAdvanceIdx int
	gen                uint64
	emitPlayback       bool
	emitQueue          bool
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
	m.logEvent("gena", ev.RendererID, ev)

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

	res := updateResult{fallbackAdvanceIdx: -1}
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
	startRev := z.revision
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
	m.logEvent("poll", zoneID, st)

	m.mu.Lock()
	if z.generation != gen || z.revision != startRev {
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
	if res.emitQueue {
		m.scheduleQueuePersist()
	}
	if res.playedTrack != nil {
		m.mu.RLock()
		onPlayed := m.onTrackPlayed
		m.mu.RUnlock()
		if onPlayed != nil {
			go onPlayed(*res.playedTrack)
		}
	}
	m.mu.RLock()
	runCtx := m.runCtx
	m.mu.RUnlock()
	if res.preloadTrack != nil {
		tr := *res.preloadTrack
		if runCtx != nil {
			go func() {
				pctx, cancel := context.WithTimeout(runCtx, 5*time.Second)
				m.preloadNext(pctx, z, tr, res.gen, res.preloadGen)
				cancel()
			}()
		} else {
			pctx, cancel := context.WithTimeout(ctx, 5*time.Second)
			m.preloadNext(pctx, z, tr, res.gen, res.preloadGen)
			cancel()
		}
	}
	if res.fallbackAdvanceIdx >= 0 {
		idx := res.fallbackAdvanceIdx
		if runCtx != nil {
			go func() {
				z.opMu.Lock()
				defer z.opMu.Unlock()
				m.mu.RLock()
				stillCurrent := z.generation == res.gen
				m.mu.RUnlock()
				if !stillCurrent {
					return
				}
				actx, cancel := context.WithTimeout(runCtx, 10*time.Second)
				defer cancel()
				if err := m.loadAndPlay(actx, z, idx, res.gen); err != nil {
					log.Printf("auto-advance failed on %s: %v", z.ctrl.Info().Name, err)
				}
			}()
		} else if err := m.loadAndPlay(ctx, z, idx, res.gen); err != nil {
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
	prevPosUpdated := z.positionUpdatedAt
	prevDur := z.status.Duration
	prevVol := z.status.Volume
	prevMute := z.status.Muted

	_, isNativeQueue := z.ctrl.(upnp.QueueController)

	// Detect external playback (e.g. Spotify Connect, AirPlay, WiiM app) so an
	// unrelated source never hijacks tacet's queue index or records false plays.
	if st.TrackURI != "" && (st.State == "PLAYING" || st.State == "PAUSED_PLAYBACK" || st.State == "TRANSITIONING") &&
		!trackURIBelongsToTacetLocked(z, st.TrackURI) {
		z.status.HasVolume = st.HasVolume
		if st.HasVolValue {
			z.status.Volume = st.Volume
		}
		if st.HasMuteVal {
			z.status.Muted = st.Muted
		}
		z.status.State = "STOPPED"
		z.wasPlaying = false
		res := updateResult{fallbackAdvanceIdx: -1, gen: z.generation}
		if NormalizePlaybackState(z.status.State) != prevState || z.status.Volume != prevVol || z.status.Muted != prevMute {
			z.revision++
			res.emitPlayback = true
			res.playbackEv = m.playbackEventLocked(z)
		}
		return res
	}

	// Ignore stale track reports (URI belongs to neither the current nor preloaded track).
	staleTrack := !isNativeQueue && z.currentURI != "" && st.TrackURI != "" &&
		st.TrackURI != z.currentURI &&
		(z.nextURI == "" || st.TrackURI != z.nextURI)

	// Preserve previous valid fields when partial status calls fail.
	// Ignore Position during TRANSITIONING because renderers often return the
	// previous track's leftover RelTime until the new decoder resets.
	z.status.State = st.State
	z.status.HasVolume = st.HasVolume
	if !staleTrack {
		if st.HasPosition && st.State != "TRANSITIONING" {
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

	// 1. Detect device-managed queue transition (Linkplay PlayQueue / Cast Media Queue),
	// native-queue GENA TrackURI advance (GENA pushes TrackURI without QueueIndex),
	// restored-zone live playback resync, or virtual-queue gapless transition.
	if isNativeQueue && st.HasQueueIndex && st.QueueIndex >= 0 && st.QueueIndex < len(z.queue) {
		if st.QueueIndex != z.currentIndex && time.Since(z.loadedAt) > 1500*time.Millisecond {
			z.currentIndex = st.QueueIndex
			if st.TrackURI != "" {
				z.currentURI = st.TrackURI
			}
			if !st.HasPosition {
				z.status.Position = 0
			}
			z.loadedAt = time.Now()
			z.positionUpdatedAt = now
		} else if z.currentURI == "" && st.TrackURI != "" {
			z.currentURI = st.TrackURI
		}
	} else if isNativeQueue && st.TrackURI != "" && st.TrackURI != z.currentURI && len(z.queue) > 0 {
		if matched := matchTrackURIInQueue(st.TrackURI, z.queue); matched >= 0 {
			if matched != z.currentIndex {
				z.currentIndex = matched
				if !st.HasPosition {
					z.status.Position = 0
				}
				z.loadedAt = time.Now()
				z.positionUpdatedAt = now
			}
			z.currentURI = st.TrackURI
		}
	} else if z.currentURI == "" && st.TrackURI != "" && len(z.queue) > 0 {
		// Freshly restored zone after server restart: match the renderer's active
		// TrackURI against our restored queue so playback/index resync seamlessly.
		if matched := matchTrackURIInQueue(st.TrackURI, z.queue); matched >= 0 {
			z.currentIndex = matched
			z.currentURI = st.TrackURI
			z.positionUpdatedAt = now
		}
	} else if !isNativeQueue && z.nextPreloaded && z.nextURI != "" && z.status.TrackURI == z.nextURI {
		if z.nextIndex >= 0 && z.nextIndex < len(z.queue) {
			z.currentIndex = z.nextIndex
		} else {
			z.currentIndex++
		}
		z.currentURI = z.nextURI
		if !st.HasPosition {
			z.status.Position = 0
		}
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
		if !isNativeQueue {
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
	}

	// 3. Fallback advance: if track finished and renderer became STOPPED/NO_MEDIA_PRESENT
	// without advancing gaplessly, explicitly load & play the next queued track.
	if !isNativeQueue && allowAutoAdvance && (st.State == "STOPPED" || st.State == "NO_MEDIA_PRESENT") &&
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

	// Emit zone.playback on discrete state/track/duration/gapless changes,
	// or whenever a poll brings a real position update (client slews smoothly
	// for small jitter and snaps on seeks/jumps).
	discreteChange := newState != prevState ||
		z.currentIndex != prevIdx ||
		z.nextPreloaded != prevGapless ||
		z.status.Duration != prevDur

	posUpdate := false
	if st.HasPosition && st.State != "TRANSITIONING" {
		if newState == StatePlaying {
			if !prevPosUpdated.IsZero() {
				expected := prevPos + now.Sub(prevPosUpdated)
				diff := z.status.Position - expected
				if diff < 0 {
					diff = -diff
				}
				posUpdate = diff > 2*time.Second || now.Sub(z.lastPlaybackEmit) >= 10*time.Second
			} else {
				posUpdate = true
			}
		} else if int(z.status.Position.Seconds()) != int(prevPos.Seconds()) {
			posUpdate = true
		}
	}

	if volChanged {
		z.revision++
		res.emitVolume = true
		res.volumeEv = m.volumeEventLocked(z)
	}
	if discreteChange || posUpdate {
		z.revision++
		z.lastPlaybackEmit = now
		res.emitPlayback = true
		res.playbackEv = m.playbackEventLocked(z)
	}

	if z.currentIndex != prevIdx {
		res.emitQueue = true
	}
	return res
}

// trackURIBelongsToTacetLocked reports whether a renderer's reported TrackURI
// originates from tacet's queue rather than an external source (Spotify, AirPlay, etc.).
func trackURIBelongsToTacetLocked(z *zone, uri string) bool {
	if uri == "" {
		return false
	}
	if z.currentURI != "" && uri == z.currentURI {
		return true
	}
	if z.nextURI != "" && uri == z.nextURI {
		return true
	}
	if strings.Contains(uri, "/media/local/") || strings.Contains(uri, "/media/qobuz/") {
		return true
	}
	// Linkplay and UPnP renderers may report the post-302 redirected Qobuz CDN URL.
	if strings.Contains(strings.ToLower(uri), "qobuz") {
		for _, it := range z.queue {
			if it.Track.Source == library.SourceQobuz {
				return true
			}
		}
	}
	return false
}

// matchTrackURIInQueue matches a renderer's reported TrackURI (e.g.
// /media/local/local:abc.flac or /media/qobuz/12345.flac?handoff=1) against a
// restored queue so playback resyncs after a server restart.
func matchTrackURIInQueue(uri string, items []Item) int {
	for i, it := range items {
		tr := it.Track
		switch tr.Source {
		case library.SourceLocal:
			if tr.ID != "" && strings.Contains(uri, "/media/local/"+tr.ID) {
				return i
			}
		case library.SourceQobuz:
			qID := tr.QobuzTrackID
			if qID <= 0 {
				if parsed, err := strconv.ParseInt(strings.TrimPrefix(tr.ID, "qobuz:"), 10, 64); err == nil {
					qID = parsed
				}
			}
			if qID > 0 && strings.Contains(uri, "/media/qobuz/"+strconv.FormatInt(qID, 10)) {
				return i
			}
		}
	}
	return -1
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
