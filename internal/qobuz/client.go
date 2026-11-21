package qobuz

import (
	"context"
	"crypto/md5"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math"
	"net/http"
	"net/url"
	"regexp"
	"strconv"
	"strings"
	"sync"
	"time"

	"tacet/internal/library"
)

const (
	DefaultBaseURL = "https://www.qobuz.com/api.json/0.2"
	webPlayerLogin = "https://play.qobuz.com/login"

	// FormatID values from Qobuz v0.2 API.
	FormatMP3320   = 5
	FormatFLAC1644 = 6
	FormatFLAC2496 = 7
	FormatFLAC192  = 27
)

// Config configures a Qobuz API client.
type Config struct {
	BaseURL   string
	AppID     string
	AppSecret string
	AuthToken string
	FormatID  int // Defaults to FormatFLAC192 (27)
}

// Status reports current Qobuz authentication state.
type Status struct {
	Configured    bool   `json:"configured"`
	Authenticated bool   `json:"authenticated"`
	UserName      string `json:"userName,omitempty"`
	Subscription  string `json:"subscription,omitempty"`
	MaxFormatID   int    `json:"maxFormatId"`
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

// SearchResult holds Qobuz search matches mapped to unified library types.
type SearchResult struct {
	Tracks []library.Track `json:"tracks"`
	Albums []library.Album `json:"albums"`
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
		baseURL:   base,
		http:      &http.Client{Timeout: 15 * time.Second},
		appID:     strings.TrimSpace(cfg.AppID),
		appSecret: strings.TrimSpace(cfg.AppSecret),
		authToken: strings.TrimSpace(cfg.AuthToken),
		formatID:  fmtID,
		urlCache:  make(map[int64]StreamInfo),
	}
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
	}
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

// OAuthURL returns the official Qobuz browser OAuth sign-in URL that redirects
// back to callbackURL with an authorization code.
func (c *Client) OAuthURL(ctx context.Context, callbackURL string) (string, error) {
	if err := c.ensureAppCredentials(ctx); err != nil {
		return "", err
	}
	c.mu.RLock()
	appID := c.appID
	c.mu.RUnlock()

	u := fmt.Sprintf("https://www.qobuz.com/signin/oauth?ext_app_id=%s&redirect_url=%s",
		url.QueryEscape(appID), url.QueryEscape(callbackURL))
	return u, nil
}

// ExchangeOAuthCode exchanges a Qobuz OAuth callback authorization code for a
// user_auth_token and validates the resulting session.
func (c *Client) ExchangeOAuthCode(ctx context.Context, code string) (Status, error) {
	code = strings.TrimSpace(code)
	if code == "" {
		return Status{}, errors.New("missing OAuth authorization code")
	}
	if err := c.ensureAppCredentials(ctx); err != nil {
		return Status{}, err
	}
	c.mu.RLock()
	privateKey := c.privateKey
	c.mu.RUnlock()
	if privateKey == "" {
		return Status{}, errors.New("Qobuz OAuth private_key was not found in web player bundle")
	}

	params := url.Values{
		"code":        {code},
		"private_key": {privateKey},
	}
	var resp struct {
		Token  string `json:"token"`
		UserID any    `json:"user_id"`
	}
	if err := c.getJSON(ctx, "/oauth/callback", params, false, &resp); err != nil {
		return Status{}, fmt.Errorf("qobuz oauth exchange failed: %w", err)
	}
	if strings.TrimSpace(resp.Token) == "" {
		return Status{}, errors.New("qobuz oauth exchange returned empty token")
	}
	return c.LoginWithToken(ctx, resp.Token)
}

// LoginWithToken validates a Qobuz user_auth_token against /user/login (auto-
// discovering app_id and signing secrets if not already configured).
func (c *Client) LoginWithToken(ctx context.Context, token string) (Status, error) {
	token = strings.TrimSpace(token)
	if token == "" {
		return Status{}, errors.New("user_auth_token is required")
	}
	if err := c.ensureAppCredentials(ctx); err != nil {
		return Status{}, err
	}

	c.mu.Lock()
	prevToken := c.authToken
	c.authToken = token
	clear(c.urlCache)
	c.mu.Unlock()

	var resp struct {
		User struct {
			DisplayName  string `json:"display_name"`
			Login        string `json:"login"`
			Subscription struct {
				Offer string `json:"offer"`
			} `json:"subscription"`
		} `json:"user"`
		UserAuthToken string `json:"user_auth_token"`
	}
	err := c.postFormJSON(ctx, "/user/login", url.Values{"extra": {"partner"}}, true, &resp)
	if err != nil {
		c.mu.Lock()
		c.authToken = prevToken
		c.mu.Unlock()
		return Status{}, fmt.Errorf("qobuz token validation failed: %w", err)
	}

	name := resp.User.DisplayName
	if name == "" {
		name = resp.User.Login
	}
	finalToken := resp.UserAuthToken
	if finalToken == "" {
		finalToken = token
	}

	c.mu.Lock()
	c.authToken = finalToken
	c.userName = name
	c.subscription = resp.User.Subscription.Offer
	clear(c.urlCache)
	c.mu.Unlock()

	return c.Status(), nil
}

// Login authenticates with Qobuz using email and password. Note: Qobuz retired
// direct password login for many accounts in favor of OAuth/token auth.
func (c *Client) Login(ctx context.Context, email, password string) (Status, error) {
	email = strings.TrimSpace(email)
	if email == "" || password == "" {
		return Status{}, errors.New("email and password are required")
	}
	if err := c.ensureAppCredentials(ctx); err != nil {
		return Status{}, err
	}

	pwHash := md5Hex(password)
	params := url.Values{
		"email":    {email},
		"password": {pwHash},
	}

	var resp struct {
		User struct {
			DisplayName  string `json:"display_name"`
			Login        string `json:"login"`
			Subscription struct {
				Offer string `json:"offer"`
			} `json:"subscription"`
		} `json:"user"`
		UserAuthToken string `json:"user_auth_token"`
	}
	if err := c.getJSON(ctx, "/user/login", params, false, &resp); err != nil {
		return Status{}, fmt.Errorf("qobuz login failed: %w", err)
	}
	if resp.UserAuthToken == "" {
		return Status{}, errors.New("qobuz login returned empty auth token")
	}

	name := resp.User.DisplayName
	if name == "" {
		name = resp.User.Login
	}

	c.mu.Lock()
	c.authToken = resp.UserAuthToken
	c.userName = name
	c.subscription = resp.User.Subscription.Offer
	clear(c.urlCache)
	c.mu.Unlock()

	return c.Status(), nil
}

// Search searches Qobuz tracks and albums.
func (c *Client) Search(ctx context.Context, query string, limit int) (SearchResult, error) {
	query = strings.TrimSpace(query)
	if query == "" {
		return SearchResult{}, nil
	}
	if limit <= 0 || limit > 100 {
		limit = 25
	}
	if err := c.requireAuth(); err != nil {
		return SearchResult{}, err
	}

	params := url.Values{
		"query": {query},
		"limit": {strconv.Itoa(limit)},
	}

	var resp struct {
		Tracks struct {
			Items []qTrack `json:"items"`
		} `json:"tracks"`
		Albums struct {
			Items []qAlbum `json:"items"`
		} `json:"albums"`
	}
	if err := c.getJSON(ctx, "/catalog/search", params, true, &resp); err != nil {
		return SearchResult{}, err
	}

	res := SearchResult{
		Tracks: make([]library.Track, 0, len(resp.Tracks.Items)),
		Albums: make([]library.Album, 0, len(resp.Albums.Items)),
	}
	for _, qt := range resp.Tracks.Items {
		res.Tracks = append(res.Tracks, qt.toLibraryTrack(nil))
	}
	for _, qa := range resp.Albums.Items {
		res.Albums = append(res.Albums, qa.toLibraryAlbum(false))
	}
	return res, nil
}

// GetAlbum fetches a Qobuz album and its track listing.
func (c *Client) GetAlbum(ctx context.Context, albumID string) (library.Album, error) {
	if err := c.requireAuth(); err != nil {
		return library.Album{}, err
	}
	params := url.Values{
		"album_id": {albumID},
		"limit":    {"500"},
	}
	var qa qAlbum
	if err := c.getJSON(ctx, "/album/get", params, true, &qa); err != nil {
		return library.Album{}, err
	}
	return qa.toLibraryAlbum(true), nil
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
		return library.Track{}, err
	}
	return qt.toLibraryTrack(nil), nil
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
		return errors.New("qobuz is not authenticated")
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
	defer resp.Body.Close()

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
	defer resp.Body.Close()

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

// ensureAppCredentials extracts app_id, candidate signing secrets, and the
// OAuth private key from the Qobuz web player bundle when not preconfigured.
func (c *Client) ensureAppCredentials(ctx context.Context) error {
	c.mu.RLock()
	haveID := c.appID != ""
	haveSecret := c.appSecret != ""
	c.mu.RUnlock()
	if haveID && haveSecret {
		return nil
	}
	if haveID != haveSecret {
		return errors.New("both QOBUZ_APP_ID and QOBUZ_APP_SECRET must be set together (or omit both for auto-discovery)")
	}

	appID, secrets, privKey, err := c.discoverWebPlayerCredentials(ctx)
	if err != nil {
		return fmt.Errorf("could not auto-discover Qobuz web player credentials (set QOBUZ_APP_ID & QOBUZ_APP_SECRET manually): %w", err)
	}

	c.mu.Lock()
	c.appID = appID
	c.appSecret = secrets[0]
	c.altSecrets = secrets
	if privKey != "" {
		c.privateKey = privKey
	}
	c.mu.Unlock()
	return nil
}

var (
	reBundlePath = regexp.MustCompile(`<script src="(/resources/\d+\.\d+\.\d+-[a-z]\d+/bundle\.js)"></script>`)
	reAppID      = regexp.MustCompile(`production:\{api:\{appId:"(\d+)",appSecret:"([^"]*)"`)
	reSeed       = regexp.MustCompile(`[a-z]\.initialSeed\("([\w=]+)",window\.utimezone\.([a-z]+)\)`)
	rePrivateKey = regexp.MustCompile(`authenticate\(\{privateKey:"([^"]+)"`)
)

func (c *Client) discoverWebPlayerCredentials(ctx context.Context) (string, []string, string, error) {
	htmlBody, err := c.fetchText(ctx, webPlayerLogin)
	if err != nil {
		return "", nil, "", err
	}
	m := reBundlePath.FindStringSubmatch(htmlBody)
	if len(m) < 2 {
		return "", nil, "", errors.New("bundle.js path not found on play.qobuz.com/login")
	}
	bundleBody, err := c.fetchText(ctx, "https://play.qobuz.com"+m[1])
	if err != nil {
		return "", nil, "", err
	}

	idMatch := reAppID.FindStringSubmatch(bundleBody)
	if len(idMatch) < 2 {
		return "", nil, "", errors.New("app_id not found in Qobuz web bundle")
	}
	appID := idMatch[1]
	literalSecret := ""
	if len(idMatch) >= 3 {
		literalSecret = strings.TrimSpace(idMatch[2])
	}

	privateKey := ""
	if pkMatch := rePrivateKey.FindStringSubmatch(bundleBody); len(pkMatch) >= 2 {
		privateKey = pkMatch[1]
	}

	seen := make(map[string]struct{})
	var secrets []string
	addSecret := func(s string) {
		if s == "" {
			return
		}
		if _, dup := seen[s]; dup {
			return
		}
		seen[s] = struct{}{}
		secrets = append(secrets, s)
	}

	for _, sm := range reSeed.FindAllStringSubmatch(bundleBody, -1) {
		seed, tz := sm[1], sm[2]
		tzTitle := strings.ToUpper(tz[:1]) + tz[1:]
		infoRe := regexp.MustCompile(`name:"\w+/` + regexp.QuoteMeta(tzTitle) + `",info:"([\w=]+)",extras:"([\w=]+)"`)
		im := infoRe.FindStringSubmatch(bundleBody)
		if len(im) < 3 {
			continue
		}
		combined := seed + im[1] + im[2]
		if len(combined) <= 44 {
			continue
		}
		b64 := combined[:len(combined)-44]
		if decoded, err := base64.StdEncoding.DecodeString(b64); err == nil && len(decoded) > 0 {
			addSecret(string(decoded))
		}
	}
	addSecret(literalSecret)

	if len(secrets) == 0 {
		return "", nil, "", errors.New("no signing secrets extracted from Qobuz web bundle")
	}
	return appID, secrets, privateKey, nil
}

func (c *Client) fetchText(ctx context.Context, targetURL string) (string, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, targetURL, nil)
	if err != nil {
		return "", err
	}
	req.Header.Set("User-Agent", "Mozilla/5.0")
	resp, err := c.http.Do(req)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return "", fmt.Errorf("GET %s returned %d", targetURL, resp.StatusCode)
	}
	b, err := io.ReadAll(io.LimitReader(resp.Body, 20<<20))
	if err != nil {
		return "", err
	}
	return string(b), nil
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

type qArtist struct {
	Name string `json:"name"`
}

type qAlbum struct {
	ID                  string  `json:"id"`
	Title               string  `json:"title"`
	Artist              qArtist `json:"artist"`
	Image               qImage  `json:"image"`
	TracksCount         int     `json:"tracks_count"`
	ReleaseDateOriginal string  `json:"release_date_original"`
	MaximumBitDepth     int     `json:"maximum_bit_depth"`
	MaximumSamplingRate float64 `json:"maximum_sampling_rate"`
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
		ID:         "qobuz:" + qa.ID,
		Source:     library.SourceQobuz,
		Title:      qa.Title,
		Artist:     qa.Artist.Name,
		Year:       year,
		TrackCount: qa.TracksCount,
		ArtworkURL: qa.Image.best(),
		MaxBit:     qa.MaximumBitDepth,
		MaxRate:    normalizeSampleRateHz(qa.MaximumSamplingRate),
	}
	if includeTracks {
		alb.Tracks = make([]library.Track, 0, len(qa.Tracks.Items))
		for _, qt := range qa.Tracks.Items {
			alb.Tracks = append(alb.Tracks, qt.toLibraryTrack(&qa))
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
	var albumTitle, albumID, artURL string
	if alb != nil {
		albumTitle = alb.Title
		albumID = "qobuz:" + alb.ID
		artURL = alb.Image.best()
		if artist == "" {
			artist = alb.Artist.Name
		}
	}
	return library.Track{
		ID:           fmt.Sprintf("qobuz:%d", qt.ID),
		Source:       library.SourceQobuz,
		Title:        title,
		Artist:       artist,
		Album:        albumTitle,
		AlbumID:      albumID,
		TrackNumber:  qt.TrackNumber,
		DiscNumber:   qt.MediaNumber,
		DurationSec:  qt.Duration,
		SampleRate:   normalizeSampleRateHz(qt.MaximumSamplingRate),
		BitDepth:     qt.MaximumBitDepth,
		Channels:     2,
		MIME:         "audio/flac",
		Ext:          ".flac",
		ArtworkURL:   artURL,
		QobuzTrackID: qt.ID,
	}
}
