package library

import (
	"context"
	"path/filepath"
	"regexp"
	"strings"
	"time"
	"unicode"

	"fmt"
)

// Source identifies where an audio track originates.
type Source string

// Track sources supported by tacet.
const (
	SourceLocal Source = "local"
	SourceQobuz Source = "qobuz"
)

// SourceType distinguishes local NAS files from remote streaming providers.
const (
	SourceTypeLocal  = "local"
	SourceTypeRemote = "remote"
)

// Track is the unified representation of a playable local or remote track.
type Track struct {
	ID           string     `json:"id"`
	Source       Source     `json:"source"`
	SourceType   string     `json:"sourceType,omitempty"`
	Provider     string     `json:"provider,omitempty"`
	ExternalID   string     `json:"externalId,omitempty"`
	Title        string     `json:"title"`
	Artist       string     `json:"artist"`
	AlbumArtist  string     `json:"albumArtist,omitempty"`
	Album        string     `json:"album"`
	AlbumID      string     `json:"albumId,omitempty"`
	Genre        string     `json:"genre,omitempty"`
	TrackNumber  int        `json:"trackNumber,omitempty"`
	DiscNumber   int        `json:"discNumber,omitempty"`
	Year         int        `json:"year,omitempty"`
	DurationSec  int        `json:"durationSec"`
	SampleRate   int        `json:"sampleRate,omitempty"`
	BitDepth     int        `json:"bitDepth,omitempty"`
	Channels     int        `json:"channels,omitempty"`
	MIME         string     `json:"mime"`
	Ext          string     `json:"ext"`
	Size         int64      `json:"size,omitempty"`
	ArtworkURL   string     `json:"artworkUrl,omitempty"`
	PlayCount    int        `json:"playCount,omitempty"`
	LastPlayedAt *time.Time `json:"lastPlayedAt,omitempty"`
	AddedAt      *time.Time `json:"addedAt,omitempty"`
	Loved        bool       `json:"loved,omitempty"`
	Available    bool       `json:"available"`
	InLibrary    bool       `json:"inLibrary"`

	// Internal fields (not exposed over JSON).
	FilePath     string `json:"-"`
	FileMtimeNS  int64  `json:"-"`
	FolderArt    string `json:"-"`
	HasEmbedArt  bool   `json:"-"`
	QobuzTrackID int64  `json:"-"`
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

// AlbumVersion summarizes a sibling edition of an album in the same version group.
type AlbumVersion struct {
	ID            string `json:"id"`
	Source        Source `json:"source"`
	SourceType    string `json:"sourceType"`
	Provider      string `json:"provider"`
	Title         string `json:"title"`
	Year          int    `json:"year,omitempty"`
	TrackCount    int    `json:"trackCount"`
	PrimaryFormat string `json:"primaryFormat,omitempty"`
	MaxRate       int    `json:"maxSampleRate,omitempty"`
	MaxBit        int    `json:"maxBitDepth,omitempty"`
	IsPrimary     bool   `json:"isPrimary"`
	Available     bool   `json:"available"`
}

// Album groups tracks belonging to the same source-specific album copy.
type Album struct {
	ID             string         `json:"id"`
	Source         Source         `json:"source"`
	SourceType     string         `json:"sourceType,omitempty"`
	Provider       string         `json:"provider,omitempty"`
	ExternalID     string         `json:"externalId,omitempty"`
	Title          string         `json:"title"`
	Artist         string         `json:"artist"`
	Genre          string         `json:"genre,omitempty"`
	Label          string         `json:"label,omitempty"`
	Description    string         `json:"description,omitempty"`
	Copyright      string         `json:"copyright,omitempty"`
	QobuzAlbumID   string         `json:"qobuzAlbumId,omitempty"`
	Year           int            `json:"year,omitempty"`
	TrackCount     int            `json:"trackCount"`
	DurationSec    int            `json:"durationSec,omitempty"`
	ArtworkURL     string         `json:"artworkUrl,omitempty"`
	PrimaryFormat  string         `json:"primaryFormat,omitempty"`
	MaxRate        int            `json:"maxSampleRate,omitempty"`
	MaxBit         int            `json:"maxBitDepth,omitempty"`
	VersionGroupID string         `json:"versionGroupId,omitempty"`
	VersionCount   int            `json:"versionCount,omitempty"`
	IsPrimary      bool           `json:"isPrimary"`
	IsPartial      bool           `json:"isPartial,omitempty"`
	PlayCount      int            `json:"playCount,omitempty"`
	AddedAt        *time.Time     `json:"addedAt,omitempty"`
	Available      bool           `json:"available"`
	InLibrary      bool           `json:"inLibrary"`
	Tracks         []Track        `json:"tracks,omitempty"`
	Versions       []AlbumVersion `json:"versions,omitempty"`
}

// ArtistSummary summarizes a unified album artist across local and remote sources.
type ArtistSummary struct {
	Name       string   `json:"name"`
	AlbumCount int      `json:"albumCount"`
	TrackCount int      `json:"trackCount"`
	ArtworkURL string   `json:"artworkUrl,omitempty"`
	Providers  []string `json:"providers"`
}

// TopSearchResult identifies the highest-intent match across artists, albums, and tracks.
type TopSearchResult struct {
	Type   string         `json:"type"` // "artist", "album", or "track"
	Score  int            `json:"score"`
	Artist *ArtistSummary `json:"artist,omitempty"`
	Album  *Album         `json:"album,omitempty"`
	Track  *Track         `json:"track,omitempty"`
}

// UnifiedSearchResult holds ranked local+synced library matches across all entity types.
type UnifiedSearchResult struct {
	Query     string           `json:"query"`
	TopResult *TopSearchResult `json:"topResult,omitempty"`
	Artists   []ArtistSummary  `json:"artists"`
	Albums    []Album          `json:"albums"`
	Tracks    []Track          `json:"tracks"`
}

// GenreSummary summarizes a genre facet across the unified library.
type GenreSummary struct {
	Name       string `json:"name"`
	AlbumCount int    `json:"albumCount"`
	TrackCount int    `json:"trackCount"`
}

// HeavyRotationAlbum pairs an album summary with its recent play activity (Rdio-style).
type HeavyRotationAlbum struct {
	Album
	RecentPlays int        `json:"recentPlays"`
	LastPlayed  *time.Time `json:"lastPlayed,omitempty"`
}

// ProviderSyncStatus reports the state of a configured library source.
type ProviderSyncStatus struct {
	Provider   string     `json:"provider"`
	SourceType string     `json:"sourceType"`
	Connected  bool       `json:"connected"`
	Syncing    bool       `json:"syncing"`
	Albums     int        `json:"albums"`
	Tracks     int        `json:"tracks"`
	LastSyncAt *time.Time `json:"lastSyncAt,omitempty"`
	LastError  string     `json:"lastError,omitempty"`
}

// RemoteCollection is the normalized payload returned by a RemoteSource sync.
type RemoteCollection struct {
	Albums []Album
	Tracks []Track // standalone favorited tracks (with their parent album metadata)
}

// RemoteSource abstracts any remote streaming service (Qobuz, future Tidal/Subsonic)
// so internal/library never imports provider-specific packages.
type RemoteSource interface {
	ProviderID() string
	IsConnected() bool
	SyncCollection(ctx context.Context) (RemoteCollection, error)
	GetAlbum(ctx context.Context, externalID string) (Album, error)
	GetTrack(ctx context.Context, externalID string) (Track, error)
	AddFavorite(ctx context.Context, kind, externalID string) error
	RemoveFavorite(ctx context.Context, kind, externalID string) error
}

// AlbumFilter scopes unified library album queries.
type AlbumFilter struct {
	Query              string
	SourceType         string // "", "all", "local", "remote"
	Provider           string // "", "local", "qobuz", ...
	Artist             string
	Genre              string
	Sort               string // "artist", "title", "year", "added", "plays"
	GroupVersions      bool
	IncludeUnavailable bool
	Limit              int
	Offset             int
}

// TrackFilter scopes unified library track queries.
type TrackFilter struct {
	Query              string
	SourceType         string // "", "all", "local", "remote"
	Provider           string
	Artist             string
	AlbumID            string
	Album              string
	Genre              string
	Sort               string // "album", "title", "artist", "year", "added", "plays", "duration", "quality"
	SortDir            string // "asc", "desc"
	IncludeUnavailable bool
	Limit              int
	Offset             int
}

// FacetFilter scopes artist and genre listing queries.
type FacetFilter struct {
	Query              string
	SourceType         string
	Provider           string
	Genre              string
	IncludeUnavailable bool
	Limit              int
	Offset             int
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

// FormatNameFromMIMEOrExt returns a compact uppercase codec label (e.g. "FLAC", "DSF", "MP3").
func FormatNameFromMIMEOrExt(mimeType, ext string) string {
	cleanExt := strings.ToUpper(strings.TrimPrefix(strings.TrimSpace(ext), "."))
	switch cleanExt {
	case "FLAC", "WAV", "AIFF", "AIF", "ALAC", "M4A", "MP3", "OGG", "OPUS", "DSF":
		if cleanExt == "AIF" {
			return "AIFF"
		}
		return cleanExt
	}
	m := strings.ToLower(strings.TrimSpace(mimeType))
	switch {
	case strings.Contains(m, "flac"):
		return "FLAC"
	case strings.Contains(m, "dsf"), strings.Contains(m, "dsd"):
		return "DSF"
	case strings.Contains(m, "wav"):
		return "WAV"
	case strings.Contains(m, "aiff"):
		return "AIFF"
	case strings.Contains(m, "mpeg"), strings.Contains(m, "mp3"):
		return "MP3"
	case strings.Contains(m, "mp4"), strings.Contains(m, "aac"), strings.Contains(m, "alac"):
		return "M4A"
	case strings.Contains(m, "opus"):
		return "OPUS"
	case strings.Contains(m, "ogg"):
		return "OGG"
	default:
		return cleanExt
	}
}

// IsLosslessFormat reports whether a codec label represents lossless/hi-res audio.
func IsLosslessFormat(format string) bool {
	switch strings.ToUpper(strings.TrimSpace(format)) {
	case "FLAC", "WAV", "AIFF", "ALAC", "DSF", "PCM":
		return true
	default:
		return false
	}
}

// SortKey normalizes display strings for classic iTunes alphabetical sorting
// (strips leading "The ", "A ", "An " and folds case).
func SortKey(s string) string {
	s = strings.TrimSpace(s)
	lower := strings.ToLower(s)
	for _, prefix := range []string{"the ", "an ", "a "} {
		if strings.HasPrefix(lower, prefix) && len(lower) > len(prefix) {
			lower = strings.TrimSpace(lower[len(prefix):])
			break
		}
	}
	return collapseSpaces(lower)
}

var trailingEditionRe = regexp.MustCompile(
	`(?i)\s*[\(\[]\s*(` +
		`[^)\]]*\b(remaster(ed)?|deluxe|super\s+deluxe|expanded|anniversary|special\s+edition|collector'?s|bonus\s+track|hi-?res|24-?bit|16-?bit|mono|stereo|explicit|clean|edition|version|reissue|legacy\s+edition)\b[^)\]]*` +
		`|\d{4}\s+mix` +
		`)\s*[\)\]]\s*$`,
)

// VersionKey computes a normalized grouping key for alternate album editions
// without altering the displayed title.
func VersionKey(albumArtist, title string) string {
	artist := normalizeVersionToken(SortKey(albumArtist))
	baseTitle := strings.TrimSpace(title)
	for range 3 {
		next := strings.TrimSpace(trailingEditionRe.ReplaceAllString(baseTitle, ""))
		if next == "" || next == baseTitle {
			break
		}
		baseTitle = next
	}
	normTitle := normalizeVersionToken(strings.ToLower(baseTitle))
	if artist == "" || normTitle == "" {
		return ""
	}
	return artist + "\x00" + normTitle
}

func normalizeVersionToken(s string) string {
	var b strings.Builder
	b.Grow(len(s))
	lastSpace := false
	for _, r := range s {
		switch {
		case unicode.IsLetter(r) || unicode.IsDigit(r):
			b.WriteRune(unicode.ToLower(r))
			lastSpace = false
		case r == '&':
			if !lastSpace && b.Len() > 0 {
				b.WriteByte(' ')
			}
			b.WriteString("and ")
			lastSpace = true
		default:
			if !lastSpace && b.Len() > 0 {
				b.WriteByte(' ')
				lastSpace = true
			}
		}
	}
	return strings.TrimSpace(b.String())
}

func collapseSpaces(s string) string {
	return strings.Join(strings.Fields(s), " ")
}

// QualityRank compares two album versions for automatic Primary Version selection:
// lossless beats lossy, then higher bit depth, then higher sample rate, then local NAS tie-breaker.
func QualityRank(format string, bitDepth, sampleRate int, sourceType string) int64 {
	var score int64
	if IsLosslessFormat(format) {
		score += 1_000_000_000_000
	}
	if strings.EqualFold(format, "DSF") {
		score += 500_000_000_000
	}
	score += int64(bitDepth) * 10_000_000
	score += int64(sampleRate) * 10
	if sourceType == SourceTypeLocal {
		score += 1
	}
	return score
}
