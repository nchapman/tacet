package queue

import (
	"context"
	"fmt"
	"math/rand/v2"
	"strings"
	"time"

	"tacet/internal/library"
	"tacet/internal/upnp"
)

func nextAutoIndexLocked(z *zone) int {
	if len(z.queue) == 0 || z.currentIndex < 0 {
		return -1
	}
	if z.repeat == RepeatOne {
		return z.currentIndex
	}
	if z.currentIndex+1 < len(z.queue) {
		return z.currentIndex + 1
	}
	if z.repeat == RepeatAll {
		return 0
	}
	return -1
}

func nextManualIndexLocked(z *zone) int {
	if len(z.queue) == 0 {
		return -1
	}
	if z.currentIndex+1 < len(z.queue) {
		return z.currentIndex + 1
	}
	if z.repeat == RepeatAll || z.repeat == RepeatOne {
		return 0
	}
	return -1
}

// shuffleItemsKeepingPin returns a shuffled copy of items with items[pinIdx]
// placed first (index 0) when pinIdx is in bounds.
func shuffleItemsKeepingPin(items []Item, pinIdx int) []Item {
	if len(items) <= 1 {
		out := make([]Item, len(items))
		copy(out, items)
		return out
	}
	rest := make([]Item, 0, len(items))
	var pinned Item
	hasPin := pinIdx >= 0 && pinIdx < len(items)
	for i, it := range items {
		if hasPin && i == pinIdx {
			pinned = it
			continue
		}
		rest = append(rest, it)
	}
	//nolint:gosec // music queue shuffle is non-cryptographic
	rand.Shuffle(len(rest), func(i, j int) {
		rest[i], rest[j] = rest[j], rest[i]
	})
	if !hasPin {
		return rest
	}
	out := make([]Item, 0, len(items))
	out = append(out, pinned)
	out = append(out, rest...)
	return out
}

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
	repeat := z.repeat
	m.mu.RUnlock()

	if qc, ok := ctrl.(upnp.QueueController); ok {
		items := make([]Item, len(tracks))
		for i, tr := range tracks {
			items[i] = Item{ItemID: m.nextItemID(), Track: tr}
		}
		// Validate every track BEFORE mutating z.queue so an invalid or
		// unauthenticated track never wipes the active queue.
		if _, _, lerr := m.buildQueueMediaItems(ctrl, items, startIndex, quality); lerr != nil {
			m.emitZoneError(zoneID, lerr)
			return ZoneSnapshot{}, lerr
		}
		m.mu.Lock()
		playIdx := startIndex
		if z.shuffle {
			z.originalQueue = append([]Item(nil), items...)
			z.queue = shuffleItemsKeepingPin(items, startIndex)
			playIdx = 0
		} else {
			z.originalQueue = nil
			z.queue = items
		}
		queueCopy := append([]Item(nil), z.queue...)
		z.currentIndex = playIdx
		z.status.Position = 0
		z.nextPreloaded = false
		z.nextPreloading = false
		z.nextFailed = false
		z.nextURI = ""
		z.nextIndex = -1
		z.userStopped = false
		z.wasPlaying = false
		z.generation++
		z.revision++
		gen := z.generation
		qEv := m.queueEventLocked(z)
		m.mu.Unlock()

		m.emit("zone.queue", zoneID, qEv)
		if rerr := m.replaceDeviceQueueAndPlay(ctx, z, queueCopy, playIdx, gen, ctrl, qc, quality, repeat); rerr != nil {
			return ZoneSnapshot{}, rerr
		}
		snap, _ := m.Zone(zoneID)
		return snap, nil
	}

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
	playIdx := startIndex
	if z.shuffle {
		z.originalQueue = append([]Item(nil), items...)
		z.queue = shuffleItemsKeepingPin(items, startIndex)
		playIdx = 0
	} else {
		z.originalQueue = nil
		z.queue = items
	}
	z.currentIndex = playIdx
	z.status.Position = 0
	z.nextPreloaded = false
	z.nextPreloading = false
	z.nextFailed = false
	z.nextURI = ""
	z.nextIndex = -1
	z.userStopped = false
	z.wasPlaying = false
	z.generation++
	z.revision++
	gen := z.generation
	qEv := m.queueEventLocked(z)
	m.mu.Unlock()

	m.emit("zone.queue", zoneID, qEv)

	if err := m.playPreparedMedia(ctx, z, playIdx, gen, ctrl, media, resolvedTrack, effQuality); err != nil {
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
	var insertAt int
	prevLen := len(z.queue)
	clearedPreload := false
	if wasEmpty {
		z.queue = added
		insertAt = 0
		if autoplay {
			z.currentIndex = 0
			startIdx = 0
		}
	} else if pos == PositionNext && z.currentIndex >= 0 && z.currentIndex < len(z.queue) {
		insertAt = z.currentIndex + 1
		updated := make([]Item, 0, len(z.queue)+len(added))
		updated = append(updated, z.queue[:insertAt]...)
		updated = append(updated, added...)
		updated = append(updated, z.queue[insertAt:]...)
		z.queue = updated
		// Next track changed, so clear any previously preloaded URI.
		clearedPreload = invalidatePreloadLocked(z)
	} else {
		insertAt = len(z.queue)
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
	queueCopy := append([]Item(nil), z.queue...)
	quality := z.audioQuality
	repeat := z.repeat
	ctrl := z.ctrl
	qEv := m.queueEventLocked(z)
	pbEv := m.playbackEventLocked(z)
	m.mu.Unlock()

	if qc, ok := ctrl.(upnp.QueueController); ok {
		m.emit("zone.queue", zoneID, qEv)
		if startIdx >= 0 {
			if err := m.replaceDeviceQueueAndPlay(ctx, z, queueCopy, startIdx, gen, ctrl, qc, quality, repeat); err != nil {
				return ZoneSnapshot{}, err
			}
		} else if !wasEmpty {
			if addedMedia, _, merr := m.buildQueueMediaItems(ctrl, added, 0, quality); merr == nil {
				if err := qc.AppendQueue(ctx, addedMedia); err == nil && insertAt < prevLen {
					for i := 0; i < len(addedMedia); i++ {
						_ = qc.MoveQueueItem(ctx, prevLen+i, insertAt+i)
					}
				}
			}
			m.emit("zone.playback", zoneID, pbEv)
		} else {
			m.emit("zone.playback", zoneID, pbEv)
		}
		snap, _ := m.Zone(zoneID)
		return snap, nil
	}

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
	ctrl := z.ctrl
	m.mu.Unlock()

	if qc, ok := ctrl.(upnp.QueueController); ok {
		if err := m.playDeviceQueueIndex(ctx, zoneID, z, qc, idx, gen); err != nil {
			return ZoneSnapshot{}, err
		}
		snap, _ := m.Zone(zoneID)
		return snap, nil
	}

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
	removingNext := idx == z.currentIndex+1 || (z.nextIndex >= 0 && idx == z.nextIndex)
	clearedPreload := false
	z.queue = append(z.queue[:idx], z.queue[idx+1:]...)

	switch {
	case removingCurrent:
		invalidatePreloadLocked(z)
		if len(z.queue) == 0 {
			z.currentIndex = -1
		} else if z.currentIndex >= len(z.queue) {
			z.currentIndex = len(z.queue) - 1
		}
	case removingNext:
		if idx < z.currentIndex {
			z.currentIndex--
		}
		clearedPreload = invalidatePreloadLocked(z)
	case idx < z.currentIndex:
		z.currentIndex--
		if z.nextIndex > idx {
			z.nextIndex--
		}
	default:
		if z.nextIndex > idx {
			z.nextIndex--
		}
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

	if qc, ok := ctrl.(upnp.QueueController); ok {
		_ = qc.RemoveQueueRange(ctx, idx, idx)
		if removingCurrent {
			if nextIdx >= 0 {
				if err := m.playDeviceQueueIndex(ctx, zoneID, z, qc, nextIdx, gen); err != nil {
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
				z.generation++
				z.revision++
				pbEv = m.playbackEventLocked(z)
				m.mu.Unlock()
				m.emit("zone.playback", zoneID, pbEv)
			}
		} else {
			m.emit("zone.playback", zoneID, pbEv)
		}
		snap, _ := m.Zone(zoneID)
		return snap, nil
	}

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
			z.generation++
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
		clearedPreload = invalidatePreloadLocked(z)
		z.revision++
	}
	ctrl := z.ctrl
	qEv := m.queueEventLocked(z)
	pbEv := m.playbackEventLocked(z)
	snap := m.snapshotLocked(z)
	m.mu.Unlock()

	if qc, ok := ctrl.(upnp.QueueController); ok && fromIndex != toIndex {
		mctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		_ = qc.MoveQueueItem(mctx, fromIndex, toIndex)
		cancel()
	} else if clearedPreload {
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
	prevLen := len(z.queue)
	z.queue = make([]Item, 0)
	z.originalQueue = nil
	delete(m.savedQueues, zoneID)
	z.currentIndex = -1
	z.nextPreloaded = false
	z.nextPreloading = false
	z.nextFailed = false
	z.nextURI = ""
	z.nextIndex = -1
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

	if qc, ok := ctrl.(upnp.QueueController); ok && prevLen > 0 {
		_ = qc.RemoveQueueRange(ctx, 0, prevLen-1)
	}
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
	needsReload := (z.status.State == "STOPPED" || z.status.State == "NO_MEDIA_PRESENT" ||
		(z.currentURI == "" && z.status.State != "PAUSED_PLAYBACK")) &&
		idx >= 0 && idx < len(z.queue)
	var gen uint64
	if needsReload {
		z.generation++
		gen = z.generation
	}
	queueCopy := append([]Item(nil), z.queue...)
	quality := z.audioQuality
	repeat := z.repeat
	ctrl := z.ctrl
	m.mu.Unlock()

	if needsReload {
		if qc, ok := ctrl.(upnp.QueueController); ok {
			if err := m.replaceDeviceQueueAndPlay(ctx, z, queueCopy, idx, gen, ctrl, qc, quality, repeat); err != nil {
				return ZoneSnapshot{}, err
			}
		} else if err := m.loadAndPlay(ctx, z, idx, gen); err != nil {
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
	now := time.Now().UTC()
	if z.status.State == "PLAYING" && !z.positionUpdatedAt.IsZero() {
		pos := z.status.Position + now.Sub(z.positionUpdatedAt)
		if z.status.Duration > 0 && pos > z.status.Duration {
			pos = z.status.Duration
		}
		z.status.Position = pos
	}
	z.status.State = "PAUSED_PLAYBACK"
	z.positionUpdatedAt = now
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
	z.generation++
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
	nextIdx := nextManualIndexLocked(z)
	if nextIdx < 0 {
		m.mu.Unlock()
		return ZoneSnapshot{}, ErrEndOfQueue
	}
	z.currentIndex = nextIdx
	z.nextPreloaded = false
	z.nextPreloading = false
	z.nextFailed = false
	z.nextURI = ""
	z.nextIndex = -1
	z.userStopped = false
	z.wasPlaying = false
	z.generation++
	z.revision++
	gen := z.generation
	ctrl := z.ctrl
	m.mu.Unlock()

	if qc, ok := ctrl.(upnp.QueueController); ok {
		if err := m.playDeviceQueueIndex(ctx, zoneID, z, qc, nextIdx, gen); err != nil {
			return ZoneSnapshot{}, err
		}
		snap, _ := m.Zone(zoneID)
		return snap, nil
	}

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
	repeat := z.repeat
	qLen := len(z.queue)
	supportsSeek := z.ctrl.Info().SupportsSeek
	m.mu.Unlock()

	if pos > 5*time.Second && supportsSeek {
		return m.seekLockedOp(ctx, zoneID, z, 0)
	}
	target := curIdx
	if pos <= 5*time.Second {
		target = curIdx - 1
		if target < 0 {
			if repeat == RepeatAll && qLen > 0 {
				target = qLen - 1
			} else {
				target = 0
			}
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
	ctrl := z.ctrl
	m.mu.Unlock()

	if qc, ok := ctrl.(upnp.QueueController); ok {
		if err := m.playDeviceQueueIndex(ctx, zoneID, z, qc, target, gen); err != nil {
			return ZoneSnapshot{}, err
		}
		snap, _ := m.Zone(zoneID)
		return snap, nil
	}

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
	m.mu.RLock()
	activeTranscode := z.activeQuality.NeedsTranscode()
	isPlayer := z.ctrl.Info().Kind == "player"
	m.mu.RUnlock()
	if activeTranscode && !isPlayer {
		return ZoneSnapshot{}, fmt.Errorf("%w: seeking is unavailable on live-transcoded UPnP streams", ErrCapabilityUnsupported)
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

// SetShuffle toggles shuffle on a zone. Turning shuffle on preserves the
// currently playing item at index 0 and shuffles the remaining tracks; turning
// it off restores the original order while staying on the active track.
func (m *Manager) SetShuffle(_ context.Context, zoneID string, enabled bool) (ZoneSnapshot, error) {
	z, err := m.lookupZone(zoneID)
	if err != nil {
		return ZoneSnapshot{}, err
	}
	z.opMu.Lock()
	defer z.opMu.Unlock()

	m.mu.Lock()
	if z.shuffle == enabled {
		snap := m.snapshotLocked(z)
		m.mu.Unlock()
		return snap, nil
	}

	z.shuffle = enabled
	curItemID := ""
	if z.currentIndex >= 0 && z.currentIndex < len(z.queue) {
		curItemID = z.queue[z.currentIndex].ItemID
	}

	if enabled {
		z.originalQueue = append([]Item(nil), z.queue...)
		if len(z.queue) > 1 {
			z.queue = shuffleItemsKeepingPin(z.queue, z.currentIndex)
			if curItemID != "" {
				z.currentIndex = 0
			}
		}
	} else {
		if len(z.originalQueue) > 0 {
			// Restore original order for items still present in the queue,
			// then append any items added while shuffle was enabled.
			present := make(map[string]Item, len(z.queue))
			for _, it := range z.queue {
				present[it.ItemID] = it
			}
			restored := make([]Item, 0, len(z.queue))
			seen := make(map[string]bool, len(z.queue))
			for _, it := range z.originalQueue {
				if cur, ok := present[it.ItemID]; ok {
					restored = append(restored, cur)
					seen[it.ItemID] = true
				}
			}
			for _, it := range z.queue {
				if !seen[it.ItemID] {
					restored = append(restored, it)
				}
			}
			z.queue = restored
		}
		z.originalQueue = nil
		if curItemID != "" {
			z.currentIndex = findItemIndexLocked(z.queue, curItemID)
		}
	}

	clearedPreload := invalidatePreloadLocked(z)
	z.revision++
	ctrl := z.ctrl
	queueCopy := append([]Item(nil), z.queue...)
	curIdx := z.currentIndex
	quality := z.audioQuality
	repeat := z.repeat
	active := z.currentURI != "" && (z.status.State == "PLAYING" || z.status.State == "PAUSED_PLAYBACK" || z.status.State == "TRANSITIONING")
	if _, isQC := ctrl.(upnp.QueueController); isQC && !active {
		// Force next Play/Jump to reload the reordered queue onto the device.
		z.currentURI = ""
	}
	qEv := m.queueEventLocked(z)
	pbEv := m.playbackEventLocked(z)
	snap := m.snapshotLocked(z)
	m.mu.Unlock()

	if qc, ok := ctrl.(upnp.QueueController); ok && active && len(queueCopy) > 0 {
		sctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		if curIdx >= 0 && curIdx < len(queueCopy) {
			// Rebuild upcoming tracks in-place on the device without interrupting
			// the currently playing track; if the current track moved (e.g. unshuffle),
			// reload the queue cleanly at curIdx.
			if enabled && curIdx == 0 && len(queueCopy) > 1 {
				_ = qc.RemoveQueueRange(sctx, 1, len(queueCopy)-1)
				if tail, _, err := m.buildQueueMediaItems(ctrl, queueCopy[1:], 0, quality); err == nil {
					_ = qc.AppendQueue(sctx, tail)
				}
			} else {
				if all, _, err := m.buildQueueMediaItems(ctrl, queueCopy, curIdx, quality); err == nil {
					_ = qc.ReplaceQueue(sctx, all, curIdx)
					_ = qc.SetLoopMode(sctx, string(repeat), false)
				}
			}
		}
		cancel()
	} else if clearedPreload {
		m.clearNextPreloadAsync(ctrl)
	}
	m.emit("zone.queue", zoneID, qEv)
	m.emit("zone.playback", zoneID, pbEv)
	return snap, nil
}

// SetRepeat sets a zone's repeat mode ("off", "all", or "one").
func (m *Manager) SetRepeat(ctx context.Context, zoneID string, mode RepeatMode) (ZoneSnapshot, error) {
	norm, ok := ParseRepeatMode(string(mode))
	if !ok {
		return ZoneSnapshot{}, fmt.Errorf("%w: invalid repeat mode %q", ErrInvalidArgument, mode)
	}
	z, err := m.lookupZone(zoneID)
	if err != nil {
		return ZoneSnapshot{}, err
	}
	z.opMu.Lock()
	defer z.opMu.Unlock()

	m.mu.Lock()
	cur := z.repeat
	if cur == "" {
		cur = RepeatOff
	}
	if cur == norm {
		snap := m.snapshotLocked(z)
		m.mu.Unlock()
		return snap, nil
	}

	z.repeat = norm
	clearedPreload := invalidatePreloadLocked(z)
	z.revision++
	ctrl := z.ctrl
	pbEv := m.playbackEventLocked(z)
	snap := m.snapshotLocked(z)
	m.mu.Unlock()

	if qc, ok := ctrl.(upnp.QueueController); ok {
		_ = qc.SetLoopMode(ctx, string(norm), false)
	} else if clearedPreload {
		m.clearNextPreloadAsync(ctrl)
	}
	m.scheduleQueuePersist()
	m.emit("zone.playback", zoneID, pbEv)
	return snap, nil
}

func (m *Manager) playDeviceQueueIndex(ctx context.Context, zoneID string, z *zone, qc upnp.QueueController, idx int, gen uint64) error {
	m.mu.RLock()
	needsReload := z.currentURI == "" || z.status.State == "STOPPED" || z.status.State == "NO_MEDIA_PRESENT" || z.deviceQueueDirty
	queueCopy := append([]Item(nil), z.queue...)
	quality := z.audioQuality
	repeat := z.repeat
	ctrl := z.ctrl
	m.mu.RUnlock()

	if needsReload && len(queueCopy) > 0 && idx >= 0 && idx < len(queueCopy) {
		return m.replaceDeviceQueueAndPlay(ctx, z, queueCopy, idx, gen, ctrl, qc, quality, repeat)
	}
	if err := qc.PlayIndex(ctx, idx); err != nil {
		// Device queue may be missing/stale (e.g. after speaker reboot or server restart);
		// fall back to reloading the full queue at idx before surfacing an error.
		if len(queueCopy) > 0 && idx >= 0 && idx < len(queueCopy) {
			return m.replaceDeviceQueueAndPlay(ctx, z, queueCopy, idx, gen, ctrl, qc, quality, repeat)
		}
		werr := wrapRendererErr("PlayIndex", err)
		m.emitZoneError(zoneID, werr)
		return werr
	}
	m.mu.Lock()
	if z.generation == gen {
		z.currentIndex = idx
		z.status.State = "TRANSITIONING"
		z.status.Position = 0
		if idx >= 0 && idx < len(z.queue) {
			z.status.Duration = time.Duration(z.queue[idx].Track.DurationSec) * time.Second
		}
		z.positionUpdatedAt = time.Now().UTC()
		z.lastPlaybackEmit = z.positionUpdatedAt
		z.wasPlaying = false
		z.loadedAt = time.Now()
		z.revision++
	}
	pbEv := m.playbackEventLocked(z)
	m.mu.Unlock()
	m.scheduleQueuePersist()
	m.emit("zone.playback", zoneID, pbEv)
	return nil
}

func findItemIndexLocked(items []Item, itemID string) int {
	for i, it := range items {
		if it.ItemID == itemID {
			return i
		}
	}
	return -1
}
