package queue

import (
	"context"
	"errors"
	"strings"
	"testing"

	"tacet/internal/library"
)

func TestZoneAudioQuality_AppliesOnNextTrackAndPersists(t *testing.T) {
	mgr := NewManager(4320, "http://127.0.0.1:4320", nil)
	mgr.SetTranscodeAvailable(true)
	defer mgr.Close()

	r := newFakeRenderer("zone-q", "Living Room")
	r.info.SinkProtocols = append(r.info.SinkProtocols, "http-get:*:audio/ogg;codecs=opus:*")
	mgr.RegisterRenderer(r)

	tracks := []library.Track{
		{ID: "local:t1", Source: library.SourceLocal, Title: "One", MIME: "audio/flac", Ext: ".flac", BitDepth: 24, SampleRate: 96000, DurationSec: 180},
		{ID: "local:t2", Source: library.SourceLocal, Title: "Two", MIME: "audio/flac", Ext: ".flac", BitDepth: 24, SampleRate: 96000, DurationSec: 200},
	}

	snap, err := mgr.ReplaceQueue(context.Background(), "zone-q", tracks, 0)
	if err != nil {
		t.Fatalf("ReplaceQueue: %v", err)
	}
	if snap.AudioQuality != AudioQualityOriginal || snap.ActiveQuality != AudioQualityOriginal {
		t.Fatalf("initial quality = (%q, active=%q), want original", snap.AudioQuality, snap.ActiveQuality)
	}

	// Switching quality mid-track updates AudioQuality but leaves ActiveQuality as original until next track.
	snap, err = mgr.SetZoneQuality("zone-q", AudioQualityOpus128)
	if err != nil {
		t.Fatalf("SetZoneQuality: %v", err)
	}
	if snap.AudioQuality != AudioQualityOpus128 || snap.ActiveQuality != AudioQualityOriginal {
		t.Fatalf("after SetZoneQuality: quality=%q active=%q", snap.AudioQuality, snap.ActiveQuality)
	}

	// Advancing to the next track delivers the Opus transcoded URL and updates ActiveQuality.
	snap, err = mgr.Next(context.Background(), "zone-q")
	if err != nil {
		t.Fatalf("Next: %v", err)
	}
	if snap.ActiveQuality != AudioQualityOpus128 {
		t.Fatalf("ActiveQuality after Next = %q, want opus_128", snap.ActiveQuality)
	}
	r.mu.Lock()
	uri := r.currentURI
	r.mu.Unlock()
	if !strings.Contains(uri, "/media/local/local:t2.ogg?q=opus_128") {
		t.Fatalf("expected transcoded opus URI, got %q", uri)
	}

	// SavedZoneQualities preserves non-default settings for persistence across restarts.
	saved := mgr.SavedZoneQualities()
	if saved["zone-q"] != AudioQualityOpus128 {
		t.Fatalf("SavedZoneQualities = %v, want opus_128", saved)
	}

	// Switching back to original removes the zone from SavedZoneQualities.
	if _, err = mgr.SetZoneQuality("zone-q", AudioQualityOriginal); err != nil {
		t.Fatalf("SetZoneQuality(original): %v", err)
	}
	if len(mgr.SavedZoneQualities()) != 0 {
		t.Fatalf("expected empty SavedZoneQualities after reset to original, got %v", mgr.SavedZoneQualities())
	}
}

func TestZoneAudioQuality_RejectsWhenTranscoderUnavailable(t *testing.T) {
	mgr := NewManager(4320, "http://127.0.0.1:4320", nil)
	mgr.SetTranscodeAvailable(false)
	defer mgr.Close()

	mgr.RegisterRenderer(newFakeRenderer("zone-no-ff", "Speaker"))

	if _, err := mgr.SetZoneQuality("zone-no-ff", AudioQualityOpus160); !errors.Is(err, ErrCapabilityUnsupported) {
		t.Fatalf("expected ErrCapabilityUnsupported, got %v", err)
	}
	if _, err := mgr.SetZoneQuality("zone-no-ff", AudioQualityWAV); !errors.Is(err, ErrCapabilityUnsupported) {
		t.Fatalf("expected ErrCapabilityUnsupported for wav, got %v", err)
	}
	if _, err := mgr.SetZoneQuality("zone-no-ff", AudioQualityOriginal); err != nil {
		t.Fatalf("original should always succeed: %v", err)
	}
}

func TestZoneAudioQuality_NegotiatesAACForUPnPWithoutOpus(t *testing.T) {
	mgr := NewManager(4320, "http://127.0.0.1:4320", nil)
	mgr.SetTranscodeAvailable(true)
	defer mgr.Close()

	// AudioPro A15 advertises audio/aac and audio/ogg (Vorbis only), no Opus.
	a15 := newFakeRenderer("zone-a15", "AudioPro A15")
	a15.info.SinkProtocols = []string{
		"http-get:*:audio/flac:*",
		"http-get:*:audio/ogg:*",
		"http-get:*:audio/aac:DLNA.ORG_PN=AAC_ISO",
		"http-get:*:audio/wav:DLNA.ORG_PN=LPCM",
	}
	mgr.RegisterRenderer(a15)

	if _, err := mgr.SetZoneQuality("zone-a15", AudioQualityOpus128); err != nil {
		t.Fatalf("SetZoneQuality(opus_128) on AAC-capable UPnP renderer failed: %v", err)
	}
	tracks := []library.Track{
		{ID: "local:t1", Source: library.SourceLocal, Title: "One", MIME: "audio/flac", Ext: ".flac", BitDepth: 16, SampleRate: 44100, DurationSec: 180},
	}
	if _, err := mgr.ReplaceQueue(context.Background(), "zone-a15", tracks, 0); err != nil {
		t.Fatalf("ReplaceQueue: %v", err)
	}
	a15.mu.Lock()
	uri := a15.currentURI
	a15.mu.Unlock()
	if !strings.Contains(uri, "/media/local/local:t1.aac?q=opus_128&container=adts") {
		t.Fatalf("expected AAC ADTS fallback URI, got %q", uri)
	}

	// Renderer with neither Opus nor AAC should still reject lossy presets.
	pcmOnly := newFakeRenderer("zone-pcm", "PCM Only")
	pcmOnly.info.SinkProtocols = []string{
		"http-get:*:audio/wav:DLNA.ORG_PN=LPCM",
		"http-get:*:audio/flac:*",
	}
	mgr.RegisterRenderer(pcmOnly)
	if _, err := mgr.SetZoneQuality("zone-pcm", AudioQualityOpus128); !errors.Is(err, ErrCapabilityUnsupported) {
		t.Fatalf("expected ErrCapabilityUnsupported for PCM-only renderer, got %v", err)
	}
	if _, err := mgr.SetZoneQuality("zone-pcm", AudioQualityWAV); err != nil {
		t.Fatalf("WAV should still succeed on PCM-only renderer: %v", err)
	}
}
