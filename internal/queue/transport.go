package queue

import (
	"context"
	"fmt"
	"strings"
	"time"

	"tacet/internal/library"
)

// Position specifies where new tracks are inserted into a zone queue.
type Position string

const (
	// PositionEnd appends items to the end of the queue.
	PositionEnd Position = "end"
	// PositionNext inserts items immediately after the currently playing item.
	PositionNext Position = "next"
)

// ReplaceQueue replaces a zone's queue with tracks and starts playback at startIndex.
func (m *Manager) ReplaceQueue(ctx context.Context, zoneID string, tracks []library.Track, startIndex int) (ZoneSnapshot, error) {
	if len(tracks) == 0 {
		return ZoneSnapshot{}, fmt.Errorf("%w: no tracks provided", ErrInvalidArgument)
	}
	if startIndex < 0 || startIndex >= len(tracks) {
		return ZoneSnapshot{}, fmt.Errorf("%w: startIndex %d out of range [0, %d)", ErrInvalidArgument, startIndex, len(tracks))
	}
	z, err := m.lookupZone(zoneID)
	if err != nil {
		return ZoneSnapshot{}, err
	}

	z.opMu.Lock()
	defer z.opMu.Unlock()

	m.mu.RLock()
	ctrl := z.ctrl
	quality := z.audioQuality
	m.mu.RUnlock()

	// Validate format support and resolve stream URL BEFORE mutating z.queue so
	// an unsupported format or unauthenticated Qobuz track never wipes the active queue.
	media, resolvedTrack, effQuality, err := m.buildMediaItem(ctx, ctrl, tracks[startIndex], quality)
	if err != nil {
		m.emitZoneError(zoneID, err)
		return ZoneSnapshot{}, err
	}

	items := make([]Item, len(tracks))
	for i, tr := range tracks {
		items[i] = Item{ItemID: m.nextItemID(), Track: tr}
	}
	items[startIndex].Track = resolvedTrack

	m.mu.Lock()
	z.queue = items
	z.currentIndex = startIndex
	z.nextPreloaded = false
	z.nextPreloading = false
	z.nextFailed = false
	z.nextURI = ""
	z.userStopped = false
	z.wasPlaying = false
	z.generation++
	z.revision++
	gen := z.generation
	qEv := m.queueEventLocked(z)
	m.mu.Unlock()

	m.emit("zone.queue", zoneID, qEv)

	if err := m.playPreparedMedia(ctx, z, startIndex, gen, ctrl, media, resolvedTrack, effQuality); err != nil {
		return ZoneSnapshot{}, err
	}
	snap, _ := m.Zone(zoneID)
	return snap, nil
}

// AddQueueItems inserts tracks at the end of the queue or immediately after the current track.
// If autoplay is true and the zone is idle/stopped (or had no current track), playback starts automatically.
func (m *Manager) AddQueueItems(ctx context.Context, zoneID string, tracks []library.Track, pos Position, autoplay bool) (ZoneSnapshot, error) {
	if len(tracks) == 0 {
		return ZoneSnapshot{}, fmt.Errorf("%w: no tracks provided", ErrInvalidArgument)
	}
	if pos == "" {
		pos = PositionEnd
	}
	if pos != PositionEnd && pos != PositionNext {
		return ZoneSnapshot{}, fmt.Errorf("%w: position must be 'end' or 'next'", ErrInvalidArgument)
	}
	z, err := m.lookupZone(zoneID)
	if err != nil {
		return ZoneSnapshot{}, err
	}
	z.opMu.Lock()
	defer z.opMu.Unlock()

	added := make([]Item, len(tracks))
	for i, tr := range tracks {
		added[i] = Item{ItemID: m.nextItemID(), Track: tr}
	}

	m.mu.Lock()
	wasEmpty := len(z.queue) == 0
	isPlaying := z.status.State == "PLAYING" || z.status.State == "TRANSITIONING"

	startIdx := -1
	clearedPreload := false
	if wasEmpty {
		z.queue = added
		if autoplay {
			z.currentIndex = 0
			startIdx = 0
		}
	} else if pos == PositionNext && z.currentIndex >= 0 && z.currentIndex < len(z.queue) {
		insertAt := z.currentIndex + 1
		updated := make([]Item, 0, len(z.queue)+len(added))
		updated = append(updated, z.queue[:insertAt]...)
		updated = append(updated, added...)
		updated = append(updated, z.queue[insertAt:]...)
		z.queue = updated
		// Next track changed, so clear any previously preloaded URI.
		clearedPreload = z.nextPreloaded
		z.nextPreloaded = false
		z.nextPreloading = false
		z.nextFailed = false
		z.nextURI = ""
	} else {
		insertAt := len(z.queue)
		z.queue = append(z.queue, added...)
		if autoplay && !isPlaying && z.currentIndex < 0 {
			z.currentIndex = insertAt
			startIdx = insertAt
		}
		if z.currentIndex == insertAt-1 {
			z.nextFailed = false
		}
	}

	var gen uint64
	if startIdx >= 0 {
		z.userStopped = false
		z.wasPlaying = false
		z.generation++
		gen = z.generation
	}
	z.revision++
	ctrl := z.ctrl
	qEv := m.queueEventLocked(z)
	pbEv := m.playbackEventLocked(z)
	m.mu.Unlock()

	if clearedPreload {
		m.clearNextPreloadAsync(ctrl)
	}
	m.emit("zone.queue", zoneID, qEv)
	if startIdx >= 0 {
		if err := m.loadAndPlay(ctx, z, startIdx, gen); err != nil {
			return ZoneSnapshot{}, err
		}
	} else {
		m.emit("zone.playback", zoneID, pbEv)
	}
	snap, _ := m.Zone(zoneID)
	return snap, nil
}

// JumpToItem starts playback at the queue entry with the given itemId.
func (m *Manager) JumpToItem(ctx context.Context, zoneID, itemID string) (ZoneSnapshot, error) {
	if strings.TrimSpace(itemID) == "" {
		return ZoneSnapshot{}, fmt.Errorf("%w: itemId is required", ErrInvalidArgument)
	}
	z, err := m.lookupZone(zoneID)
	if err != nil {
		return ZoneSnapshot{}, err
	}
	z.opMu.Lock()
	defer z.opMu.Unlock()

	m.mu.Lock()
	idx := findItemIndexLocked(z.queue, itemID)
	if idx < 0 {
		m.mu.Unlock()
		return ZoneSnapshot{}, fmt.Errorf("%w: %q", ErrQueueItemNotFound, itemID)
	}
	z.currentIndex = idx
	z.nextPreloaded = false
	z.nextPreloading = false
	z.nextFailed = false
	z.nextURI = ""
	z.userStopped = false
	z.wasPlaying = false
	z.generation++
	z.revision++
	gen := z.generation
	m.mu.Unlock()

	if err := m.loadAndPlay(ctx, z, idx, gen); err != nil {
		return ZoneSnapshot{}, err
	}
	snap, _ := m.Zone(zoneID)
	return snap, nil
}

// RemoveItem deletes a queue entry by itemId, returning ErrQueueItemNotFound if absent.
func (m *Manager) RemoveItem(ctx context.Context, zoneID, itemID string) (ZoneSnapshot, error) {
	if strings.TrimSpace(itemID) == "" {
		return ZoneSnapshot{}, fmt.Errorf("%w: itemId is required", ErrInvalidArgument)
	}
	z, err := m.lookupZone(zoneID)
	if err != nil {
		return ZoneSnapshot{}, err
	}
	z.opMu.Lock()
	defer z.opMu.Unlock()

	m.mu.Lock()
	idx := findItemIndexLocked(z.queue, itemID)
	if idx < 0 {
		m.mu.Unlock()
		return ZoneSnapshot{}, fmt.Errorf("%w: %q", ErrQueueItemNotFound, itemID)
	}
	removingCurrent := idx == z.currentIndex
	removingNext := idx == z.currentIndex+1
	clearedPreload := removingNext && z.nextPreloaded
	z.queue = append(z.queue[:idx], z.queue[idx+1:]...)

	switch {
	case idx < z.currentIndex:
		z.currentIndex--
	case removingCurrent:
		z.nextPreloaded = false
		z.nextPreloading = false
		z.nextFailed = false
		z.nextURI = ""
		if len(z.queue) == 0 {
			z.currentIndex = -1
		} else if z.currentIndex >= len(z.queue) {
			z.currentIndex = len(z.queue) - 1
		}
	case removingNext:
		z.nextPreloaded = false
		z.nextPreloading = false
		z.nextFailed = false
		z.nextURI = ""
	}
	z.revision++
	nextIdx := z.currentIndex
	var gen uint64
	if removingCurrent {
		z.generation++
		gen = z.generation
	}
	qEv := m.queueEventLocked(z)
	pbEv := m.playbackEventLocked(z)
	ctrl := z.ctrl
	m.mu.Unlock()

	m.emit("zone.queue", zoneID, qEv)

	if removingCurrent {
		if nextIdx >= 0 {
			if err := m.loadAndPlay(ctx, z, nextIdx, gen); err != nil {
				return ZoneSnapshot{}, err
			}
		} else {
			_ = ctrl.Stop(ctx)
			m.mu.Lock()
			z.status.State = "STOPPED"
			z.status.Position = 0
			z.positionUpdatedAt = time.Now().UTC()
			z.userStopped = true
			z.wasPlaying = false
			z.revision++
			pbEv = m.playbackEventLocked(z)
			m.mu.Unlock()
			m.emit("zone.playback", zoneID, pbEv)
		}
	} else if removingNext {
		if clearedPreload {
			m.clearNextPreloadAsync(ctrl)
		}
		m.emit("zone.playback", zoneID, pbEv)
	}

	snap, _ := m.Zone(zoneID)
	return snap, nil
}

// MoveItem reorders a queue entry to toIndex (0-based), preserving the current playing item.
func (m *Manager) MoveItem(zoneID, itemID string, toIndex int) (ZoneSnapshot, error) {
	if strings.TrimSpace(itemID) == "" {
		return ZoneSnapshot{}, fmt.Errorf("%w: itemId is required", ErrInvalidArgument)
	}
	z, err := m.lookupZone(zoneID)
	if err != nil {
		return ZoneSnapshot{}, err
	}
	z.opMu.Lock()
	defer z.opMu.Unlock()

	m.mu.Lock()
	fromIndex := findItemIndexLocked(z.queue, itemID)
	if fromIndex < 0 {
		m.mu.Unlock()
		return ZoneSnapshot{}, fmt.Errorf("%w: %q", ErrQueueItemNotFound, itemID)
	}
	if toIndex < 0 || toIndex >= len(z.queue) {
		m.mu.Unlock()
		return ZoneSnapshot{}, fmt.Errorf("%w: toIndex %d out of range [0, %d)", ErrInvalidArgument, toIndex, len(z.queue))
	}

	clearedPreload := false
	if fromIndex != toIndex {
		curItemID := ""
		if z.currentIndex >= 0 && z.currentIndex < len(z.queue) {
			curItemID = z.queue[z.currentIndex].ItemID
		}
		moved := z.queue[fromIndex]
		without := append(append([]Item(nil), z.queue[:fromIndex]...), z.queue[fromIndex+1:]...)
		reordered := make([]Item, 0, len(z.queue))
		reordered = append(reordered, without[:toIndex]...)
		reordered = append(reordered, moved)
		reordered = append(reordered, without[toIndex:]...)
		z.queue = reordered

		if curItemID != "" {
			z.currentIndex = findItemIndexLocked(z.queue, curItemID)
		}
		// Order around next track may have changed; re-arm gapless cleanly on next tick.
		clearedPreload = z.nextPreloaded
		z.nextPreloaded = false
		z.nextPreloading = false
		z.nextFailed = false
		z.nextURI = ""
		z.revision++
	}
	ctrl := z.ctrl
	qEv := m.queueEventLocked(z)
	pbEv := m.playbackEventLocked(z)
	snap := m.snapshotLocked(z)
	m.mu.Unlock()

	if clearedPreload {
		m.clearNextPreloadAsync(ctrl)
	}
	m.emit("zone.queue", zoneID, qEv)
	m.emit("zone.playback", zoneID, pbEv)
	return snap, nil
}

// ClearQueue empties a zone's queue and stops playback.
func (m *Manager) ClearQueue(ctx context.Context, zoneID string) (ZoneSnapshot, error) {
	z, err := m.lookupZone(zoneID)
	if err != nil {
		return ZoneSnapshot{}, err
	}
	z.opMu.Lock()
	defer z.opMu.Unlock()

	m.mu.Lock()
	z.queue = make([]Item, 0)
	z.currentIndex = -1
	z.nextPreloaded = false
	z.nextPreloading = false
	z.nextFailed = false
	z.nextURI = ""
	z.currentURI = ""
	z.activeQuality = ""
	z.nextQuality = ""
	z.userStopped = true
	z.wasPlaying = false
	z.status.State = "STOPPED"
	z.status.Position = 0
	z.positionUpdatedAt = time.Now().UTC()
	z.generation++
	z.revision++
	ctrl := z.ctrl
	qEv := m.queueEventLocked(z)
	pbEv := m.playbackEventLocked(z)
	snap := m.snapshotLocked(z)
	m.mu.Unlock()

	_ = ctrl.Stop(ctx)
	m.emit("zone.queue", zoneID, qEv)
	m.emit("zone.playback", zoneID, pbEv)
	return snap, nil
}

// Play resumes playback or reloads the current queued track if stopped.
func (m *Manager) Play(ctx context.Context, zoneID string) (ZoneSnapshot, error) {
	z, err := m.lookupZone(zoneID)
	if err != nil {
		return ZoneSnapshot{}, err
	}
	z.opMu.Lock()
	defer z.opMu.Unlock()

	m.mu.Lock()
	// Never send a Play command to a renderer when nothing is loaded or paused on it.
	if len(z.queue) == 0 && z.currentURI == "" && z.status.State != "PLAYING" && z.status.State != "PAUSED_PLAYBACK" {
		snap := m.snapshotLocked(z)
		m.mu.Unlock()
		return snap, nil
	}
	z.userStopped = false
	idx := z.currentIndex
	if idx < 0 && len(z.queue) > 0 {
		idx = 0
	}
	needsReload := (z.status.State == "STOPPED" || z.status.State == "NO_MEDIA_PRESENT" || z.currentURI == "") &&
		idx >= 0 && idx < len(z.queue)
	var gen uint64
	if needsReload {
		z.generation++
		gen = z.generation
	}
	ctrl := z.ctrl
	m.mu.Unlock()

	if needsReload {
		if err := m.loadAndPlay(ctx, z, idx, gen); err != nil {
			return ZoneSnapshot{}, err
		}
		snap, _ := m.Zone(zoneID)
		return snap, nil
	}
	if err := ctrl.Play(ctx); err != nil {
		return ZoneSnapshot{}, wrapRendererErr("Play", err)
	}
	m.mu.Lock()
	z.status.State = "PLAYING"
	z.positionUpdatedAt = time.Now().UTC()
	z.revision++
	pbEv := m.playbackEventLocked(z)
	snap := m.snapshotLocked(z)
	m.mu.Unlock()
	m.emit("zone.playback", zoneID, pbEv)
	return snap, nil
}

// Pause pauses playback on the target zone.
func (m *Manager) Pause(ctx context.Context, zoneID string) (ZoneSnapshot, error) {
	z, err := m.lookupZone(zoneID)
	if err != nil {
		return ZoneSnapshot{}, err
	}
	z.opMu.Lock()
	defer z.opMu.Unlock()

	if err := z.ctrl.Pause(ctx); err != nil {
		return ZoneSnapshot{}, wrapRendererErr("Pause", err)
	}
	m.mu.Lock()
	z.status.State = "PAUSED_PLAYBACK"
	z.positionUpdatedAt = time.Now().UTC()
	z.revision++
	pbEv := m.playbackEventLocked(z)
	snap := m.snapshotLocked(z)
	m.mu.Unlock()
	m.emit("zone.playback", zoneID, pbEv)
	return snap, nil
}

// Stop halts playback on the target zone and suppresses auto-advance.
func (m *Manager) Stop(ctx context.Context, zoneID string) (ZoneSnapshot, error) {
	z, err := m.lookupZone(zoneID)
	if err != nil {
		return ZoneSnapshot{}, err
	}
	z.opMu.Lock()
	defer z.opMu.Unlock()

	m.mu.Lock()
	z.userStopped = true
	z.wasPlaying = false
	ctrl := z.ctrl
	m.mu.Unlock()

	if err := ctrl.Stop(ctx); err != nil {
		return ZoneSnapshot{}, wrapRendererErr("Stop", err)
	}
	m.mu.Lock()
	z.status.State = "STOPPED"
	z.status.Position = 0
	z.positionUpdatedAt = time.Now().UTC()
	z.revision++
	pbEv := m.playbackEventLocked(z)
	snap := m.snapshotLocked(z)
	m.mu.Unlock()
	m.emit("zone.playback", zoneID, pbEv)
	return snap, nil
}

// Next skips to the next track in the zone queue.
func (m *Manager) Next(ctx context.Context, zoneID string) (ZoneSnapshot, error) {
	z, err := m.lookupZone(zoneID)
	if err != nil {
		return ZoneSnapshot{}, err
	}
	z.opMu.Lock()
	defer z.opMu.Unlock()

	m.mu.Lock()
	if len(z.queue) == 0 {
		m.mu.Unlock()
		return ZoneSnapshot{}, ErrQueueEmpty
	}
	nextIdx := z.currentIndex + 1
	if nextIdx >= len(z.queue) {
		m.mu.Unlock()
		return ZoneSnapshot{}, ErrEndOfQueue
	}
	z.currentIndex = nextIdx
	z.nextPreloaded = false
	z.nextPreloading = false
	z.nextFailed = false
	z.nextURI = ""
	z.userStopped = false
	z.wasPlaying = false
	z.generation++
	z.revision++
	gen := z.generation
	m.mu.Unlock()

	if err := m.loadAndPlay(ctx, z, nextIdx, gen); err != nil {
		return ZoneSnapshot{}, err
	}
	snap, _ := m.Zone(zoneID)
	return snap, nil
}

// Prev restarts the current track if past 5s, otherwise jumps to the previous track.
func (m *Manager) Prev(ctx context.Context, zoneID string) (ZoneSnapshot, error) {
	z, err := m.lookupZone(zoneID)
	if err != nil {
		return ZoneSnapshot{}, err
	}
	z.opMu.Lock()
	defer z.opMu.Unlock()

	m.mu.Lock()
	if len(z.queue) == 0 {
		m.mu.Unlock()
		return ZoneSnapshot{}, ErrQueueEmpty
	}
	pos := z.status.Position
	curIdx := z.currentIndex
	supportsSeek := z.ctrl.Info().SupportsSeek
	m.mu.Unlock()

	if pos > 5*time.Second && supportsSeek {
		return m.seekLockedOp(ctx, zoneID, z, 0)
	}
	target := curIdx
	if pos <= 5*time.Second {
		target = curIdx - 1
		if target < 0 {
			target = 0
		}
	}
	m.mu.Lock()
	z.currentIndex = target
	z.nextPreloaded = false
	z.nextPreloading = false
	z.nextFailed = false
	z.nextURI = ""
	z.userStopped = false
	z.wasPlaying = false
	z.generation++
	z.revision++
	gen := z.generation
	m.mu.Unlock()

	if err := m.loadAndPlay(ctx, z, target, gen); err != nil {
		return ZoneSnapshot{}, err
	}
	snap, _ := m.Zone(zoneID)
	return snap, nil
}

// Seek moves playback to positionSec seconds on the target zone.
func (m *Manager) Seek(ctx context.Context, zoneID string, positionSec int) (ZoneSnapshot, error) {
	if positionSec < 0 {
		return ZoneSnapshot{}, fmt.Errorf("%w: positionSec must be >= 0", ErrInvalidArgument)
	}
	z, err := m.lookupZone(zoneID)
	if err != nil {
		return ZoneSnapshot{}, err
	}
	z.opMu.Lock()
	defer z.opMu.Unlock()
	return m.seekLockedOp(ctx, zoneID, z, positionSec)
}

func (m *Manager) seekLockedOp(ctx context.Context, zoneID string, z *zone, positionSec int) (ZoneSnapshot, error) {
	if !z.ctrl.Info().SupportsSeek {
		return ZoneSnapshot{}, fmt.Errorf("%w: renderer does not support Seek", ErrCapabilityUnsupported)
	}
	d := time.Duration(positionSec) * time.Second
	if err := z.ctrl.Seek(ctx, d); err != nil {
		return ZoneSnapshot{}, wrapRendererErr("Seek", err)
	}
	m.mu.Lock()
	z.status.Position = d
	z.positionUpdatedAt = time.Now().UTC()
	z.revision++
	pbEv := m.playbackEventLocked(z)
	snap := m.snapshotLocked(z)
	m.mu.Unlock()
	m.emit("zone.playback", zoneID, pbEv)
	return snap, nil
}

// SetVolume sets renderer hardware volume in [0, 100].
func (m *Manager) SetVolume(ctx context.Context, zoneID string, vol int) (ZoneSnapshot, error) {
	if vol < 0 || vol > 100 {
		return ZoneSnapshot{}, fmt.Errorf("%w: volume must be between 0 and 100", ErrInvalidArgument)
	}
	z, err := m.lookupZone(zoneID)
	if err != nil {
		return ZoneSnapshot{}, err
	}
	if !z.ctrl.Info().HasVolume {
		return ZoneSnapshot{}, fmt.Errorf("%w: renderer does not expose RenderingControl", ErrCapabilityUnsupported)
	}
	if err := z.ctrl.SetVolume(ctx, vol); err != nil {
		return ZoneSnapshot{}, wrapRendererErr("SetVolume", err)
	}
	m.mu.Lock()
	z.status.Volume = vol
	z.revision++
	volEv := m.volumeEventLocked(z)
	snap := m.snapshotLocked(z)
	m.mu.Unlock()
	m.emit("zone.volume", zoneID, volEv)
	return snap, nil
}

// SetMute toggles renderer mute state.
func (m *Manager) SetMute(ctx context.Context, zoneID string, muted bool) (ZoneSnapshot, error) {
	z, err := m.lookupZone(zoneID)
	if err != nil {
		return ZoneSnapshot{}, err
	}
	if !z.ctrl.Info().HasVolume {
		return ZoneSnapshot{}, fmt.Errorf("%w: renderer does not expose RenderingControl", ErrCapabilityUnsupported)
	}
	if err := z.ctrl.SetMute(ctx, muted); err != nil {
		return ZoneSnapshot{}, wrapRendererErr("SetMute", err)
	}
	m.mu.Lock()
	z.status.Muted = muted
	z.revision++
	volEv := m.volumeEventLocked(z)
	snap := m.snapshotLocked(z)
	m.mu.Unlock()
	m.emit("zone.volume", zoneID, volEv)
	return snap, nil
}

func findItemIndexLocked(items []Item, itemID string) int {
	for i, it := range items {
		if it.ItemID == itemID {
			return i
		}
	}
	return -1
}
