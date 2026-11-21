package library

import (
	"encoding/binary"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func buildSyntheticFLAC(sampleRate, channels, bitDepth int, totalSamples uint64) []byte {
	buf := make([]byte, 42)
	copy(buf[0:4], "fLaC")
	buf[4] = 0x80 // last metadata block + STREAMINFO (0)
	buf[5], buf[6], buf[7] = 0, 0, 34

	si := buf[8:42]
	sr := uint32(sampleRate)
	ch := uint32(channels - 1)
	bps := uint32(bitDepth - 1)
	si[10] = byte(sr >> 12)
	si[11] = byte(sr >> 4)
	si[12] = byte((sr&0x0F)<<4) | byte((ch&0x07)<<1) | byte((bps>>4)&0x01)
	si[13] = byte((bps&0x0F)<<4) | byte((totalSamples>>32)&0x0F)
	si[14] = byte(totalSamples >> 24)
	si[15] = byte(totalSamples >> 16)
	si[16] = byte(totalSamples >> 8)
	si[17] = byte(totalSamples)
	return buf
}

func buildSyntheticWAV(sampleRate, channels, bitDepth int, seconds int) []byte {
	dataSize := uint32(sampleRate * channels * (bitDepth / 8) * seconds)
	buf := make([]byte, 44)
	copy(buf[0:4], "RIFF")
	binary.LittleEndian.PutUint32(buf[4:8], 36+dataSize)
	copy(buf[8:12], "WAVE")
	copy(buf[12:16], "fmt ")
	binary.LittleEndian.PutUint32(buf[16:20], 16)
	binary.LittleEndian.PutUint16(buf[20:22], 1) // PCM
	binary.LittleEndian.PutUint16(buf[22:24], uint16(channels))
	binary.LittleEndian.PutUint32(buf[24:28], uint32(sampleRate))
	binary.LittleEndian.PutUint32(buf[28:32], uint32(sampleRate*channels*(bitDepth/8)))
	binary.LittleEndian.PutUint16(buf[32:34], uint16(channels*(bitDepth/8)))
	binary.LittleEndian.PutUint16(buf[34:36], uint16(bitDepth))
	copy(buf[36:40], "data")
	binary.LittleEndian.PutUint32(buf[40:44], dataSize)
	return buf
}

func TestProbeAudio_FLAC_24_192(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "hires.flac")
	// 24-bit / 192 kHz / stereo / 180 seconds
	data := buildSyntheticFLAC(192000, 2, 24, 192000*180)
	if err := os.WriteFile(path, data, 0o600); err != nil {
		t.Fatal(err)
	}

	p := ProbeAudio(path)
	if p.SampleRate != 192000 || p.BitDepth != 24 || p.Channels != 2 || p.Duration != 180*time.Second {
		t.Fatalf("unexpected FLAC props: %+v", p)
	}
}

func TestProbeAudio_WAV_24_96(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "master.wav")
	data := buildSyntheticWAV(96000, 2, 24, 125)
	if err := os.WriteFile(path, data, 0o600); err != nil {
		t.Fatal(err)
	}

	p := ProbeAudio(path)
	if p.SampleRate != 96000 || p.BitDepth != 24 || p.Channels != 2 || p.Duration != 125*time.Second {
		t.Fatalf("unexpected WAV props: %+v", p)
	}
}

func TestCatalogScan_AndFolderArt(t *testing.T) {
	root := t.TempDir()
	albDir := filepath.Join(root, "Miles Davis - Kind of Blue")
	if err := os.MkdirAll(albDir, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(albDir, "01 So What.flac"), buildSyntheticFLAC(96000, 2, 24, 96000*562), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(albDir, "cover.jpg"), []byte{0xFF, 0xD8, 0xFF, 0xE0}, 0o600); err != nil {
		t.Fatal(err)
	}

	cat := NewCatalog(root)
	n, err := cat.Scan()
	if err != nil {
		t.Fatalf("Scan failed: %v", err)
	}
	if n != 1 {
		t.Fatalf("expected 1 track, got %d", n)
	}

	tracks := cat.Tracks()
	tr := tracks[0]
	if tr.SampleRate != 96000 || tr.BitDepth != 24 || tr.DurationSec != 562 {
		t.Fatalf("unexpected track audio props: %+v", tr)
	}
	if tr.MIME != "audio/flac" {
		t.Fatalf("expected audio/flac, got %q", tr.MIME)
	}
	if tr.ArtworkURL == "" {
		t.Fatal("expected folder cover art URL to be populated")
	}

	art, mimeType, ok := cat.CoverArt(tr.ID)
	if !ok || len(art) != 4 || mimeType != "image/jpeg" {
		t.Fatalf("unexpected CoverArt result: ok=%v len=%d mime=%q", ok, len(art), mimeType)
	}

	albums := cat.Albums()
	if len(albums) != 1 || albums[0].TrackCount != 1 {
		t.Fatalf("unexpected albums: %+v", albums)
	}
}

func TestCatalogScan_MultipleRootsNoIDCollision(t *testing.T) {
	r1 := t.TempDir()
	r2 := t.TempDir()
	if err := os.WriteFile(filepath.Join(r1, "track.flac"), buildSyntheticFLAC(44100, 2, 16, 44100*10), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(r2, "track.flac"), buildSyntheticFLAC(96000, 2, 24, 96000*20), 0o600); err != nil {
		t.Fatal(err)
	}

	cat := NewCatalog(r1, r2)
	n, err := cat.Scan()
	if err != nil {
		t.Fatal(err)
	}
	if n != 2 {
		t.Fatalf("expected 2 tracks, got %d", n)
	}
	tracks := cat.Tracks()
	if tracks[0].ID == tracks[1].ID {
		t.Fatalf("track IDs collided across roots: %q", tracks[0].ID)
	}
}

func TestProbeAudio_AIFFAndDSF(t *testing.T) {
	dir := t.TempDir()

	// Synthetic AIFF: 2ch, 96000 frames (1s), 24-bit, 96000 Hz IEEE-80 extended.
	// 96000 = 1.46484375 * 2^16 -> exp = 16383 + 16 = 16399 (0x400F), mantissa = 96000 << (63-16) = 0xBB80000000000000.
	aiff := make([]byte, 38)
	copy(aiff[0:4], "FORM")
	binary.BigEndian.PutUint32(aiff[4:8], 30)
	copy(aiff[8:12], "AIFF")
	copy(aiff[12:16], "COMM")
	binary.BigEndian.PutUint32(aiff[16:20], 18)
	binary.BigEndian.PutUint16(aiff[20:22], 2)     // channels
	binary.BigEndian.PutUint32(aiff[22:26], 96000) // frames
	binary.BigEndian.PutUint16(aiff[26:28], 24)    // bit depth
	binary.BigEndian.PutUint16(aiff[28:30], 0x400F)
	binary.BigEndian.PutUint64(aiff[30:38], 0xBB80000000000000)
	aiffPath := filepath.Join(dir, "test.aiff")
	if err := os.WriteFile(aiffPath, aiff, 0o600); err != nil {
		t.Fatal(err)
	}
	ap := ProbeAudio(aiffPath)
	if ap.SampleRate != 96000 || ap.BitDepth != 24 || ap.Channels != 2 || ap.Duration != time.Second {
		t.Fatalf("unexpected AIFF props: %+v", ap)
	}

	// Synthetic DSF (DSD64): 28-byte DSD chunk + 52-byte fmt chunk.
	dsf := make([]byte, 80)
	copy(dsf[0:4], "DSD ")
	binary.LittleEndian.PutUint64(dsf[4:12], 28)
	copy(dsf[28:32], "fmt ")
	binary.LittleEndian.PutUint64(dsf[32:40], 52)
	binary.LittleEndian.PutUint32(dsf[52:56], 2)       // channels
	binary.LittleEndian.PutUint32(dsf[56:60], 2822400) // DSD64 sample rate
	binary.LittleEndian.PutUint32(dsf[60:64], 1)       // 1-bit
	binary.LittleEndian.PutUint64(dsf[64:72], 2822400*10)
	dsfPath := filepath.Join(dir, "test.dsf")
	if err := os.WriteFile(dsfPath, dsf, 0o600); err != nil {
		t.Fatal(err)
	}
	dp := ProbeAudio(dsfPath)
	if dp.SampleRate != 2822400 || dp.BitDepth != 1 || dp.Channels != 2 || dp.Duration != 10*time.Second {
		t.Fatalf("unexpected DSF props: %+v", dp)
	}
}

func TestCatalog_SearchLookupAndMissingRootError(t *testing.T) {
	missing := NewCatalog(filepath.Join(t.TempDir(), "does-not-exist"))
	if _, err := missing.Scan(); err == nil {
		t.Fatal("expected Scan to fail on missing root directory")
	}

	root := t.TempDir()
	hiddenDir := filepath.Join(root, ".hidden")
	if err := os.MkdirAll(hiddenDir, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(hiddenDir, "secret.flac"), buildSyntheticFLAC(44100, 2, 16, 44100), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(root, "Blue in Green.flac"), buildSyntheticFLAC(96000, 2, 24, 96000*60), 0o600); err != nil {
		t.Fatal(err)
	}

	cat := NewCatalog(root)
	n, err := cat.Scan()
	if err != nil || n != 1 {
		t.Fatalf("expected 1 visible track (hidden skipped), got n=%d err=%v", n, err)
	}
	hits := cat.Search("blue", 10)
	if len(hits) != 1 {
		t.Fatalf("expected 1 search hit, got %d", len(hits))
	}
	if _, ok := cat.Track(hits[0].ID); !ok {
		t.Fatal("expected Track lookup to succeed")
	}
	if _, ok := cat.Album(hits[0].AlbumID); !ok {
		t.Fatal("expected Album lookup to succeed")
	}
}
