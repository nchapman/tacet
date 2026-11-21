package library

import (
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"io"
	"io/fs"
	"log"
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

// Status describes the current local library catalog state.
type Status struct {
	Configured bool   `json:"configured"`
	Scanning   bool   `json:"scanning"`
	Tracks     int    `json:"tracks"`
	Albums     int    `json:"albums"`
	ETag       string `json:"etag"`
}

// Catalog holds the scanned in-memory local NAS music index.
type Catalog struct {
	mu      sync.RWMutex
	roots   []string
	tracks  map[string]Track
	ordered []Track
	albums  map[string]Album
	albList []Album
	gen     uint64

	scanMu     sync.Mutex
	scanning   bool
	scanWaitCh chan struct{}
	lastErr    error
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

// Status returns a summary of the library catalog and scan status.
func (c *Catalog) Status() Status {
	c.scanMu.Lock()
	scanning := c.scanning
	c.scanMu.Unlock()

	c.mu.RLock()
	defer c.mu.RUnlock()
	return Status{
		Configured: len(c.roots) > 0,
		Scanning:   scanning,
		Tracks:     len(c.ordered),
		Albums:     len(c.albList),
		ETag:       c.etagLocked(),
	}
}

// ETag returns an HTTP ETag representing the current catalog snapshot.
func (c *Catalog) ETag() string {
	c.mu.RLock()
	defer c.mu.RUnlock()
	return c.etagLocked()
}

func (c *Catalog) etagLocked() string {
	return fmt.Sprintf(`"lib-%d-%d-%d"`, c.gen, len(c.ordered), len(c.albList))
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
// Concurrent Scan calls coalesce into a single in-flight disk walk.
func (c *Catalog) Scan() (int, error) {
	c.scanMu.Lock()
	if c.scanning {
		ch := c.scanWaitCh
		c.scanMu.Unlock()
		<-ch
		c.scanMu.Lock()
		err := c.lastErr
		c.scanMu.Unlock()
		c.mu.RLock()
		n := len(c.ordered)
		c.mu.RUnlock()
		return n, err
	}
	ch := make(chan struct{})
	c.scanning = true
	c.scanWaitCh = ch
	c.scanMu.Unlock()

	n, err := c.runScan()

	c.scanMu.Lock()
	c.lastErr = err
	c.scanning = false
	close(ch)
	c.scanWaitCh = nil
	c.scanMu.Unlock()
	return n, err
}

func (c *Catalog) runScan() (int, error) {
	tracks := make(map[string]Track)
	ordered := make([]Track, 0)

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
				log.Printf("library scan skipping unreadable path %s: %v", path, walkErr)
				return nil //nolint:nilerr // continue indexing remaining library files when one entry is unreadable
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

			t, ok := scanFile(path, dirArtCache)
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
		if !strings.EqualFold(albList[i].Artist, albList[j].Artist) {
			return strings.ToLower(albList[i].Artist) < strings.ToLower(albList[j].Artist)
		}
		return strings.ToLower(albList[i].Title) < strings.ToLower(albList[j].Title)
	})

	c.mu.Lock()
	c.tracks = tracks
	c.ordered = ordered
	c.albums = albums
	c.albList = albList
	c.gen++
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

// AlbumsPage filters, sorts, and slices albums with total count for pagination.
func (c *Catalog) AlbumsPage(query, sortBy string, limit, offset int) ([]Album, int) {
	q := strings.ToLower(strings.TrimSpace(query))
	c.mu.RLock()
	filtered := make([]Album, 0, len(c.albList))
	for _, a := range c.albList {
		if q != "" && !strings.Contains(strings.ToLower(a.Title), q) && !strings.Contains(strings.ToLower(a.Artist), q) {
			continue
		}
		a.Tracks = nil
		filtered = append(filtered, a)
	}
	c.mu.RUnlock()

	switch strings.ToLower(strings.TrimSpace(sortBy)) {
	case "title":
		sort.Slice(filtered, func(i, j int) bool {
			return strings.ToLower(filtered[i].Title) < strings.ToLower(filtered[j].Title)
		})
	case "year":
		sort.Slice(filtered, func(i, j int) bool {
			if filtered[i].Year != filtered[j].Year {
				return filtered[i].Year > filtered[j].Year
			}
			return strings.ToLower(filtered[i].Title) < strings.ToLower(filtered[j].Title)
		})
	}

	total := len(filtered)
	if offset < 0 {
		offset = 0
	}
	if offset >= total {
		return make([]Album, 0), total
	}
	if limit <= 0 || offset+limit > total {
		limit = total - offset
	}
	out := make([]Album, limit)
	copy(out, filtered[offset:offset+limit])
	return out, total
}

// TracksPage filters and slices tracks with total count for pagination.
func (c *Catalog) TracksPage(query string, limit, offset int) ([]Track, int) {
	q := strings.ToLower(strings.TrimSpace(query))
	c.mu.RLock()
	filtered := make([]Track, 0, len(c.ordered))
	for _, t := range c.ordered {
		if q == "" ||
			strings.Contains(strings.ToLower(t.Title), q) ||
			strings.Contains(strings.ToLower(t.Artist), q) ||
			strings.Contains(strings.ToLower(t.Album), q) {
			filtered = append(filtered, t)
		}
	}
	c.mu.RUnlock()

	total := len(filtered)
	if offset < 0 {
		offset = 0
	}
	if offset >= total {
		return make([]Track, 0), total
	}
	if limit <= 0 || offset+limit > total {
		limit = total - offset
	}
	out := make([]Track, limit)
	copy(out, filtered[offset:offset+limit])
	return out, total
}

// Search performs a case-insensitive substring search across title, artist, and album.
func (c *Catalog) Search(query string, limit int) []Track {
	if strings.TrimSpace(query) == "" {
		return make([]Track, 0)
	}
	if limit <= 0 {
		limit = 50
	}
	items, _ := c.TracksPage(query, limit, 0)
	return items
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
			defer func() { _ = f.Close() }()
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
			defer func() { _ = f.Close() }()
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

func scanFile(path string, dirArtCache map[string]string) (Track, bool) {
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
	id := "local:" + shortHash("local:"+filepath.Clean(path))

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

	if f, err := os.Open(filepath.Clean(path)); err == nil { //nolint:gosec // local library path from configured music root
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
		_ = f.Close()
	}

	albArtist := t.AlbumArtist
	if albArtist == "" {
		albArtist = t.Artist
	}
	t.AlbumID = "local:" + shortHash("album:"+strings.ToLower(albArtist)+"\x00"+strings.ToLower(t.Album))

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
