package queue

import (
	"context"
	"errors"
	"strings"
	"testing"

	"tacet/internal/library"
	"tacet/internal/upnp"
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

func TestZoneAudioQuality_AutoDecodesDSDForNonDSDEndpoints(t *testing.T) {
	mgr := NewManager(4320, "http://127.0.0.1:4320", nil)
	mgr.SetTranscodeAvailable(true)
	defer mgr.Close()

	dsdTrack := []library.Track{
		{ID: "local:dsd1", Source: library.SourceLocal, Title: "Meaning Of The Blues", MIME: "audio/x-dsf", Ext: ".dsf", BitDepth: 1, SampleRate: 2822400, Channels: 2, DurationSec: 570},
	}

	// UPnP renderer advertising audio/x-dsd receives native bit-perfect DSF.
	wiim := newFakeRenderer("zone-wiim", "Living Room Wiim")
	wiim.info.SinkProtocols = []string{"http-get:*:audio/flac:*", "http-get:*:audio/x-dsd:*"}
	mgr.RegisterRenderer(wiim)
	snap, err := mgr.ReplaceQueue(context.Background(), "zone-wiim", dsdTrack, 0)
	if err != nil {
		t.Fatalf("ReplaceQueue(wiim): %v", err)
	}
	if snap.ActiveQuality != AudioQualityOriginal {
		t.Fatalf("wiim ActiveQuality = %q, want original", snap.ActiveQuality)
	}
	wiim.mu.Lock()
	wiimURI := wiim.currentURI
	wiim.mu.Unlock()
	if !strings.HasSuffix(wiimURI, "/media/local/local:dsd1.dsf") {
		t.Fatalf("wiim should get native .dsf URI, got %q", wiimURI)
	}

	// UPnP renderer without DSD (e.g. AudioPro A15) auto-promotes DSD to 24-bit/88.2kHz WAV.
	a15 := newFakeRenderer("zone-a15", "AudioPro A15")
	a15.info.SinkProtocols = []string{"http-get:*:audio/flac:*", "http-get:*:audio/wav:DLNA.ORG_PN=LPCM"}
	mgr.RegisterRenderer(a15)
	snap, err = mgr.ReplaceQueue(context.Background(), "zone-a15", dsdTrack, 0)
	if err != nil {
		t.Fatalf("ReplaceQueue(a15): %v", err)
	}
	if snap.ActiveQuality != AudioQualityWAV {
		t.Fatalf("a15 ActiveQuality = %q, want wav", snap.ActiveQuality)
	}
	a15.mu.Lock()
	a15URI := a15.currentURI
	a15.mu.Unlock()
	if !strings.Contains(a15URI, "/media/local/local:dsd1.wav?q=wav") {
		t.Fatalf("a15 should get WAV transcoded URI for DSD, got %q", a15URI)
	}
}

func TestZoneAudioQuality_ReloadsNativeDeviceQueueOnNextTrack(t *testing.T) {
	mgr := NewManager(4320, "http://192.168.1.10:4320", nil)
	mgr.SetTranscodeAvailable(true)
	defer mgr.Close()

	ctrl := &fakeQueueController{
		info: upnp.RendererInfo{
			ID:            "zone-dmr",
			Name:          "Tacet Desktop",
			SupportsQueue: true,
			SinkProtocols: []string{
				"http-get:*:audio/flac:*",
				"http-get:*:audio/aac:*",
			},
		},
	}
	mgr.RegisterRenderer(ctrl)

	tracks := []library.Track{
		{ID: "local:t1", Source: library.SourceLocal, Title: "One", MIME: "audio/flac", Ext: ".flac", BitDepth: 16, SampleRate: 44100, DurationSec: 180},
		{ID: "local:t2", Source: library.SourceLocal, Title: "Two", MIME: "audio/flac", Ext: ".flac", BitDepth: 16, SampleRate: 44100, DurationSec: 200},
	}
	if _, err := mgr.ReplaceQueue(context.Background(), "zone-dmr", tracks, 0); err != nil {
		t.Fatalf("ReplaceQueue: %v", err)
	}
	if ctrl.replaceCalls != 1 {
		t.Fatalf("initial replaceCalls = %d, want 1", ctrl.replaceCalls)
	}

	// Switch quality mid-track: current track stays original, device queue is marked dirty.
	snap, err := mgr.SetZoneQuality("zone-dmr", AudioQualityOpus128)
	if err != nil {
		t.Fatalf("SetZoneQuality: %v", err)
	}
	if snap.AudioQuality != AudioQualityOpus128 || snap.ActiveQuality != AudioQualityOriginal {
		t.Fatalf("after SetZoneQuality: quality=%q active=%q", snap.AudioQuality, snap.ActiveQuality)
	}

	// Advancing to the next track reloads the native device queue with AAC transcode URLs.
	snap, err = mgr.Next(context.Background(), "zone-dmr")
	if err != nil {
		t.Fatalf("Next: %v", err)
	}
	if snap.ActiveQuality != AudioQualityOpus128 {
		t.Fatalf("ActiveQuality after Next = %q, want opus_128", snap.ActiveQuality)
	}
	if ctrl.replaceCalls != 2 {
		t.Fatalf("replaceCalls after quality change + Next = %d, want 2", ctrl.replaceCalls)
	}
	if len(ctrl.lastItems) != 2 || !strings.Contains(ctrl.lastItems[1].URL, "/media/local/local:t2.aac?q=opus_128&container=adts") {
		t.Fatalf("expected transcoded AAC URL in reloaded queue, got %+v", ctrl.lastItems)
	}
}
