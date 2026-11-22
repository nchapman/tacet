package media

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"image"
	"image/color"
	_ "image/gif" // register GIF decoder
	"image/jpeg"
	"image/png"
	"math"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"time"

	xdraw "golang.org/x/image/draw"
	_ "golang.org/x/image/webp" // register WebP decoder
)

const (
	artSizeSmall    = 400
	artSizeLarge    = 1000
	artJPEGQuality  = 80
	artWebPQuality  = 80
	maxArtDimension = 8192
	maxArtPixels    = 40_000_000 // ~160 MiB RGBA cap against decompression bombs
	artEncodeLimit  = 10 * time.Second
)

var artOpaqueBg = color.RGBA{R: 9, G: 9, B: 11, A: 255}

type artFormat struct {
	ext  string
	mime string
}

var (
	artFormatJPEG = artFormat{ext: "jpg", mime: "image/jpeg"}
	artFormatWebP = artFormat{ext: "webp", mime: "image/webp"}
)

// ArtEncoders describes the external image encoders detected on the host.
type ArtEncoders struct {
	CWebPPath string
	CJPEGPath string
}

var (
	artEncOnce   sync.Once
	cachedArtEnc ArtEncoders
)

// DetectArtEncoders probes the host for standalone cwebp and MozJPEG/libjpeg
// cjpeg binaries once and caches the result.
func DetectArtEncoders() ArtEncoders {
	artEncOnce.Do(func() {
		if p, err := exec.LookPath("cwebp"); err == nil {
			cachedArtEnc.CWebPPath = p
		}
		// Prefer MozJPEG keg-only paths on macOS when present, then PATH cjpeg.
		for _, candidate := range []string{
			"/opt/homebrew/opt/mozjpeg/bin/cjpeg",
			"/usr/local/opt/mozjpeg/bin/cjpeg",
		} {
			if st, err := os.Stat(candidate); err == nil && !st.IsDir() {
				cachedArtEnc.CJPEGPath = candidate
				break
			}
		}
		if cachedArtEnc.CJPEGPath == "" {
			if p, err := exec.LookPath("cjpeg"); err == nil {
				cachedArtEnc.CJPEGPath = p
			}
		}
	})
	return cachedArtEnc
}

// acceptsWebP reports whether an HTTP Accept header explicitly advertises
// image/webp support (excluding q=0 and never matching bare */* so UPnP/DLNA
// renderers always receive JPEG).
func acceptsWebP(accept string) bool {
	if accept == "" {
		return false
	}
	for _, part := range strings.Split(accept, ",") {
		part = strings.TrimSpace(part)
		if part == "" {
			continue
		}
		semi := strings.IndexByte(part, ';')
		mediaType := part
		params := ""
		if semi >= 0 {
			mediaType = strings.TrimSpace(part[:semi])
			params = part[semi+1:]
		}
		if !strings.EqualFold(mediaType, "image/webp") {
			continue
		}
		if hasZeroQuality(params) {
			return false
		}
		return true
	}
	return false
}

func hasZeroQuality(params string) bool {
	for _, p := range strings.Split(params, ";") {
		p = strings.TrimSpace(p)
		if len(p) < 2 {
			continue
		}
		if (p[0] == 'q' || p[0] == 'Q') && strings.Contains(p, "=") {
			kv := strings.SplitN(p, "=", 2)
			if strings.EqualFold(strings.TrimSpace(kv[0]), "q") {
				if q, err := strconv.ParseFloat(strings.TrimSpace(kv[1]), 64); err == nil && q <= 0 {
					return true
				}
			}
		}
	}
	return false
}

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

// decodeAndResizeArt decodes raw cover art (JPEG/PNG/GIF/WebP), enforces
// decompression-bomb bounds, downscales to fit within maxDim×maxDim without
// upscaling using CatmullRom, and composites any alpha onto the dark background.
func decodeAndResizeArt(raw []byte, maxDim int) (*image.RGBA, bool) {
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
	return dst, true
}

// encodePPM serializes an opaque RGBA image into binary P6 PPM (raw RGB bytes)
// for near-zero-overhead piping into MozJPEG / libjpeg-turbo cjpeg.
func encodePPM(img *image.RGBA) []byte {
	b := img.Bounds()
	w, h := b.Dx(), b.Dy()
	hdr := fmt.Sprintf("P6\n%d %d\n255\n", w, h)
	out := make([]byte, len(hdr)+w*h*3)
	copy(out, hdr)
	pos := len(hdr)
	for y := 0; y < h; y++ {
		row := img.Pix[y*img.Stride : y*img.Stride+w*4]
		for x := 0; x < w*4; x += 4 {
			out[pos] = row[x]
			out[pos+1] = row[x+1]
			out[pos+2] = row[x+2]
			pos += 3
		}
	}
	return out
}

func encodeArtWebP(dst *image.RGBA, cwebpPath string) ([]byte, bool) {
	if cwebpPath == "" {
		return nil, false
	}
	var pngBuf bytes.Buffer
	enc := png.Encoder{CompressionLevel: png.NoCompression}
	if err := enc.Encode(&pngBuf, dst); err != nil {
		return nil, false
	}
	ctx, cancel := context.WithTimeout(context.Background(), artEncodeLimit)
	defer cancel()
	//nolint:gosec // cwebpPath resolved via LookPath; all args are static constants
	cmd := exec.CommandContext(ctx, cwebpPath, "-quiet", "-q", strconv.Itoa(artWebPQuality), "-m", "4", "-mt", "-sharp_yuv", "-o", "-", "--", "-")
	cmd.Stdin = &pngBuf
	var out bytes.Buffer
	cmd.Stdout = &out
	if err := cmd.Run(); err != nil {
		return nil, false
	}
	data := out.Bytes()
	if len(data) < 12 || string(data[0:4]) != "RIFF" || string(data[8:12]) != "WEBP" {
		return nil, false
	}
	return data, true
}

func encodeArtJPEG(dst *image.RGBA, cjpegPath string) ([]byte, bool) {
	if cjpegPath != "" {
		ppm := encodePPM(dst)
		ctx, cancel := context.WithTimeout(context.Background(), artEncodeLimit)
		defer cancel()
		//nolint:gosec // cjpegPath resolved via LookPath or fixed Homebrew mozjpeg path; args are static constants
		cmd := exec.CommandContext(ctx, cjpegPath, "-quality", strconv.Itoa(artJPEGQuality), "-optimize")
		cmd.Stdin = bytes.NewReader(ppm)
		var out bytes.Buffer
		cmd.Stdout = &out
		if err := cmd.Run(); err == nil {
			data := out.Bytes()
			if len(data) >= 2 && data[0] == 0xFF && data[1] == 0xD8 {
				return data, true
			}
		}
	}
	var buf bytes.Buffer
	if err := jpeg.Encode(&buf, dst, &jpeg.Options{Quality: artJPEGQuality}); err != nil {
		return nil, false
	}
	return buf.Bytes(), true
}

// transcodeArt decodes and resizes raw cover art and encodes it in the
// negotiated target format (WebP via cwebp when requested and available,
// otherwise MozJPEG/libjpeg cjpeg with pure-Go JPEG fallback).
func transcodeArt(raw []byte, maxDim int, format artFormat, enc ArtEncoders) ([]byte, string, bool) {
	dst, ok := decodeAndResizeArt(raw, maxDim)
	if !ok {
		return nil, "", false
	}
	if format == artFormatWebP {
		if webp, encoded := encodeArtWebP(dst, enc.CWebPPath); encoded {
			return webp, artFormatWebP.mime, true
		}
	}
	if jpg, encoded := encodeArtJPEG(dst, enc.CJPEGPath); encoded {
		return jpg, artFormatJPEG.mime, true
	}
	return nil, "", false
}

// transcodeArtJPEG is kept for callers/tests requesting JPEG output directly.
func transcodeArtJPEG(raw []byte, maxDim int) ([]byte, bool) {
	data, _, ok := transcodeArt(raw, maxDim, artFormatJPEG, DetectArtEncoders())
	return data, ok
}

type artFlight struct {
	done chan struct{}
	data []byte
	mime string
	ok   bool
}

// artCache stores resized WebP/JPEG cover art variants on disk (keyed by content
// hash, size bucket, and format) and coalesces concurrent cold transcodes.
type artCache struct {
	dir      string
	enc      ArtEncoders
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
		enc:      DetectArtEncoders(),
		inflight: make(map[string]*artFlight),
	}
}

func (c *artCache) preferredFormat() artFormat {
	if c.enc.CWebPPath != "" {
		return artFormatWebP
	}
	return artFormatJPEG
}

func (c *artCache) negotiateFormat(acceptHeader string) artFormat {
	if c.enc.CWebPPath != "" && acceptsWebP(acceptHeader) {
		return artFormatWebP
	}
	return artFormatJPEG
}

func (c *artCache) cachePath(hash string, bucket int, format artFormat) string {
	if c.dir == "" || !isArtHash(hash) {
		return ""
	}
	return filepath.Join(c.dir, fmt.Sprintf("%s_%d.%s", hash, bucket, format.ext))
}

func (c *artCache) existsDisk(hash string, bucket int, format artFormat) bool {
	p := c.cachePath(hash, bucket, format)
	if p == "" {
		return false
	}
	st, err := os.Stat(filepath.Clean(p))
	return err == nil && !st.IsDir() && st.Size() > 0
}

func (c *artCache) readDisk(hash string, bucket int, format artFormat) ([]byte, bool) {
	p := c.cachePath(hash, bucket, format)
	if p == "" {
		return nil, false
	}
	data, err := os.ReadFile(filepath.Clean(p)) //nolint:gosec // p is strictly <dir>/<16-hex>_<bucket>.<ext> validated by isArtHash
	if err != nil || len(data) == 0 {
		return nil, false
	}
	return data, true
}

func (c *artCache) writeDisk(hash string, bucket int, format artFormat, data []byte) {
	p := c.cachePath(hash, bucket, format)
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

// getOrBuild returns a cached resized image for (hash, bucket, format) or
// invokes loadRaw once across concurrent callers, encodes, and persists to disk.
func (c *artCache) getOrBuild(hash string, bucket int, format artFormat, loadRaw func() ([]byte, string, bool)) ([]byte, string, bool) {
	if hash != "" {
		if data, ok := c.readDisk(hash, bucket, format); ok {
			return data, format.mime, true
		}
	}

	key := fmt.Sprintf("%s_%d_%s", hash, bucket, format.ext)
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
		if data, ok := c.readDisk(hash, bucket, format); ok {
			fl.data, fl.mime, fl.ok = data, format.mime, true
			return fl.data, fl.mime, fl.ok
		}

		raw, rawMIME, ok := loadRaw()
		if !ok {
			return nil, "", false
		}
		if encoded, mime, ok := transcodeArt(raw, bucket, format, c.enc); ok {
			actualFmt := artFormatJPEG
			if mime == artFormatWebP.mime {
				actualFmt = artFormatWebP
			}
			c.writeDisk(hash, bucket, actualFmt, encoded)
			fl.data, fl.mime, fl.ok = encoded, mime, true
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
	if data, hit := c.readDisk(derivedHash, bucket, format); hit {
		return data, format.mime, true
	}
	if encoded, mime, ok := transcodeArt(raw, bucket, format, c.enc); ok {
		actualFmt := artFormatJPEG
		if mime == artFormatWebP.mime {
			actualFmt = artFormatWebP
		}
		c.writeDisk(derivedHash, bucket, actualFmt, encoded)
		return encoded, mime, true
	}
	return raw, safeImageMIME(rawMIME), true
}
