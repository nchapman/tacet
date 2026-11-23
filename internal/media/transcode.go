package media

import (
	"bufio"
	"bytes"
	"context"
	"encoding/binary"
	"errors"
	"io"
	"log"
	"net/http"
	"os/exec"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"time"

	"tacet/internal/library"
	"tacet/internal/upnp"
)

// TranscoderInfo reports detected ffmpeg, libopus, libfdk_aac, and libsoxr capabilities.
type TranscoderInfo struct {
	FFmpegPath   string
	HasLibOpus   bool
	HasLibFDKAAC bool
	HasLibSoxr   bool
}

// Available reports whether ffmpeg with the Xiph libopus encoder is available.
func (t TranscoderInfo) Available() bool {
	return t.FFmpegPath != "" && t.HasLibOpus
}

var (
	detectOnce sync.Once
	detected   TranscoderInfo

	transcodeBufPool = sync.Pool{
		New: func() any {
			b := make([]byte, 64*1024)
			return &b
		},
	}

	transcodeSem = make(chan struct{}, max(4, runtime.NumCPU()))
)

// DetectTranscoder probes $PATH for ffmpeg and verifies libopus and libsoxr support.
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
	hasLibFDKAAC := false
	sc := bufio.NewScanner(bytes.NewReader(out))
	for sc.Scan() {
		fields := strings.Fields(sc.Text())
		// Encoder lines look like: " A....D libopus              libopus Opus (codec opus)"
		if len(fields) >= 2 && strings.HasPrefix(fields[0], "A") {
			switch fields[1] {
			case "libopus":
				hasLibOpus = true
			case "libfdk_aac":
				hasLibFDKAAC = true
			}
		}
	}

	hasLibSoxr := false
	//nolint:gosec // ffmpeg binary resolved strictly from PATH via exec.LookPath
	if cfgOut, cfgErr := exec.CommandContext(ctx, path, "-hide_banner", "-buildconf").Output(); cfgErr == nil {
		hasLibSoxr = bytes.Contains(cfgOut, []byte("--enable-libsoxr"))
	}

	return TranscoderInfo{
		FFmpegPath:   path,
		HasLibOpus:   hasLibOpus,
		HasLibFDKAAC: hasLibFDKAAC,
		HasLibSoxr:   hasLibSoxr,
	}
}

type transcodeSpec struct {
	quality       string
	mime          string
	filterArgs    []string
	codecArgs     []string
	fmtArgs       []string
	wavHeader     []byte // canonical 44-byte WAVE_FORMAT_PCM header when piping raw PCM
	exactLength   int64  // >0 when exact HTTP Content-Length is known (canonical local lossless WAV)
	sampleRate    int
	channels      int
	bitsPerSample int
}

func parseTranscodeSpec(quality, container string, track library.Track, totalSamples uint64, hasSoxr bool, hasFDKAAC ...bool) (transcodeSpec, bool) {
	q := strings.ToLower(strings.TrimSpace(quality))
	if q == "" || q == "original" {
		return transcodeSpec{}, false
	}

	if q == "wav" {
		return buildWAVSpec(track, totalSamples, hasSoxr), true
	}

	var bitrate string
	switch q {
	case "opus_96":
		bitrate = "96k"
	case "opus_128":
		bitrate = "128k"
	case "opus_160":
		bitrate = "160k"
	case "opus_256":
		bitrate = "256k"
	default:
		return transcodeSpec{}, false
	}

	c := strings.ToLower(strings.TrimSpace(container))
	if c == "adts" || c == "aac" {
		useFDK := len(hasFDKAAC) > 0 && hasFDKAAC[0]
		return buildAACSpec(q, bitrate, track, hasSoxr, useFDK), true
	}

	// Resample in 32-bit float (osf=flt) so hot masters never clip on 44.1->48kHz
	// interpolation before libopus, and fold multichannel cleanly to mono|stereo.
	var af string
	if hasSoxr {
		af = "aresample=48000:resampler=soxr:precision=28:osf=flt:rematrix_maxval=1.0,aformat=channel_layouts=mono|stereo"
	} else {
		af = "aresample=48000:resampler=swr:filter_size=128:phase_shift=10:kaiser_beta=12:cutoff=0.96:tsf=fltp:osf=flt:rematrix_maxval=1.0,aformat=channel_layouts=mono|stereo"
	}

	// Explicitly select Xiph libopus with highest-complexity VBR music encoding.
	codecArgs := []string{
		"-c:a", "libopus",
		"-b:a", bitrate,
		"-vbr", "on",
		"-compression_level", "10",
		"-application", "audio",
		"-frame_duration", "20",
	}
	if track.Channels > 2 {
		codecArgs = append(codecArgs, "-ac", "2")
	}

	switch c {
	case "webm":
		return transcodeSpec{
			quality:    q,
			mime:       "audio/webm",
			filterArgs: []string{"-af", af},
			codecArgs:  codecArgs,
			fmtArgs:    []string{"-f", "webm", "-cluster_time_limit", "1000"},
		}, true
	case "mp4":
		return transcodeSpec{
			quality:    q,
			mime:       "audio/mp4",
			filterArgs: []string{"-af", af},
			codecArgs:  codecArgs,
			fmtArgs: []string{
				"-f", "mp4",
				"-movflags", "empty_moov+default_base_moof+omit_tfhd_offset+skip_trailer",
				"-frag_duration", "1000000",
			},
		}, true
	default:
		return transcodeSpec{
			quality:    q,
			mime:       "audio/ogg",
			filterArgs: []string{"-af", af},
			codecArgs:  codecArgs,
			fmtArgs:    []string{"-f", "ogg", "-page_duration", "500000"},
		}, true
	}
}

func buildAACSpec(quality, bitrate string, track library.Track, hasSoxr, hasFDKAAC bool) transcodeSpec {
	// AAC natively supports 44.1 kHz and 48 kHz; only downsample hi-res (>48 kHz)
	// or 1-bit DSD sources to 48 kHz for universal UPnP/DLNA hardware compatibility.
	af := "aformat=channel_layouts=mono|stereo"
	if track.SampleRate > 48000 || track.BitDepth == 1 {
		if hasSoxr {
			af = "aresample=48000:resampler=soxr:precision=28:osf=flt:rematrix_maxval=1.0,aformat=channel_layouts=mono|stereo"
		} else {
			af = "aresample=48000:resampler=swr:filter_size=128:phase_shift=10:kaiser_beta=12:cutoff=0.96:tsf=fltp:osf=flt:rematrix_maxval=1.0,aformat=channel_layouts=mono|stereo"
		}
	}

	encoder := "aac"
	if hasFDKAAC {
		encoder = "libfdk_aac"
	}
	codecArgs := []string{"-c:a", encoder, "-b:a", bitrate}
	if track.Channels > 2 {
		codecArgs = append(codecArgs, "-ac", "2")
	}

	return transcodeSpec{
		quality:    quality,
		mime:       "audio/aac",
		filterArgs: []string{"-af", af},
		codecArgs:  codecArgs,
		fmtArgs:    []string{"-f", "adts"},
	}
}

func buildWAVSpec(track library.Track, totalSamples uint64, hasSoxr bool) transcodeSpec {
	// Only use 16-bit PCM when source is confirmed <=16-bit (and not 1-bit DSD).
	// Unknown bit depth (e.g. 24-bit ALAC .m4a or lossy) defaults to 24-bit PCM.
	bitsPerSample := 24
	pcmCodec := "pcm_s24le"
	rawFmt := "s24le"
	if track.BitDepth > 1 && track.BitDepth <= 16 {
		bitsPerSample = 16
		pcmCodec = "pcm_s16le"
		rawFmt = "s16le"
	}

	sampleRate := track.SampleRate
	var filterArgs []string
	if track.BitDepth == 1 {
		// Decimate DSD cleanly to 24-bit / 88.2 kHz PCM with ultrasonic noise filtering.
		sampleRate = 88200
		if hasSoxr {
			filterArgs = []string{"-af", "aresample=88200:resampler=soxr:precision=28:dither_method=triangular,lowpass=f=28000:poles=2"}
		} else {
			filterArgs = []string{"-af", "aresample=88200:resampler=swr:filter_size=128:phase_shift=10:kaiser_beta=12:cutoff=0.68:tsf=fltp:dither_method=triangular"}
		}
	} else if track.BitDepth > 24 {
		// Triangular dither when reducing 32-bit/float sources to 24-bit PCM.
		filterArgs = []string{"-af", "aresample=dither_method=triangular"}
	}

	channels := track.Channels
	if channels <= 0 {
		channels = 2
	}

	// For 1-2 channel streams with a known sample rate, emit a canonical 44-byte
	// WAVE_FORMAT_PCM (0x0001) header directly and pipe raw PCM from ffmpeg.
	// This avoids WAVE_FORMAT_EXTENSIBLE (0xFFFE) and LIST/INFO compatibility issues
	// on strict UPnP renderers (Sonos, HEOS, Linn) and enables exact Content-Length
	// for local lossless files.
	if channels <= 2 && sampleRate > 0 {
		var dataSize uint32 = 0xFFFFFFFF
		var exactLen int64
		if totalSamples > 0 && track.BitDepth != 1 {
			bytesPerFrame := uint64(channels) * uint64(bitsPerSample/8)
			totalBytes := totalSamples * bytesPerFrame
			// Standard RIFF/WAVE caps data chunk at < 4 GiB (~3.2h of 24/192 stereo).
			if totalBytes > 0 && totalBytes <= 0xFFFFFF00 {
				dataSize = uint32(totalBytes)
				exactLen = int64(44 + totalBytes)
			}
		}
		return transcodeSpec{
			quality:       "wav",
			mime:          "audio/wav",
			filterArgs:    filterArgs,
			codecArgs:     []string{"-c:a", pcmCodec},
			fmtArgs:       []string{"-f", rawFmt},
			wavHeader:     buildCanonicalWAVHeader(sampleRate, channels, bitsPerSample, dataSize),
			exactLength:   exactLen,
			sampleRate:    sampleRate,
			channels:      channels,
			bitsPerSample: bitsPerSample,
		}
	}

	// Fallback for multichannel (>2ch) or unknown sample rate: standard ffmpeg WAV muxer.
	return transcodeSpec{
		quality:    "wav",
		mime:       "audio/wav",
		filterArgs: filterArgs,
		codecArgs:  []string{"-c:a", pcmCodec},
		fmtArgs:    []string{"-f", "wav", "-fflags", "+bitexact"},
	}
}

func buildCanonicalWAVHeader(sampleRate, channels, bitsPerSample int, dataSize uint32) []byte {
	hdr := make([]byte, 44)
	blockAlign := uint16(channels * (bitsPerSample / 8)) //nolint:gosec // channels <= 2, bitsPerSample in {16, 24}
	byteRate := uint32(sampleRate) * uint32(blockAlign)  //nolint:gosec // validated positive audio sample rate
	riffSize := uint32(0xFFFFFFFF)
	if dataSize != 0xFFFFFFFF {
		riffSize = 36 + dataSize
	}

	copy(hdr[0:4], "RIFF")
	binary.LittleEndian.PutUint32(hdr[4:8], riffSize)
	copy(hdr[8:12], "WAVE")
	copy(hdr[12:16], "fmt ")
	binary.LittleEndian.PutUint32(hdr[16:20], 16)                    // PCM fmt chunk size
	binary.LittleEndian.PutUint16(hdr[20:22], 1)                     // WAVE_FORMAT_PCM (0x0001)
	binary.LittleEndian.PutUint16(hdr[22:24], uint16(channels))      //nolint:gosec // channels is 1 or 2
	binary.LittleEndian.PutUint32(hdr[24:28], uint32(sampleRate))    //nolint:gosec // validated > 0
	binary.LittleEndian.PutUint32(hdr[28:32], byteRate)              // bytes/sec
	binary.LittleEndian.PutUint16(hdr[32:34], blockAlign)            // bytes/frame
	binary.LittleEndian.PutUint16(hdr[34:36], uint16(bitsPerSample)) //nolint:gosec // 16 or 24
	copy(hdr[36:40], "data")
	binary.LittleEndian.PutUint32(hdr[40:44], dataSize)
	return hdr
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

// parseByteRange parses a single HTTP Range header (e.g. "bytes=0-1023",
// "bytes=44-", "bytes=-512") against total bytes. Multi-range headers are
// ignored (ranged=false) so callers fall back to 200 OK.
func parseByteRange(hdr string, total int64) (start, end int64, ranged, invalid bool) {
	hdr = strings.TrimSpace(hdr)
	if hdr == "" || total <= 0 {
		return 0, total - 1, false, false
	}
	if !strings.HasPrefix(hdr, "bytes=") {
		return 0, total - 1, false, false
	}
	spec := strings.TrimSpace(strings.TrimPrefix(hdr, "bytes="))
	if spec == "" || strings.Contains(spec, ",") {
		return 0, total - 1, false, false
	}
	dash := strings.IndexByte(spec, '-')
	if dash < 0 {
		return 0, 0, false, true
	}
	left := strings.TrimSpace(spec[:dash])
	right := strings.TrimSpace(spec[dash+1:])
	if left == "" {
		// Suffix byte range: bytes=-N
		suffix, err := strconv.ParseInt(right, 10, 64)
		if err != nil || suffix <= 0 {
			return 0, 0, false, true
		}
		if suffix >= total {
			return 0, total - 1, true, false
		}
		return total - suffix, total - 1, true, false
	}
	s, err := strconv.ParseInt(left, 10, 64)
	if err != nil || s < 0 || s >= total {
		return 0, 0, false, true
	}
	e := total - 1
	if right != "" {
		parsedEnd, err := strconv.ParseInt(right, 10, 64)
		if err != nil || parsedEnd < s {
			return 0, 0, false, true
		}
		if parsedEnd < e {
			e = parsedEnd
		}
	}
	return s, e, true, false
}

func clearStreamHeaders(w http.ResponseWriter) {
	h := w.Header()
	h.Del("Content-Type")
	h.Del("Content-Length")
	h.Del("Content-Range")
	h.Del("Accept-Ranges")
	h.Del("transferMode.dlna.org")
	h.Del("contentFeatures.dlna.org")
}

func (h *Handler) streamTranscode(
	w http.ResponseWriter,
	r *http.Request,
	spec transcodeSpec,
	inputPath string,
	inputFormat string,
	stdin io.Reader,
	closeStdin func(),
) {
	info := h.transcoderInfo()
	if !info.Available() {
		http.Error(w, "ffmpeg with libopus is not available on this server", http.StatusServiceUnavailable)
		return
	}

	seekableWAV := inputPath != "" &&
		spec.exactLength > 0 &&
		len(spec.wavHeader) == 44 &&
		spec.sampleRate > 0 &&
		spec.channels > 0 &&
		spec.bitsPerSample > 0

	statusCode := http.StatusOK
	var rangeStart, rangeEnd int64
	var ranged bool
	if seekableWAV {
		var invalid bool
		rangeStart, rangeEnd, ranged, invalid = parseByteRange(r.Header.Get("Range"), spec.exactLength)
		if invalid {
			w.Header().Set("Content-Range", "bytes */"+strconv.FormatInt(spec.exactLength, 10))
			http.Error(w, "range not satisfiable", http.StatusRequestedRangeNotSatisfiable)
			return
		}
		if ranged {
			statusCode = http.StatusPartialContent
			w.Header().Set("Content-Range", "bytes "+strconv.FormatInt(rangeStart, 10)+"-"+strconv.FormatInt(rangeEnd, 10)+"/"+strconv.FormatInt(spec.exactLength, 10))
		}
	}

	w.Header().Set("Content-Type", spec.mime)
	w.Header().Set("Cache-Control", "no-store")
	if seekableWAV {
		w.Header().Set("Accept-Ranges", "bytes")
		w.Header().Set("Content-Length", strconv.FormatInt(rangeEnd-rangeStart+1, 10))
	} else {
		w.Header().Set("Accept-Ranges", "none")
		if spec.exactLength > 0 {
			w.Header().Set("Content-Length", strconv.FormatInt(spec.exactLength, 10))
		}
	}
	w.Header().Set("transferMode.dlna.org", "Streaming")
	w.Header().Set("contentFeatures.dlna.org", upnp.DLNAFeaturesTranscoded)

	if r.Method == http.MethodHead {
		w.WriteHeader(statusCode)
		return
	}

	// Serve header-only WAV byte ranges directly from memory without spawning ffmpeg.
	if seekableWAV && rangeEnd < int64(len(spec.wavHeader)) {
		w.WriteHeader(statusCode)
		_, _ = w.Write(spec.wavHeader[rangeStart : rangeEnd+1])
		return
	}

	select {
	case transcodeSem <- struct{}{}:
		defer func() { <-transcodeSem }()
	case <-r.Context().Done():
		return
	}

	var headerSlice []byte
	var pcmWant int64 = -1
	var skipBytes int64
	var seekSec float64
	var blockAlign int64
	if seekableWAV {
		blockAlign = int64(spec.channels * (spec.bitsPerSample / 8))
		hdrLen := int64(len(spec.wavHeader))
		if rangeStart < hdrLen {
			headerSlice = spec.wavHeader[rangeStart:hdrLen]
			pcmWant = rangeEnd - hdrLen + 1
		} else {
			pcmStart := rangeStart - hdrLen
			pcmWant = rangeEnd - rangeStart + 1
			startFrame := pcmStart / blockAlign
			skipBytes = pcmStart % blockAlign
			if startFrame > 0 {
				seekSec = float64(startFrame) / float64(spec.sampleRate)
			}
		}
	} else if len(spec.wavHeader) > 0 {
		headerSlice = spec.wavHeader
	}

	args := []string{
		"-hide_banner",
		"-loglevel", "error",
		"-threads", "2",
	}
	if inputPath != "" {
		args = append(args, "-nostdin")
		if seekSec > 0 {
			args = append(args, "-ss", strconv.FormatFloat(seekSec, 'f', 9, 64))
		}
		args = append(args, "-i", filepath.Clean(inputPath))
	} else {
		if inputFormat != "" {
			args = append(args, "-f", inputFormat)
		}
		args = append(args, "-i", "pipe:0")
	}
	args = append(args, "-vn", "-map", "0:a:0", "-map_metadata", "-1")
	args = append(args, spec.filterArgs...)
	args = append(args, spec.codecArgs...)
	args = append(args, spec.fmtArgs...)
	args = append(args, "-flush_packets", "1", "pipe:1")

	cmdCtx, cancel := context.WithCancel(r.Context())
	defer cancel()

	//nolint:gosec // ffmpeg binary resolved via LookPath; args are validated enum flags + library path or pipe:0
	cmd := exec.CommandContext(cmdCtx, info.FFmpegPath, args...)
	cmd.WaitDelay = 2 * time.Second
	if stdin != nil {
		cmd.Stdin = stdin
	}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		clearStreamHeaders(w)
		http.Error(w, "transcoder pipe failed", http.StatusInternalServerError)
		return
	}
	errBuf := &limitedBuffer{max: 4096}
	cmd.Stderr = errBuf

	if err := cmd.Start(); err != nil {
		clearStreamHeaders(w)
		http.Error(w, "failed to start transcoder", http.StatusServiceUnavailable)
		return
	}

	if skipBytes > 0 {
		if _, err := io.CopyN(io.Discard, stdout, skipBytes); err != nil && !errors.Is(err, io.EOF) {
			cancel()
			_ = stdout.Close()
			_ = cmd.Wait()
			if r.Context().Err() == nil {
				clearStreamHeaders(w)
				http.Error(w, "audio seek failed", http.StatusBadGateway)
			}
			return
		}
	}

	// Read the first chunk before committing HTTP 200/206 so immediate ffmpeg errors
	// (corrupt file, bad codec, failed upstream) return 502 instead of an empty body.
	firstCap := 4096
	if pcmWant > 0 && pcmWant < int64(firstCap) {
		firstCap = int(pcmWant)
	}
	first := make([]byte, firstCap)
	n, firstErr := stdout.Read(first)
	if n == 0 {
		cancel()
		if closeStdin != nil {
			closeStdin()
		}
		_ = stdout.Close()
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
		w.Header().Del("Content-Length")
		w.Header().Del("Content-Range")
		w.Header().Del("Accept-Ranges")
		w.Header().Del("transferMode.dlna.org")
		w.Header().Del("contentFeatures.dlna.org")
		http.Error(w, "audio transcoding failed", http.StatusBadGateway)
		return
	}

	w.WriteHeader(statusCode)
	var writeErr error
	if len(headerSlice) > 0 {
		_, writeErr = w.Write(headerSlice)
	}
	if writeErr == nil {
		_, writeErr = w.Write(first[:n])
	}
	pcmWritten := int64(n)
	if writeErr == nil {
		if flusher, ok := w.(http.Flusher); ok {
			flusher.Flush()
		}
		if firstErr == nil && (pcmWant < 0 || pcmWritten < pcmWant) {
			var src io.Reader = stdout
			if pcmWant > 0 {
				src = io.LimitReader(stdout, pcmWant-pcmWritten)
			}
			bufPtr := transcodeBufPool.Get().(*[]byte)
			copied, cErr := io.CopyBuffer(w, src, *bufPtr)
			transcodeBufPool.Put(bufPtr)
			pcmWritten += copied
			writeErr = cErr
		}
	}

	// If a seeked WAV range ends a few samples shy of exactLength due to
	// timestamp rounding at EOF, pad the tiny tail with silence so Content-Length
	// is satisfied cleanly without a browser premature-EOF error.
	if writeErr == nil && pcmWant > 0 && pcmWritten < pcmWant && r.Context().Err() == nil {
		rem := pcmWant - pcmWritten
		maxPad := int64(spec.sampleRate) * blockAlign / 10 // <= 100ms
		if rem <= maxPad {
			zeros := make([]byte, rem)
			_, writeErr = w.Write(zeros)
		}
	}

	// Always unblock ffmpeg & stdin before Wait() so write errors or bounded
	// range requests (where LimitReader stops early) never hang.
	stoppedEarly := pcmWant > 0 && pcmWritten >= pcmWant
	if writeErr != nil || stoppedEarly {
		cancel()
		if closeStdin != nil {
			closeStdin()
		}
	}
	_ = stdout.Close()
	if err := cmd.Wait(); err != nil && r.Context().Err() == nil && writeErr == nil && !stoppedEarly {
		if msg := strings.TrimSpace(errBuf.buf.String()); msg != "" {
			log.Printf("media transcode (%s) error: %v: %s", spec.quality, err, msg)
		} else {
			log.Printf("media transcode (%s) error: %v", spec.quality, err)
		}
	}
}
