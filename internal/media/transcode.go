package media

import (
	"bufio"
	"bytes"
	"context"
	"io"
	"log"
	"net/http"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"tacet/internal/library"
	"tacet/internal/upnp"
)

// TranscoderInfo reports detected ffmpeg and libopus encoder capabilities.
type TranscoderInfo struct {
	FFmpegPath string
	HasLibOpus bool
}

// Available reports whether ffmpeg with the Xiph libopus encoder is available.
func (t TranscoderInfo) Available() bool {
	return t.FFmpegPath != "" && t.HasLibOpus
}

var (
	detectOnce sync.Once
	detected   TranscoderInfo
)

// DetectTranscoder probes $PATH for ffmpeg and verifies libopus encoder support.
func DetectTranscoder() TranscoderInfo {
	detectOnce.Do(func() {
		detected = probeTranscoder()
	})
	return detected
}

func probeTranscoder() TranscoderInfo {
	path, err := exec.LookPath("ffmpeg")
	if err != nil || path == "" {
		return TranscoderInfo{}
	}
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()

	//nolint:gosec // ffmpeg binary resolved strictly from PATH via exec.LookPath
	out, err := exec.CommandContext(ctx, path, "-hide_banner", "-encoders").Output()
	if err != nil {
		return TranscoderInfo{FFmpegPath: path}
	}
	hasLibOpus := false
	sc := bufio.NewScanner(bytes.NewReader(out))
	for sc.Scan() {
		fields := strings.Fields(sc.Text())
		// Encoder lines look like: " A....D libopus              libopus Opus (codec opus)"
		if len(fields) >= 2 && strings.HasPrefix(fields[0], "A") && fields[1] == "libopus" {
			hasLibOpus = true
			break
		}
	}
	return TranscoderInfo{
		FFmpegPath: path,
		HasLibOpus: hasLibOpus,
	}
}

type transcodeSpec struct {
	quality   string
	mime      string
	codecArgs []string
	fmtArgs   []string
}

func parseTranscodeSpec(quality, container string, track library.Track) (transcodeSpec, bool) {
	q := strings.ToLower(strings.TrimSpace(quality))
	if q == "" || q == "original" {
		return transcodeSpec{}, false
	}

	var opusBitrate string
	switch q {
	case "opus_96":
		opusBitrate = "96k"
	case "opus_128":
		opusBitrate = "128k"
	case "opus_160":
		opusBitrate = "160k"
	case "opus_256":
		opusBitrate = "256k"
	case "wav":
		codec := "pcm_s16le"
		var extra []string
		if track.BitDepth == 1 {
			// Decimate DSD cleanly to 24-bit / 88.2 kHz PCM.
			codec = "pcm_s24le"
			extra = []string{"-ar", "88200"}
		} else if track.BitDepth > 16 {
			codec = "pcm_s24le"
		}
		codecArgs := append([]string{"-c:a", codec}, extra...)
		return transcodeSpec{
			quality:   "wav",
			mime:      "audio/wav",
			codecArgs: codecArgs,
			fmtArgs:   []string{"-f", "wav"},
		}, true
	default:
		return transcodeSpec{}, false
	}

	// Explicitly select Xiph libopus with highest-complexity VBR music encoding.
	codecArgs := []string{
		"-c:a", "libopus",
		"-b:a", opusBitrate,
		"-vbr", "on",
		"-compression_level", "10",
		"-application", "audio",
	}
	if strings.EqualFold(strings.TrimSpace(container), "mp4") {
		return transcodeSpec{
			quality:   q,
			mime:      "audio/mp4",
			codecArgs: codecArgs,
			fmtArgs:   []string{"-f", "mp4", "-movflags", "frag_keyframe+empty_moov+default_base_moof"},
		}, true
	}
	return transcodeSpec{
		quality:   q,
		mime:      "audio/ogg",
		codecArgs: codecArgs,
		fmtArgs:   []string{"-f", "ogg"},
	}, true
}

type limitedBuffer struct {
	buf bytes.Buffer
	max int
}

func (l *limitedBuffer) Write(p []byte) (int, error) {
	rem := l.max - l.buf.Len()
	if rem > 0 {
		if len(p) > rem {
			l.buf.Write(p[:rem])
		} else {
			l.buf.Write(p)
		}
	}
	return len(p), nil
}

func (h *Handler) streamTranscode(
	w http.ResponseWriter,
	r *http.Request,
	spec transcodeSpec,
	inputPath string,
	stdin io.Reader,
) {
	info := h.transcoderInfo()
	if !info.Available() {
		http.Error(w, "ffmpeg with libopus is not available on this server", http.StatusServiceUnavailable)
		return
	}

	w.Header().Set("Content-Type", spec.mime)
	w.Header().Set("Cache-Control", "no-store")
	w.Header().Set("transferMode.dlna.org", "Streaming")
	w.Header().Set("contentFeatures.dlna.org", upnp.DLNAFeaturesTranscoded)

	if r.Method == http.MethodHead {
		w.WriteHeader(http.StatusOK)
		return
	}

	args := []string{
		"-hide_banner",
		"-loglevel", "error",
	}
	if inputPath != "" {
		args = append(args, "-i", filepath.Clean(inputPath))
	} else {
		args = append(args, "-i", "pipe:0")
	}
	args = append(args, "-vn", "-map", "0:a:0")
	args = append(args, spec.codecArgs...)
	args = append(args, spec.fmtArgs...)
	args = append(args, "pipe:1")

	//nolint:gosec // ffmpeg binary resolved via LookPath; args are validated enum flags + library path or pipe:0
	cmd := exec.CommandContext(r.Context(), info.FFmpegPath, args...)
	if stdin != nil {
		cmd.Stdin = stdin
	}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		http.Error(w, "transcoder pipe failed", http.StatusInternalServerError)
		return
	}
	errBuf := &limitedBuffer{max: 4096}
	cmd.Stderr = errBuf

	if err := cmd.Start(); err != nil {
		http.Error(w, "failed to start transcoder", http.StatusServiceUnavailable)
		return
	}

	// Read the first chunk before committing HTTP 200 so immediate ffmpeg errors
	// (corrupt file, bad codec, failed upstream) return 502 instead of an empty 200.
	first := make([]byte, 4096)
	n, firstErr := stdout.Read(first)
	if n == 0 {
		waitErr := cmd.Wait()
		if r.Context().Err() != nil {
			return
		}
		if msg := strings.TrimSpace(errBuf.buf.String()); msg != "" {
			log.Printf("media transcode (%s) failed before output: %v: %s", spec.quality, waitErr, msg)
		} else if waitErr != nil || firstErr != nil {
			log.Printf("media transcode (%s) failed before output: wait=%v read=%v", spec.quality, waitErr, firstErr)
		}
		w.Header().Del("Content-Type")
		w.Header().Del("transferMode.dlna.org")
		w.Header().Del("contentFeatures.dlna.org")
		http.Error(w, "audio transcoding failed", http.StatusBadGateway)
		return
	}

	w.WriteHeader(http.StatusOK)
	if _, err := w.Write(first[:n]); err == nil && firstErr == nil {
		_, _ = io.Copy(w, stdout)
	}
	if err := cmd.Wait(); err != nil && r.Context().Err() == nil {
		if msg := strings.TrimSpace(errBuf.buf.String()); msg != "" {
			log.Printf("media transcode (%s) error: %v: %s", spec.quality, err, msg)
		} else {
			log.Printf("media transcode (%s) error: %v", spec.quality, err)
		}
	}
}
