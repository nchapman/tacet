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
	mu           sync.Mutex
	info         upnp.RendererInfo
	localIP      string
	state        string
	currentURI   string
	nextURI      string
	position     time.Duration
	duration     time.Duration
	volume       int
	muted        bool
	setURICalls  []string
	setNextCalls []string
	playCalls    int
	playErr      error
	onPlay       func(uri string)
	failSetNext  bool
	setNextErr   error
}

func newFakeRenderer(id, name string) *fakeRenderer {
	return &fakeRenderer{
		info: upnp.RendererInfo{
			ID:              id,
			Name:            name,
			Host:            "192.168.1.50",
			HasVolume:       true,
			SupportsSetNext: true,
			SupportsSeek:    true,
			SinkProtocols:   []string{"http-get:*:audio/flac:*"},
		},
		localIP: "192.168.1.10",
		state:   "STOPPED",
		volume:  40,
	}
}

func (f *fakeRenderer) Info() upnp.RendererInfo { return f.info }
func (f *fakeRenderer) LocalIP() string         { return f.localIP }

func (f *fakeRenderer) SetURI(_ context.Context, item upnp.MediaItem) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.currentURI = item.URL
	f.duration = item.Duration
	f.position = 0
	f.setURICalls = append(f.setURICalls, item.URL)
	return nil
}

func (f *fakeRenderer) SetNextURI(_ context.Context, item upnp.MediaItem) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	if f.setNextErr != nil {
		return f.setNextErr
	}
	if f.failSetNext {
		return errors.New("SetNextAVTransportURI unsupported")
	}
	f.nextURI = item.URL
	f.setNextCalls = append(f.setNextCalls, item.URL)
	return nil
}

func (f *fakeRenderer) Play(_ context.Context) error {
	f.mu.Lock()
	f.playCalls++
	if f.playErr != nil {
		err := f.playErr
		f.mu.Unlock()
		return err
	}
	f.state = "PLAYING"
	if f.position == 0 {
		f.position = time.Second
	}
	uri := f.currentURI
	cb := f.onPlay
	f.mu.Unlock()
	if cb != nil {
		cb(uri)
	}
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
	return nil
}

func (f *fakeRenderer) Seek(_ context.Context, pos time.Duration) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.position = pos
	return nil
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
		HasVolume:   f.info.HasVolume,
	}, nil
}

func TestManager_GaplessPreloadAndTransition(t *testing.T) {
	r := newFakeRenderer("uuid:wiim-1", "Living Room WiiM")
	mgr := NewManager(8080, "", nil)
	mgr.RegisterRenderer(r)

	tracks := []library.Track{
		{ID: "local:t1", Source: library.SourceLocal, Title: "Track 1", MIME: "audio/flac", Ext: ".flac", DurationSec: 180},
		{ID: "local:t2", Source: library.SourceLocal, Title: "Track 2", MIME: "audio/flac", Ext: ".flac", DurationSec: 200},
	}

	ctx := context.Background()
	snap, err := mgr.ReplaceQueue(ctx, "uuid:wiim-1", tracks, 0)
	if err != nil {
		t.Fatalf("ReplaceQueue failed: %v", err)
	}
	if len(snap.Queue) != 2 || snap.Queue[0].ItemID == "" || snap.Queue[0].ItemID == snap.Queue[1].ItemID {
		t.Fatalf("expected unique QueueItem IDs, got %+v", snap.Queue)
	}

	// First poll sees PLAYING -> arms SetNextAVTransportURI for Track 2.
	mgr.pollOne(ctx, "uuid:wiim-1")

	r.mu.Lock()
	nextCount := len(r.setNextCalls)
	nextURL := r.nextURI
	r.mu.Unlock()

	if nextCount != 1 {
		t.Fatalf("expected 1 SetNextURI call, got %d", nextCount)
	}
	wantNext := "http://192.168.1.10:8080/media/local/local:t2.flac"
	if nextURL != wantNext {
		t.Fatalf("expected nextURI %q, got %q", wantNext, nextURL)
	}

	// Simulate renderer seamlessly transitioning to preloaded Track 2.
	r.mu.Lock()
	r.currentURI = nextURL
	r.mu.Unlock()
	mgr.pollOne(ctx, "uuid:wiim-1")

	snap, ok := mgr.Zone("uuid:wiim-1")
	if !ok {
		t.Fatal("missing zone snapshot")
	}
	if snap.CurrentIndex != 1 || snap.CurrentItemID != snap.Queue[1].ItemID || snap.State != StatePlaying {
		t.Fatalf("expected gapless transition to index 1 playing, got idx=%d itemId=%q state=%q",
			snap.CurrentIndex, snap.CurrentItemID, snap.State)
	}
}

func TestManager_FallbackAutoAdvanceWhenSetNextUnsupported(t *testing.T) {
	r := newFakeRenderer("uuid:legacy-1", "Legacy Renderer")
	r.failSetNext = true
	mgr := NewManager(8080, "", nil)
	mgr.RegisterRenderer(r)

	tracks := []library.Track{
		{ID: "local:t1", Source: library.SourceLocal, Title: "Track 1", MIME: "audio/flac", Ext: ".flac", DurationSec: 180},
		{ID: "local:t2", Source: library.SourceLocal, Title: "Track 2", MIME: "audio/flac", Ext: ".flac", DurationSec: 200},
	}

	ctx := context.Background()
	if _, err := mgr.ReplaceQueue(ctx, "uuid:legacy-1", tracks, 0); err != nil {
		t.Fatal(err)
	}

	mgr.pollOne(ctx, "uuid:legacy-1")
	mgr.mu.Lock()
	mgr.zones["uuid:legacy-1"].loadedAt = time.Now().Add(-5 * time.Second)
	mgr.mu.Unlock()

	r.mu.Lock()
	r.state = "STOPPED"
	r.mu.Unlock()

	mgr.pollOne(ctx, "uuid:legacy-1")

	snap, _ := mgr.Zone("uuid:legacy-1")
	if snap.CurrentIndex != 1 {
		t.Fatalf("expected fallback auto-advance to index 1, got %d", snap.CurrentIndex)
	}
}

func TestManager_GENAEventUpdatesStateAndGaplessImmediately(t *testing.T) {
	r := newFakeRenderer("uuid:wiim-gena", "Office WiiM")
	mgr := NewManager(8080, "", nil)
	mgr.RegisterRenderer(r)

	tracks := []library.Track{
		{ID: "local:t1", Source: library.SourceLocal, Title: "Track 1", MIME: "audio/flac", Ext: ".flac", DurationSec: 180},
		{ID: "local:t2", Source: library.SourceLocal, Title: "Track 2", MIME: "audio/flac", Ext: ".flac", DurationSec: 200},
		{ID: "local:t3", Source: library.SourceLocal, Title: "Track 3", MIME: "audio/flac", Ext: ".flac", DurationSec: 210},
	}
	if _, err := mgr.ReplaceQueue(context.Background(), "uuid:wiim-gena", tracks, 0); err != nil {
		t.Fatal(err)
	}

	mgr.HandleDeviceEvent(upnp.DeviceEvent{
		RendererID:  "uuid:wiim-gena",
		Service:     upnp.ServiceAVTransport,
		Seq:         1,
		HasState:    true,
		State:       "PLAYING",
		HasPosition: true,
		Position:    time.Second,
	})

	snap, _ := mgr.Zone("uuid:wiim-gena")
	if !snap.GaplessArmed || len(r.setNextCalls) != 1 {
		t.Fatalf("expected GENA PLAYING event to arm gapless, gapless=%v calls=%v", snap.GaplessArmed, r.setNextCalls)
	}

	mgr.HandleDeviceEvent(upnp.DeviceEvent{
		RendererID: "uuid:wiim-gena",
		Service:    upnp.ServiceRenderingControl,
		Seq:        2,
		HasVolume:  true,
		Volume:     68,
		HasMute:    true,
		Muted:      true,
	})
	snap, _ = mgr.Zone("uuid:wiim-gena")
	if snap.Volume != 68 || !snap.Muted {
		t.Fatalf("expected GENA volume=68 muted=true, got vol=%d muted=%v", snap.Volume, snap.Muted)
	}

	mgr.HandleDeviceEvent(upnp.DeviceEvent{
		RendererID:  "uuid:wiim-gena",
		Service:     upnp.ServiceAVTransport,
		Seq:         3,
		HasState:    true,
		State:       "PLAYING",
		HasTrackURI: true,
		TrackURI:    "http://192.168.1.10:8080/media/local/local:t2.flac",
	})
	snap, _ = mgr.Zone("uuid:wiim-gena")
	if snap.CurrentIndex != 1 {
		t.Fatalf("expected GENA gapless transition to index 1, got %d", snap.CurrentIndex)
	}
	if !snap.GaplessArmed || len(r.setNextCalls) != 2 {
		t.Fatalf("expected track 3 preloaded immediately, setNextCalls=%v", r.setNextCalls)
	}
}

func TestManager_QueueItemIDsIdempotentDeleteMoveAndValidation(t *testing.T) {
	r1 := newFakeRenderer("uuid:living", "Living Room")
	r2 := newFakeRenderer("uuid:office", "Office")
	mgr := NewManager(8080, "", nil)
	mgr.RegisterRenderer(r1)
	mgr.RegisterRenderer(r2)

	ch, unsub := mgr.Subscribe()
	defer unsub()

	tracks := []library.Track{
		{ID: "local:t1", Source: library.SourceLocal, Title: "Track 1", MIME: "audio/flac", Ext: ".flac", DurationSec: 180},
		{ID: "local:t2", Source: library.SourceLocal, Title: "Track 2", MIME: "audio/flac", Ext: ".flac", DurationSec: 200},
		{ID: "local:t3", Source: library.SourceLocal, Title: "Track 3", MIME: "audio/flac", Ext: ".flac", DurationSec: 220},
	}
	ctx := context.Background()

	// AddQueueItems on empty queue with autoplay=true starts playback.
	snap, err := mgr.AddQueueItems(ctx, "uuid:living", tracks[:2], PositionEnd, true)
	if err != nil {
		t.Fatal(err)
	}
	if snap.CurrentIndex != 0 || len(snap.Queue) != 2 {
		t.Fatalf("expected idx=0 len=2, got idx=%d len=%d", snap.CurrentIndex, len(snap.Queue))
	}

	// Play Next inserts t3 right after current (at index 1).
	snap, err = mgr.AddQueueItems(ctx, "uuid:living", tracks[2:], PositionNext, false)
	if err != nil {
		t.Fatal(err)
	}
	if len(snap.Queue) != 3 || snap.Queue[1].Track.ID != "local:t3" {
		t.Fatalf("expected t3 inserted at index 1, got %+v", snap.Queue)
	}

	// MoveItem moves t3 from index 1 to index 2 while keeping current item intact.
	t3ItemID := snap.Queue[1].ItemID
	snap, err = mgr.MoveItem("uuid:living", t3ItemID, 2)
	if err != nil {
		t.Fatal(err)
	}
	if snap.Queue[2].ItemID != t3ItemID || snap.CurrentIndex != 0 {
		t.Fatalf("unexpected order after MoveItem: %+v", snap.Queue)
	}

	// Idempotent RemoveItem: first delete succeeds, second delete returns ErrQueueItemNotFound without touching neighbors.
	snap, err = mgr.RemoveItem(ctx, "uuid:living", t3ItemID)
	if err != nil || len(snap.Queue) != 2 {
		t.Fatalf("first RemoveItem failed: err=%v len=%d", err, len(snap.Queue))
	}
	if _, err = mgr.RemoveItem(ctx, "uuid:living", t3ItemID); !errors.Is(err, ErrQueueItemNotFound) {
		t.Fatalf("expected ErrQueueItemNotFound on second delete, got %v", err)
	}
	snap, _ = mgr.Zone("uuid:living")
	if len(snap.Queue) != 2 {
		t.Fatalf("idempotent retry altered queue length: %d", len(snap.Queue))
	}

	// Strict validation: out-of-range volume / negative seek / end-of-queue errors.
	if _, err = mgr.SetVolume(ctx, "uuid:living", 150); !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("expected ErrInvalidArgument for volume 150, got %v", err)
	}
	if _, err = mgr.Seek(ctx, "uuid:living", -5); !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("expected ErrInvalidArgument for negative seek, got %v", err)
	}
	if _, err = mgr.JumpToItem(ctx, "uuid:living", snap.Queue[1].ItemID); err != nil {
		t.Fatal(err)
	}
	if _, err = mgr.Next(ctx, "uuid:living"); !errors.Is(err, ErrEndOfQueue) {
		t.Fatalf("expected ErrEndOfQueue, got %v", err)
	}

	// Multi-zone SSE: mutating uuid:office must emit events independently of uuid:living.
	if _, err = mgr.SetVolume(ctx, "uuid:office", 55); err != nil {
		t.Fatal(err)
	}
	var sawLiving, sawOffice bool
	for {
		select {
		case ev := <-ch:
			if ev.ZoneID == "uuid:living" {
				sawLiving = true
			}
			if ev.ZoneID == "uuid:office" && ev.Type == "zone.volume" {
				sawOffice = true
			}
		default:
			goto doneDrain
		}
	}
doneDrain:
	if !sawLiving || !sawOffice {
		t.Fatalf("expected independent SSE events for both zones, got living=%v office=%v", sawLiving, sawOffice)
	}
}

func TestManager_PartialStatusFailurePreservesPositionAndVolume(t *testing.T) {
	r := newFakeRenderer("uuid:flaky", "Flaky Renderer")
	mgr := NewManager(8080, "", nil)
	mgr.RegisterRenderer(r)

	tracks := []library.Track{
		{ID: "local:t1", Source: library.SourceLocal, Title: "Track 1", MIME: "audio/flac", Ext: ".flac", DurationSec: 200},
	}
	ctx := context.Background()
	if _, err := mgr.ReplaceQueue(ctx, "uuid:flaky", tracks, 0); err != nil {
		t.Fatal(err)
	}

	r.mu.Lock()
	r.position = 42 * time.Second
	r.duration = 200 * time.Second
	r.volume = 64
	r.muted = true
	r.mu.Unlock()
	mgr.pollOne(ctx, "uuid:flaky")

	mgr.mu.Lock()
	z := mgr.zones["uuid:flaky"]
	mgr.applyTransportUpdateLocked(z, upnp.TransportStatus{
		State:       "PLAYING",
		HasPosition: false,
		HasVolValue: false,
		HasMuteVal:  false,
		HasVolume:   true,
	}, true)
	mgr.mu.Unlock()

	snap, _ := mgr.Zone("uuid:flaky")
	if snap.PositionSec != 42 || snap.DurationSec != 200 || snap.Volume != 64 || !snap.Muted {
		t.Fatalf("expected previous pos/dur/vol/mute preserved, got %+v", snap)
	}
}

func TestManager_ValidateBeforeMutateAndStandbyClearAndPrevFallback(t *testing.T) {
	r := newFakeRenderer("uuid:strict", "Strict Renderer")
	r.info.Kind = "player"
	r.info.SinkProtocols = []string{"http-get:*:audio/flac:*"}
	r.info.SupportsSeek = false

	mgr := NewManager(8080, "", nil)
	mgr.RegisterRenderer(r)

	flac1 := library.Track{ID: "local:t1", Source: library.SourceLocal, Title: "FLAC 1", MIME: "audio/flac", Ext: ".flac", DurationSec: 180}
	flac2 := library.Track{ID: "local:t2", Source: library.SourceLocal, Title: "FLAC 2", MIME: "audio/flac", Ext: ".flac", DurationSec: 190}
	dsd := library.Track{ID: "local:t3", Source: library.SourceLocal, Title: "DSD Track", MIME: "audio/x-dsf", Ext: ".dsf", DurationSec: 200}

	ctx := context.Background()
	snap, err := mgr.ReplaceQueue(ctx, "uuid:strict", []library.Track{flac1, flac2}, 0)
	if err != nil {
		t.Fatalf("ReplaceQueue valid FLAC failed: %v", err)
	}
	// First poll observes PLAYING and arms SetNextAVTransportURI for Track 2.
	mgr.pollOne(ctx, "uuid:strict")
	snap, _ = mgr.Zone("uuid:strict")
	if !snap.GaplessArmed || len(snap.Queue) != 2 {
		t.Fatalf("expected 2 tracks and gaplessArmed=true, got len=%d armed=%v", len(snap.Queue), snap.GaplessArmed)
	}

	// Unsupported DSD track must fail BEFORE mutating the existing queue.
	if _, err = mgr.ReplaceQueue(ctx, "uuid:strict", []library.Track{dsd}, 0); !errors.Is(err, ErrCapabilityUnsupported) {
		t.Fatalf("expected ErrCapabilityUnsupported, got %v", err)
	}
	afterFail, _ := mgr.Zone("uuid:strict")
	if len(afterFail.Queue) != 2 || afterFail.Queue[0].Track.ID != "local:t1" {
		t.Fatalf("failed ReplaceQueue mutated existing queue: %+v", afterFail.Queue)
	}

	// Removing the armed next track must clear the standby URI on the renderer.
	snap, err = mgr.RemoveItem(ctx, "uuid:strict", afterFail.Queue[1].ItemID)
	if err != nil {
		t.Fatalf("RemoveItem armed next failed: %v", err)
	}
	if snap.GaplessArmed {
		t.Fatalf("expected gaplessArmed=false after removing next track")
	}
	deadline := time.Now().Add(500 * time.Millisecond)
	for time.Now().Before(deadline) {
		r.mu.Lock()
		cleared := r.nextURI == ""
		r.mu.Unlock()
		if cleared {
			break
		}
		time.Sleep(5 * time.Millisecond)
	}
	r.mu.Lock()
	clearedNext := r.nextURI
	r.mu.Unlock()
	if clearedNext != "" {
		t.Fatalf("expected renderer nextURI to be cleared to empty string, got %q", clearedNext)
	}

	// Prev when SupportsSeek=false should fall back to re-sending track 0 cleanly instead of ErrUnsupported.
	if _, err := mgr.Prev(ctx, "uuid:strict"); err != nil {
		t.Fatalf("expected Prev fallback on non-seeking renderer to succeed, got %v", err)
	}
}

func TestPlayNoOpWhenNothingLoaded(t *testing.T) {
	mgr := NewManager(8080, "http://192.168.1.10:8080", nil)
	defer mgr.Close()

	r := newFakeRenderer("player:browser1", "This Browser")
	r.playErr = errors.New("no media loaded")
	mgr.RegisterRenderer(r)

	snap, err := mgr.Play(context.Background(), "player:browser1")
	if err != nil {
		t.Fatalf("Play on empty zone returned error: %v", err)
	}
	r.mu.Lock()
	calls := r.playCalls
	r.mu.Unlock()
	if calls != 0 {
		t.Fatalf("expected 0 Play calls to renderer when nothing is loaded, got %d", calls)
	}
	if snap.State != "stopped" {
		t.Fatalf("expected state stopped, got %q", snap.State)
	}
}

func TestManager_ShuffleAndRepeat(t *testing.T) {
	ctx := context.Background()
	mgr := NewManager(8080, "http://192.168.1.10:8080", nil)
	defer mgr.Close()

	r := newFakeRenderer("uuid:sr", "Studio")
	mgr.RegisterRenderer(r)

	tracks := []library.Track{
		{ID: "t1", Source: library.SourceLocal, Title: "One", Ext: ".flac", MIME: "audio/flac", DurationSec: 100},
		{ID: "t2", Source: library.SourceLocal, Title: "Two", Ext: ".flac", MIME: "audio/flac", DurationSec: 100},
		{ID: "t3", Source: library.SourceLocal, Title: "Three", Ext: ".flac", MIME: "audio/flac", DurationSec: 100},
		{ID: "t4", Source: library.SourceLocal, Title: "Four", Ext: ".flac", MIME: "audio/flac", DurationSec: 100},
	}

	snap, err := mgr.ReplaceQueue(ctx, "uuid:sr", tracks, 1) // start on t2
	if err != nil {
		t.Fatal(err)
	}
	if snap.Shuffle || snap.Repeat != RepeatOff {
		t.Fatalf("expected default shuffle=false repeat=off, got %v %q", snap.Shuffle, snap.Repeat)
	}

	// Shuffle pins active track (t2) at index 0 and keeps all 4 items.
	snap, err = mgr.SetShuffle(ctx, "uuid:sr", true)
	if err != nil {
		t.Fatal(err)
	}
	if !snap.Shuffle || snap.CurrentIndex != 0 || snap.Queue[0].Track.ID != "t2" || len(snap.Queue) != 4 {
		t.Fatalf("unexpected shuffled snapshot: idx=%d first=%q len=%d", snap.CurrentIndex, snap.Queue[0].Track.ID, len(snap.Queue))
	}

	// Unshuffling restores original order and keeps currentIndex pointing to t2 (index 1).
	snap, err = mgr.SetShuffle(ctx, "uuid:sr", false)
	if err != nil {
		t.Fatal(err)
	}
	if snap.Shuffle || snap.CurrentIndex != 1 || snap.Queue[1].Track.ID != "t2" || snap.Queue[0].Track.ID != "t1" {
		t.Fatalf("unexpected restored snapshot: idx=%d order=%s,%s", snap.CurrentIndex, snap.Queue[0].Track.ID, snap.Queue[1].Track.ID)
	}

	// Repeat All wraps Next at the end of the queue back to index 0, and Prev from 0 to last.
	snap, err = mgr.SetRepeat(ctx, "uuid:sr", RepeatAll)
	if err != nil || snap.Repeat != RepeatAll {
		t.Fatalf("SetRepeat all failed: err=%v repeat=%q", err, snap.Repeat)
	}
	if _, err = mgr.JumpToItem(ctx, "uuid:sr", snap.Queue[3].ItemID); err != nil {
		t.Fatal(err)
	}
	snap, err = mgr.Next(ctx, "uuid:sr")
	if err != nil || snap.CurrentIndex != 0 {
		t.Fatalf("RepeatAll Next from last should wrap to 0, got idx=%d err=%v", snap.CurrentIndex, err)
	}
	snap, err = mgr.Prev(ctx, "uuid:sr")
	if err != nil || snap.CurrentIndex != 3 {
		t.Fatalf("RepeatAll Prev from 0 should wrap to 3, got idx=%d err=%v", snap.CurrentIndex, err)
	}

	// Invalid repeat mode fails cleanly.
	if _, err := mgr.SetRepeat(ctx, "uuid:sr", RepeatMode("bogus")); !errors.Is(err, ErrInvalidArgument) {
		t.Fatalf("expected ErrInvalidArgument for bogus repeat mode, got %v", err)
	}
}

func TestManager_PreservesImmediatePlayingConfirmedDuringPlay(t *testing.T) {
	ctx := context.Background()
	mgr := NewManager(8080, "http://192.168.1.10:8080", nil)
	defer mgr.Close()

	r := newFakeRenderer("player:browser1", "Mac Chrome")
	r.onPlay = func(uri string) {
		// Simulate RemotePlayer delivering PLAYING via HandleDeviceEvent before Play() returns.
		mgr.HandleDeviceEvent(upnp.DeviceEvent{
			RendererID:  "player:browser1",
			Service:     upnp.ServiceAVTransport,
			Seq:         1,
			HasState:    true,
			State:       "PLAYING",
			HasPosition: true,
			Position:    2 * time.Second,
			HasTrackURI: true,
			TrackURI:    uri,
		})
	}
	mgr.RegisterRenderer(r)

	tracks := []library.Track{
		{ID: "t1", Source: library.SourceLocal, Title: "Instant", Ext: ".flac", MIME: "audio/flac", DurationSec: 180},
	}

	snap, err := mgr.ReplaceQueue(ctx, "player:browser1", tracks, 0)
	if err != nil {
		t.Fatalf("ReplaceQueue failed: %v", err)
	}
	if snap.State != StatePlaying {
		t.Fatalf("expected immediate %q when renderer confirms PLAYING during Play(), got %q", StatePlaying, snap.State)
	}
	if snap.PositionSec != 2 {
		t.Fatalf("expected confirmed position 2s to be preserved, got %d", snap.PositionSec)
	}
}
