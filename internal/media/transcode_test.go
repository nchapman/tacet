package media

import (
	"bytes"
	"encoding/binary"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"slices"
	"testing"

	"tacet/internal/library"
)

func makeTestWAV(t *testing.T, sampleRate, numSamples int) []byte {
	t.Helper()
	const (
		channels      = 2
		bitsPerSample = 16
	)
	dataSize := numSamples * channels * (bitsPerSample / 8)
	var buf bytes.Buffer
	buf.WriteString("RIFF")
	_ = binary.Write(&buf, binary.LittleEndian, uint32(36+dataSize))
	buf.WriteString("WAVE")
	buf.WriteString("fmt ")
	_ = binary.Write(&buf, binary.LittleEndian, uint32(16))
	_ = binary.Write(&buf, binary.LittleEndian, uint16(1)) // PCM
	_ = binary.Write(&buf, binary.LittleEndian, uint16(channels))
	_ = binary.Write(&buf, binary.LittleEndian, uint32(sampleRate))
	_ = binary.Write(&buf, binary.LittleEndian, uint32(sampleRate*channels*(bitsPerSample/8)))
	_ = binary.Write(&buf, binary.LittleEndian, uint16(channels*(bitsPerSample/8)))
	_ = binary.Write(&buf, binary.LittleEndian, uint16(bitsPerSample))
	buf.WriteString("data")
	_ = binary.Write(&buf, binary.LittleEndian, uint32(dataSize))
	buf.Write(make([]byte, dataSize))
	return buf.Bytes()
}

func TestParseTranscodeSpec(t *testing.T) {
	if _, ok := parseTranscodeSpec("", "", library.Track{}); ok {
		t.Fatal("empty quality should not transcode")
	}
	if _, ok := parseTranscodeSpec("original", "", library.Track{}); ok {
		t.Fatal("original quality should not transcode")
	}

	for _, tc := range []struct {
		q       string
		bitrate string
	}{
		{"opus_96", "96k"},
		{"opus_128", "128k"},
		{"opus_160", "160k"},
		{"opus_256", "256k"},
	} {
		spec, ok := parseTranscodeSpec(tc.q, "", library.Track{})
		if !ok {
			t.Fatalf("expected %s to parse", tc.q)
		}
		if spec.mime != "audio/ogg" {
			t.Fatalf("%s mime = %q, want audio/ogg", tc.q, spec.mime)
		}
		if !slices.Contains(spec.codecArgs, "libopus") || !slices.Contains(spec.codecArgs, tc.bitrate) {
			t.Fatalf("%s codecArgs = %v, want libopus + %s", tc.q, spec.codecArgs, tc.bitrate)
		}
	}

	mp4Spec, ok := parseTranscodeSpec("opus_128", "mp4", library.Track{})
	if !ok || mp4Spec.mime != "audio/mp4" || !slices.Contains(mp4Spec.fmtArgs, "mp4") {
		t.Fatalf("unexpected mp4 spec: %+v ok=%v", mp4Spec, ok)
	}

	wav16, ok := parseTranscodeSpec("wav", "", library.Track{BitDepth: 16, SampleRate: 44100})
	if !ok || wav16.mime != "audio/wav" || !slices.Contains(wav16.codecArgs, "pcm_s16le") {
		t.Fatalf("unexpected wav16 spec: %+v", wav16)
	}
	wav24, ok := parseTranscodeSpec("wav", "", library.Track{BitDepth: 24, SampleRate: 96000})
	if !ok || !slices.Contains(wav24.codecArgs, "pcm_s24le") {
		t.Fatalf("unexpected wav24 spec: %+v", wav24)
	}
	wavDSD, ok := parseTranscodeSpec("wav", "", library.Track{BitDepth: 1, SampleRate: 2822400})
	if !ok || !slices.Contains(wavDSD.codecArgs, "pcm_s24le") || !slices.Contains(wavDSD.codecArgs, "88200") {
		t.Fatalf("unexpected wavDSD spec: %+v", wavDSD)
	}
}

func TestServeLocal_TranscodeOpusAndWAV(t *testing.T) {
	if !DetectTranscoder().Available() {
		t.Skip("ffmpeg with libopus not installed")
	}

	root := t.TempDir()
	wavPath := filepath.Join(root, "tone.wav")
	if err := os.WriteFile(wavPath, makeTestWAV(t, 44100, 8820), 0o600); err != nil {
		t.Fatal(err)
	}
	cat := library.NewCatalog(root)
	if _, err := cat.Scan(); err != nil {
		t.Fatal(err)
	}
	tr := cat.Tracks()[0]

	mux := http.NewServeMux()
	NewHandler(cat, nil).Register(mux)
	srv := httptest.NewServer(mux)
	defer srv.Close()

	// 1. Opus 128k Ogg stream starts with OggS magic header.
	resp, err := http.Get(srv.URL + "/media/local/" + tr.ID + ".ogg?q=opus_128")
	if err != nil {
		t.Fatal(err)
	}
	oggBody, _ := io.ReadAll(resp.Body)
	resp.Body.Close()
	if resp.StatusCode != http.StatusOK || resp.Header.Get("Content-Type") != "audio/ogg" {
		t.Fatalf("opus status=%d ct=%q", resp.StatusCode, resp.Header.Get("Content-Type"))
	}
	if len(oggBody) < 4 || string(oggBody[:4]) != "OggS" {
		t.Fatalf("expected OggS header, got len=%d prefix=%q", len(oggBody), string(oggBody[:min(4, len(oggBody))]))
	}

	// 2. WAV stream starts with RIFF....WAVE magic header.
	wavResp, err := http.Get(srv.URL + "/media/local/" + tr.ID + ".wav?q=wav")
	if err != nil {
		t.Fatal(err)
	}
	wavBody, _ := io.ReadAll(wavResp.Body)
	wavResp.Body.Close()
	if wavResp.StatusCode != http.StatusOK || wavResp.Header.Get("Content-Type") != "audio/wav" {
		t.Fatalf("wav status=%d ct=%q", wavResp.StatusCode, wavResp.Header.Get("Content-Type"))
	}
	if len(wavBody) < 12 || string(wavBody[:4]) != "RIFF" || string(wavBody[8:12]) != "WAVE" {
		t.Fatalf("expected RIFF WAVE header, got len=%d", len(wavBody))
	}

	// 3. HEAD returns 200 with empty body.
	headResp, err := http.Head(srv.URL + "/media/local/" + tr.ID + ".ogg?q=opus_96")
	if err != nil {
		t.Fatal(err)
	}
	headBody, _ := io.ReadAll(headResp.Body)
	headResp.Body.Close()
	if headResp.StatusCode != http.StatusOK || len(headBody) != 0 || headResp.Header.Get("Content-Type") != "audio/ogg" {
		t.Fatalf("HEAD status=%d ct=%q len=%d", headResp.StatusCode, headResp.Header.Get("Content-Type"), len(headBody))
	}
}
