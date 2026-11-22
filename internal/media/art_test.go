package media

import (
	"bytes"
	"context"
	"image"
	"image/color"
	"image/png"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"testing"

	"tacet/internal/library"
)

func makeTestPNG(t *testing.T, w, h int) []byte {
	t.Helper()
	img := image.NewNRGBA(image.Rect(0, 0, w, h))
	for y := 0; y < h; y++ {
		for x := 0; x < w; x++ {
			img.SetNRGBA(x, y, color.NRGBA{
				R: uint8((x * 255) / max(1, w)),
				G: uint8((y * 255) / max(1, h)),
				B: 160,
				A: 255,
			})
		}
	}
	var buf bytes.Buffer
	if err := png.Encode(&buf, img); err != nil {
		t.Fatalf("encode test png: %v", err)
	}
	return buf.Bytes()
}

func TestServeCoverArt_ImmutableHashResizeAndDiskCache(t *testing.T) {
	root := t.TempDir()
	cacheDir := filepath.Join(t.TempDir(), "art-cache")

	if err := os.WriteFile(filepath.Join(root, "song.flac"), []byte("fLaC1234"), 0o600); err != nil {
		t.Fatal(err)
	}
	coverBytes := makeTestPNG(t, 1600, 1200)
	coverPath := filepath.Join(root, "cover.png")
	if err := os.WriteFile(coverPath, coverBytes, 0o600); err != nil {
		t.Fatal(err)
	}

	cat := library.NewCatalog(root)
	if _, err := cat.Scan(); err != nil {
		t.Fatal(err)
	}
	tr := cat.Tracks()[0]
	if !strings.HasPrefix(tr.ArtworkURL, "/media/art/") {
		t.Fatalf("expected /media/art/ URL, got %q", tr.ArtworkURL)
	}
	hash := strings.TrimPrefix(tr.ArtworkURL, "/media/art/")
	if !isArtHash(hash) {
		t.Fatalf("expected 16-char hex content hash, got %q", hash)
	}

	h := NewHandler(cat, nil)
	h.SetArtCacheDir(cacheDir)
	mux := http.NewServeMux()
	h.Register(mux)
	srv := httptest.NewServer(mux)
	defer srv.Close()

	// 1. Small bucket (?w=400) -> 400x300 JPEG with immutable caching + ETag.
	resp, err := http.Get(srv.URL + tr.ArtworkURL + "?w=400")
	if err != nil {
		t.Fatal(err)
	}
	body400, _ := io.ReadAll(resp.Body)
	resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		t.Fatalf("status = %d, want 200", resp.StatusCode)
	}
	if ct := resp.Header.Get("Content-Type"); ct != "image/jpeg" {
		t.Fatalf("Content-Type = %q, want image/jpeg", ct)
	}
	if cc := resp.Header.Get("Cache-Control"); cc != "public, max-age=31536000, immutable" {
		t.Fatalf("Cache-Control = %q, want immutable", cc)
	}
	if vary := resp.Header.Get("Vary"); !strings.Contains(vary, "Accept") {
		t.Fatalf("Vary = %q, want Accept", vary)
	}
	etag := resp.Header.Get("ETag")
	if etag == "" {
		t.Fatal("expected ETag header on content-hashed art")
	}
	cfg400, fmt400, err := image.DecodeConfig(bytes.NewReader(body400))
	if err != nil || fmt400 != "jpeg" {
		t.Fatalf("decode 400px jpeg: fmt=%q err=%v", fmt400, err)
	}
	if cfg400.Width != 400 || cfg400.Height != 300 {
		t.Fatalf("400px bucket dims = %dx%d, want 400x300", cfg400.Width, cfg400.Height)
	}

	// 2. Conditional If-None-Match returns 304 Not Modified.
	req304, _ := http.NewRequest(http.MethodGet, srv.URL+tr.ArtworkURL+"?w=400", nil)
	req304.Header.Set("If-None-Match", etag)
	resp304, err := http.DefaultClient.Do(req304)
	if err != nil {
		t.Fatal(err)
	}
	resp304.Body.Close()
	if resp304.StatusCode != http.StatusNotModified {
		t.Fatalf("If-None-Match status = %d, want 304", resp304.StatusCode)
	}

	// 3. Default bucket (no ?w=) -> 1000x750 JPEG.
	respLg, err := http.Get(srv.URL + tr.ArtworkURL)
	if err != nil {
		t.Fatal(err)
	}
	body1000, _ := io.ReadAll(respLg.Body)
	respLg.Body.Close()
	cfg1000, _, err := image.DecodeConfig(bytes.NewReader(body1000))
	if err != nil {
		t.Fatalf("decode 1000px jpeg: %v", err)
	}
	if cfg1000.Width != 1000 || cfg1000.Height != 750 {
		t.Fatalf("1000px bucket dims = %dx%d, want 1000x750", cfg1000.Width, cfg1000.Height)
	}

	// 4. Deleting source cover.png still serves from on-disk art-cache.
	if err = os.Remove(coverPath); err != nil {
		t.Fatal(err)
	}
	respCached, err := http.Get(srv.URL + tr.ArtworkURL + "?w=400")
	if err != nil {
		t.Fatal(err)
	}
	cachedBody, _ := io.ReadAll(respCached.Body)
	respCached.Body.Close()
	if respCached.StatusCode != http.StatusOK || !bytes.Equal(cachedBody, body400) {
		t.Fatalf("expected disk cache hit after removing source file: status=%d len=%d", respCached.StatusCode, len(cachedBody))
	}
}

func TestArtTranscode_NoUpscaleAndCoalescing(t *testing.T) {
	smallPNG := makeTestPNG(t, 200, 150)
	jpg, ok := transcodeArtJPEG(smallPNG, artSizeSmall)
	if !ok {
		t.Fatal("expected transcodeArtJPEG to succeed")
	}
	cfg, _, err := image.DecodeConfig(bytes.NewReader(jpg))
	if err != nil {
		t.Fatalf("decode small jpeg: %v", err)
	}
	if cfg.Width != 200 || cfg.Height != 150 {
		t.Fatalf("expected no upscale (200x150), got %dx%d", cfg.Width, cfg.Height)
	}

	// Concurrent cold requests for the same (hash, bucket) should invoke loadRaw once.
	cache := newArtCache(filepath.Join(t.TempDir(), "art-cache"))
	var calls atomic.Int32
	started := make(chan struct{})
	gate := make(chan struct{})
	hash := shortArtHash(smallPNG)

	load := func() ([]byte, string, bool) {
		if calls.Add(1) == 1 {
			close(started)
		}
		<-gate
		return smallPNG, "image/png", true
	}

	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		data, mime, hit := cache.getOrBuild(context.Background(), hash, artSizeSmall, artFormatJPEG, load)
		if !hit || mime != "image/jpeg" || len(data) == 0 {
			t.Errorf("unexpected leader result: hit=%v mime=%q len=%d", hit, mime, len(data))
		}
	}()
	// Wait until leader is guaranteed inside loadRaw before launching followers.
	<-started
	for range 5 {
		wg.Add(1)
		go func() {
			defer wg.Done()
			data, mime, hit := cache.getOrBuild(context.Background(), hash, artSizeSmall, artFormatJPEG, load)
			if !hit || mime != "image/jpeg" || len(data) == 0 {
				t.Errorf("unexpected coalesced result: hit=%v mime=%q len=%d", hit, mime, len(data))
			}
		}()
	}
	close(gate)
	wg.Wait()

	if calls.Load() != 1 {
		t.Fatalf("expected 1 coalesced loadRaw call, got %d", calls.Load())
	}
}

func TestAcceptsWebPAndPPM(t *testing.T) {
	cases := []struct {
		accept string
		want   bool
	}{
		{"", false},
		{"*/*", false},
		{"image/jpeg, image/png, */*", false},
		{"image/avif,image/webp,image/apng,*/*;q=0.8", true},
		{"image/webp", true},
		{"image/webp;q=0", false},
		{"image/webp;q=0.0, image/jpeg", false},
		{"IMAGE/WEBP;q=0.9", true},
	}
	for _, tc := range cases {
		if got := acceptsWebP(tc.accept); got != tc.want {
			t.Errorf("acceptsWebP(%q) = %v, want %v", tc.accept, got, tc.want)
		}
	}

	img := image.NewRGBA(image.Rect(0, 0, 2, 1))
	img.SetRGBA(0, 0, color.RGBA{R: 10, G: 20, B: 30, A: 255})
	img.SetRGBA(1, 0, color.RGBA{R: 40, G: 50, B: 60, A: 255})
	ppm := encodePPM(img)
	want := append([]byte("P6\n2 1\n255\n"), 10, 20, 30, 40, 50, 60)
	if !bytes.Equal(ppm, want) {
		t.Fatalf("encodePPM mismatch: got %v, want %v", ppm, want)
	}

	// If cwebp is installed on the host, verify real WebP transcoding end-to-end.
	if enc := DetectArtEncoders(); enc.CWebPPath != "" {
		raw := makeTestPNG(t, 300, 300)
		data, mime, ok := transcodeArt(context.Background(), raw, artSizeSmall, artFormatWebP, enc)
		if !ok || mime != "image/webp" || len(data) < 12 || string(data[8:12]) != "WEBP" {
			t.Fatalf("cwebp transcode failed: ok=%v mime=%q len=%d", ok, mime, len(data))
		}
	}
}

func TestHandler_PrewarmArt_IncrementalAndIdempotent(t *testing.T) {
	root := t.TempDir()
	cacheDir := filepath.Join(t.TempDir(), "art-cache")

	albumA := filepath.Join(root, "AlbumA")
	albumB := filepath.Join(root, "AlbumB")
	if err := os.MkdirAll(albumA, 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(albumB, 0o700); err != nil {
		t.Fatal(err)
	}
	// Two tracks in AlbumA share one cover; AlbumB has a distinct cover.
	for _, p := range []string{
		filepath.Join(albumA, "01.flac"),
		filepath.Join(albumA, "02.flac"),
		filepath.Join(albumB, "01.flac"),
	} {
		if err := os.WriteFile(p, []byte("fLaC1234"), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.WriteFile(filepath.Join(albumA, "cover.png"), makeTestPNG(t, 800, 800), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(albumB, "cover.png"), makeTestPNG(t, 900, 700), 0o600); err != nil {
		t.Fatal(err)
	}

	cat := library.NewCatalog(root)
	if _, err := cat.Scan(); err != nil {
		t.Fatal(err)
	}

	h := NewHandler(cat, nil)
	h.SetArtCacheDir(cacheDir)
	defer h.Close()

	// First pass should build exactly 2 deduplicated 400px thumbnails.
	built := h.prewarmArtSync(context.Background())
	if built != 2 {
		t.Fatalf("first prewarm built = %d, want 2", built)
	}

	// Verify only 400px preferred-format files exist (no 1000px files).
	entries, err := os.ReadDir(cacheDir)
	if err != nil {
		t.Fatal(err)
	}
	if len(entries) != 2 {
		t.Fatalf("cache dir entries = %d, want 2", len(entries))
	}
	wantSuffix := "_400." + h.art.preferredFormat().ext
	for _, e := range entries {
		if !strings.HasSuffix(e.Name(), wantSuffix) {
			t.Errorf("unexpected cached file %q, want suffix %q", e.Name(), wantSuffix)
		}
	}

	// Second pass must be a no-op (incremental).
	if again := h.prewarmArtSync(context.Background()); again != 0 {
		t.Fatalf("second prewarm built = %d, want 0", again)
	}
}
