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

type fakeRenderer struct {
	mu            sync.Mutex
	info          upnp.RendererInfo
	localIP       string
	state         string
	currentURI    string
	nextURI       string
	setNextErr    error
	position      time.Duration
	duration      time.Duration
	volume        int
	muted         bool
	setURICalls   []string
	setNextCalls  []string
	playCalls     int
	stopCalls     int
}

func newFakeRenderer(id, name string) *fakeRenderer {
	return &fakeRenderer{
		info: upnp.RendererInfo{
			ID:              id,
			Name:            name,
			HasVolume:       true,
			SupportsSetNext: true,
			SupportsSeek:    true,
		},
		localIP: "192.168.1.5",
		state:   "STOPPED",
		volume:  50,
	}
}

func (f *fakeRenderer) Info() upnp.RendererInfo { return f.info }
func (f *fakeRenderer) LocalIP() string         { return f.localIP }

func (f *fakeRenderer) SetURI(_ context.Context, item upnp.MediaItem) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.currentURI = item.URL
	f.nextURI = ""
	f.duration = item.Duration
	f.position = 0
	f.setURICalls = append(f.setURICalls, item.URL)
	return nil
}

func (f *fakeRenderer) SetNextURI(_ context.Context, item upnp.MediaItem) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.setNextCalls = append(f.setNextCalls, item.URL)
	if f.setNextErr != nil {
		return f.setNextErr
	}
	f.nextURI = item.URL
	return nil
}

func (f *fakeRenderer) Play(_ context.Context) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.state = "PLAYING"
	f.playCalls++
	return nil
}

func (f *fakeRenderer) Pause(_ context.Context) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.state = "PAUSED_PLAYBACK"
	return nil
}

func (f *fakeRenderer) Stop(_ context.Context) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.state = "STOPPED"
	f.stopCalls++
	return nil
}

func (f *fakeRenderer) Seek(_ context.Context, pos time.Duration) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.position = pos
	return nil
}

func (f *fakeRenderer) Status(_ context.Context) (upnp.TransportStatus, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	return upnp.TransportStatus{
		State:       f.state,
		Position:    f.position,
		Duration:    f.duration,
		TrackURI:    f.currentURI,
		HasPosition: true,
		Volume:      f.volume,
		HasVolValue: true,
		Muted:       f.muted,
		HasMuteVal:  true,
		HasVolume:   true,
	}, nil
}

func (f *fakeRenderer) SetVolume(_ context.Context, vol int) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.volume = vol
	return nil
}

func (f *fakeRenderer) SetMute(_ context.Context, muted bool) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.muted = muted
	return nil
}

func TestManager_GaplessSetNextAndTransition(t *testing.T) {
	r := newFakeRenderer("uuid:wiim", "WiiM Ultra")
	mgr := NewManager(8080, "", nil)
	mgr.RegisterRenderer(r)

	tracks := []library.Track{
		{ID: "t1", Source: library.SourceLocal, Title: "Track 1", MIME: "audio/flac", Ext: ".flac", DurationSec: 200},
		{ID: "t2", Source: library.SourceLocal, Title: "Track 2", MIME: "audio/flac", Ext: ".flac", DurationSec: 210},
		{ID: "t3", Source: library.SourceLocal, Title: "Track 3", MIME: "audio/flac", Ext: ".flac", DurationSec: 220},
	}

	ctx := context.Background()
	if err := mgr.PlayTracks(ctx, "", tracks, 0); err != nil {
		t.Fatalf("PlayTracks failed: %v", err)
	}

	// First poll sees PLAYING and preloads track 2 via SetNextAVTransportURI.
	mgr.pollOne(ctx, "uuid:wiim")

	snap, _ := mgr.Snapshot("uuid:wiim")
	if !snap.GaplessArmed {
		t.Fatal("expected GaplessArmed=true after poll while playing")
	}
	if len(r.setNextCalls) != 1 || r.setNextCalls[0] != "http://192.168.1.5:8080/media/local/t2.flac" {
		t.Fatalf("unexpected SetNextURI calls: %v", r.setNextCalls)
	}

	// Simulate WiiM Ultra seamlessly transitioning to track 2!
	r.mu.Lock()
	r.currentURI = r.nextURI
	r.nextURI = ""
	r.mu.Unlock()

	mgr.pollOne(ctx, "uuid:wiim")

	snap, _ = mgr.Snapshot("uuid:wiim")
	if snap.CurrentIndex != 1 || snap.CurrentTrack.ID != "t2" {
		t.Fatalf("expected queue to advance to t2 (index 1), got idx=%d track=%+v", snap.CurrentIndex, snap.CurrentTrack)
	}
	// And track 3 should immediately be preloaded!
	if !snap.GaplessArmed || len(r.setNextCalls) != 2 || r.setNextCalls[1] != "http://192.168.1.5:8080/media/local/t3.flac" {
		t.Fatalf("expected t3 to be preloaded, got calls=%v armed=%v", r.setNextCalls, snap.GaplessArmed)
	}
}

func TestManager_FallbackAutoAdvanceWhenSetNextUnsupported(t *testing.T) {
	r := newFakeRenderer("uuid:legacy", "Legacy Renderer")
	r.setNextErr = errors.New("action SetNextAVTransportURI not implemented")
	mgr := NewManager(8080, "", nil)
	mgr.RegisterRenderer(r)

	tracks := []library.Track{
		{ID: "t1", Source: library.SourceLocal, Title: "Track 1", MIME: "audio/flac", Ext: ".flac"},
		{ID: "t2", Source: library.SourceLocal, Title: "Track 2", MIME: "audio/flac", Ext: ".flac"},
	}

	ctx := context.Background()
	if err := mgr.PlayTracks(ctx, "", tracks, 0); err != nil {
		t.Fatal(err)
	}

	// Poll while playing: SetNext fails gracefully.
	mgr.pollOne(ctx, "uuid:legacy")
	snap, _ := mgr.Snapshot("uuid:legacy")
	if snap.GaplessArmed {
		t.Fatal("expected GaplessArmed=false when SetNext fails")
	}

	// Simulate track 1 finishing -> renderer enters STOPPED.
	r.mu.Lock()
	r.state = "STOPPED"
	r.mu.Unlock()

	mgr.pollOne(ctx, "uuid:legacy")
	snap, _ = mgr.Snapshot("uuid:legacy")
	if snap.CurrentIndex != 1 || snap.CurrentTrack.ID != "t2" {
		t.Fatalf("expected fallback auto-advance to t2, got idx=%d", snap.CurrentIndex)
	}
	if len(r.setURICalls) != 2 || r.setURICalls[1] != "http://192.168.1.5:8080/media/local/t2.flac" {
		t.Fatalf("expected SetURI called for t2, got %v", r.setURICalls)
	}
}

func TestManager_DoesNotCascadeAdvanceWhileBuffering(t *testing.T) {
	r := newFakeRenderer("uuid:wiim-buf", "Living Room Wiim")
	mgr := NewManager(8080, "", nil)
	mgr.RegisterRenderer(r)

	tracks := []library.Track{
		{ID: "t1", Source: library.SourceLocal, Title: "Track 1", MIME: "audio/flac", Ext: ".flac"},
		{ID: "t2", Source: library.SourceLocal, Title: "Track 2", MIME: "audio/flac", Ext: ".flac"},
		{ID: "t3", Source: library.SourceLocal, Title: "Track 3", MIME: "audio/flac", Ext: ".flac"},
	}

	ctx := context.Background()
	if err := mgr.PlayTracks(ctx, "", tracks, 0); err != nil {
		t.Fatal(err)
	}

	// Simulate WiiM Ultra reporting STOPPED briefly right after Play() while buffering.
	r.mu.Lock()
	r.state = "STOPPED"
	r.mu.Unlock()

	mgr.pollOne(ctx, "uuid:wiim-buf")
	mgr.pollOne(ctx, "uuid:wiim-buf")

	snap, _ := mgr.Snapshot("uuid:wiim-buf")
	if snap.CurrentIndex != 0 || snap.CurrentTrack.ID != "t1" {
		t.Fatalf("expected manager to stay on t1 while buffering, cascaded to idx=%d (%+v)", snap.CurrentIndex, snap.CurrentTrack)
	}
	if len(r.setURICalls) != 1 {
		t.Fatalf("expected only 1 SetURI call, got %v", r.setURICalls)
	}
}

func TestManager_CachesPermanentSetNextUnsupported(t *testing.T) {
	r := newFakeRenderer("uuid:audiopro", "AudioPro A15")
	r.setNextErr = upnp.ErrActionUnsupported

	mgr := NewManager(8080, "", nil)
	mgr.RegisterRenderer(r)

	tracks := []library.Track{
		{ID: "t1", Source: library.SourceLocal, Title: "Track 1", MIME: "audio/flac", Ext: ".flac"},
		{ID: "t2", Source: library.SourceLocal, Title: "Track 2", MIME: "audio/flac", Ext: ".flac"},
		{ID: "t3", Source: library.SourceLocal, Title: "Track 3", MIME: "audio/flac", Ext: ".flac"},
	}

	ctx := context.Background()
	if err := mgr.PlayTracks(ctx, "", tracks, 0); err != nil {
		t.Fatal(err)
	}

	// Track 1 plays -> tries SetNextURI once, gets ErrActionUnsupported, caches it.
	mgr.pollOne(ctx, "uuid:audiopro")
	if len(r.setNextCalls) != 1 {
		t.Fatalf("expected 1 SetNextURI attempt, got %d", len(r.setNextCalls))
	}

	// Renderer capability list should now reflect SupportsSetNext=false.
	if rs := mgr.Renderers(); len(rs) != 1 || rs[0].SupportsSetNext {
		t.Fatalf("expected cached SupportsSetNext=false, got %+v", rs)
	}

	// Track 1 finishes -> fallback auto-advances to Track 2.
	r.mu.Lock()
	r.state = "STOPPED"
	r.mu.Unlock()
	mgr.pollOne(ctx, "uuid:audiopro")

	// Track 2 plays -> should NOT attempt SetNextURI again because it's cached unsupported.
	mgr.pollOne(ctx, "uuid:audiopro")
	if len(r.setNextCalls) != 1 {
		t.Fatalf("expected SetNextURI not to be retried after permanent unsupported error, got %d calls", len(r.setNextCalls))
	}
}

func TestManager_HandleDeviceEvent_InstantGaplessAndVolume(t *testing.T) {
	r := newFakeRenderer("uuid:wiim-gena", "Living Room Wiim")
	mgr := NewManager(8080, "", nil)
	mgr.RegisterRenderer(r)

	tracks := []library.Track{
		{ID: "t1", Source: library.SourceLocal, Title: "Track 1", MIME: "audio/flac", Ext: ".flac"},
		{ID: "t2", Source: library.SourceLocal, Title: "Track 2", MIME: "audio/flac", Ext: ".flac"},
		{ID: "t3", Source: library.SourceLocal, Title: "Track 3", MIME: "audio/flac", Ext: ".flac"},
	}

	ctx := context.Background()
	if err := mgr.PlayTracks(ctx, "", tracks, 0); err != nil {
		t.Fatal(err)
	}

	// Renderer pushes PLAYING event via GENA -> immediately preloads track 2!
	mgr.HandleDeviceEvent(upnp.DeviceEvent{
		RendererID:  "uuid:wiim-gena",
		Service:     upnp.ServiceAVTransport,
		Seq:         1,
		HasState:    true,
		State:       "PLAYING",
		HasTrackURI: true,
		TrackURI:    "http://192.168.1.5:8080/media/local/t1.flac",
	})

	snap, _ := mgr.Snapshot("uuid:wiim-gena")
	if !snap.GaplessArmed {
		t.Fatal("expected GENA PLAYING event to arm gapless preload for track 2")
	}

	// Renderer pushes hardware volume knob change via RenderingControl GENA.
	mgr.HandleDeviceEvent(upnp.DeviceEvent{
		RendererID: "uuid:wiim-gena",
		Service:    upnp.ServiceRenderingControl,
		Seq:        1,
		HasVolume:  true,
		Volume:     73,
		HasMute:    true,
		Muted:      true,
	})
	snap, _ = mgr.Snapshot("uuid:wiim-gena")
	if snap.Volume != 73 || !snap.Muted {
		t.Fatalf("expected hardware volume=73 muted=true, got vol=%d muted=%v", snap.Volume, snap.Muted)
	}

	// Renderer pushes gapless transition to track 2 via GENA -> advances index & preloads track 3!
	mgr.HandleDeviceEvent(upnp.DeviceEvent{
		RendererID:  "uuid:wiim-gena",
		Service:     upnp.ServiceAVTransport,
		Seq:         2,
		HasState:    true,
		State:       "PLAYING",
		HasTrackURI: true,
		TrackURI:    "http://192.168.1.5:8080/media/local/t2.flac",
	})
	snap, _ = mgr.Snapshot("uuid:wiim-gena")
	if snap.CurrentIndex != 1 || snap.CurrentTrack.ID != "t2" {
		t.Fatalf("expected instant gapless transition to t2, got idx=%d (%+v)", snap.CurrentIndex, snap.CurrentTrack)
	}
	if !snap.GaplessArmed || len(r.setNextCalls) != 2 {
		t.Fatalf("expected track 3 preloaded immediately, setNextCalls=%v", r.setNextCalls)
	}
}
