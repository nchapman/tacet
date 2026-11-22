package qobuz

import (
	"context"
	"crypto/md5"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"sync"
	"time"

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
	FormatID  int    // Defaults to FormatFLAC192 (27)
	CachePath string // Optional JSON disk cache path for slow-changing catalog metadata
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
}

// ArtistDetail pairs a Qobuz artist profile with their remote discography.
type ArtistDetail struct {
	Artist Artist          `json:"artist"`
	Albums []library.Album `json:"albums"`
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
	cache        *catalogCache
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
	return &Client{
		baseURL:    base,
		http:       &http.Client{Timeout: 15 * time.Second},
		appID:      strings.TrimSpace(cfg.AppID),
		appSecret:  strings.TrimSpace(cfg.AppSecret),
		authToken:  strings.TrimSpace(cfg.AuthToken),
		formatID:   fmtID,
		urlCache:   make(map[int64]StreamInfo),
		trackCache: make(map[int64]library.Track),
		cache:      newCatalogCache(cfg.CachePath),
	}
}

// Close flushes any pending Qobuz catalog cache writes to disk.
func (c *Client) Close() error {
	if c.cache == nil {
		return nil
	}
	return c.cache.Close()
}

// CacheStats returns the number of cached albums, artists, artist names, and searches.
func (c *Client) CacheStats() (albums, artists, artistNames, searches int) {
	if c.cache == nil {
		return 0, 0, 0, 0
	}
	return c.cache.stats()
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
	c.mu.Lock()
	c.authToken = ""
	c.userName = ""
	c.subscription = ""
	clear(c.urlCache)
	st := Status{
		Configured:    c.appID != "" && c.appSecret != "",
		Authenticated: false,
		MaxFormatID:   c.formatID,
		MaxQuality:    QualityLabel(c.formatID),
	}
	c.mu.Unlock()
	if c.cache != nil {
		c.cache.clear()
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
	defer c.mu.Unlock()
	if strings.TrimSpace(appID) != "" {
		c.appID = strings.TrimSpace(appID)
	}
	if strings.TrimSpace(appSecret) != "" {
		c.appSecret = strings.TrimSpace(appSecret)
	}
	if strings.TrimSpace(authToken) != "" {
		c.authToken = strings.TrimSpace(authToken)
	}
	clear(c.urlCache)
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
	if cached, ok := c.cache.getSearch(query, limit, off); ok {
		for _, tr := range cached.Tracks {
			c.cacheTrack(tr)
		}
		return cached, nil
	}

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
		c.cacheArtistName(art.Name, art.ID)
		res.Artists = append(res.Artists, art)
	}
	for _, qt := range resp.Tracks.Items {
		tr := qt.toLibraryTrack(nil)
		c.cacheTrack(tr)
		res.Tracks = append(res.Tracks, tr)
	}
	for _, qa := range resp.Albums.Items {
		if qa.Streamable != nil && !*qa.Streamable {
			continue
		}
		res.Albums = append(res.Albums, qa.toLibraryAlbum(false))
	}
	c.cache.putSearch(query, limit, off, res)
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
	if cached, ok := c.cache.getArtist(artistID, limit); ok {
		return cached, nil
	}
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
	if art.Name != "" {
		c.cacheArtistName(art.Name, art.ID)
	}
	albums := make([]library.Album, 0, len(resp.Albums.Items))
	for _, qa := range resp.Albums.Items {
		if qa.Streamable != nil && !*qa.Streamable {
			continue
		}
		alb := qa.toLibraryAlbum(false)
		if alb.Artist == "" {
			alb.Artist = art.Name
		}
		albums = append(albums, alb)
	}
	if art.ArtworkURL == "" && len(albums) > 0 {
		art.ArtworkURL = albums[0].ArtworkURL
	}
	detail := ArtistDetail{Artist: art, Albums: albums}
	c.cache.putArtist(artistID, limit, detail)
	return detail, nil
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

	if cachedID, ok := c.cache.getArtistName(name); ok {
		if cachedID == 0 {
			return ArtistDetail{}, fmt.Errorf("%w: artist %q", ErrNotFound, name)
		}
		return c.GetArtist(ctx, cachedID, limit)
	}

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
		c.cache.putArtistName(name, 0)
		return ArtistDetail{}, fmt.Errorf("%w: artist %q", ErrNotFound, name)
	}
	c.cacheArtistName(name, matchedID)
	return c.GetArtist(ctx, matchedID, limit)
}

func (c *Client) cacheArtistName(name string, id int64) {
	if c.cache == nil || id <= 0 {
		return
	}
	c.cache.putArtistName(name, id)
}

// GetAlbum fetches a Qobuz album and its track listing. Accepts bare or "qobuz:"-prefixed IDs.
func (c *Client) GetAlbum(ctx context.Context, albumID string) (library.Album, error) {
	if err := c.requireAuth(); err != nil {
		return library.Album{}, err
	}
	rawID := strings.TrimPrefix(strings.TrimSpace(albumID), "qobuz:")
	if cached, ok := c.cache.getAlbum(rawID); ok {
		for _, tr := range cached.Tracks {
			c.cacheTrack(tr)
		}
		return cached, nil
	}
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
	for _, tr := range alb.Tracks {
		c.cacheTrack(tr)
	}
	c.cache.putAlbum(rawID, alb)
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

const maxTrackCacheEntries = 512

func (c *Client) cacheTrack(tr library.Track) {
	if tr.QobuzTrackID <= 0 {
		return
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if len(c.trackCache) >= maxTrackCacheEntries {
		clear(c.trackCache)
	}
	c.trackCache[tr.QobuzTrackID] = tr
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
	c.mu.RLock()
	appID := c.appID
	token := c.authToken
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

type qArtist struct {
	ID          int64         `json:"id"`
	Name        string        `json:"name"`
	AlbumsCount int           `json:"albums_count"`
	Picture     string        `json:"picture"`
	Image       *qArtistImage `json:"image"`
}

func (qa qArtist) toArtist() Artist {
	art := qa.Image.best()
	if art == "" {
		art = qa.Picture
	}
	return Artist{
		ID:          qa.ID,
		Name:        strings.TrimSpace(qa.Name),
		ArtworkURL:  art,
		AlbumsCount: qa.AlbumsCount,
	}
}

type qGenre struct {
	Name string `json:"name"`
}

type qAlbum struct {
	ID                  string  `json:"id"`
	Title               string  `json:"title"`
	Version             string  `json:"version"`
	Artist              qArtist `json:"artist"`
	Genre               qGenre  `json:"genre"`
	Image               qImage  `json:"image"`
	TracksCount         int     `json:"tracks_count"`
	ReleaseDateOriginal string  `json:"release_date_original"`
	MaximumBitDepth     int     `json:"maximum_bit_depth"`
	MaximumSamplingRate float64 `json:"maximum_sampling_rate"`
	Streamable          *bool   `json:"streamable"`
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
	Performer           qArtist `json:"performer"`
	Album               *qAlbum `json:"album"`
}

func (qa qAlbum) toLibraryAlbum(includeTracks bool) library.Album {
	var year int
	if len(qa.ReleaseDateOriginal) >= 4 {
		year, _ = strconv.Atoi(qa.ReleaseDateOriginal[:4])
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
		if alb.TrackCount == 0 {
			alb.TrackCount = len(alb.Tracks)
		}
	}
	return alb
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
