package library

import (
	"context"
	"encoding/binary"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func writeTinyWAV(t *testing.T, path string) {
	t.Helper()
	// Minimal valid 44-byte canonical PCM WAV (16-bit, 44.1kHz, stereo, 4 bytes of silence).
	hdr := make([]byte, 48)
	copy(hdr[0:4], "RIFF")
	binary.LittleEndian.PutUint32(hdr[4:8], 40)
	copy(hdr[8:12], "WAVE")
	copy(hdr[12:16], "fmt ")
	binary.LittleEndian.PutUint32(hdr[16:20], 16)
	binary.LittleEndian.PutUint16(hdr[20:22], 1)
	binary.LittleEndian.PutUint16(hdr[22:24], 2)
	binary.LittleEndian.PutUint32(hdr[24:28], 44100)
	binary.LittleEndian.PutUint32(hdr[28:32], 44100*4)
	binary.LittleEndian.PutUint16(hdr[32:34], 4)
	binary.LittleEndian.PutUint16(hdr[34:36], 16)
	copy(hdr[36:40], "data")
	binary.LittleEndian.PutUint32(hdr[40:44], 4)
	if err := os.WriteFile(path, hdr, 0o600); err != nil {
		t.Fatalf("writeTinyWAV(%s): %v", path, err)
	}
}

func waitForTrackCount(t *testing.T, cat *Catalog, want int, timeout time.Duration) {
	t.Helper()
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		if cat.Status().LocalTracks == want {
			return
		}
		time.Sleep(25 * time.Millisecond)
	}
	t.Fatalf("timed out waiting for LocalTracks=%d (got %d)", want, cat.Status().LocalTracks)
}

func TestAutoScanner_WatchesNewAlbumDirAndFiles(t *testing.T) {
	root := t.TempDir()
	cat, err := NewCatalogWithDB(":memory:", root)
	if err != nil {
		t.Fatalf("NewCatalogWithDB: %v", err)
	}
	defer func() { _ = cat.Close() }()

	scanDone := make(chan int, 8)
	auto := NewAutoScanner(cat, AutoScanConfig{
		WatchEnabled: true,
		Interval:     0,
		Debounce:     100 * time.Millisecond,
	}, func(_ context.Context, trackCount int) {
		select {
		case scanDone <- trackCount:
		default:
		}
	})
	auto.Start(context.Background())
	defer auto.Stop()

	// Create a brand-new album folder and drop a track inside it.
	albumDir := filepath.Join(root, "Artist - New Album")
	if err := os.MkdirAll(albumDir, 0o755); err != nil {
		t.Fatalf("MkdirAll: %v", err)
	}
	writeTinyWAV(t, filepath.Join(albumDir, "01 Track One.wav"))

	deadline := time.NewTimer(3 * time.Second)
	defer deadline.Stop()
	for {
		select {
		case n := <-scanDone:
			if n == 1 {
				if got := cat.Status().LocalTracks; got != 1 {
					t.Fatalf("LocalTracks = %d after onScanDone(1), want 1", got)
				}
				return
			}
		case <-deadline.C:
			t.Fatalf("timed out waiting for onScanDone(1) (LocalTracks=%d)", cat.Status().LocalTracks)
		}
	}
}

func TestAutoScanner_ScheduledRescanAndLiveReconfigure(t *testing.T) {
	root := t.TempDir()
	cat, err := NewCatalogWithDB(":memory:", root)
	if err != nil {
		t.Fatalf("NewCatalogWithDB: %v", err)
	}
	defer func() { _ = cat.Close() }()

	auto := NewAutoScanner(cat, AutoScanConfig{
		WatchEnabled: false,
		Interval:     120 * time.Millisecond,
		Debounce:     50 * time.Millisecond,
	}, nil)
	auto.Start(context.Background())
	defer auto.Stop()

	// Drop a track while watching is disabled — scheduled ticker should pick it up.
	writeTinyWAV(t, filepath.Join(root, "01 Scheduled.wav"))
	waitForTrackCount(t, cat, 1, 3*time.Second)

	// Reconfigure live to enable watching and disable scheduled ticker.
	auto.Configure(AutoScanConfig{
		WatchEnabled: true,
		Interval:     0,
		Debounce:     80 * time.Millisecond,
	})

	writeTinyWAV(t, filepath.Join(root, "02 Watched.wav"))
	waitForTrackCount(t, cat, 2, 3*time.Second)
}
