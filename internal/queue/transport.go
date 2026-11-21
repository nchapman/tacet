package queue

import (
	"context"
	"errors"
	"fmt"
	"time"

	"tacet/internal/library"
)

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
	} else if !z.nextPreloaded {
		// Allow next-track preload to run if we were on the final track.
		z.nextFailed = false
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
