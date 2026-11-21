package library

import (
	"fmt"
	"path/filepath"
	"strings"
)

// Source identifies where an audio track originates.
type Source string

const (
	SourceLocal Source = "local"
	SourceQobuz Source = "qobuz"
)

// Track is the unified representation of a playable local or Qobuz track.
type Track struct {
	ID          string `json:"id"`
	Source      Source `json:"source"`
	Title       string `json:"title"`
	Artist      string `json:"artist"`
	AlbumArtist string `json:"albumArtist,omitempty"`
	Album       string `json:"album"`
	AlbumID     string `json:"albumId,omitempty"`
	TrackNumber int    `json:"trackNumber,omitempty"`
	DiscNumber  int    `json:"discNumber,omitempty"`
	Year        int    `json:"year,omitempty"`
	DurationSec int    `json:"durationSec"`
	SampleRate  int    `json:"sampleRate,omitempty"`
	BitDepth    int    `json:"bitDepth,omitempty"`
	Channels    int    `json:"channels,omitempty"`
	MIME        string `json:"mime"`
	Ext         string `json:"ext"`
	Size        int64  `json:"size,omitempty"`
	ArtworkURL  string `json:"artworkUrl,omitempty"`

	// Internal fields (not exposed over JSON).
	FilePath     string `json:"-"`
	FolderArt    string `json:"-"`
	HasEmbedArt  bool   `json:"-"`
	QobuzTrackID int64  `json:"qobuzTrackId,omitempty"`
}

// QualityLabel returns a human-readable quality badge like "FLAC 24-bit / 192 kHz".
func (t Track) QualityLabel() string {
	codec := strings.ToUpper(strings.TrimPrefix(t.Ext, "."))
	if codec == "" {
		codec = t.MIME
	}
	if t.BitDepth > 0 && t.SampleRate > 0 {
		khz := float64(t.SampleRate) / 1000.0
		if t.SampleRate%1000 == 0 {
			return fmt.Sprintf("%s %d-bit / %.0f kHz", codec, t.BitDepth, khz)
		}
		return fmt.Sprintf("%s %d-bit / %.1f kHz", codec, t.BitDepth, khz)
	}
	if t.SampleRate > 0 {
		return fmt.Sprintf("%s %.1f kHz", codec, float64(t.SampleRate)/1000.0)
	}
	return codec
}

// Album groups tracks belonging to the same album.
type Album struct {
	ID         string  `json:"id"`
	Source     Source  `json:"source"`
	Title      string  `json:"title"`
	Artist     string  `json:"artist"`
	Year       int     `json:"year,omitempty"`
	TrackCount int     `json:"trackCount"`
	ArtworkURL string  `json:"artworkUrl,omitempty"`
	MaxRate    int     `json:"maxSampleRate,omitempty"`
	MaxBit     int     `json:"maxBitDepth,omitempty"`
	Tracks     []Track `json:"tracks,omitempty"`
}

// MIMEForExt returns the preferred RFC MIME type for a supported audio extension.
func MIMEForExt(ext string) (string, bool) {
	switch strings.ToLower(ext) {
	case ".flac":
		return "audio/flac", true
	case ".wav":
		return "audio/wav", true
	case ".aiff", ".aif":
		return "audio/aiff", true
	case ".m4a", ".alac":
		return "audio/mp4", true
	case ".mp3":
		return "audio/mpeg", true
	case ".ogg":
		return "audio/ogg", true
	case ".opus":
		return "audio/opus", true
	case ".dsf":
		return "audio/x-dsf", true
	default:
		return "", false
	}
}

// IsSupportedAudio reports whether a filename has a supported audio extension.
func IsSupportedAudio(path string) bool {
	_, ok := MIMEForExt(filepath.Ext(path))
	return ok
}
