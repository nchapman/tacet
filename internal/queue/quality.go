package queue

import "strings"

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
