package media

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"image"
	"image/color"
	_ "image/gif" // register GIF decoder
	"image/jpeg"
	_ "image/png" // register PNG decoder
	"math"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"

	xdraw "golang.org/x/image/draw"
	_ "golang.org/x/image/webp" // register WebP decoder
)

const (
	artSizeSmall    = 400
	artSizeLarge    = 1000
	artJPEGQuality  = 85
	maxArtDimension = 8192
	maxArtPixels    = 40_000_000 // ~160 MiB RGBA cap against decompression bombs
)

var artOpaqueBg = color.RGBA{R: 9, G: 9, B: 11, A: 255}

// parseArtBucket snaps an optional ?w= query param to one of two fixed size
// buckets (400px for cards/rows, 1000px for hero/Now Playing/UPnP) so clients
// cannot cache-bomb the server with arbitrary dimensions.
func parseArtBucket(raw string) int {
	raw = strings.TrimSpace(raw)
	if raw == "" {
		return artSizeLarge
	}
	w, err := strconv.Atoi(raw)
	if err != nil || w <= 0 {
		return artSizeLarge
	}
	if w <= 500 {
		return artSizeSmall
	}
	return artSizeLarge
}

// isArtHash reports whether id is a 16-character lowercase hex content hash.
func isArtHash(id string) bool {
	if len(id) != 16 {
		return false
	}
	for i := 0; i < len(id); i++ {
		c := id[i]
		if (c < '0' || c > '9') && (c < 'a' || c > 'f') {
			return false
		}
	}
	return true
}

func shortArtHash(data []byte) string {
	sum := sha256.Sum256(data)
	return hex.EncodeToString(sum[:8])
}

// transcodeArtJPEG decodes raw cover art (JPEG/PNG/GIF/WebP), downscales it to
// fit within maxDim×maxDim without upscaling, composites any alpha channel onto
// the dark app background, and encodes a compressed JPEG.
func transcodeArtJPEG(raw []byte, maxDim int) ([]byte, bool) {
	cfg, _, err := image.DecodeConfig(bytes.NewReader(raw))
	if err != nil || cfg.Width <= 0 || cfg.Height <= 0 ||
		cfg.Width > maxArtDimension || cfg.Height > maxArtDimension ||
		int64(cfg.Width)*int64(cfg.Height) > maxArtPixels {
		return nil, false
	}
	src, _, err := image.Decode(bytes.NewReader(raw))
	if err != nil {
		return nil, false
	}
	sb := src.Bounds()
	sw, sh := sb.Dx(), sb.Dy()
	if sw <= 0 || sh <= 0 {
		return nil, false
	}

	dw, dh := sw, sh
	if maxDim > 0 && (sw > maxDim || sh > maxDim) {
		if sw >= sh {
			dw = maxDim
			dh = max(1, int(math.Round(float64(sh)*float64(maxDim)/float64(sw))))
		} else {
			dh = maxDim
			dw = max(1, int(math.Round(float64(sw)*float64(maxDim)/float64(sh))))
		}
	}

	dst := image.NewRGBA(image.Rect(0, 0, dw, dh))
	xdraw.Draw(dst, dst.Bounds(), &image.Uniform{C: artOpaqueBg}, image.Point{}, xdraw.Src)
	if dw != sw || dh != sh {
		xdraw.CatmullRom.Scale(dst, dst.Bounds(), src, sb, xdraw.Over, nil)
	} else {
		xdraw.Draw(dst, dst.Bounds(), src, sb.Min, xdraw.Over)
	}

	var buf bytes.Buffer
	if err := jpeg.Encode(&buf, dst, &jpeg.Options{Quality: artJPEGQuality}); err != nil {
		return nil, false
	}
	return buf.Bytes(), true
}

type artFlight struct {
	done chan struct{}
	data []byte
	mime string
	ok   bool
}

// artCache stores resized JPEG cover art variants on disk (keyed by content hash
// and size bucket) and coalesces concurrent cold transcodes.
type artCache struct {
	dir      string
	mu       sync.Mutex
	inflight map[string]*artFlight
}

func newArtCache(dir string) *artCache {
	dir = strings.TrimSpace(dir)
	if dir != "" {
		if matches, err := filepath.Glob(filepath.Join(dir, ".art-*.tmp")); err == nil {
			for _, m := range matches {
				_ = os.Remove(m)
			}
		}
	}
	return &artCache{
		dir:      dir,
		inflight: make(map[string]*artFlight),
	}
}

func (c *artCache) cachePath(hash string, bucket int) string {
	if c.dir == "" || !isArtHash(hash) {
		return ""
	}
	return filepath.Join(c.dir, fmt.Sprintf("%s_%d.jpg", hash, bucket))
}

func (c *artCache) readDisk(hash string, bucket int) ([]byte, bool) {
	p := c.cachePath(hash, bucket)
	if p == "" {
		return nil, false
	}
	data, err := os.ReadFile(filepath.Clean(p)) //nolint:gosec // p is strictly <dir>/<16-hex>_<bucket>.jpg validated by isArtHash
	if err != nil || len(data) == 0 {
		return nil, false
	}
	return data, true
}

func (c *artCache) writeDisk(hash string, bucket int, data []byte) {
	p := c.cachePath(hash, bucket)
	if p == "" || len(data) == 0 {
		return
	}
	if err := os.MkdirAll(c.dir, 0o700); err != nil {
		return
	}
	tmp, err := os.CreateTemp(c.dir, ".art-*.tmp")
	if err != nil {
		return
	}
	tmpPath := tmp.Name()
	cleanup := func() { _ = os.Remove(tmpPath) }

	_ = tmp.Chmod(0o600)
	if _, err := tmp.Write(data); err != nil {
		_ = tmp.Close()
		cleanup()
		return
	}
	if err := tmp.Close(); err != nil {
		cleanup()
		return
	}
	if err := os.Rename(tmpPath, p); err != nil {
		cleanup()
	}
}

// getOrBuild returns a cached resized JPEG for (hash, bucket) or invokes loadRaw
// once across concurrent callers, transcodes to JPEG, and persists it to disk.
func (c *artCache) getOrBuild(hash string, bucket int, loadRaw func() ([]byte, string, bool)) ([]byte, string, bool) {
	if hash != "" {
		if data, ok := c.readDisk(hash, bucket); ok {
			return data, "image/jpeg", true
		}
	}

	key := fmt.Sprintf("%s_%d", hash, bucket)
	if hash != "" {
		c.mu.Lock()
		if fl, exists := c.inflight[key]; exists {
			c.mu.Unlock()
			<-fl.done
			return fl.data, fl.mime, fl.ok
		}
		fl := &artFlight{done: make(chan struct{})}
		c.inflight[key] = fl
		c.mu.Unlock()

		defer func() {
			c.mu.Lock()
			delete(c.inflight, key)
			c.mu.Unlock()
			close(fl.done)
		}()

		// Re-check disk inside flight in case a prior flight finished right before we acquired the lock.
		if data, ok := c.readDisk(hash, bucket); ok {
			fl.data, fl.mime, fl.ok = data, "image/jpeg", true
			return fl.data, fl.mime, fl.ok
		}

		raw, rawMIME, ok := loadRaw()
		if !ok {
			return nil, "", false
		}
		if jpg, encoded := transcodeArtJPEG(raw, bucket); encoded {
			c.writeDisk(hash, bucket, jpg)
			fl.data, fl.mime, fl.ok = jpg, "image/jpeg", true
			return fl.data, fl.mime, fl.ok
		}
		fl.data, fl.mime, fl.ok = raw, safeImageMIME(rawMIME), true
		return fl.data, fl.mime, fl.ok
	}

	// Unhashed fallback path (legacy track ID URLs): load raw, hash it, then reuse disk cache.
	raw, rawMIME, ok := loadRaw()
	if !ok {
		return nil, "", false
	}
	derivedHash := shortArtHash(raw)
	if data, hit := c.readDisk(derivedHash, bucket); hit {
		return data, "image/jpeg", true
	}
	if jpg, encoded := transcodeArtJPEG(raw, bucket); encoded {
		c.writeDisk(derivedHash, bucket, jpg)
		return jpg, "image/jpeg", true
	}
	return raw, safeImageMIME(rawMIME), true
}
