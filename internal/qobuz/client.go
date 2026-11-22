package qobuz

import (
	"context"
	"crypto/md5"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"html"
	"io"
	"log"
	"math"
	"net/http"
	"net/url"
	"regexp"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"tacet/internal/cache"
	"tacet/internal/library"
)

// Qobuz API endpoints and audio format_id values.
const (
	DefaultBaseURL = "https://www.qobuz.com/api.json/0.2"
	webPlayerLogin = "https://play.qobuz.com/login"

	FormatMP3320   = 5
	FormatFLAC1644 = 6
	FormatFLAC2496 = 7
	FormatFLAC192  = 27
)

// Domain sentinel errors returned by Qobuz client operations.
var (
	ErrUnauthenticated = errors.New("qobuz is not authenticated")
	ErrNotFound        = errors.New("qobuz resource not found")
	ErrInvalidQuality  = errors.New("invalid qobuz max quality (use mp3_320, flac_16_44, flac_24_96, or flac_24_192)")
)

// QualityLabel converts a Qobuz format_id to its canonical quality string.
func QualityLabel(formatID int) string {
	switch formatID {
	case FormatMP3320:
		return "mp3_320"
	case FormatFLAC1644:
		return "flac_16_44"
	case FormatFLAC2496:
		return "flac_24_96"
	default:
		return "flac_24_192"
	}
}

// ParseQuality converts a canonical quality string to its Qobuz format_id.
func ParseQuality(q string) (int, error) {
	switch strings.ToLower(strings.TrimSpace(q)) {
	case "mp3_320":
		return FormatMP3320, nil
	case "flac_16_44":
		return FormatFLAC1644, nil
	case "flac_24_96":
		return FormatFLAC2496, nil
	case "flac_24_192":
		return FormatFLAC192, nil
	default:
		return 0, ErrInvalidQuality
	}
}

// Config configures a Qobuz API client.
type Config struct {
	BaseURL   string
	AppID     string
	AppSecret string
	AuthToken string
	FormatID  int          // Defaults to FormatFLAC192 (27)
	Cache     *cache.Cache // Optional shared SQLite KV cache (defaults to an in-memory SQLite cache)
}

// Status reports current Qobuz authentication state.
type Status struct {
	Configured    bool   `json:"configured"`
	Authenticated bool   `json:"authenticated"`
	UserName      string `json:"userName,omitempty"`
	Subscription  string `json:"subscription,omitempty"`
	MaxFormatID   int    `json:"maxFormatId"`
	MaxQuality    string `json:"maxQuality"`
}

// StreamInfo is the resolved Qobuz CDN stream URL and audio properties.
type StreamInfo struct {
	URL        string
	MIME       string
	SampleRate int
	BitDepth   int
	Duration   time.Duration
	FormatID   int
	ExpiresAt  time.Time
}

// Artist represents a Qobuz artist match or profile.
type Artist struct {
	ID          int64  `json:"id"`
	Name        string `json:"name"`
	ArtworkURL  string `json:"artworkUrl,omitempty"`
	AlbumsCount int    `json:"albumsCount,omitempty"`
	Biography   string `json:"biography,omitempty"`
}

// ArtistDetail pairs a Qobuz artist profile with their remote discography, popular tracks, and similar artists.
type ArtistDetail struct {
	Artist         Artist          `json:"artist"`
	Albums         []library.Album `json:"albums"`
	Tracks         []library.Track `json:"tracks,omitempty"`
	SimilarArtists []Artist        `json:"similarArtists,omitempty"`
}

// SearchResult holds Qobuz search matches mapped to unified library types.
type SearchResult struct {
	Artists []Artist        `json:"artists"`
	Tracks  []library.Track `json:"tracks"`
	Albums  []library.Album `json:"albums"`
}

// Client talks to the Qobuz v0.2 REST API.
type Client struct {
	mu           sync.RWMutex
	baseURL      string
	http         *http.Client
	appID        string
	appSecret    string
	privateKey   string
	altSecrets   []string
	authToken    string
	userName     string
	subscription string
	formatID     int
	urlCache     map[int64]StreamInfo
	trackCache   map[int64]library.Track
	kv           *cache.Namespaced
	ownedCache   *cache.Cache
	authGen      atomic.Uint64
	prewarmMu    sync.Mutex
	prewarmStop  context.CancelFunc
	prewarmDone  bool
	prewarmWG    sync.WaitGroup
}

// NewClient initializes a Qobuz client.
func NewClient(cfg Config) *Client {
	base := strings.TrimRight(strings.TrimSpace(cfg.BaseURL), "/")
	if base == "" {
		base = DefaultBaseURL
	}
	fmtID := cfg.FormatID
	if fmtID == 0 {
		fmtID = FormatFLAC192
	}
	backing := cfg.Cache
	var owned *cache.Cache
	if backing == nil {
		if mem, err := cache.Open(":memory:"); err == nil {
			backing = mem
			owned = mem
		}
	}
	var kv *cache.Namespaced
	if backing != nil {
		kv = backing.Namespace("qobuz")
	}
	return &Client{
		baseURL:    base,
		http:       &http.Client{Timeout: 15 * time.Second},
		appID:      strings.TrimSpace(cfg.AppID),
		appSecret:  strings.TrimSpace(cfg.AppSecret),
		authToken:  strings.TrimSpace(cfg.AuthToken),
		formatID:   fmtID,
		urlCache:   make(map[int64]StreamInfo),
		trackCache: make(map[int64]library.Track),
		kv:         kv,
		ownedCache: owned,
	}
}

// Close stops background prewarming and closes any client-owned cache.
func (c *Client) Close() error {
	c.prewarmMu.Lock()
	c.prewarmDone = true
	stop := c.prewarmStop
	c.prewarmStop = nil
	c.prewarmMu.Unlock()
	if stop != nil {
		stop()
	}
	c.prewarmWG.Wait()
	if c.ownedCache != nil {
		return c.ownedCache.Close()
	}
	return nil
}

// CacheStats returns the number of cached albums, artists, artist names, and searches.
func (c *Client) CacheStats() (albums, artists, artistNames, searches int) {
	if c.kv == nil {
		return 0, 0, 0, 0
	}
	if ks, err := c.kv.Keys("album:*"); err == nil {
		albums = len(ks)
	}
	if ks, err := c.kv.Keys("artist:*"); err == nil {
		artists = len(ks)
	}
	if ks, err := c.kv.Keys("artist-name:*"); err == nil {
		artistNames = len(ks)
	}
	if ks, err := c.kv.Keys("search:*"); err == nil {
		searches = len(ks)
	}
	return albums, artists, artistNames, searches
}

// Status returns the current Qobuz auth/config state.
func (c *Client) Status() Status {
	c.mu.RLock()
	defer c.mu.RUnlock()
	return Status{
		Configured:    c.appID != "" && c.appSecret != "",
		Authenticated: c.appID != "" && c.authToken != "",
		UserName:      c.userName,
		Subscription:  c.subscription,
		MaxFormatID:   c.formatID,
		MaxQuality:    QualityLabel(c.formatID),
	}
}

// MaxQuality returns the configured maximum streaming quality label.
func (c *Client) MaxQuality() string {
	c.mu.RLock()
	defer c.mu.RUnlock()
	return QualityLabel(c.formatID)
}

// SetMaxQuality updates the maximum Qobuz streaming format_id and clears cached stream URLs.
func (c *Client) SetMaxQuality(quality string) error {
	fmtID, err := ParseQuality(quality)
	if err != nil {
		return err
	}
	c.mu.Lock()
	c.formatID = fmtID
	clear(c.urlCache)
	c.mu.Unlock()
	return nil
}

// Logout clears the active user session and cached stream/catalog entries.
func (c *Client) Logout() Status {
	c.authGen.Add(1)
	c.mu.Lock()
	c.authToken = ""
	c.userName = ""
	c.subscription = ""
	clear(c.urlCache)
	clear(c.trackCache)
	st := Status{
		Configured:    c.appID != "" && c.appSecret != "",
		Authenticated: false,
		MaxFormatID:   c.formatID,
		MaxQuality:    QualityLabel(c.formatID),
	}
	c.mu.Unlock()
	if c.kv != nil {
		_ = c.kv.Clear()
	}
	return st
}

// PersistedCredentials captures the Qobuz session state stored in SQLite
// so restarts keep the user signed in without re-running browser OAuth.
type PersistedCredentials struct {
	AppID        string   `json:"appId,omitempty"`
	AppSecret    string   `json:"appSecret,omitempty"`
	AltSecrets   []string `json:"altSecrets,omitempty"`
	AuthToken    string   `json:"authToken,omitempty"`
	UserName     string   `json:"userName,omitempty"`
	Subscription string   `json:"subscription,omitempty"`
}

// ExportCredentials returns a snapshot of the active Qobuz credentials for persistence.
func (c *Client) ExportCredentials() PersistedCredentials {
	c.mu.RLock()
	defer c.mu.RUnlock()
	return PersistedCredentials{
		AppID:        c.appID,
		AppSecret:    c.appSecret,
		AltSecrets:   append([]string(nil), c.altSecrets...),
		AuthToken:    c.authToken,
		UserName:     c.userName,
		Subscription: c.subscription,
	}
}

// RestoreCredentials loads previously persisted Qobuz credentials into any fields
// not already overridden by explicit environment variables.
func (c *Client) RestoreCredentials(creds PersistedCredentials) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.appID == "" && strings.TrimSpace(creds.AppID) != "" {
		c.appID = strings.TrimSpace(creds.AppID)
	}
	if c.appSecret == "" && strings.TrimSpace(creds.AppSecret) != "" {
		c.appSecret = strings.TrimSpace(creds.AppSecret)
	}
	if len(c.altSecrets) == 0 && len(creds.AltSecrets) > 0 {
		c.altSecrets = append([]string(nil), creds.AltSecrets...)
	}
	if c.authToken == "" && strings.TrimSpace(creds.AuthToken) != "" {
		c.authToken = strings.TrimSpace(creds.AuthToken)
		c.userName = strings.TrimSpace(creds.UserName)
		c.subscription = strings.TrimSpace(creds.Subscription)
	}
	clear(c.urlCache)
}

// SetCredentials updates explicit app credentials and/or user auth token.
func (c *Client) SetCredentials(appID, appSecret, authToken string) {
	c.mu.Lock()
	if strings.TrimSpace(appID) != "" {
		c.appID = strings.TrimSpace(appID)
	}
	if strings.TrimSpace(appSecret) != "" {
		c.appSecret = strings.TrimSpace(appSecret)
	}
	newTok := strings.TrimSpace(authToken)
	switchedAccount := newTok != "" && c.authToken != "" && c.authToken != newTok
	if newTok != "" {
		c.authToken = newTok
	}
	clear(c.urlCache)
	if switchedAccount {
		clear(c.trackCache)
	}
	c.mu.Unlock()
	if switchedAccount {
		c.authGen.Add(1)
		if c.kv != nil {
			_ = c.kv.Clear()
		}
	}
}

// Search searches Qobuz artists, albums, and tracks in a single catalog/search call.
func (c *Client) Search(ctx context.Context, query string, limit int, offset ...int) (SearchResult, error) {
	empty := SearchResult{
		Artists: make([]Artist, 0),
		Tracks:  make([]library.Track, 0),
		Albums:  make([]library.Album, 0),
	}
	query = strings.TrimSpace(query)
	if query == "" {
		return empty, nil
	}
	if limit <= 0 || limit > 100 {
		limit = 25
	}
	off := 0
	if len(offset) > 0 && offset[0] > 0 {
		off = offset[0]
	}
	if err := c.requireAuth(); err != nil {
		return empty, err
	}
	if cached, ok := c.getCachedSearch(query, limit, off); ok {
		c.cacheTracks(cached.Tracks)
		return cached, nil
	}

	gen := c.authGen.Load()
	params := url.Values{
		"query":  {query},
		"limit":  {strconv.Itoa(limit)},
		"offset": {strconv.Itoa(off)},
	}

	var resp struct {
		Artists struct {
			Items []qArtist `json:"items"`
		} `json:"artists"`
		Tracks struct {
			Items []qTrack `json:"items"`
		} `json:"tracks"`
		Albums struct {
			Items []qAlbum `json:"items"`
		} `json:"albums"`
	}
	if err := c.getJSON(ctx, "/catalog/search", params, true, &resp); err != nil {
		return empty, err
	}

	res := SearchResult{
		Artists: make([]Artist, 0, len(resp.Artists.Items)),
		Tracks:  make([]library.Track, 0, len(resp.Tracks.Items)),
		Albums:  make([]library.Album, 0, len(resp.Albums.Items)),
	}
	for _, qa := range resp.Artists.Items {
		if qa.ID <= 0 || strings.TrimSpace(qa.Name) == "" {
			continue
		}
		art := qa.toArtist()
		res.Artists = append(res.Artists, art)
	}
	for _, qt := range resp.Tracks.Items {
		res.Tracks = append(res.Tracks, qt.toLibraryTrack(nil))
	}
	for _, qa := range resp.Albums.Items {
		if qa.Streamable != nil && !*qa.Streamable {
			continue
		}
		res.Albums = append(res.Albums, qa.toLibraryAlbum(false))
	}
	if c.authGen.Load() == gen {
		for _, art := range res.Artists {
			c.cacheArtistName(art.Name, art.ID, false)
		}
		c.cacheTracks(res.Tracks)
		c.putCachedSearch(query, limit, off, res)
	}
	return res, nil
}

// GetArtist fetches a Qobuz artist profile and their remote discography.
func (c *Client) GetArtist(ctx context.Context, artistID int64, limit int) (ArtistDetail, error) {
	if err := c.requireAuth(); err != nil {
		return ArtistDetail{}, err
	}
	if artistID <= 0 {
		return ArtistDetail{}, fmt.Errorf("%w: invalid artist id", ErrNotFound)
	}
	if limit <= 0 || limit > 200 {
		limit = 50
	}
	if cached, ok := c.getCachedArtist(artistID, limit); ok {
		return cached, nil
	}
	gen := c.authGen.Load()
	params := url.Values{
		"artist_id": {strconv.FormatInt(artistID, 10)},
		"extra":     {"albums"},
		"limit":     {strconv.Itoa(limit)},
		"offset":    {"0"},
	}
	var resp struct {
		qArtist
		Albums struct {
			Items []qAlbum `json:"items"`
		} `json:"albums"`
	}

	// Fetch /artist/page (popular top_tracks, portrait, similar_artists) concurrently
	// with /artist/get so cold-cache artist loads pay one round-trip and eliminate
	// the separate /artist/getSimilarArtists request.
	pageCh := make(chan artistPageExtras, 1)
	go func() {
		pageCh <- c.fetchArtistPageExtras(ctx, artistID)
	}()

	if err := c.getJSON(ctx, "/artist/get", params, true, &resp); err != nil {
		var ae *apiError
		if errors.As(err, &ae) && ae.status == http.StatusNotFound {
			return ArtistDetail{}, fmt.Errorf("%w: artist %d", ErrNotFound, artistID)
		}
		return ArtistDetail{}, err
	}
	art := resp.toArtist()
	if art.ID == 0 {
		art.ID = artistID
	}
	albums := make([]library.Album, 0, len(resp.Albums.Items))
	wantArtistKey := normMatchKey(art.Name)
	for _, qa := range resp.Albums.Items {
		if qa.Streamable != nil && !*qa.Streamable {
			continue
		}
		if !isPrimaryArtistAlbumOrEP(qa, art.ID, wantArtistKey) {
			continue
		}
		alb := qa.toLibraryAlbum(false)
		if alb.Artist == "" {
			alb.Artist = art.Name
		}
		albums = append(albums, alb)
	}
	pageRes := <-pageCh
	tracks := pageRes.tracks
	for i := range tracks {
		if tracks[i].Artist == "" {
			tracks[i].Artist = art.Name
		}
		if tracks[i].AlbumArtist == "" {
			tracks[i].AlbumArtist = art.Name
		}
	}
	c.cacheTracks(tracks)
	if art.ArtworkURL == "" && pageRes.portraitURL != "" {
		art.ArtworkURL = pageRes.portraitURL
	}
	if art.ArtworkURL == "" && len(albums) > 0 {
		art.ArtworkURL = albums[0].ArtworkURL
	}
	detail := ArtistDetail{
		Artist:         art,
		Albums:         albums,
		Tracks:         tracks,
		SimilarArtists: pageRes.similarArtists,
	}
	if c.authGen.Load() == gen {
		if art.Name != "" {
			c.cacheArtistName(art.Name, art.ID, true)
		}
		if pageRes.ok {
			c.putCachedArtist(artistID, limit, detail)
		}
	}
	return detail, nil
}

type artistPageExtras struct {
	tracks         []library.Track
	portraitURL    string
	similarArtists []Artist
	ok             bool
}

type qArtistPortrait struct {
	Hash   string `json:"hash"`
	Format string `json:"format"`
}

func (p *qArtistPortrait) url() string {
	if p == nil {
		return ""
	}
	h := strings.TrimSpace(p.Hash)
	if h == "" {
		return ""
	}
	ext := strings.TrimSpace(p.Format)
	if ext == "" {
		ext = "jpg"
	}
	return fmt.Sprintf("https://static.qobuz.com/images/artists/covers/large/%s.%s", h, ext)
}

// fetchArtistPageExtras queries Qobuz's /artist/page endpoint for curated popular
// tracks, artist portrait, and similar artists in a single request. Returns ok=false
// on transient errors so callers avoid caching a degraded artist profile.
func (c *Client) fetchArtistPageExtras(ctx context.Context, artistID int64) artistPageExtras {
	if artistID <= 0 {
		return artistPageExtras{ok: true}
	}
	params := url.Values{
		"artist_id": {strconv.FormatInt(artistID, 10)},
		"sort":      {"relevant"},
	}
	var page struct {
		Images struct {
			Portrait *qArtistPortrait `json:"portrait"`
		} `json:"images"`
		SimilarArtists struct {
			Items []struct {
				ID   int64 `json:"id"`
				Name struct {
					Display string `json:"display"`
				} `json:"name"`
				Images struct {
					Portrait *qArtistPortrait `json:"portrait"`
				} `json:"images"`
			} `json:"items"`
		} `json:"similar_artists"`
		TopTracks []struct {
			ID        int64  `json:"id"`
			Title     string `json:"title"`
			Version   string `json:"version"`
			Duration  int    `json:"duration"`
			AudioInfo struct {
				MaximumBitDepth     int     `json:"maximum_bit_depth"`
				MaximumSamplingRate float64 `json:"maximum_sampling_rate"`
			} `json:"audio_info"`
			Rights struct {
				Streamable *bool `json:"streamable"`
			} `json:"rights"`
			Artist struct {
				Name struct {
					Display string `json:"display"`
				} `json:"name"`
			} `json:"artist"`
			Album struct {
				ID    string `json:"id"`
				Title string `json:"title"`
				Image qImage `json:"image"`
			} `json:"album"`
		} `json:"top_tracks"`
	}
	if err := c.getJSON(ctx, "/artist/page", params, true, &page); err != nil {
		var ae *apiError
		if errors.As(err, &ae) && ae.status == http.StatusNotFound {
			return artistPageExtras{ok: true}
		}
		return artistPageExtras{ok: false}
	}
	const maxPopularTracks = 10
	out := make([]library.Track, 0, min(len(page.TopTracks), maxPopularTracks))
	for _, pt := range page.TopTracks {
		if len(out) >= maxPopularTracks {
			break
		}
		if pt.ID <= 0 || (pt.Rights.Streamable != nil && !*pt.Rights.Streamable) {
			continue
		}
		title := strings.TrimSpace(pt.Title)
		if pt.Version != "" && !strings.Contains(strings.ToLower(title), strings.ToLower(pt.Version)) {
			title = title + " (" + pt.Version + ")"
		}
		artistName := strings.TrimSpace(pt.Artist.Name.Display)
		albumID := ""
		if pt.Album.ID != "" {
			albumID = "qobuz:" + pt.Album.ID
		}
		out = append(out, library.Track{
			ID:           fmt.Sprintf("qobuz:%d", pt.ID),
			Source:       library.SourceQobuz,
			SourceType:   library.SourceTypeRemote,
			Provider:     string(library.SourceQobuz),
			ExternalID:   strconv.FormatInt(pt.ID, 10),
			QobuzTrackID: pt.ID,
			AlbumID:      albumID,
			Title:        title,
			Artist:       artistName,
			AlbumArtist:  artistName,
			Album:        strings.TrimSpace(pt.Album.Title),
			DurationSec:  pt.Duration,
			ArtworkURL:   pt.Album.Image.best(),
			Channels:     2,
			MIME:         "audio/flac",
			Ext:          ".flac",
			BitDepth:     pt.AudioInfo.MaximumBitDepth,
			SampleRate:   normalizeSampleRateHz(pt.AudioInfo.MaximumSamplingRate),
			Available:    true,
		})
	}

	similar := make([]Artist, 0, len(page.SimilarArtists.Items))
	for _, sa := range page.SimilarArtists.Items {
		name := strings.TrimSpace(sa.Name.Display)
		if sa.ID <= 0 || name == "" {
			continue
		}
		similar = append(similar, Artist{
			ID:         sa.ID,
			Name:       name,
			ArtworkURL: sa.Images.Portrait.url(),
		})
	}

	return artistPageExtras{
		tracks:         out,
		portraitURL:    page.Images.Portrait.url(),
		similarArtists: similar,
		ok:             true,
	}
}

// AlbumLookup identifies a local album to match against Qobuz editorial metadata.
type AlbumLookup struct {
	Artist string
	Title  string
}

// MatchAlbum resolves a local (artist, title) pair to its Qobuz album and returns
// the cached Qobuz album metadata (review, label, copyright, ID). Results and
// negative misses are cached in SQLite so repeated views are instant.
func (c *Client) MatchAlbum(ctx context.Context, artist, title string) (library.Album, error) {
	if err := c.requireAuth(); err != nil {
		return library.Album{}, err
	}
	artist = strings.TrimSpace(artist)
	title = strings.TrimSpace(title)
	if artist == "" || title == "" {
		return library.Album{}, ErrNotFound
	}
	if cachedID, ok := c.getCachedAlbumMatch(artist, title); ok {
		if cachedID == "" {
			return library.Album{}, ErrNotFound
		}
		return c.GetAlbum(ctx, cachedID)
	}

	gen := c.authGen.Load()
	wantTitle := normMatchKey(title)
	wantArtist := normMatchKey(artist)
	var matchedID string

	// 1. Check the artist's cached/fetched Qobuz discography first (usually already warm).
	if detail, err := c.GetArtistByName(ctx, artist, 50); err == nil {
		matchedID = pickMatchingAlbumID(detail.Albums, wantArtist, wantTitle, false)
	}

	// 2. Fall back to /catalog/search if not found in the artist's top 50 albums.
	if matchedID == "" {
		if res, err := c.Search(ctx, artist+" "+title, 10); err == nil {
			matchedID = pickMatchingAlbumID(res.Albums, wantArtist, wantTitle, true)
		}
	}

	if c.authGen.Load() == gen {
		c.putCachedAlbumMatch(artist, title, matchedID)
	}
	if matchedID == "" {
		return library.Album{}, ErrNotFound
	}
	return c.GetAlbum(ctx, matchedID)
}

func pickMatchingAlbumID(albums []library.Album, wantArtist, wantTitle string, requireArtistMatch bool) string {
	var fallbackID string
	for _, a := range albums {
		if requireArtistMatch && wantArtist != "" && normMatchKey(a.Artist) != wantArtist {
			continue
		}
		rawID := strings.TrimPrefix(strings.TrimSpace(a.ID), "qobuz:")
		if rawID == "" {
			continue
		}
		candTitle := normMatchKey(a.Title)
		if candTitle == wantTitle {
			return rawID
		}
		// Secondary match: base title before parenthetical edition suffix (e.g. "19 (Deluxe Edition)").
		if fallbackID == "" {
			if idx := strings.IndexAny(a.Title, "(["); idx > 0 {
				if normMatchKey(a.Title[:idx]) == wantTitle {
					fallbackID = rawID
				}
			}
		}
	}
	return fallbackID
}

// PrewarmLibrary gently pre-caches Qobuz artist profiles (bio, popular tracks,
// remote discography) and local album editorial matches in SQLite so navigation
// is instant with zero layout shift.
func (c *Client) PrewarmLibrary(ctx context.Context, artistNames []string, localAlbums []AlbumLookup) {
	if !c.IsConnected() || (len(artistNames) == 0 && len(localAlbums) == 0) {
		return
	}
	const defaultLimit = 50
	const maxPerPass = 250

	seenArtists := make(map[string]struct{}, len(artistNames))
	pendingArtists := make([]string, 0, len(artistNames))
	for _, raw := range artistNames {
		name := strings.TrimSpace(raw)
		if name == "" {
			continue
		}
		key := normMatchKey(name)
		if _, dup := seenArtists[key]; dup {
			continue
		}
		seenArtists[key] = struct{}{}
		if cachedID, ok := c.getCachedArtistName(name); ok {
			if cachedID == 0 {
				continue // negative cache hit
			}
			if _, ok := c.getCachedArtist(cachedID, defaultLimit); ok {
				continue // already warm
			}
		}
		pendingArtists = append(pendingArtists, name)
		if len(pendingArtists) >= maxPerPass {
			break
		}
	}

	seenAlbums := make(map[string]struct{}, len(localAlbums))
	pendingAlbums := make([]AlbumLookup, 0, len(localAlbums))
	for _, a := range localAlbums {
		artist := strings.TrimSpace(a.Artist)
		title := strings.TrimSpace(a.Title)
		if artist == "" || title == "" {
			continue
		}
		key := normMatchKey(artist) + "|" + normMatchKey(title)
		if _, dup := seenAlbums[key]; dup {
			continue
		}
		seenAlbums[key] = struct{}{}
		if cachedID, ok := c.getCachedAlbumMatch(artist, title); ok {
			if cachedID == "" {
				continue // negative cache hit
			}
			if _, ok := c.getCachedAlbum(cachedID); ok {
				continue // already warm
			}
		}
		pendingAlbums = append(pendingAlbums, AlbumLookup{Artist: artist, Title: title})
		if len(pendingAlbums) >= maxPerPass {
			break
		}
	}

	if len(pendingArtists) == 0 && len(pendingAlbums) == 0 {
		return
	}

	c.prewarmMu.Lock()
	if c.prewarmDone {
		c.prewarmMu.Unlock()
		return
	}
	prev := c.prewarmStop
	runCtx, cancel := context.WithCancel(ctx)
	c.prewarmStop = cancel
	c.prewarmWG.Add(1)
	c.prewarmMu.Unlock()

	if prev != nil {
		prev()
	}

	go func() {
		defer c.prewarmWG.Done()
		defer cancel()
		start := time.Now()

		// 1. Warm artists first so local album matching can reuse their cached discographies.
		var warmedArtists atomic.Int64
		if len(pendingArtists) > 0 {
			jobs := make(chan string, len(pendingArtists))
			for _, n := range pendingArtists {
				jobs <- n
			}
			close(jobs)
			var wg sync.WaitGroup
			for range min(2, len(pendingArtists)) {
				wg.Add(1)
				go func() {
					defer wg.Done()
					for name := range jobs {
						if runCtx.Err() != nil || !c.IsConnected() {
							return
						}
						reqCtx, reqCancel := context.WithTimeout(runCtx, 10*time.Second)
						_, err := c.GetArtistByName(reqCtx, name, defaultLimit)
						reqCancel()
						if err == nil {
							warmedArtists.Add(1)
						}
						select {
						case <-runCtx.Done():
							return
						case <-time.After(120 * time.Millisecond):
						}
					}
				}()
			}
			wg.Wait()
		}

		// 2. Warm local album editorial matches.
		var warmedAlbums atomic.Int64
		if len(pendingAlbums) > 0 && runCtx.Err() == nil && c.IsConnected() {
			jobs := make(chan AlbumLookup, len(pendingAlbums))
			for _, a := range pendingAlbums {
				jobs <- a
			}
			close(jobs)
			var wg sync.WaitGroup
			for range min(2, len(pendingAlbums)) {
				wg.Add(1)
				go func() {
					defer wg.Done()
					for a := range jobs {
						if runCtx.Err() != nil || !c.IsConnected() {
							return
						}
						reqCtx, reqCancel := context.WithTimeout(runCtx, 10*time.Second)
						_, err := c.MatchAlbum(reqCtx, a.Artist, a.Title)
						reqCancel()
						if err == nil {
							warmedAlbums.Add(1)
						}
						select {
						case <-runCtx.Done():
							return
						case <-time.After(120 * time.Millisecond):
						}
					}
				}()
			}
			wg.Wait()
		}

		if a, al := warmedArtists.Load(), warmedAlbums.Load(); a > 0 || al > 0 {
			log.Printf("prewarmed Qobuz SQLite cache (%d artists, %d local albums) in %s", a, al, time.Since(start).Round(time.Millisecond))
		}
	}()
}

// GetArtistByName resolves an artist name via Qobuz /artist/search (exact normalized match, cached)
// and fetches their remote discography.
func (c *Client) GetArtistByName(ctx context.Context, name string, limit int) (ArtistDetail, error) {
	if err := c.requireAuth(); err != nil {
		return ArtistDetail{}, err
	}
	name = strings.TrimSpace(name)
	if name == "" {
		return ArtistDetail{}, fmt.Errorf("%w: empty artist name", ErrNotFound)
	}

	if cachedID, ok := c.getCachedArtistName(name); ok {
		if cachedID == 0 {
			return ArtistDetail{}, fmt.Errorf("%w: artist %q", ErrNotFound, name)
		}
		return c.GetArtist(ctx, cachedID, limit)
	}

	gen := c.authGen.Load()
	params := url.Values{
		"query":  {name},
		"limit":  {"10"},
		"offset": {"0"},
	}
	var resp struct {
		Artists struct {
			Items []qArtist `json:"items"`
		} `json:"artists"`
	}
	if err := c.getJSON(ctx, "/artist/search", params, true, &resp); err != nil {
		return ArtistDetail{}, err
	}

	var matchedID int64
	for _, qa := range resp.Artists.Items {
		if qa.ID <= 0 {
			continue
		}
		if strings.EqualFold(strings.TrimSpace(qa.Name), name) {
			matchedID = qa.ID
			break
		}
	}
	if matchedID == 0 {
		if c.authGen.Load() == gen {
			c.putCachedArtistName(name, 0, true)
		}
		return ArtistDetail{}, fmt.Errorf("%w: artist %q", ErrNotFound, name)
	}
	if c.authGen.Load() == gen {
		c.cacheArtistName(name, matchedID, true)
	}
	return c.GetArtist(ctx, matchedID, limit)
}

func (c *Client) cacheArtistName(name string, id int64, overwrite bool) {
	if c.kv == nil || id <= 0 {
		return
	}
	c.putCachedArtistName(name, id, overwrite)
}

// GetAlbum fetches a Qobuz album and its track listing. Accepts bare or "qobuz:"-prefixed IDs.
func (c *Client) GetAlbum(ctx context.Context, albumID string) (library.Album, error) {
	if err := c.requireAuth(); err != nil {
		return library.Album{}, err
	}
	rawID := strings.TrimPrefix(strings.TrimSpace(albumID), "qobuz:")
	if cached, ok := c.getCachedAlbum(rawID); ok {
		c.cacheTracks(cached.Tracks)
		return cached, nil
	}
	gen := c.authGen.Load()
	params := url.Values{
		"album_id": {rawID},
		"limit":    {"500"},
	}
	var qa qAlbum
	if err := c.getJSON(ctx, "/album/get", params, true, &qa); err != nil {
		var ae *apiError
		if errors.As(err, &ae) && ae.status == http.StatusNotFound {
			return library.Album{}, fmt.Errorf("%w: album %q", ErrNotFound, albumID)
		}
		return library.Album{}, err
	}
	alb := qa.toLibraryAlbum(true)
	if c.authGen.Load() == gen {
		c.cacheTracks(alb.Tracks)
		c.putCachedAlbum(rawID, alb)
	}
	return alb, nil
}

// GetTrack fetches metadata for a single Qobuz track.
func (c *Client) GetTrack(ctx context.Context, trackID int64) (library.Track, error) {
	if err := c.requireAuth(); err != nil {
		return library.Track{}, err
	}
	params := url.Values{
		"track_id": {strconv.FormatInt(trackID, 10)},
	}
	var qt qTrack
	if err := c.getJSON(ctx, "/track/get", params, true, &qt); err != nil {
		var ae *apiError
		if errors.As(err, &ae) && ae.status == http.StatusNotFound {
			return library.Track{}, fmt.Errorf("%w: track %d", ErrNotFound, trackID)
		}
		return library.Track{}, err
	}
	tr := qt.toLibraryTrack(nil)
	c.cacheTrack(tr)
	return tr, nil
}

// ResolveTrack returns cached track metadata for trackID or fetches it from Qobuz on cache miss.
func (c *Client) ResolveTrack(ctx context.Context, trackID int64) (library.Track, error) {
	if err := c.requireAuth(); err != nil {
		return library.Track{}, err
	}
	c.mu.RLock()
	tr, ok := c.trackCache[trackID]
	c.mu.RUnlock()
	if ok {
		return tr, nil
	}
	return c.GetTrack(ctx, trackID)
}

const maxTrackCacheEntries = 2048

func (c *Client) cacheTrack(tr library.Track) {
	rehydrateTrackQobuzID(&tr)
	if tr.QobuzTrackID <= 0 {
		return
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	c.evictTrackCacheLocked()
	c.trackCache[tr.QobuzTrackID] = tr
}

func (c *Client) cacheTracks(tracks []library.Track) {
	if len(tracks) == 0 {
		return
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	for i := range tracks {
		tr := tracks[i]
		rehydrateTrackQobuzID(&tr)
		if tr.QobuzTrackID <= 0 {
			continue
		}
		c.evictTrackCacheLocked()
		c.trackCache[tr.QobuzTrackID] = tr
	}
}

func (c *Client) evictTrackCacheLocked() {
	if len(c.trackCache) < maxTrackCacheEntries {
		return
	}
	// Evict roughly half the map to avoid cliff-clearing recently viewed tracks.
	target := maxTrackCacheEntries / 2
	for k := range c.trackCache {
		delete(c.trackCache, k)
		if len(c.trackCache) <= target {
			break
		}
	}
}

const maxURLCacheEntries = 128

type apiError struct {
	status  int
	message string
}

func (e *apiError) Error() string {
	return fmt.Sprintf("qobuz HTTP %d: %s", e.status, e.message)
}

func isSignatureOrAuthError(err error) bool {
	var ae *apiError
	if !errors.As(err, &ae) {
		return false
	}
	return ae.status == http.StatusBadRequest || ae.status == http.StatusUnauthorized || ae.status == http.StatusForbidden
}

// StreamURL resolves a signed Qobuz streaming CDN URL for trackID.
func (c *Client) StreamURL(ctx context.Context, trackID int64) (StreamInfo, error) {
	if err := c.requireAuth(); err != nil {
		return StreamInfo{}, err
	}

	now := time.Now()
	c.mu.Lock()
	if cached, ok := c.urlCache[trackID]; ok {
		if now.Before(cached.ExpiresAt) {
			c.mu.Unlock()
			return cached, nil
		}
		delete(c.urlCache, trackID)
	}
	formatID := c.formatID
	primarySecret := c.appSecret
	alts := append([]string(nil), c.altSecrets...)
	c.mu.Unlock()

	secrets := make([]string, 0, 1+len(alts))
	if primarySecret != "" {
		secrets = append(secrets, primarySecret)
	}
	for _, s := range alts {
		if s != "" && s != primarySecret {
			secrets = append(secrets, s)
		}
	}
	if len(secrets) == 0 {
		return StreamInfo{}, errors.New("qobuz app_secret is not configured")
	}

	var lastErr error
	primaryAuthFailed := false
	for i, secret := range secrets {
		if err := ctx.Err(); err != nil {
			return StreamInfo{}, err
		}
		info, err := c.fetchFileURL(ctx, trackID, formatID, secret)
		if err == nil {
			c.mu.Lock()
			if i == 0 || primaryAuthFailed {
				c.appSecret = secret
			}
			c.putCacheLocked(trackID, info)
			c.mu.Unlock()
			return info, nil
		}
		lastErr = err
		if !isSignatureOrAuthError(err) {
			return StreamInfo{}, err
		}
		if i == 0 {
			primaryAuthFailed = true
		}
	}
	return StreamInfo{}, lastErr
}

func (c *Client) putCacheLocked(trackID int64, info StreamInfo) {
	now := time.Now()
	for k, v := range c.urlCache {
		if !now.Before(v.ExpiresAt) {
			delete(c.urlCache, k)
		}
	}
	if len(c.urlCache) >= maxURLCacheEntries {
		// Evict oldest expiring entry to keep cache bounded.
		var oldestKey int64
		var oldestExp time.Time
		first := true
		for k, v := range c.urlCache {
			if first || v.ExpiresAt.Before(oldestExp) {
				oldestKey = k
				oldestExp = v.ExpiresAt
				first = false
			}
		}
		if !first {
			delete(c.urlCache, oldestKey)
		}
	}
	c.urlCache[trackID] = info
}

// SignFileURL computes the Qobuz MD5 request_sig for /track/getFileUrl.
func SignFileURL(trackID int64, formatID int, intent string, unixTS int64, appSecret string) string {
	payload := fmt.Sprintf("trackgetFileUrlformat_id%dintent%strack_id%d%d%s",
		formatID, intent, trackID, unixTS, appSecret)
	return md5Hex(payload)
}

func (c *Client) fetchFileURL(ctx context.Context, trackID int64, formatID int, secret string) (StreamInfo, error) {
	ts := time.Now().Unix()
	intent := "stream"
	sig := SignFileURL(trackID, formatID, intent, ts, secret)

	params := url.Values{
		"track_id":    {strconv.FormatInt(trackID, 10)},
		"format_id":   {strconv.Itoa(formatID)},
		"intent":      {intent},
		"request_ts":  {strconv.FormatInt(ts, 10)},
		"request_sig": {sig},
	}

	var resp struct {
		URL          string  `json:"url"`
		MimeType     string  `json:"mime_type"`
		BitDepth     int     `json:"bit_depth"`
		SamplingRate float64 `json:"sampling_rate"`
		Duration     int     `json:"duration"`
		FormatID     int     `json:"format_id"`
	}
	if err := c.getJSON(ctx, "/track/getFileUrl", params, true, &resp); err != nil {
		return StreamInfo{}, err
	}
	if resp.URL == "" {
		return StreamInfo{}, errors.New("qobuz returned empty stream URL (check subscription or region restrictions)")
	}

	mimeType := strings.TrimSpace(resp.MimeType)
	if mimeType == "" || mimeType == "audio/x-flac" {
		mimeType = "audio/flac"
	}

	sr := normalizeSampleRateHz(resp.SamplingRate)
	return StreamInfo{
		URL:        resp.URL,
		MIME:       mimeType,
		SampleRate: sr,
		BitDepth:   resp.BitDepth,
		Duration:   time.Duration(resp.Duration) * time.Second,
		FormatID:   resp.FormatID,
		ExpiresAt:  time.Now().Add(5 * time.Minute),
	}, nil
}

func (c *Client) requireAuth() error {
	c.mu.RLock()
	defer c.mu.RUnlock()
	if c.appID == "" || c.authToken == "" {
		return ErrUnauthenticated
	}
	return nil
}

func (c *Client) getJSON(ctx context.Context, path string, params url.Values, withAuth bool, dst any) error {
	c.mu.RLock()
	appID := c.appID
	token := c.authToken
	base := c.baseURL
	c.mu.RUnlock()

	if params == nil {
		params = url.Values{}
	}
	params.Set("app_id", appID)

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, base+path+"?"+params.Encode(), nil)
	if err != nil {
		return err
	}
	req.Header.Set("X-App-Id", appID)
	if withAuth && token != "" {
		req.Header.Set("X-User-Auth-Token", token)
	}
	req.Header.Set("User-Agent", "Mozilla/5.0")

	resp, err := c.http.Do(req)
	if err != nil {
		return err
	}
	defer func() { _ = resp.Body.Close() }()

	body, err := io.ReadAll(io.LimitReader(resp.Body, 8<<20))
	if err != nil {
		return err
	}
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		var apiErr struct {
			Message string `json:"message"`
			Code    int    `json:"code"`
		}
		msg := strings.TrimSpace(string(body))
		if json.Unmarshal(body, &apiErr) == nil && apiErr.Message != "" {
			msg = apiErr.Message
		}
		return &apiError{status: resp.StatusCode, message: msg}
	}
	return json.Unmarshal(body, dst)
}

func (c *Client) postFormJSON(ctx context.Context, path string, form url.Values, withAuth bool, dst any) error {
	var token string
	if withAuth {
		c.mu.RLock()
		token = c.authToken
		c.mu.RUnlock()
	}
	return c.postFormJSONWithToken(ctx, path, form, token, dst)
}

func (c *Client) postFormJSONWithToken(ctx context.Context, path string, form url.Values, token string, dst any) error {
	c.mu.RLock()
	appID := c.appID
	base := c.baseURL
	c.mu.RUnlock()

	if form == nil {
		form = url.Values{}
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, base+path, strings.NewReader(form.Encode()))
	if err != nil {
		return err
	}
	req.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	req.Header.Set("X-App-Id", appID)
	if token != "" {
		req.Header.Set("X-User-Auth-Token", token)
	}
	req.Header.Set("User-Agent", "Mozilla/5.0")

	resp, err := c.http.Do(req)
	if err != nil {
		return err
	}
	defer func() { _ = resp.Body.Close() }()

	body, err := io.ReadAll(io.LimitReader(resp.Body, 8<<20))
	if err != nil {
		return err
	}
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		var apiErr struct {
			Message string `json:"message"`
			Code    int    `json:"code"`
		}
		msg := strings.TrimSpace(string(body))
		if json.Unmarshal(body, &apiErr) == nil && apiErr.Message != "" {
			msg = apiErr.Message
		}
		return &apiError{status: resp.StatusCode, message: msg}
	}
	return json.Unmarshal(body, dst)
}

func md5Hex(s string) string {
	sum := md5.Sum([]byte(s))
	return hex.EncodeToString(sum[:])
}

func normalizeSampleRateHz(v float64) int {
	if v <= 0 {
		return 0
	}
	// Qobuz reports sampling_rate in kHz (e.g. 44.1, 96, 192), occasionally in Hz.
	if v < 1000 {
		return int(math.Round(v * 1000))
	}
	return int(math.Round(v))
}

type qImage struct {
	Small     string `json:"small"`
	Thumbnail string `json:"thumbnail"`
	Large     string `json:"large"`
}

func (img qImage) best() string {
	if img.Large != "" {
		return img.Large
	}
	if img.Small != "" {
		return img.Small
	}
	return img.Thumbnail
}

type qArtistImage struct {
	Small      string `json:"small"`
	Medium     string `json:"medium"`
	Large      string `json:"large"`
	ExtraLarge string `json:"extralarge"`
	Mega       string `json:"mega"`
}

func (img *qArtistImage) best() string {
	if img == nil {
		return ""
	}
	for _, u := range []string{img.Large, img.Medium, img.ExtraLarge, img.Small, img.Mega} {
		if u != "" {
			return u
		}
	}
	return ""
}

type qBiography struct {
	Summary string `json:"summary"`
	Content string `json:"content"`
}

type qArtist struct {
	ID          int64         `json:"id"`
	Name        string        `json:"name"`
	AlbumsCount int           `json:"albums_count"`
	Picture     string        `json:"picture"`
	Image       *qArtistImage `json:"image"`
	Biography   *qBiography   `json:"biography"`
}

func (qa qArtist) toArtist() Artist {
	art := qa.Image.best()
	if art == "" {
		art = qa.Picture
	}
	var bio string
	if qa.Biography != nil {
		bio = cleanQobuzText(qa.Biography.Content)
		if bio == "" {
			bio = cleanQobuzText(qa.Biography.Summary)
		}
	}
	return Artist{
		ID:          qa.ID,
		Name:        strings.TrimSpace(qa.Name),
		ArtworkURL:  art,
		AlbumsCount: qa.AlbumsCount,
		Biography:   bio,
	}
}

type qGenre struct {
	Name string `json:"name"`
}

type qLabel struct {
	Name string `json:"name"`
}

type qAward struct {
	Name string `json:"name"`
}

type qGoody struct {
	Name         string `json:"name"`
	URL          string `json:"url"`
	OriginalURL  string `json:"original_url"`
	FileFormatID int    `json:"file_format_id"`
}

type qAlbum struct {
	ID                  string   `json:"id"`
	Title               string   `json:"title"`
	Version             string   `json:"version"`
	ReleaseType         string   `json:"release_type"`
	Artist              qArtist  `json:"artist"`
	Genre               qGenre   `json:"genre"`
	Label               *qLabel  `json:"label"`
	Description         string   `json:"description"`
	Copyright           string   `json:"copyright"`
	Awards              []qAward `json:"awards"`
	Goodies             []qGoody `json:"goodies"`
	Image               qImage   `json:"image"`
	TracksCount         int      `json:"tracks_count"`
	ReleaseDateOriginal string   `json:"release_date_original"`
	MaximumBitDepth     int      `json:"maximum_bit_depth"`
	MaximumSamplingRate float64  `json:"maximum_sampling_rate"`
	Streamable          *bool    `json:"streamable"`
	Tracks              struct {
		Items []qTrack `json:"items"`
	} `json:"tracks"`
}

type qTrack struct {
	ID                  int64   `json:"id"`
	Title               string  `json:"title"`
	Version             string  `json:"version"`
	TrackNumber         int     `json:"track_number"`
	MediaNumber         int     `json:"media_number"`
	Duration            int     `json:"duration"`
	MaximumBitDepth     int     `json:"maximum_bit_depth"`
	MaximumSamplingRate float64 `json:"maximum_sampling_rate"`
	Streamable          *bool   `json:"streamable"`
	Performer           qArtist `json:"performer"`
	Performers          string  `json:"performers"`
	Album               *qAlbum `json:"album"`
}

var (
	reQobuzBreak = regexp.MustCompile(`(?i)<(?:br\s*/?|/p\s*)>`)
	reQobuzTag   = regexp.MustCompile(`<[^>]+>`)
	reMultiNL    = regexp.MustCompile(`\n{3,}`)
)

// cleanQobuzText converts Qobuz's lightweight HTML editorial copy (reviews and
// artist biographies) into clean plain-text paragraphs.
func cleanQobuzText(raw string) string {
	s := strings.TrimSpace(raw)
	if s == "" {
		return ""
	}
	s = reQobuzBreak.ReplaceAllString(s, "\n")
	s = reQobuzTag.ReplaceAllString(s, "")
	s = html.UnescapeString(s)
	s = strings.ReplaceAll(s, "\r\n", "\n")
	s = strings.ReplaceAll(s, "\r", "\n")
	lines := strings.Split(s, "\n")
	for i, line := range lines {
		lines[i] = strings.TrimSpace(line)
	}
	s = strings.Join(lines, "\n")
	s = reMultiNL.ReplaceAllString(s, "\n\n")
	return strings.TrimSpace(s)
}

// isPrimaryArtistAlbumOrEP filters Qobuz /artist/get album results down to the
// artist's own proper album, live, and EP releases — excluding covers/composer
// credits by other artists, compilations, and 1-2 track singles.
func isPrimaryArtistAlbumOrEP(qa qAlbum, artistID int64, artistKey string) bool {
	// Require primary artist match when artist metadata is present on the release.
	if qa.Artist.ID > 0 && artistID > 0 {
		if qa.Artist.ID != artistID {
			return false
		}
	} else if qa.Artist.Name != "" && artistKey != "" {
		if normMatchKey(qa.Artist.Name) != artistKey {
			return false
		}
	}

	switch strings.ToLower(strings.TrimSpace(qa.ReleaseType)) {
	case "single", "compilation", "other":
		return false
	}
	if qa.TracksCount > 0 && qa.TracksCount < 3 {
		return false
	}
	return true
}

func (qa qAlbum) toLibraryAlbum(includeTracks bool) library.Album {
	var year int
	if len(qa.ReleaseDateOriginal) >= 4 {
		year, _ = strconv.Atoi(qa.ReleaseDateOriginal[:4])
	}
	var labelName string
	if qa.Label != nil {
		labelName = strings.TrimSpace(qa.Label.Name)
	}
	var awards []string
	seenAwards := make(map[string]struct{}, len(qa.Awards))
	for _, aw := range qa.Awards {
		name := strings.TrimSpace(aw.Name)
		if name == "" {
			continue
		}
		key := strings.ToLower(name)
		if _, dup := seenAwards[key]; dup {
			continue
		}
		seenAwards[key] = struct{}{}
		awards = append(awards, name)
	}
	var bookletURL string
	for _, g := range qa.Goodies {
		u := strings.TrimSpace(g.URL)
		if u == "" {
			u = strings.TrimSpace(g.OriginalURL)
		}
		if strings.HasPrefix(u, "https://") && strings.HasSuffix(strings.ToLower(u), ".pdf") {
			bookletURL = u
			break
		}
	}
	alb := library.Album{
		ID:            "qobuz:" + qa.ID,
		Source:        library.SourceQobuz,
		SourceType:    library.SourceTypeRemote,
		Provider:      string(library.SourceQobuz),
		ExternalID:    qa.ID,
		Title:         qa.Title,
		Artist:        qa.Artist.Name,
		Genre:         strings.TrimSpace(qa.Genre.Name),
		Label:         labelName,
		Description:   cleanQobuzText(qa.Description),
		Copyright:     strings.TrimSpace(qa.Copyright),
		ReleaseDate:   strings.TrimSpace(qa.ReleaseDateOriginal),
		Awards:        awards,
		BookletURL:    bookletURL,
		Year:          year,
		TrackCount:    qa.TracksCount,
		ArtworkURL:    qa.Image.best(),
		PrimaryFormat: "FLAC",
		MaxBit:        qa.MaximumBitDepth,
		MaxRate:       normalizeSampleRateHz(qa.MaximumSamplingRate),
		Available:     true,
	}
	if includeTracks {
		alb.Tracks = make([]library.Track, 0, len(qa.Tracks.Items))
		for _, qt := range qa.Tracks.Items {
			tr := qt.toLibraryTrack(&qa)
			alb.Tracks = append(alb.Tracks, tr)
			alb.DurationSec += tr.DurationSec
		}
		alb.Credits = parseAlbumCredits(qa.Tracks.Items)
		if alb.TrackCount == 0 {
			alb.TrackCount = len(alb.Tracks)
		}
	}
	return alb
}

// parseAlbumCredits aggregates and deduplicates Qobuz's track-level performers
// strings ("Name, Role1, Role2 - Name2, Role3") into clean album-level credits.
func parseAlbumCredits(tracks []qTrack) []library.AlbumCredit {
	type acc struct {
		name      string
		roles     []string
		seenRoles map[string]struct{}
	}
	var order []string
	byKey := make(map[string]*acc)
	for _, qt := range tracks {
		if strings.TrimSpace(qt.Performers) == "" {
			continue
		}
		for _, seg := range strings.Split(qt.Performers, " - ") {
			parts := strings.Split(seg, ",")
			if len(parts) < 2 {
				continue
			}
			name := strings.TrimSpace(parts[0])
			if name == "" {
				continue
			}
			key := strings.ToLower(name)
			entry, ok := byKey[key]
			if !ok {
				entry = &acc{name: name, seenRoles: make(map[string]struct{})}
				byKey[key] = entry
				order = append(order, key)
			}
			for _, rawRole := range parts[1:] {
				role := formatCreditRole(rawRole)
				if role == "" {
					continue
				}
				rk := strings.ToLower(role)
				if _, dup := entry.seenRoles[rk]; dup {
					continue
				}
				entry.seenRoles[rk] = struct{}{}
				entry.roles = append(entry.roles, role)
			}
		}
	}
	out := make([]library.AlbumCredit, 0, len(order))
	for _, key := range order {
		entry := byKey[key]
		if len(entry.roles) == 0 {
			continue
		}
		out = append(out, library.AlbumCredit{
			Name:  entry.name,
			Roles: entry.roles,
		})
	}
	return out
}

func formatCreditRole(raw string) string {
	s := strings.TrimSpace(raw)
	if s == "" {
		return ""
	}
	switch s {
	case "MainArtist":
		return "Main Artist"
	case "FeaturedArtist":
		return "Featured Artist"
	case "AssociatedPerformer":
		return "Performer"
	case "ComposerLyricist":
		return "Composer / Lyricist"
	case "MixingEngineer":
		return "Mixing Engineer"
	case "MasteringEngineer":
		return "Mastering Engineer"
	case "RecordingEngineer":
		return "Recording Engineer"
	case "AssistantRecordingEngineer":
		return "Assistant Engineer"
	case "StudioPersonnel", "MusicPublisher":
		return "" // noisy label/generic tags
	default:
		return s
	}
}

func (qt qTrack) toLibraryTrack(parentAlbum *qAlbum) library.Track {
	alb := qt.Album
	if alb == nil {
		alb = parentAlbum
	}
	title := qt.Title
	if qt.Version != "" && !strings.Contains(strings.ToLower(title), strings.ToLower(qt.Version)) {
		title = title + " (" + qt.Version + ")"
	}
	artist := qt.Performer.Name
	var albumArtist, albumTitle, albumID, artURL, genre string
	var year int
	if alb != nil {
		albumTitle = alb.Title
		albumArtist = alb.Artist.Name
		albumID = "qobuz:" + alb.ID
		artURL = alb.Image.best()
		genre = strings.TrimSpace(alb.Genre.Name)
		if len(alb.ReleaseDateOriginal) >= 4 {
			year, _ = strconv.Atoi(alb.ReleaseDateOriginal[:4])
		}
		if artist == "" {
			artist = alb.Artist.Name
		}
	}
	return library.Track{
		ID:           fmt.Sprintf("qobuz:%d", qt.ID),
		Source:       library.SourceQobuz,
		SourceType:   library.SourceTypeRemote,
		Provider:     string(library.SourceQobuz),
		ExternalID:   strconv.FormatInt(qt.ID, 10),
		Title:        title,
		Artist:       artist,
		AlbumArtist:  albumArtist,
		Album:        albumTitle,
		AlbumID:      albumID,
		Genre:        genre,
		Year:         year,
		TrackNumber:  qt.TrackNumber,
		DiscNumber:   qt.MediaNumber,
		DurationSec:  qt.Duration,
		SampleRate:   normalizeSampleRateHz(qt.MaximumSamplingRate),
		BitDepth:     qt.MaximumBitDepth,
		Channels:     2,
		MIME:         "audio/flac",
		Ext:          ".flac",
		ArtworkURL:   artURL,
		Available:    true,
		QobuzTrackID: qt.ID,
	}
}
