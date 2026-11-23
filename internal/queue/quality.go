package queue

import (
	"strings"

	"tacet/internal/upnp"
)

// AudioQuality controls the audio format delivered to a specific playback endpoint.
type AudioQuality string

const (
	// AudioQualityOpus96 transcodes audio to 96 kbps VBR Opus via libopus.
	AudioQualityOpus96 AudioQuality = "opus_96"
	// AudioQualityOpus128 transcodes audio to 128 kbps VBR Opus via libopus.
	AudioQualityOpus128 AudioQuality = "opus_128"
	// AudioQualityOpus160 transcodes audio to 160 kbps VBR Opus via libopus.
	AudioQualityOpus160 AudioQuality = "opus_160"
	// AudioQualityOpus256 transcodes audio to 256 kbps VBR Opus via libopus.
	AudioQualityOpus256 AudioQuality = "opus_256"
	// AudioQualityOriginal streams the untouched source format (default).
	AudioQualityOriginal AudioQuality = "original"
	// AudioQualityWAV transcodes audio to uncompressed PCM WAV preserving sample rate and bit depth.
	AudioQualityWAV AudioQuality = "wav"
)

// NormalizeAudioQuality validates and canonicalizes a quality string.
func NormalizeAudioQuality(raw string) (AudioQuality, bool) {
	switch AudioQuality(strings.ToLower(strings.TrimSpace(raw))) {
	case "", AudioQualityOriginal:
		return AudioQualityOriginal, true
	case AudioQualityOpus96:
		return AudioQualityOpus96, true
	case AudioQualityOpus128:
		return AudioQualityOpus128, true
	case AudioQualityOpus160:
		return AudioQualityOpus160, true
	case AudioQualityOpus256:
		return AudioQualityOpus256, true
	case AudioQualityWAV:
		return AudioQualityWAV, true
	default:
		return "", false
	}
}

// IsOpus reports whether q is one of the Opus bitrate presets.
func (q AudioQuality) IsOpus() bool {
	switch q {
	case AudioQualityOpus96, AudioQualityOpus128, AudioQualityOpus160, AudioQualityOpus256:
		return true
	default:
		return false
	}
}

// NeedsTranscode reports whether q requires server-side ffmpeg transcoding.
func (q AudioQuality) NeedsTranscode() bool {
	return q.IsOpus() || q == AudioQualityWAV
}

// RendererSupportsOpus reports whether info can decode Opus streams.
// HTTP/SSE player endpoints negotiate Ogg/WebM/MP4 Opus directly. UPnP/DLNA
// renderers that advertise SinkProtocols (e.g. Linkplay/Audio Pro/WiiM) only
// support Ogg Vorbis for audio/ogg unless they explicitly advertise Opus.
func RendererSupportsOpus(info upnp.RendererInfo) bool {
	if info.Kind == "player" || len(info.SinkProtocols) == 0 {
		return true
	}
	for _, proto := range info.SinkProtocols {
		if strings.Contains(strings.ToLower(proto), "opus") {
			return true
		}
	}
	return false
}

// RendererSupportsAAC reports whether info can decode AAC ADTS streams.
func RendererSupportsAAC(info upnp.RendererInfo) bool {
	if len(info.SinkProtocols) == 0 {
		return true
	}
	return sinkSupportsMIME(info.SinkProtocols, "audio/aac") || sinkSupportsMIME(info.SinkProtocols, "audio/mp4")
}

// RendererSupportsLossy reports whether info can decode at least one lossy
// transcode format (Opus or AAC).
func RendererSupportsLossy(info upnp.RendererInfo) bool {
	return RendererSupportsOpus(info) || RendererSupportsAAC(info)
}
