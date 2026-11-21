package library

import (
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"io"
	"io/fs"
	"mime"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"

	"github.com/dhowden/tag"
)

var coverArtNames = []string{
	"cover.jpg", "cover.jpeg", "cover.png", "cover.webp",
	"folder.jpg", "folder.jpeg", "folder.png",
	"front.jpg", "front.jpeg", "front.png",
	"album.jpg", "album.png",
}

// Catalog holds the scanned in-memory local NAS music index.
type Catalog struct {
	mu      sync.RWMutex
	roots   []string
	tracks  map[string]Track
	ordered []Track
	albums  map[string]Album
	albList []Album
}

// NewCatalog creates an empty catalog configured for the given music directories.
func NewCatalog(roots ...string) *Catalog {
	cleaned := make([]string, 0, len(roots))
	for _, r := range roots {
		r = strings.TrimSpace(r)
		if r != "" {
			if abs, err := filepath.Abs(r); err == nil {
				cleaned = append(cleaned, abs)
			} else {
				cleaned = append(cleaned, r)
			}
		}
	}
	return &Catalog{
		roots:  cleaned,
		tracks: make(map[string]Track),
		albums: make(map[string]Album),
	}
}

// Roots returns the configured music root directories.
func (c *Catalog) Roots() []string {
	c.mu.RLock()
	defer c.mu.RUnlock()
	out := make([]string, len(c.roots))
	copy(out, c.roots)
	return out
}

// Scan walks all configured root directories and rebuilds the library index.
func (c *Catalog) Scan() (int, error) {
	tracks := make(map[string]Track)
	var ordered []Track

	dirArtCache := make(map[string]string)

	for _, root := range c.Roots() {
		info, err := os.Stat(root)
		if err != nil {
			return 0, fmt.Errorf("music dir %q: %w", root, err)
		}
		if !info.IsDir() {
			return 0, fmt.Errorf("music dir %q is not a directory", root)
		}
		err = filepath.WalkDir(root, func(path string, d fs.DirEntry, walkErr error) error {
			if walkErr != nil {
				return nil
			}
			if d.IsDir() {
				if strings.HasPrefix(d.Name(), ".") && path != root {
					return filepath.SkipDir
				}
				return nil
			}
			if strings.HasPrefix(d.Name(), ".") || !IsSupportedAudio(path) {
				return nil
			}

			t, ok := scanFile(root, path, dirArtCache)
			if !ok {
				return nil
			}
			tracks[t.ID] = t
			ordered = append(ordered, t)
			return nil
		})
		if err != nil {
			return 0, fmt.Errorf("scan %s: %w", root, err)
		}
	}

	sort.Slice(ordered, func(i, j int) bool {
		ai, aj := albumSortKey(ordered[i]), albumSortKey(ordered[j])
		if ai != aj {
			return ai < aj
		}
		if ordered[i].DiscNumber != ordered[j].DiscNumber {
			return ordered[i].DiscNumber < ordered[j].DiscNumber
		}
		if ordered[i].TrackNumber != ordered[j].TrackNumber {
			return ordered[i].TrackNumber < ordered[j].TrackNumber
		}
		return strings.ToLower(ordered[i].Title) < strings.ToLower(ordered[j].Title)
	})

	albums := make(map[string]Album)
	var albList []Album
	for _, t := range ordered {
		alb, exists := albums[t.AlbumID]
		if !exists {
			artist := t.AlbumArtist
			if artist == "" {
				artist = t.Artist
			}
			alb = Album{
				ID:         t.AlbumID,
				Source:     SourceLocal,
				Title:      t.Album,
				Artist:     artist,
				Year:       t.Year,
				ArtworkURL: t.ArtworkURL,
			}
		}
		if alb.ArtworkURL == "" && t.ArtworkURL != "" {
			alb.ArtworkURL = t.ArtworkURL
		}
		if t.SampleRate > alb.MaxRate {
			alb.MaxRate = t.SampleRate
		}
		if t.BitDepth > alb.MaxBit {
			alb.MaxBit = t.BitDepth
		}
		alb.Tracks = append(alb.Tracks, t)
		alb.TrackCount = len(alb.Tracks)
		albums[t.AlbumID] = alb
	}
	for _, alb := range albums {
		albList = append(albList, alb)
	}
	sort.Slice(albList, func(i, j int) bool {
		if strings.ToLower(albList[i].Artist) != strings.ToLower(albList[j].Artist) {
			return strings.ToLower(albList[i].Artist) < strings.ToLower(albList[j].Artist)
		}
		return strings.ToLower(albList[i].Title) < strings.ToLower(albList[j].Title)
	})

	c.mu.Lock()
	c.tracks = tracks
	c.ordered = ordered
	c.albums = albums
	c.albList = albList
	c.mu.Unlock()

	return len(ordered), nil
}

// Tracks returns all indexed local tracks.
func (c *Catalog) Tracks() []Track {
	c.mu.RLock()
	defer c.mu.RUnlock()
	out := make([]Track, len(c.ordered))
	copy(out, c.ordered)
	return out
}

// Track looks up a single track by ID.
func (c *Catalog) Track(id string) (Track, bool) {
	c.mu.RLock()
	defer c.mu.RUnlock()
	t, ok := c.tracks[id]
	return t, ok
}

// Albums returns summary info for all local albums (without full track slices).
func (c *Catalog) Albums() []Album {
	c.mu.RLock()
	defer c.mu.RUnlock()
	out := make([]Album, len(c.albList))
	for i, a := range c.albList {
		a.Tracks = nil
		out[i] = a
	}
	return out
}

// Album returns a single album including its ordered tracks.
func (c *Catalog) Album(id string) (Album, bool) {
	c.mu.RLock()
	defer c.mu.RUnlock()
	a, ok := c.albums[id]
	if !ok {
		return Album{}, false
	}
	cp := make([]Track, len(a.Tracks))
	copy(cp, a.Tracks)
	a.Tracks = cp
	return a, true
}

// Search performs a case-insensitive substring search across title, artist, and album.
func (c *Catalog) Search(query string, limit int) []Track {
	q := strings.ToLower(strings.TrimSpace(query))
	if q == "" {
		return nil
	}
	if limit <= 0 {
		limit = 50
	}
	c.mu.RLock()
	defer c.mu.RUnlock()

	var results []Track
	for _, t := range c.ordered {
		if strings.Contains(strings.ToLower(t.Title), q) ||
			strings.Contains(strings.ToLower(t.Artist), q) ||
			strings.Contains(strings.ToLower(t.Album), q) {
			results = append(results, t)
			if len(results) >= limit {
				break
			}
		}
	}
	return results
}

const maxCoverArtBytes = 10 << 20 // 10 MiB safety cap

// CoverArt loads embedded or folder artwork bytes and MIME type for a local track.
func (c *Catalog) CoverArt(trackID string) ([]byte, string, bool) {
	t, ok := c.Track(trackID)
	if !ok {
		return nil, "", false
	}
	if t.HasEmbedArt {
		if f, err := os.Open(t.FilePath); err == nil {
			defer f.Close()
			if m, err := tag.ReadFrom(f); err == nil && m.Picture() != nil && len(m.Picture().Data) > 0 && len(m.Picture().Data) <= maxCoverArtBytes {
				mimeType := m.Picture().MIMEType
				if mimeType == "" {
					mimeType = "image/jpeg"
				}
				return m.Picture().Data, mimeType, true
			}
		}
	}
	if t.FolderArt != "" {
		if f, err := os.Open(t.FolderArt); err == nil {
			defer f.Close()
			if fi, err := f.Stat(); err == nil && fi.Size() > 0 && fi.Size() <= maxCoverArtBytes {
				if data, err := io.ReadAll(io.LimitReader(f, maxCoverArtBytes)); err == nil && len(data) > 0 {
					mimeType := mime.TypeByExtension(filepath.Ext(t.FolderArt))
					if mimeType == "" {
						mimeType = "image/jpeg"
					}
					return data, mimeType, true
				}
			}
		}
	}
	return nil, "", false
}

func scanFile(root, path string, dirArtCache map[string]string) (Track, bool) {
	fi, err := os.Stat(path)
	if err != nil {
		return Track{}, false
	}
	ext := strings.ToLower(filepath.Ext(path))
	mimeType, ok := MIMEForExt(ext)
	if !ok {
		return Track{}, false
	}

	// Hash the full cleaned path so identically named files across multiple
	// configured roots never collide.
	id := shortHash("local:" + filepath.Clean(path))

	dir := filepath.Dir(path)
	folderArt, cached := dirArtCache[dir]
	if !cached {
		folderArt = findFolderArt(dir)
		dirArtCache[dir] = folderArt
	}

	props := ProbeAudio(path)
	baseName := strings.TrimSuffix(filepath.Base(path), filepath.Ext(path))

	t := Track{
		ID:          id,
		Source:      SourceLocal,
		Title:       baseName,
		Artist:      "Unknown Artist",
		Album:       filepath.Base(dir),
		DurationSec: int(props.Duration.Seconds() + 0.5),
		SampleRate:  props.SampleRate,
		BitDepth:    props.BitDepth,
		Channels:    props.Channels,
		MIME:        mimeType,
		Ext:         ext,
		Size:        fi.Size(),
		FilePath:    path,
		FolderArt:   folderArt,
	}

	if f, err := os.Open(path); err == nil {
		if m, err := tag.ReadFrom(f); err == nil {
			if s := strings.TrimSpace(m.Title()); s != "" {
				t.Title = s
			}
			if s := strings.TrimSpace(m.Artist()); s != "" {
				t.Artist = s
			}
			if s := strings.TrimSpace(m.AlbumArtist()); s != "" {
				t.AlbumArtist = s
			}
			if s := strings.TrimSpace(m.Album()); s != "" {
				t.Album = s
			}
			if tr, _ := m.Track(); tr > 0 {
				t.TrackNumber = tr
			}
			if dc, _ := m.Disc(); dc > 0 {
				t.DiscNumber = dc
			}
			if y := m.Year(); y > 0 {
				t.Year = y
			}
			if pic := m.Picture(); pic != nil && len(pic.Data) > 0 {
				t.HasEmbedArt = true
			}
		}
		f.Close()
	}

	albArtist := t.AlbumArtist
	if albArtist == "" {
		albArtist = t.Artist
	}
	t.AlbumID = shortHash("album:" + strings.ToLower(albArtist) + "\x00" + strings.ToLower(t.Album))

	if t.HasEmbedArt || t.FolderArt != "" {
		t.ArtworkURL = "/media/art/" + t.ID
	}

	return t, true
}

func findFolderArt(dir string) string {
	entries, err := os.ReadDir(dir)
	if err != nil {
		return ""
	}
	byLower := make(map[string]string, len(entries))
	for _, e := range entries {
		if !e.IsDir() {
			byLower[strings.ToLower(e.Name())] = filepath.Join(dir, e.Name())
		}
	}
	for _, name := range coverArtNames {
		if full, ok := byLower[name]; ok {
			return full
		}
	}
	return ""
}

func albumSortKey(t Track) string {
	a := t.AlbumArtist
	if a == "" {
		a = t.Artist
	}
	return strings.ToLower(a) + "\x00" + strings.ToLower(t.Album)
}

func shortHash(s string) string {
	sum := sha256.Sum256([]byte(s))
	return hex.EncodeToString(sum[:8])
}
