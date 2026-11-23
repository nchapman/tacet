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
