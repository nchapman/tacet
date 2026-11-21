package library

import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"io"
	"io/fs"
	"log"
	"mime"
	"os"
	"path/filepath"
	"runtime"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/dhowden/tag"
)

var coverArtNames = []string{
	"cover.jpg", "cover.jpeg", "cover.png", "cover.webp",
	"folder.jpg", "folder.jpeg", "folder.png",
	"front.jpg", "front.jpeg", "front.png",
	"album.jpg", "album.png",
}

// Status describes the current unified library catalog state across local and remote sources.
type Status struct {
	Configured    bool                 `json:"configured"`
	Scanning      bool                 `json:"scanning"`
	Tracks        int                  `json:"tracks"`
	Albums        int                  `json:"albums"`
	LocalTracks   int                  `json:"localTracks"`
	LocalAlbums   int                  `json:"localAlbums"`
	RemoteTracks  int                  `json:"remoteTracks"`
	RemoteAlbums  int                  `json:"remoteAlbums"`
	MissingTracks int                  `json:"missingTracks,omitempty"`
	MissingAlbums int                  `json:"missingAlbums,omitempty"`
	Providers     []ProviderSyncStatus `json:"providers,omitempty"`
	ETag          string               `json:"etag"`
}

// Catalog holds the SQLite-backed unified music library spanning local NAS files
// and registered remote sources (e.g. Qobuz).
type Catalog struct {
	mu      sync.RWMutex
	roots   []string
	store   *libraryStore
	remotes map[string]RemoteSource

	scanMu     sync.Mutex
	scanning   bool
	scanWaitCh chan struct{}
	lastErr    error

	syncMu       sync.Mutex
	syncingProvs map[string]bool

	onChange func(Status)
}

// NewCatalog creates an in-memory SQLite-backed catalog for the given music directories.
func NewCatalog(roots ...string) *Catalog {
	cat, err := NewCatalogWithDB(":memory:", roots...)
	if err != nil {
		// Opening an in-memory SQLite DB should never fail; panic only if driver/OS is broken.
		panic(fmt.Sprintf("open in-memory library store: %v", err))
	}
	return cat
}

// NewCatalogWithDB creates a persistent SQLite-backed unified library at dbPath.
func NewCatalogWithDB(dbPath string, roots ...string) (*Catalog, error) {
	store, err := openStore(dbPath)
	if err != nil {
		return nil, err
	}
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
		roots:        cleaned,
		store:        store,
		remotes:      make(map[string]RemoteSource),
		syncingProvs: make(map[string]bool),
	}, nil
}

// SetMissingRetention configures how long soft-deleted missing music is preserved
// before a scan permanently purges it (<= 0 disables automatic purging).
func (c *Catalog) SetMissingRetention(d time.Duration) {
	if c.store != nil {
		c.store.setMissingRetention(d)
	}
}

// SchemaVersion returns the currently applied SQLite migration version.
func (c *Catalog) SchemaVersion() (int, error) {
	return c.store.schemaVersion(context.Background())
}

// Close closes the underlying SQLite database cleanly.
func (c *Catalog) Close() error {
	if c.store == nil {
		return nil
	}
	return c.store.Close()
}

// SetOnChange registers an optional callback invoked whenever library contents change
// (e.g. to broadcast SSE library.updated frames).
func (c *Catalog) SetOnChange(fn func(Status)) {
	c.mu.Lock()
	c.onChange = fn
	c.mu.Unlock()
}

func (c *Catalog) notifyChange() {
	c.mu.RLock()
	fn := c.onChange
	c.mu.RUnlock()
	if fn != nil {
		fn(c.Status())
	}
}

// RegisterRemoteSource plugs a generic streaming provider (e.g. Qobuz) into the unified library.
func (c *Catalog) RegisterRemoteSource(src RemoteSource) {
	if src == nil || strings.TrimSpace(src.ProviderID()) == "" {
		return
	}
	c.mu.Lock()
	c.remotes[strings.ToLower(strings.TrimSpace(src.ProviderID()))] = src
	c.mu.Unlock()
}

func (c *Catalog) remoteSource(provider string) (RemoteSource, bool) {
	c.mu.RLock()
	defer c.mu.RUnlock()
	src, ok := c.remotes[strings.ToLower(strings.TrimSpace(provider))]
	return src, ok
}

// Status returns a summary of the unified library catalog and per-provider sync state.
func (c *Catalog) Status() Status {
	c.scanMu.Lock()
	localScanning := c.scanning
	c.scanMu.Unlock()

	c.syncMu.Lock()
	syncingCopy := make(map[string]bool, len(c.syncingProvs))
	anyRemoteSyncing := false
	for k, v := range c.syncingProvs {
		syncingCopy[k] = v
		if v {
			anyRemoteSyncing = true
		}
	}
	c.syncMu.Unlock()

	counts := c.store.countsAndProviders(context.Background())

	c.mu.RLock()
	hasRoots := len(c.roots) > 0
	remotes := make(map[string]RemoteSource, len(c.remotes))
	for k, v := range c.remotes {
		remotes[k] = v
	}
	c.mu.RUnlock()

	providers := make([]ProviderSyncStatus, 0, len(remotes)+1)
	if hasRoots || counts.localAlbums > 0 || counts.localTracks > 0 {
		ps := counts.states[string(SourceLocal)]
		ps.Provider = string(SourceLocal)
		ps.SourceType = SourceTypeLocal
		ps.Connected = hasRoots
		ps.Syncing = localScanning
		ps.Albums = counts.localAlbums
		ps.Tracks = counts.localTracks
		providers = append(providers, ps)
	}
	remoteIDs := make([]string, 0, len(remotes))
	for id := range remotes {
		remoteIDs = append(remoteIDs, id)
	}
	sort.Strings(remoteIDs)
	for _, id := range remoteIDs {
		src := remotes[id]
		ps := counts.states[id]
		ps.Provider = id
		ps.SourceType = SourceTypeRemote
		ps.Connected = src.IsConnected()
		ps.Syncing = syncingCopy[id]
		providers = append(providers, ps)
	}

	return Status{
		Configured:    hasRoots || len(remotes) > 0,
		Scanning:      localScanning || anyRemoteSyncing,
		Tracks:        counts.totalTracks,
		Albums:        counts.totalAlbums,
		LocalTracks:   counts.localTracks,
		LocalAlbums:   counts.localAlbums,
		RemoteTracks:  counts.remoteTracks,
		RemoteAlbums:  counts.remoteAlbums,
		MissingTracks: counts.missingTracks,
		MissingAlbums: counts.missingAlbums,
		Providers:     providers,
		ETag:          fmt.Sprintf(`"lib-%d-%d-%d"`, counts.gen, counts.totalTracks, counts.totalAlbums),
	}
}

// ETag returns an HTTP ETag representing the current catalog snapshot.
func (c *Catalog) ETag() string {
	return c.Status().ETag
}

// Roots returns the configured music root directories.
func (c *Catalog) Roots() []string {
	c.mu.RLock()
	defer c.mu.RUnlock()
	out := make([]string, len(c.roots))
	copy(out, c.roots)
	return out
}

// Scan walks all configured root directories incrementally and updates SQLite.
// Concurrent Scan calls coalesce into a single in-flight disk walk.
func (c *Catalog) Scan() (int, error) {
	return c.ScanContext(context.Background())
}

// ScanContext walks configured root directories incrementally, probing new or
// modified files concurrently while respecting context cancellation.
func (c *Catalog) ScanContext(ctx context.Context) (int, error) {
	c.scanMu.Lock()
	if c.scanning {
		ch := c.scanWaitCh
		c.scanMu.Unlock()
		select {
		case <-ch:
		case <-ctx.Done():
			return 0, ctx.Err()
		}
		c.scanMu.Lock()
		err := c.lastErr
		c.scanMu.Unlock()
		return c.Status().LocalTracks, err
	}
	ch := make(chan struct{})
	c.scanning = true
	c.scanWaitCh = ch
	c.scanMu.Unlock()

	c.notifyChange()
	n, err := c.runScan(ctx)

	c.scanMu.Lock()
	c.lastErr = err
	c.scanning = false
	close(ch)
	c.scanWaitCh = nil
	c.scanMu.Unlock()

	c.notifyChange()
	return n, err
}

func (c *Catalog) runScan(ctx context.Context) (int, error) {
	roots := c.Roots()
	if len(roots) == 0 {
		return 0, nil
	}

	fileCache, err := c.store.loadLocalFileCache(ctx)
	if err != nil {
		fileCache = make(map[string]localFileCacheEntry)
	}

	type probeJob struct {
		path string
		fi   fs.FileInfo
	}

	tracks := make([]Track, 0, len(fileCache))
	jobs := make([]probeJob, 0)
	seenPaths := make(map[string]struct{}, len(fileCache))
	allCachedAvailable := true
	for _, entry := range fileCache {
		if !entry.Available {
			allCachedAvailable = false
			break
		}
	}
	var missingRoots []string

	for _, root := range roots {
		if ctx.Err() != nil {
			return 0, ctx.Err()
		}
		info, statErr := os.Stat(root)
		if statErr != nil {
			// If a NAS drive/mount is temporarily disconnected after files were already
			// indexed, soft-delete its tracks rather than aborting or wiping history.
			if os.IsNotExist(statErr) && len(fileCache) > 0 {
				log.Printf("library scan: music dir %q is currently unavailable; soft-deleting missing entries", root)
				missingRoots = append(missingRoots, root)
				continue
			}
			_ = c.store.replaceProvider(ctx, SourceTypeLocal, string(SourceLocal), nil, nil, statErr)
			return 0, fmt.Errorf("music dir %q: %w", root, statErr)
		}
		if !info.IsDir() {
			dirErr := fmt.Errorf("music dir %q is not a directory", root)
			_ = c.store.replaceProvider(ctx, SourceTypeLocal, string(SourceLocal), nil, nil, dirErr)
			return 0, dirErr
		}
		walkErr := filepath.WalkDir(root, func(path string, d fs.DirEntry, wErr error) error {
			if ctx.Err() != nil {
				return ctx.Err()
			}
			if wErr != nil {
				log.Printf("library scan skipping unreadable path %s: %v", path, wErr)
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

			fi, fiErr := d.Info()
			if fiErr != nil {
				return nil //nolint:nilerr // skip transient unreadable files while indexing the rest of the library
			}
			cleanedPath := filepath.Clean(path)
			if _, dup := seenPaths[cleanedPath]; dup {
				return nil
			}
			seenPaths[cleanedPath] = struct{}{}
			mtimeNS := fi.ModTime().UnixNano()
			if cached, ok := fileCache[cleanedPath]; ok && cached.Mtime == mtimeNS && cached.Size == fi.Size() {
				tracks = append(tracks, cached.Track)
				return nil
			}
			jobs = append(jobs, probeJob{path: cleanedPath, fi: fi})
			return nil
		})
		if walkErr != nil {
			if ctx.Err() != nil {
				return 0, ctx.Err()
			}
			_ = c.store.replaceProvider(ctx, SourceTypeLocal, string(SourceLocal), nil, nil, walkErr)
			return 0, fmt.Errorf("scan %s: %w", root, walkErr)
		}
	}

	// Fast path: when every indexed file is still present with identical mtime &
	// size, and zero new/modified/soft-deleted files exist, skip rewriting SQLite.
	if len(jobs) == 0 && len(missingRoots) == 0 && len(fileCache) > 0 && len(seenPaths) == len(fileCache) && allCachedAvailable {
		if err := c.store.touchProviderSync(ctx, SourceTypeLocal, string(SourceLocal)); err != nil {
			return 0, err
		}
		return len(tracks), nil
	}

	if len(jobs) > 0 {
		workers := runtime.NumCPU()
		if workers < 2 {
			workers = 2
		}
		if workers > 8 {
			workers = 8
		}
		if workers > len(jobs) {
			workers = len(jobs)
		}

		var dirArtMu sync.Mutex
		dirArtCache := make(map[string]string)
		lookupDirArt := func(dir string) string {
			dirArtMu.Lock()
			art, ok := dirArtCache[dir]
			dirArtMu.Unlock()
			if ok {
				return art
			}
			art = findFolderArt(dir)
			dirArtMu.Lock()
			dirArtCache[dir] = art
			dirArtMu.Unlock()
			return art
		}

		jobCh := make(chan probeJob, workers*2)
		var outMu sync.Mutex
		var wg sync.WaitGroup
		for range workers {
			wg.Add(1)
			go func() {
				defer wg.Done()
				for job := range jobCh {
					if ctx.Err() != nil {
						return
					}
					if t, ok := scanFileWithArt(job.path, job.fi, lookupDirArt); ok {
						outMu.Lock()
						tracks = append(tracks, t)
						outMu.Unlock()
					}
				}
			}()
		}

	feedLoop:
		for _, job := range jobs {
			select {
			case jobCh <- job:
			case <-ctx.Done():
				break feedLoop
			}
		}
		close(jobCh)
		wg.Wait()
		if ctx.Err() != nil {
			return 0, ctx.Err()
		}
	}

	albums := buildAlbumsFromTracks(SourceTypeLocal, string(SourceLocal), tracks)
	if err := c.store.replaceProvider(ctx, SourceTypeLocal, string(SourceLocal), albums, tracks, nil); err != nil {
		return 0, err
	}
	if len(missingRoots) > 0 {
		_ = c.store.replaceProvider(ctx, SourceTypeLocal, string(SourceLocal), nil, nil, fmt.Errorf("unavailable music dirs: %s", strings.Join(missingRoots, ", ")))
	}
	return len(tracks), nil
}

func buildAlbumsFromTracks(sourceType, provider string, tracks []Track) []Album {
	byID := make(map[string]*Album)
	for i := range tracks {
		t := &tracks[i]
		t.Source = Source(provider)
		t.SourceType = sourceType
		t.Provider = provider
		t.Available = true
		t.InLibrary = true

		alb, exists := byID[t.AlbumID]
		if !exists {
			artist := t.AlbumArtist
			if artist == "" {
				artist = t.Artist
			}
			alb = &Album{
				ID:            t.AlbumID,
				Source:        Source(provider),
				SourceType:    sourceType,
				Provider:      provider,
				ExternalID:    strings.TrimPrefix(t.AlbumID, provider+":"),
				Title:         t.Album,
				Artist:        artist,
				Genre:         t.Genre,
				Year:          t.Year,
				ArtworkURL:    t.ArtworkURL,
				PrimaryFormat: FormatNameFromMIMEOrExt(t.MIME, t.Ext),
				Available:     true,
				InLibrary:     true,
				AddedAt:       t.AddedAt,
			}
			byID[t.AlbumID] = alb
		}
		if alb.ArtworkURL == "" && t.ArtworkURL != "" {
			alb.ArtworkURL = t.ArtworkURL
		}
		if alb.Genre == "" && t.Genre != "" {
			alb.Genre = t.Genre
		}
		if alb.Year == 0 && t.Year > 0 {
			alb.Year = t.Year
		}
		if t.SampleRate > alb.MaxRate {
			alb.MaxRate = t.SampleRate
		}
		if t.BitDepth > alb.MaxBit {
			alb.MaxBit = t.BitDepth
		}
		if f := FormatNameFromMIMEOrExt(t.MIME, t.Ext); f != "" && (alb.PrimaryFormat == "" || (!IsLosslessFormat(alb.PrimaryFormat) && IsLosslessFormat(f))) {
			alb.PrimaryFormat = f
		}
		alb.TrackCount++
		alb.DurationSec += t.DurationSec
	}

	out := make([]Album, 0, len(byID))
	for _, a := range byID {
		out = append(out, *a)
	}
	return out
}

// SyncRemote pulls a remote provider's collection (e.g. Qobuz favorites) into SQLite.
func (c *Catalog) SyncRemote(ctx context.Context, provider string) (int, error) {
	p := strings.ToLower(strings.TrimSpace(provider))
	src, ok := c.remoteSource(p)
	if !ok {
		return 0, fmt.Errorf("%w: %s", ErrProviderNotFound, provider)
	}
	if !src.IsConnected() {
		_ = c.store.setProviderAvailable(ctx, p, false)
		c.notifyChange()
		return 0, fmt.Errorf("%w: %s", ErrProviderOffline, provider)
	}

	c.syncMu.Lock()
	if c.syncingProvs[p] {
		c.syncMu.Unlock()
		return 0, nil
	}
	c.syncingProvs[p] = true
	c.syncMu.Unlock()
	c.notifyChange()

	defer func() {
		c.syncMu.Lock()
		delete(c.syncingProvs, p)
		c.syncMu.Unlock()
		c.notifyChange()
	}()

	col, err := src.SyncCollection(ctx)
	if err != nil {
		_ = c.store.replaceProvider(ctx, SourceTypeRemote, p, nil, nil, err)
		return 0, err
	}

	albMap := make(map[string]Album, len(col.Albums))
	trackMap := make(map[string]Track)

	for _, a := range col.Albums {
		normalizeRemoteAlbum(p, &a)
		a.IsPartial = false
		for i := range a.Tracks {
			normalizeRemoteTrack(p, a.ID, &a.Tracks[i])
			trackMap[a.Tracks[i].ID] = a.Tracks[i]
		}
		albMap[a.ID] = a
	}

	// Merge standalone favorited tracks (creating partial parent albums if not already present).
	for _, t := range col.Tracks {
		albID := t.AlbumID
		if albID == "" {
			albID = p + ":track-album:" + t.ExternalID
		}
		normalizeRemoteTrack(p, albID, &t)
		trackMap[t.ID] = t
		if _, exists := albMap[albID]; !exists {
			artist := t.AlbumArtist
			if artist == "" {
				artist = t.Artist
			}
			albMap[albID] = Album{
				ID:            albID,
				Source:        Source(p),
				SourceType:    SourceTypeRemote,
				Provider:      p,
				ExternalID:    strings.TrimPrefix(albID, p+":"),
				Title:         t.Album,
				Artist:        artist,
				Genre:         t.Genre,
				Year:          t.Year,
				ArtworkURL:    t.ArtworkURL,
				PrimaryFormat: FormatNameFromMIMEOrExt(t.MIME, t.Ext),
				MaxBit:        t.BitDepth,
				MaxRate:       t.SampleRate,
				IsPartial:     true,
				Available:     true,
				InLibrary:     true,
			}
		}
	}

	// Recompute exact album counts/durations/resolutions from the deduplicated track map.
	trackSlice := make([]Track, 0, len(trackMap))
	perAlbumCount := make(map[string]int)
	perAlbumDur := make(map[string]int)
	perAlbumBit := make(map[string]int)
	perAlbumRate := make(map[string]int)
	perAlbumFmt := make(map[string]string)

	for _, t := range trackMap {
		trackSlice = append(trackSlice, t)
		perAlbumCount[t.AlbumID]++
		perAlbumDur[t.AlbumID] += t.DurationSec
		if t.BitDepth > perAlbumBit[t.AlbumID] {
			perAlbumBit[t.AlbumID] = t.BitDepth
		}
		if t.SampleRate > perAlbumRate[t.AlbumID] {
			perAlbumRate[t.AlbumID] = t.SampleRate
		}
		if f := FormatNameFromMIMEOrExt(t.MIME, t.Ext); f != "" && perAlbumFmt[t.AlbumID] == "" {
			perAlbumFmt[t.AlbumID] = f
		}
	}

	albSlice := make([]Album, 0, len(albMap))
	for id, a := range albMap {
		if cnt := perAlbumCount[id]; cnt > 0 {
			a.TrackCount = cnt
			a.DurationSec = perAlbumDur[id]
		}
		if perAlbumBit[id] > a.MaxBit {
			a.MaxBit = perAlbumBit[id]
		}
		if perAlbumRate[id] > a.MaxRate {
			a.MaxRate = perAlbumRate[id]
		}
		if a.PrimaryFormat == "" {
			a.PrimaryFormat = perAlbumFmt[id]
		}
		a.Tracks = nil
		albSlice = append(albSlice, a)
	}

	if err := c.store.replaceProvider(ctx, SourceTypeRemote, p, albSlice, trackSlice, nil); err != nil {
		return 0, err
	}
	return len(trackSlice), nil
}

// SetProviderAvailable toggles availability of a remote provider's rows (e.g. on sign-out)
// without deleting user play history or playlists.
func (c *Catalog) SetProviderAvailable(ctx context.Context, provider string, available bool) error {
	if err := c.store.setProviderAvailable(ctx, strings.ToLower(strings.TrimSpace(provider)), available); err != nil {
		return err
	}
	c.notifyChange()
	return nil
}

// AddRemoteAlbum adds a remote album into the SQLite library AND mirrors the favorite two-way
// to the remote source (e.g. Qobuz).
func (c *Catalog) AddRemoteAlbum(ctx context.Context, provider, externalID string) (Album, error) {
	p := strings.ToLower(strings.TrimSpace(provider))
	extID := strings.TrimPrefix(strings.TrimSpace(externalID), p+":")
	if extID == "" {
		return Album{}, fmt.Errorf("%w: externalId is required", ErrInvalidArgument)
	}
	src, ok := c.remoteSource(p)
	if !ok {
		return Album{}, fmt.Errorf("%w: %s", ErrProviderNotFound, provider)
	}
	if !src.IsConnected() {
		return Album{}, fmt.Errorf("%w: %s", ErrProviderOffline, provider)
	}

	alb, err := src.GetAlbum(ctx, extID)
	if err != nil {
		return Album{}, err
	}
	if err = src.AddFavorite(ctx, "album", extID); err != nil {
		return Album{}, err
	}

	normalizeRemoteAlbum(p, &alb)
	alb.ExternalID = extID
	alb.ID = p + ":" + extID
	alb.IsPartial = false
	for i := range alb.Tracks {
		normalizeRemoteTrack(p, alb.ID, &alb.Tracks[i])
	}
	if err = c.store.upsertAlbumWithTracks(ctx, alb, alb.Tracks); err != nil {
		return Album{}, err
	}
	c.notifyChange()
	out, _, err := c.store.getAlbum(ctx, alb.ID)
	return out, err
}

// RemoveRemoteAlbum removes a remote album from the SQLite library AND unfavorites it two-way.
func (c *Catalog) RemoveRemoteAlbum(ctx context.Context, albumID string) error {
	alb, ok, err := c.store.getAlbum(ctx, albumID)
	if err != nil {
		return err
	}
	if !ok {
		return ErrNotFound
	}
	if alb.SourceType != SourceTypeRemote {
		return fmt.Errorf("%w: cannot remove local album via remote endpoint", ErrInvalidArgument)
	}
	if src, exists := c.remoteSource(alb.Provider); exists && src.IsConnected() {
		if err := src.RemoveFavorite(ctx, "album", alb.ExternalID); err != nil {
			return err
		}
	}
	if err := c.store.deleteAlbum(ctx, albumID); err != nil {
		return err
	}
	c.notifyChange()
	return nil
}

// AddRemoteTrack adds a single remote track into the SQLite library AND mirrors two-way to remote favorites.
func (c *Catalog) AddRemoteTrack(ctx context.Context, provider, externalID string) (Track, error) {
	p := strings.ToLower(strings.TrimSpace(provider))
	extID := strings.TrimPrefix(strings.TrimSpace(externalID), p+":")
	if extID == "" {
		return Track{}, fmt.Errorf("%w: externalId is required", ErrInvalidArgument)
	}
	src, ok := c.remoteSource(p)
	if !ok {
		return Track{}, fmt.Errorf("%w: %s", ErrProviderNotFound, provider)
	}
	if !src.IsConnected() {
		return Track{}, fmt.Errorf("%w: %s", ErrProviderOffline, provider)
	}

	tr, err := src.GetTrack(ctx, extID)
	if err != nil {
		return Track{}, err
	}
	if err = src.AddFavorite(ctx, "track", extID); err != nil {
		return Track{}, err
	}

	albID := tr.AlbumID
	if albID == "" {
		albID = p + ":track-album:" + extID
	}
	normalizeRemoteTrack(p, albID, &tr)
	artist := tr.AlbumArtist
	if artist == "" {
		artist = tr.Artist
	}
	alb := Album{
		ID:            albID,
		Source:        Source(p),
		SourceType:    SourceTypeRemote,
		Provider:      p,
		ExternalID:    strings.TrimPrefix(albID, p+":"),
		Title:         tr.Album,
		Artist:        artist,
		Genre:         tr.Genre,
		Year:          tr.Year,
		ArtworkURL:    tr.ArtworkURL,
		PrimaryFormat: FormatNameFromMIMEOrExt(tr.MIME, tr.Ext),
		MaxBit:        tr.BitDepth,
		MaxRate:       tr.SampleRate,
		IsPartial:     true,
		Available:     true,
		InLibrary:     true,
	}
	if err = c.store.upsertAlbumWithTracks(ctx, alb, []Track{tr}); err != nil {
		return Track{}, err
	}
	c.notifyChange()
	saved, _, err := c.store.getTrack(ctx, tr.ID)
	return saved, err
}

// RemoveRemoteTrack removes a remote track from SQLite AND unfavorites it two-way.
func (c *Catalog) RemoveRemoteTrack(ctx context.Context, trackID string) error {
	tr, ok, err := c.store.getTrack(ctx, trackID)
	if err != nil {
		return err
	}
	if !ok {
		return ErrNotFound
	}
	if tr.SourceType != SourceTypeRemote {
		return fmt.Errorf("%w: cannot remove local track via remote endpoint", ErrInvalidArgument)
	}
	if src, exists := c.remoteSource(tr.Provider); exists && src.IsConnected() {
		if err := src.RemoveFavorite(ctx, "track", tr.ExternalID); err != nil {
			return err
		}
	}
	if err := c.store.deleteTrack(ctx, trackID); err != nil {
		return err
	}
	c.notifyChange()
	return nil
}

// QueryAlbums runs a filtered, sorted, paginated album query across all sources.
func (c *Catalog) QueryAlbums(f AlbumFilter) ([]Album, int, error) {
	return c.store.queryAlbums(context.Background(), f)
}

// QueryTracks runs a filtered, sorted, paginated track query across all sources.
func (c *Catalog) QueryTracks(f TrackFilter) ([]Track, int, error) {
	return c.store.queryTracks(context.Background(), f)
}

// ListArtists returns unified album artists across local and remote sources.
func (c *Catalog) ListArtists(f FacetFilter) ([]ArtistSummary, int, error) {
	return c.store.listArtists(context.Background(), f)
}

// ListGenres returns distinct genres for the classic iTunes column browser.
func (c *Catalog) ListGenres(f FacetFilter) ([]GenreSummary, error) {
	return c.store.listGenres(context.Background(), f)
}

// HasAlbum reports whether an album ID is currently in the SQLite library.
func (c *Catalog) HasAlbum(id string) bool {
	return c.store.hasAlbum(context.Background(), id)
}

// SetPrimaryVersion pins an album as the Primary Version of its version group.
func (c *Catalog) SetPrimaryVersion(albumID string) (Album, error) {
	if err := c.store.setPrimaryVersion(context.Background(), albumID); err != nil {
		return Album{}, err
	}
	c.notifyChange()
	a, _, err := c.store.getAlbum(context.Background(), albumID)
	return a, err
}

// GroupVersions manually groups two or more albums as alternate editions.
func (c *Catalog) GroupVersions(albumIDs []string) (string, error) {
	gid, err := c.store.groupVersions(context.Background(), albumIDs)
	if err != nil {
		return "", err
	}
	c.notifyChange()
	return gid, nil
}

// UngroupVersion excludes an album from version grouping so it displays independently.
func (c *Catalog) UngroupVersion(albumID string) (Album, error) {
	if err := c.store.ungroupVersion(context.Background(), albumID); err != nil {
		return Album{}, err
	}
	c.notifyChange()
	a, _, err := c.store.getAlbum(context.Background(), albumID)
	return a, err
}

// RecordPlay increments play_count and records a play_event for Heavy Rotation.
func (c *Catalog) RecordPlay(track Track) {
	if err := c.store.recordPlay(context.Background(), track); err != nil {
		log.Printf("library record play %s: %v", track.ID, err)
	}
}

// HeavyRotation returns the most-played library albums over the last N days (Rdio-style).
func (c *Catalog) HeavyRotation(days, limit int) ([]HeavyRotationAlbum, error) {
	return c.store.heavyRotation(context.Background(), days, limit)
}

// Tracks returns all indexed available tracks across sources.
func (c *Catalog) Tracks() []Track {
	items, _, err := c.store.queryTracks(context.Background(), TrackFilter{Limit: 100000})
	if err != nil {
		return make([]Track, 0)
	}
	return items
}

// Track looks up a single track by ID.
func (c *Catalog) Track(id string) (Track, bool) {
	t, ok, err := c.store.getTrack(context.Background(), id)
	if err != nil {
		return Track{}, false
	}
	return t, ok
}

// Albums returns summary info for all available albums.
func (c *Catalog) Albums() []Album {
	items, _, err := c.store.queryAlbums(context.Background(), AlbumFilter{Limit: 100000})
	if err != nil {
		return make([]Album, 0)
	}
	return items
}

// Album returns a single album including its ordered tracks and sibling versions.
func (c *Catalog) Album(id string) (Album, bool) {
	a, ok, err := c.store.getAlbum(context.Background(), id)
	if err != nil {
		return Album{}, false
	}
	return a, ok
}

// AlbumsPage filters, sorts, and slices albums with total count for pagination.
func (c *Catalog) AlbumsPage(query, sortBy string, limit, offset int) ([]Album, int) {
	items, total, err := c.store.queryAlbums(context.Background(), AlbumFilter{
		Query:  query,
		Sort:   sortBy,
		Limit:  limit,
		Offset: offset,
	})
	if err != nil {
		return make([]Album, 0), 0
	}
	return items, total
}

// TracksPage filters and slices tracks with total count for pagination.
func (c *Catalog) TracksPage(query string, limit, offset int) ([]Track, int) {
	items, total, err := c.store.queryTracks(context.Background(), TrackFilter{
		Query:  query,
		Limit:  limit,
		Offset: offset,
	})
	if err != nil {
		return make([]Track, 0), 0
	}
	return items, total
}

// Search performs a full-text search across title, artist, album, and genre.
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
	if t.HasEmbedArt && t.FilePath != "" {
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

func normalizeRemoteAlbum(provider string, a *Album) {
	a.Source = Source(provider)
	a.SourceType = SourceTypeRemote
	a.Provider = provider
	if a.ExternalID == "" {
		a.ExternalID = strings.TrimPrefix(a.ID, provider+":")
	}
	if a.ID == "" {
		a.ID = provider + ":" + a.ExternalID
	}
	if a.PrimaryFormat == "" && len(a.Tracks) > 0 {
		a.PrimaryFormat = FormatNameFromMIMEOrExt(a.Tracks[0].MIME, a.Tracks[0].Ext)
	}
	if a.PrimaryFormat == "" {
		a.PrimaryFormat = "FLAC"
	}
	a.Available = true
	a.InLibrary = true
}

func normalizeRemoteTrack(provider, albumID string, t *Track) {
	t.Source = Source(provider)
	t.SourceType = SourceTypeRemote
	t.Provider = provider
	if t.ExternalID == "" {
		t.ExternalID = strings.TrimPrefix(t.ID, provider+":")
	}
	if t.ID == "" {
		t.ID = provider + ":" + t.ExternalID
	}
	if t.AlbumID == "" {
		t.AlbumID = albumID
	}
	t.Available = true
	t.InLibrary = true
}

func scanFileWithArt(path string, fi fs.FileInfo, lookupDirArt func(string) string) (Track, bool) {
	ext := strings.ToLower(filepath.Ext(path))
	mimeType, ok := MIMEForExt(ext)
	if !ok {
		return Track{}, false
	}

	// Hash the full cleaned path so identically named files across multiple
	// configured roots never collide.
	id := "local:" + shortHash("local:"+filepath.Clean(path))
	dir := filepath.Dir(path)
	folderArt := lookupDirArt(dir)

	props := ProbeAudio(path)
	baseName := strings.TrimSuffix(filepath.Base(path), filepath.Ext(path))
	mtime := fi.ModTime().UTC()

	t := Track{
		ID:          id,
		Source:      SourceLocal,
		SourceType:  SourceTypeLocal,
		Provider:    string(SourceLocal),
		ExternalID:  filepath.Clean(path),
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
		FileMtimeNS: fi.ModTime().UnixNano(),
		FolderArt:   folderArt,
		AddedAt:     &mtime,
		Available:   true,
		InLibrary:   true,
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
			if s := strings.TrimSpace(m.Genre()); s != "" {
				t.Genre = s
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

func shortHash(s string) string {
	sum := sha256.Sum256([]byte(s))
	return hex.EncodeToString(sum[:8])
}
