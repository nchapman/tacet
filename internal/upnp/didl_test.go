package upnp

import (
	"encoding/xml"
	"strings"
	"testing"
	"time"
)

func TestNegotiateMIME_WiimPrefersStandardFLAC(t *testing.T) {
	wiimSinks := []string{
		"http-get:*:audio/mpeg:*",
		"http-get:*:audio/flac:*",
		"http-get:*:audio/wav:*",
		"http-get:*:audio/mp4:*",
	}

	if got := NegotiateMIME("audio/flac", wiimSinks); got != "audio/flac" {
		t.Fatalf("expected audio/flac, got %q", got)
	}
	if got := NegotiateMIME("audio/x-flac", wiimSinks); got != "audio/flac" {
		t.Fatalf("expected audio/x-flac to negotiate to audio/flac, got %q", got)
	}

	legacySinks := []string{"http-get:*:audio/x-flac:*"}
	if got := NegotiateMIME("audio/flac", legacySinks); got != "audio/x-flac" {
		t.Fatalf("expected audio/flac to fall back to audio/x-flac, got %q", got)
	}
}

func TestBuildDIDL_ValidXMLAndAudioAttributes(t *testing.T) {
	didl := BuildDIDL(MediaItem{
		ID:          "track-1",
		URL:         "http://192.168.1.10:8080/media/local/abc.flac?a=1&b=2",
		Title:       "Time & Space",
		Artist:      "Pink Floyd",
		Album:       "The Dark Side of the Moon",
		AlbumArtURL: "http://192.168.1.10:8080/media/art/abc",
		TrackNumber: 4,
		MIME:        "audio/flac",
		Duration:    6*time.Minute + 53*time.Second,
		Size:        123456789,
		SampleRate:  192000,
		BitDepth:    24,
		Channels:    2,
		Seekable:    true,
	}, []string{"http-get:*:audio/flac:*"})

	// Must parse cleanly as XML.
	var doc any
	if err := xml.Unmarshal([]byte(didl), &doc); err != nil {
		t.Fatalf("BuildDIDL emitted invalid XML: %v\n%s", err, didl)
	}

	checks := []string{
		`object.item.audioItem.musicTrack`,
		`<dc:title>Time &amp; Space</dc:title>`,
		`<upnp:artist>Pink Floyd</upnp:artist>`,
		`<upnp:album>The Dark Side of the Moon</upnp:album>`,
		`<upnp:albumArtURI>http://192.168.1.10:8080/media/art/abc</upnp:albumArtURI>`,
		`http-get:*:audio/flac:DLNA.ORG_OP=01`,
		`duration="0:06:53"`,
		`sampleFrequency="192000"`,
		`bitsPerSample="24"`,
		`nrAudioChannels="2"`,
		`size="123456789"`,
		`abc.flac?a=1&amp;b=2`,
	}
	for _, want := range checks {
		if !strings.Contains(didl, want) {
			t.Errorf("DIDL missing %q:\n%s", want, didl)
		}
	}
}

func TestDurationRoundTrip(t *testing.T) {
	cases := []struct {
		raw  string
		want time.Duration
	}{
		{"0:03:45", 3*time.Minute + 45*time.Second},
		{"01:02:03.500", time.Hour + 2*time.Minute + 3*time.Second + 500*time.Millisecond},
		{"00:03:45.abc", 0},
		{"9999999999:00:00", 0},
		{"0:99:00", 0},
		{"0:00:99", 0},
		{"-1:00:00", 0},
		{"NOT_IMPLEMENTED", 0},
		{"", 0},
	}
	for _, tc := range cases {
		if got := ParseUPnPDuration(tc.raw); got != tc.want {
			t.Errorf("ParseUPnPDuration(%q) = %v, want %v", tc.raw, got, tc.want)
		}
	}
	if got := FormatUPnPDuration(time.Hour + 5*time.Minute + 9*time.Second); got != "1:05:09" {
		t.Errorf("FormatUPnPDuration = %q, want 1:05:09", got)
	}
}

func TestValidateRendererURL_AllowsPrivateLANOnly(t *testing.T) {
	ctx := t.Context()

	valid := []string{
		"http://192.168.1.50:49152/description.xml",
		"http://10.0.0.25:49152/description.xml",
		"http://172.16.0.10:8080/desc.xml",
	}
	for _, u := range valid {
		if _, err := validateRendererURL(ctx, u); err != nil {
			t.Errorf("expected %q to be valid, got %v", u, err)
		}
	}

	invalid := []string{
		"http://127.0.0.1:8080/description.xml",
		"http://169.254.169.254/latest/meta-data/",
		"http://8.8.8.8/description.xml",
		"file:///etc/passwd",
		"ftp://192.168.1.50/description.xml",
	}
	for _, u := range invalid {
		if _, err := validateRendererURL(ctx, u); err == nil {
			t.Errorf("expected %q to be rejected", u)
		}
	}
}
