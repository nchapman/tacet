package queue

import (
	"context"
	"errors"
	"sync"
	"testing"
	"time"

	"tacet/internal/library"
	"tacet/internal/upnp"
)

type fakeQueueController struct {
	mu           sync.Mutex
	info         upnp.RendererInfo
	status       upnp.TransportStatus
	replaceCalls int
	playIdxCalls int
	playIdxErr   error
	lastItems    []upnp.MediaItem
	lastStartIdx int
	loopMode     string
}

func (f *fakeQueueController) Info() upnp.RendererInfo { return f.info }
func (f *fakeQueueController) LocalIP() string         { return "192.168.1.10" }
func (f *fakeQueueController) SetURI(_ context.Context, _ upnp.MediaItem) error {
	return nil
}
func (f *fakeQueueController) SetNextURI(_ context.Context, _ upnp.MediaItem) error {
	return nil
}
func (f *fakeQueueController) Play(_ context.Context) error  { return nil }
func (f *fakeQueueController) Pause(_ context.Context) error { return nil }
func (f *fakeQueueController) Stop(_ context.Context) error  { return nil }
func (f *fakeQueueController) Seek(_ context.Context, _ time.Duration) error {
	return nil
}
func (f *fakeQueueController) Status(_ context.Context) (upnp.TransportStatus, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.status, nil
}
func (f *fakeQueueController) SetVolume(_ context.Context, _ int) error { return nil }
func (f *fakeQueueController) SetMute(_ context.Context, _ bool) error  { return nil }
func (f *fakeQueueController) Next(_ context.Context) error             { return nil }
func (f *fakeQueueController) Previous(_ context.Context) error         { return nil }
func (f *fakeQueueController) AppendQueue(_ context.Context, _ []upnp.MediaItem) error {
	return nil
}
func (f *fakeQueueController) RemoveQueueRange(_ context.Context, _, _ int) error {
	return nil
}
func (f *fakeQueueController) MoveQueueItem(_ context.Context, _, _ int) error {
	return nil
}
func (f *fakeQueueController) ReplaceQueue(_ context.Context, items []upnp.MediaItem, startIndex int) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.replaceCalls++
	f.lastItems = append([]upnp.MediaItem(nil), items...)
	f.lastStartIdx = startIndex
	return nil
}
func (f *fakeQueueController) PlayIndex(_ context.Context, _ int) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.playIdxCalls++
	return f.playIdxErr
}
func (f *fakeQueueController) SetLoopMode(_ context.Context, repeat string, _ bool) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.loopMode = repeat
	return nil
}

func TestRestartResync_ExternalPlaybackIgnored(t *testing.T) {
	mgr := NewManager(4320, "http://192.168.1.10:4320", nil)
	zoneID := "uuid:linkplay-1"
	tracks := []library.Track{
		{ID: "local:t1", Source: library.SourceLocal, Title: "Track 1", DurationSec: 200, MIME: "audio/flac"},
		{ID: "local:t2", Source: library.SourceLocal, Title: "Track 2", DurationSec: 210, MIME: "audio/flac"},
	}
	mgr.LoadZoneQueues(map[string]SavedZoneQueue{
		zoneID: {Tracks: tracks, CurrentIndex: 0, Repeat: RepeatAll},
	})
	ctrl := &fakeQueueController{
		info: upnp.RendererInfo{ID: zoneID, Name: "AudioPro A15", SupportsQueue: true, HasVolume: true},
		status: upnp.TransportStatus{
			State:         "PLAYING",
			TrackURI:      "spotify:track:external123",
			HasPosition:   true,
			Position:      45 * time.Second,
			Duration:      180 * time.Second,
			HasQueueIndex: true,
			QueueIndex:    1,
			HasVolume:     true,
			HasVolValue:   true,
			Volume:        42,
		},
	}
	mgr.RegisterRenderer(ctrl)

	mgr.pollOne(context.Background(), zoneID)

	snap, ok := mgr.Zone(zoneID)
	if !ok {
		t.Fatal("zone missing")
	}
	if snap.State != "stopped" {
		t.Fatalf("State = %q, want stopped when external source is playing", snap.State)
	}
	if snap.CurrentIndex != 0 {
		t.Fatalf("CurrentIndex = %d, want 0 (external source must not hijack index)", snap.CurrentIndex)
	}
	if len(snap.Queue) != 2 {
		t.Fatalf("Queue len = %d, want 2 (saved queue preserved)", len(snap.Queue))
	}
	if snap.Volume != 42 {
		t.Fatalf("Volume = %d, want 42 (volume still synced)", snap.Volume)
	}
}

func TestRestartResync_RestoredQueueJumpFallsBackToReplaceQueue(t *testing.T) {
	mgr := NewManager(4320, "http://192.168.1.10:4320", nil)
	zoneID := "cast:nest-1"
	tracks := []library.Track{
		{ID: "local:t1", Source: library.SourceLocal, Title: "Track 1", DurationSec: 200, MIME: "audio/flac"},
		{ID: "local:t2", Source: library.SourceLocal, Title: "Track 2", DurationSec: 210, MIME: "audio/flac"},
	}
	mgr.LoadZoneQueues(map[string]SavedZoneQueue{
		zoneID: {Tracks: tracks, CurrentIndex: 0},
	})
	ctrl := &fakeQueueController{
		info:       upnp.RendererInfo{ID: zoneID, Name: "Master Bathroom", Kind: "cast", SupportsQueue: true, HasVolume: true},
		status:     upnp.TransportStatus{State: "STOPPED"},
		playIdxErr: errors.New("cast PlayIndex 1 out of range [0,0)"),
	}
	mgr.RegisterRenderer(ctrl)

	snap, err := mgr.Next(context.Background(), zoneID)
	if err != nil {
		t.Fatalf("Next on restored stopped zone failed: %v", err)
	}
	if snap.CurrentIndex != 1 {
		t.Fatalf("CurrentIndex = %d, want 1", snap.CurrentIndex)
	}
	ctrl.mu.Lock()
	replaceCalls := ctrl.replaceCalls
	lastStartIdx := ctrl.lastStartIdx
	ctrl.mu.Unlock()
	if replaceCalls != 1 || lastStartIdx != 1 {
		t.Fatalf("expected ReplaceQueue(startIndex=1), got replaceCalls=%d lastStartIdx=%d", replaceCalls, lastStartIdx)
	}
}

func TestClearQueue_DoesNotResurrectAfterUnregister(t *testing.T) {
	mgr := NewManager(4320, "http://192.168.1.10:4320", nil)
	zoneID := "uuid:linkplay-2"
	tracks := []library.Track{
		{ID: "local:t1", Source: library.SourceLocal, Title: "Track 1", DurationSec: 200, MIME: "audio/flac"},
	}
	mgr.LoadZoneQueues(map[string]SavedZoneQueue{
		zoneID: {Tracks: tracks, CurrentIndex: 0},
	})
	ctrl := &fakeQueueController{
		info: upnp.RendererInfo{ID: zoneID, Name: "AudioPro C10", SupportsQueue: true},
	}
	mgr.RegisterRenderer(ctrl)

	if _, err := mgr.ClearQueue(context.Background(), zoneID); err != nil {
		t.Fatalf("ClearQueue: %v", err)
	}
	mgr.UnregisterRenderer(zoneID)
	if saved := mgr.SavedZoneQueues(); len(saved) != 0 {
		t.Fatalf("SavedZoneQueues = %v, want empty after ClearQueue+Unregister", saved)
	}

	mgr.RegisterRenderer(ctrl)
	snap, _ := mgr.Zone(zoneID)
	if len(snap.Queue) != 0 {
		t.Fatalf("cleared queue resurrected with %d items", len(snap.Queue))
	}
}

func TestHandleDeviceEvent_VolumeOnlyDoesNotTriggerAutoPlay(t *testing.T) {
	mgr := NewManager(4320, "http://192.168.1.10:4320", nil)
	mgr.runCtx = context.Background()
	zoneID := "uuid:linkplay-vol"
	tracks := []library.Track{
		{ID: "local:t1", Source: library.SourceLocal, Title: "Track 1", DurationSec: 200, MIME: "audio/flac"},
	}
	mgr.LoadZoneQueues(map[string]SavedZoneQueue{
		zoneID: {Tracks: tracks, CurrentIndex: 0},
	})
	playCalled := false
	ctrl := &fakeQueueController{
		info: upnp.RendererInfo{ID: zoneID, Name: "AudioPro A15", SupportsQueue: true, HasVolume: true},
	}
	mgr.RegisterRenderer(ctrl)

	// Simulate an initial RenderingControl GENA volume-only event on startup.
	mgr.HandleDeviceEvent(upnp.DeviceEvent{
		RendererID: zoneID,
		HasVolume:  true,
		Volume:     35,
	})

	snap, _ := mgr.Zone(zoneID)
	if snap.State != "stopped" {
		t.Fatalf("State = %q, want stopped after volume-only GENA event", snap.State)
	}
	if snap.Volume != 35 {
		t.Fatalf("Volume = %d, want 35", snap.Volume)
	}
	if playCalled || ctrl.replaceCalls != 0 || ctrl.playIdxCalls != 0 {
		t.Fatalf("volume-only GENA event must not trigger playback")
	}
}

func TestAwaitingAudio_HoldsBufferingUntilPositionAdvances(t *testing.T) {
	mgr := NewManager(4320, "http://192.168.1.10:4320", nil)
	zoneID := "uuid:linkplay-buf"
	tracks := []library.Track{
		{ID: "local:t1", Source: library.SourceLocal, Title: "Slow Disk Track", DurationSec: 200, MIME: "audio/flac"},
	}
	ctrl := &fakeQueueController{
		info: upnp.RendererInfo{ID: zoneID, Name: "AudioPro A15", SupportsQueue: true, HasVolume: true},
		status: upnp.TransportStatus{
			State:       "PLAYING",
			TrackURI:    "http://192.168.1.10:4320/media/local/local:t1.flac",
			HasPosition: true,
			Position:    0,
			Duration:    200 * time.Second,
		},
	}
	mgr.RegisterRenderer(ctrl)

	if _, err := mgr.ReplaceQueue(context.Background(), zoneID, tracks, 0); err != nil {
		t.Fatalf("ReplaceQueue: %v", err)
	}

	// GENA push PLAYING without position while still awaiting audio must NOT flicker out of buffering.
	mgr.HandleDeviceEvent(upnp.DeviceEvent{
		RendererID: zoneID,
		Seq:        1,
		HasState:   true,
		State:      "PLAYING",
	})
	snap, _ := mgr.Zone(zoneID)
	if snap.State != "buffering" {
		t.Fatalf("State = %q after positionless GENA PLAYING while awaiting audio, want buffering", snap.State)
	}

	// Renderer reports PLAYING at 0:00 while still waiting on disk/CDN -> must stay buffering.
	mgr.pollOne(context.Background(), zoneID)
	snap, _ = mgr.Zone(zoneID)
	if snap.State != "buffering" {
		t.Fatalf("State = %q at pos=0 while awaiting audio, want buffering", snap.State)
	}

	// Once audio actually starts flowing (Position > 0) -> transitions cleanly to playing.
	ctrl.mu.Lock()
	ctrl.status.Position = 1 * time.Second
	ctrl.mu.Unlock()
	mgr.pollOne(context.Background(), zoneID)
	snap, _ = mgr.Zone(zoneID)
	if snap.State != "playing" {
		t.Fatalf("State = %q once pos=1s, want playing", snap.State)
	}
	if snap.PositionSec != 1 {
		t.Fatalf("PositionSec = %d, want 1", snap.PositionSec)
	}
}

func TestAwaitingAudio_UnconditionalTimeoutAndStopClear(t *testing.T) {
	mgr := NewManager(4320, "http://192.168.1.10:4320", nil)
	zoneID := "uuid:linkplay-timeout"
	tracks := []library.Track{
		{ID: "local:t1", Source: library.SourceLocal, Title: "Live Stream", DurationSec: 200, MIME: "audio/flac"},
	}
	ctrl := &fakeQueueController{
		info: upnp.RendererInfo{ID: zoneID, Name: "Quirky Renderer", SupportsQueue: true},
		status: upnp.TransportStatus{
			State:       "PLAYING",
			TrackURI:    "http://192.168.1.10:4320/media/local/local:t1.flac",
			HasPosition: true,
			Position:    0,
		},
	}
	mgr.RegisterRenderer(ctrl)
	if _, err := mgr.ReplaceQueue(context.Background(), zoneID, tracks, 0); err != nil {
		t.Fatalf("ReplaceQueue: %v", err)
	}

	// Simulate loadedAt > 10s ago with Position still 0 -> must exit buffering unconditionally.
	mgr.mu.Lock()
	mgr.zones[zoneID].loadedAt = time.Now().Add(-11 * time.Second)
	mgr.mu.Unlock()
	mgr.pollOne(context.Background(), zoneID)
	snap, _ := mgr.Zone(zoneID)
	if snap.State != "playing" {
		t.Fatalf("State = %q after 10s timeout at pos=0, want playing", snap.State)
	}

	// Stop clears awaitingAudio and bumps generation.
	if _, err := mgr.ReplaceQueue(context.Background(), zoneID, tracks, 0); err != nil {
		t.Fatalf("ReplaceQueue 2: %v", err)
	}
	mgr.mu.RLock()
	genBefore := mgr.zones[zoneID].generation
	mgr.mu.RUnlock()
	if _, err := mgr.Stop(context.Background(), zoneID); err != nil {
		t.Fatalf("Stop: %v", err)
	}
	mgr.mu.RLock()
	z := mgr.zones[zoneID]
	stillAwaiting := z.awaitingAudio
	genAfter := z.generation
	mgr.mu.RUnlock()
	if stillAwaiting {
		t.Fatal("Stop did not clear awaitingAudio")
	}
	if genAfter <= genBefore {
		t.Fatalf("Stop did not bump generation (%d -> %d)", genBefore, genAfter)
	}
}

func TestGaplessTransition_WithoutPositionResetsPositionAndDoesNotFalseScrobble(t *testing.T) {
	mgr := NewManager(4320, "http://192.168.1.10:4320", nil)
	played := make([]string, 0)
	var playedMu sync.Mutex
	mgr.SetOnTrackPlayed(func(tr library.Track) {
		playedMu.Lock()
		played = append(played, tr.ID)
		playedMu.Unlock()
	})
	zoneID := "uuid:upnp-gapless"
	r := newFakeRenderer(zoneID, "WiiM")
	mgr.RegisterRenderer(r)
	tracks := []library.Track{
		{ID: "local:t1", Source: library.SourceLocal, Title: "Track 1", DurationSec: 200, MIME: "audio/flac"},
		{ID: "local:t2", Source: library.SourceLocal, Title: "Track 2", DurationSec: 200, MIME: "audio/flac"},
	}
	if _, err := mgr.ReplaceQueue(context.Background(), zoneID, tracks, 0); err != nil {
		t.Fatalf("ReplaceQueue: %v", err)
	}
	// Advance Track 1 past 60s and arm gapless.
	r.mu.Lock()
	r.position = 60 * time.Second
	r.mu.Unlock()
	mgr.pollOne(context.Background(), zoneID)

	// GENA announces gapless transition to Track 2 with HasPosition=false.
	r.mu.Lock()
	nextURL := r.nextURI
	r.mu.Unlock()
	if nextURL == "" {
		t.Fatal("expected SetNextURI to arm gapless before transition")
	}
	mgr.HandleDeviceEvent(upnp.DeviceEvent{
		RendererID:  zoneID,
		Seq:         2,
		HasState:    true,
		State:       "PLAYING",
		HasTrackURI: true,
		TrackURI:    nextURL,
	})
	snap, _ := mgr.Zone(zoneID)
	if snap.CurrentIndex != 1 {
		t.Fatalf("CurrentIndex = %d, want 1", snap.CurrentIndex)
	}
	if snap.PositionSec != 0 {
		t.Fatalf("PositionSec = %d after gapless transition without RelTime, want 0", snap.PositionSec)
	}
	time.Sleep(20 * time.Millisecond)
	playedMu.Lock()
	defer playedMu.Unlock()
	for _, id := range played {
		if id == "local:t2" {
			t.Fatal("Track 2 was falsely scrobbled at 0:00 on gapless transition")
		}
	}
}
