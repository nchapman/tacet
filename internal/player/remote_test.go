package player

import (
	"context"
	"errors"
	"math"
	"sync"
	"testing"
	"time"

	"tacet/internal/upnp"
)

func TestRegistry_LifecycleCommandAckTimeoutAndGraceReap(t *testing.T) {
	var (
		mu         sync.Mutex
		registered []string
		removed    []string
		events     []upnp.DeviceEvent
	)

	reg := NewRegistry(
		func(ctrl upnp.Controller) {
			mu.Lock()
			registered = append(registered, ctrl.Info().ID)
			mu.Unlock()
		},
		func(id string) {
			mu.Lock()
			removed = append(removed, id)
			mu.Unlock()
		},
		func(ev upnp.DeviceEvent) {
			mu.Lock()
			events = append(events, ev)
			mu.Unlock()
		},
		WithGracePeriod(60*time.Millisecond),
		WithCommandTimeout(80*time.Millisecond),
	)
	defer reg.Close()

	// Register generic remote player endpoint.
	res, err := reg.Register(RegisterRequest{
		PlayerID: "desk-dac",
		Name:     "Desk DAC",
		Model:    "macOS Client",
		Codecs:   []string{"audio/flac", "audio/mpeg"},
		Host:     "127.0.0.1",
	}, "")
	if err != nil {
		t.Fatalf("Register failed: %v", err)
	}
	if res.ZoneID != "player:desk-dac" || res.Token == "" || res.Info.Kind != "player" {
		t.Fatalf("unexpected registration: %+v", res)
	}

	// Re-registering same ID with wrong token is rejected.
	if _, err = reg.Register(RegisterRequest{PlayerID: "desk-dac", Name: "Hijack"}, "wrong-token"); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("expected ErrUnauthorized on hijack attempt, got %v", err)
	}

	// Re-registering with valid token updates name and keeps token.
	res2, err := reg.Register(RegisterRequest{PlayerID: "desk-dac", Name: "Office DAC"}, res.Token)
	if err != nil || res2.Info.Name != "Office DAC" || res2.Token != res.Token {
		t.Fatalf("expected clean token reattach, got err=%v res=%+v", err, res2)
	}

	rp, err := reg.Get(res.ZoneID, res.Token)
	if err != nil {
		t.Fatalf("Get failed: %v", err)
	}

	// Sending command while disconnected fails fast with ErrPlayerDisconnected.
	if err = rp.Play(context.Background()); !errors.Is(err, ErrPlayerDisconnected) {
		t.Fatalf("expected ErrPlayerDisconnected when stream absent, got %v", err)
	}

	// Attach command stream and verify command delivery + ack + PLAYBACK_BLOCKED + timeout distinction.
	stream := rp.AttachStream()
	if !rp.PushConnected() {
		t.Fatal("expected PushConnected=true after AttachStream")
	}

	go func() {
		cmd := <-stream.Commands
		okTrue := true
		pos := 12.5
		vol := 75
		rp.ReportState(StateReport{
			CmdID:       cmd.ID,
			OK:          &okTrue,
			State:       "PLAYING",
			PositionSec: &pos,
			TrackURI:    cmd.Media.URL,
			Volume:      &vol,
		})

		cmd2 := <-stream.Commands
		okFalse := false
		rp.ReportState(StateReport{
			CmdID:        cmd2.ID,
			OK:           &okFalse,
			ErrorCode:    "PLAYBACK_BLOCKED",
			ErrorMessage: "user interaction required",
		})

		// Read cmd3 and intentionally do NOT ack to exercise ErrCommandTimeout.
		<-stream.Commands
	}()

	if err = rp.SetURI(context.Background(), upnp.MediaItem{ID: "local:1", URL: "/media/local/local:1.flac", MIME: "audio/flac"}); err != nil {
		t.Fatalf("SetURI failed: %v", err)
	}
	if err = rp.Play(context.Background()); !errors.Is(err, ErrPlaybackBlocked) {
		t.Fatalf("expected ErrPlaybackBlocked, got %v", err)
	}
	if err = rp.Pause(context.Background()); !errors.Is(err, ErrCommandTimeout) {
		t.Fatalf("expected ErrCommandTimeout while still connected, got %v", err)
	}

	// Verify overflow/NaN positions are safely ignored and DeviceEvent.Seq is non-zero.
	huge := 1e15
	nan := math.NaN()
	rp.ReportState(StateReport{PositionSec: &huge, DurationSec: &nan})

	st, _ := rp.Status(context.Background())
	if st.State != "PLAYING" || st.Volume != 75 || int(st.Position.Seconds()) != 12 {
		t.Fatalf("unexpected cached status: %+v", st)
	}

	mu.Lock()
	if len(events) == 0 || events[0].Seq == 0 {
		mu.Unlock()
		t.Fatalf("expected non-zero Seq on pushed DeviceEvent, got %+v", events)
	}
	mu.Unlock()

	// Attaching a second stream replaces the first stream cleanly.
	stream2 := rp.AttachStream()
	select {
	case <-stream.Replaced:
	case <-time.After(200 * time.Millisecond):
		t.Fatal("expected first stream Replaced channel to close")
	}

	// Detaching stream2 triggers grace-period cleanup.
	reg.DetachStream(rp, stream2.ID)
	deadline := time.Now().Add(400 * time.Millisecond)
	for time.Now().Before(deadline) {
		mu.Lock()
		n := len(removed)
		mu.Unlock()
		if n > 0 {
			break
		}
		time.Sleep(10 * time.Millisecond)
	}
	mu.Lock()
	if len(removed) != 1 || removed[0] != res.ZoneID {
		mu.Unlock()
		t.Fatalf("expected grace reaper to unregister %s, got %v", res.ZoneID, removed)
	}
	mu.Unlock()
}
