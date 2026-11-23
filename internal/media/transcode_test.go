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
	"strconv"
	"strings"
	"testing"

	"tacet/internal/library"
)

func makeTestWAV(t *testing.T, sampleRate, channels, numSamples int) []byte {
	t.Helper()
	const bitsPerSample = 16
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
	if _, ok := parseTranscodeSpec("", "", library.Track{}, 0, false); ok {
		t.Fatal("empty quality should not transcode")
	}
	if _, ok := parseTranscodeSpec("original", "", library.Track{}, 0, false); ok {
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
		spec, ok := parseTranscodeSpec(tc.q, "", library.Track{}, 0, false)
		if !ok {
			t.Fatalf("expected %s to parse", tc.q)
		}
		if spec.mime != "audio/ogg" {
			t.Fatalf("%s mime = %q, want audio/ogg", tc.q, spec.mime)
		}
		if !slices.Contains(spec.codecArgs, "libopus") || !slices.Contains(spec.codecArgs, tc.bitrate) {
			t.Fatalf("%s codecArgs = %v, want libopus + %s", tc.q, spec.codecArgs, tc.bitrate)
		}
		if len(spec.filterArgs) < 2 || !strings.Contains(spec.filterArgs[1], "osf=flt") || !strings.Contains(spec.filterArgs[1], "mono|stereo") {
			t.Fatalf("%s filterArgs = %v, want float resampling + mono|stereo", tc.q, spec.filterArgs)
		}
	}

	// SoXR path when available.
	soxrSpec, ok := parseTranscodeSpec("opus_160", "", library.Track{}, 0, true)
	if !ok || len(soxrSpec.filterArgs) < 2 || !strings.Contains(soxrSpec.filterArgs[1], "resampler=soxr") {
		t.Fatalf("expected soxr filtergraph, got %+v", soxrSpec.filterArgs)
	}

	// WebM and duration-fragmented MP4 (never frag_keyframe on audio-only streams).
	webmSpec, ok := parseTranscodeSpec("opus_128", "webm", library.Track{}, 0, false)
	if !ok || webmSpec.mime != "audio/webm" || !slices.Contains(webmSpec.fmtArgs, "webm") {
		t.Fatalf("unexpected webm spec: %+v ok=%v", webmSpec, ok)
	}
	mp4Spec, ok := parseTranscodeSpec("opus_128", "mp4", library.Track{}, 0, false)
	if !ok || mp4Spec.mime != "audio/mp4" || !slices.Contains(mp4Spec.fmtArgs, "-frag_duration") {
		t.Fatalf("unexpected mp4 spec: %+v ok=%v", mp4Spec, ok)
	}
	for _, arg := range mp4Spec.fmtArgs {
		if strings.Contains(arg, "frag_keyframe") {
			t.Fatalf("mp4 fmtArgs must not use frag_keyframe on audio-only streams: %v", mp4Spec.fmtArgs)
		}
	}

	// AAC ADTS fallback for UPnP speakers without Opus support (auto-uses libfdk_aac if available).
	aacSpec, ok := parseTranscodeSpec("opus_128", "adts", library.Track{SampleRate: 44100, Channels: 2}, 0, false, false)
	if !ok || aacSpec.mime != "audio/aac" || !slices.Contains(aacSpec.codecArgs, "aac") || !slices.Contains(aacSpec.fmtArgs, "adts") {
		t.Fatalf("unexpected aac spec: %+v ok=%v", aacSpec, ok)
	}
	if strings.Contains(aacSpec.filterArgs[1], "aresample=") {
		t.Fatalf("44.1 kHz AAC should not needlessly resample: %v", aacSpec.filterArgs)
	}
	fdkSpec, ok := parseTranscodeSpec("opus_160", "adts", library.Track{SampleRate: 96000, Channels: 2}, 0, true, true)
	if !ok || !slices.Contains(fdkSpec.codecArgs, "libfdk_aac") || !strings.Contains(fdkSpec.filterArgs[1], "aresample=48000:resampler=soxr") {
		t.Fatalf("unexpected fdk hi-res aac spec: %+v ok=%v", fdkSpec, ok)
	}

	// Canonical 44-byte WAV with exact length for local 16-bit lossless.
	wav16, ok := parseTranscodeSpec("wav", "", library.Track{BitDepth: 16, SampleRate: 44100, Channels: 2}, 44100, false)
	if !ok || wav16.mime != "audio/wav" || !slices.Contains(wav16.codecArgs, "pcm_s16le") || len(wav16.wavHeader) != 44 {
		t.Fatalf("unexpected wav16 spec: %+v", wav16)
	}
	if wav16.exactLength != 44+44100*4 {
		t.Fatalf("wav16 exactLength = %d, want %d", wav16.exactLength, 44+44100*4)
	}
	if tag := binary.LittleEndian.Uint16(wav16.wavHeader[20:22]); tag != 1 {
		t.Fatalf("wav16 wFormatTag = 0x%04x, want 0x0001 (WAVE_FORMAT_PCM)", tag)
	}

	// Unknown bit depth (e.g. 24-bit ALAC .m4a or lossy) defaults to 24-bit PCM.
	wavUnknown, ok := parseTranscodeSpec("wav", "", library.Track{BitDepth: 0, SampleRate: 96000, Channels: 2}, 0, false)
	if !ok || !slices.Contains(wavUnknown.codecArgs, "pcm_s24le") {
		t.Fatalf("unknown bit depth should default to pcm_s24le, got %+v", wavUnknown.codecArgs)
	}

	// DSD decimation to 24-bit / 88.2 kHz with ultrasonic filtering.
	wavDSD, ok := parseTranscodeSpec("wav", "", library.Track{BitDepth: 1, SampleRate: 2822400, Channels: 2}, 0, false)
	if !ok || !slices.Contains(wavDSD.codecArgs, "pcm_s24le") || len(wavDSD.filterArgs) < 2 || !strings.Contains(wavDSD.filterArgs[1], "88200") {
		t.Fatalf("unexpected wavDSD spec: %+v", wavDSD)
	}
}

func TestServeLocal_TranscodeOpusAndWAV(t *testing.T) {
	if !DetectTranscoder().Available() {
		t.Skip("ffmpeg with libopus not installed")
	}

	root := t.TempDir()
	const samples = 8820
	wavPath := filepath.Join(root, "tone.wav")
	if err := os.WriteFile(wavPath, makeTestWAV(t, 44100, 2, samples), 0o600); err != nil {
		t.Fatal(err)
	}
	// Also write a 6-channel (5.1) file to verify clean stereo fold-down in libopus.
	surroundPath := filepath.Join(root, "surround.wav")
	if err := os.WriteFile(surroundPath, makeTestWAV(t, 44100, 6, samples), 0o600); err != nil {
		t.Fatal(err)
	}

	cat := library.NewCatalog(root)
	if _, err := cat.Scan(); err != nil {
		t.Fatal(err)
	}
	var stereoTr, surroundTr library.Track
	for _, tr := range cat.Tracks() {
		if strings.HasSuffix(tr.FilePath, "tone.wav") {
			stereoTr = tr
		} else {
			surroundTr = tr
		}
	}

	mux := http.NewServeMux()
	NewHandler(cat, nil).Register(mux)
	srv := httptest.NewServer(mux)
	defer srv.Close()

	// 1. Opus 128k Ogg stream starts with OggS magic header.
	resp, err := http.Get(srv.URL + "/media/local/" + stereoTr.ID + ".ogg?q=opus_128")
	if err != nil {
		t.Fatal(err)
	}
	oggBody, _ := io.ReadAll(resp.Body)
	resp.Body.Close()
	if resp.StatusCode != http.StatusOK || resp.Header.Get("Content-Type") != "audio/ogg" || resp.Header.Get("Accept-Ranges") != "none" {
		t.Fatalf("opus status=%d ct=%q ar=%q", resp.StatusCode, resp.Header.Get("Content-Type"), resp.Header.Get("Accept-Ranges"))
	}
	if len(oggBody) < 4 || string(oggBody[:4]) != "OggS" {
		t.Fatalf("expected OggS header, got len=%d", len(oggBody))
	}

	// 2. 6-channel surround input folds down cleanly to libopus without crashing.
	surResp, err := http.Get(srv.URL + "/media/local/" + surroundTr.ID + ".ogg?q=opus_128")
	if err != nil {
		t.Fatal(err)
	}
	surBody, _ := io.ReadAll(surResp.Body)
	surResp.Body.Close()
	if surResp.StatusCode != http.StatusOK || len(surBody) < 4 || string(surBody[:4]) != "OggS" {
		t.Fatalf("surround -> opus failed: status=%d len=%d", surResp.StatusCode, len(surBody))
	}

	// 3. WebM and fMP4 Opus containers succeed.
	webmResp, err := http.Get(srv.URL + "/media/local/" + stereoTr.ID + ".webm?q=opus_128&container=webm")
	if err != nil {
		t.Fatal(err)
	}
	webmBody, _ := io.ReadAll(webmResp.Body)
	webmResp.Body.Close()
	if webmResp.StatusCode != http.StatusOK || webmResp.Header.Get("Content-Type") != "audio/webm" || len(webmBody) < 4 {
		t.Fatalf("webm failed: status=%d ct=%q len=%d", webmResp.StatusCode, webmResp.Header.Get("Content-Type"), len(webmBody))
	}

	mp4Resp, err := http.Get(srv.URL + "/media/local/" + stereoTr.ID + ".m4a?q=opus_128&container=mp4")
	if err != nil {
		t.Fatal(err)
	}
	mp4Body, _ := io.ReadAll(mp4Resp.Body)
	mp4Resp.Body.Close()
	if mp4Resp.StatusCode != http.StatusOK || mp4Resp.Header.Get("Content-Type") != "audio/mp4" || !bytes.Contains(mp4Body, []byte("moof")) {
		t.Fatalf("mp4 failed: status=%d ct=%q len=%d", mp4Resp.StatusCode, mp4Resp.Header.Get("Content-Type"), len(mp4Body))
	}

	// 4. Canonical 44-byte WAV stream has WAVE_FORMAT_PCM (0x0001) and exact Content-Length.
	wantWAVLen := 44 + samples*2*2
	wavResp, err := http.Get(srv.URL + "/media/local/" + stereoTr.ID + ".wav?q=wav")
	if err != nil {
		t.Fatal(err)
	}
	wavBody, _ := io.ReadAll(wavResp.Body)
	wavResp.Body.Close()
	if wavResp.StatusCode != http.StatusOK || wavResp.Header.Get("Content-Type") != "audio/wav" {
		t.Fatalf("wav status=%d ct=%q", wavResp.StatusCode, wavResp.Header.Get("Content-Type"))
	}
	if wavResp.Header.Get("Accept-Ranges") != "bytes" {
		t.Fatalf("wav Accept-Ranges = %q, want bytes", wavResp.Header.Get("Accept-Ranges"))
	}
	if wavResp.Header.Get("Content-Length") != strconv.Itoa(wantWAVLen) {
		t.Fatalf("wav Content-Length = %q, want %d", wavResp.Header.Get("Content-Length"), wantWAVLen)
	}
	if len(wavBody) != wantWAVLen || string(wavBody[:4]) != "RIFF" || string(wavBody[8:12]) != "WAVE" || binary.LittleEndian.Uint16(wavBody[20:22]) != 1 {
		t.Fatalf("unexpected canonical WAV output: len=%d", len(wavBody))
	}

	// 4b. Byte-range seeking on local canonical WAV returns 206 Partial Content.
	wavURL := srv.URL + "/media/local/" + stereoTr.ID + ".wav?q=wav"
	reqHdr, _ := http.NewRequest(http.MethodGet, wavURL, nil)
	reqHdr.Header.Set("Range", "bytes=0-43")
	hdrResp, err := http.DefaultClient.Do(reqHdr)
	if err != nil {
		t.Fatal(err)
	}
	hdrBytes, _ := io.ReadAll(hdrResp.Body)
	hdrResp.Body.Close()
	if hdrResp.StatusCode != http.StatusPartialContent || len(hdrBytes) != 44 || string(hdrBytes[:4]) != "RIFF" {
		t.Fatalf("wav header range status=%d len=%d", hdrResp.StatusCode, len(hdrBytes))
	}

	midOffset := 44 + (samples/2)*4
	reqMid, _ := http.NewRequest(http.MethodGet, wavURL, nil)
	reqMid.Header.Set("Range", "bytes="+strconv.Itoa(midOffset)+"-")
	midResp, err := http.DefaultClient.Do(reqMid)
	if err != nil {
		t.Fatal(err)
	}
	midBytes, _ := io.ReadAll(midResp.Body)
	midResp.Body.Close()
	wantMidLen := wantWAVLen - midOffset
	if midResp.StatusCode != http.StatusPartialContent || len(midBytes) != wantMidLen {
		t.Fatalf("wav mid seek status=%d len=%d want=%d cr=%q", midResp.StatusCode, len(midBytes), wantMidLen, midResp.Header.Get("Content-Range"))
	}

	reqBad, _ := http.NewRequest(http.MethodGet, wavURL, nil)
	reqBad.Header.Set("Range", "bytes=99999999-")
	badResp, err := http.DefaultClient.Do(reqBad)
	if err != nil {
		t.Fatal(err)
	}
	badResp.Body.Close()
	if badResp.StatusCode != http.StatusRequestedRangeNotSatisfiable {
		t.Fatalf("invalid range status=%d, want 416", badResp.StatusCode)
	}

	// 5. HEAD returns 200 with empty body.
	headResp, err := http.Head(srv.URL + "/media/local/" + stereoTr.ID + ".ogg?q=opus_96")
	if err != nil {
		t.Fatal(err)
	}
	headBody, _ := io.ReadAll(headResp.Body)
	headResp.Body.Close()
	if headResp.StatusCode != http.StatusOK || len(headBody) != 0 || headResp.Header.Get("Content-Type") != "audio/ogg" {
		t.Fatalf("HEAD status=%d ct=%q len=%d", headResp.StatusCode, headResp.Header.Get("Content-Type"), len(headBody))
	}
}
