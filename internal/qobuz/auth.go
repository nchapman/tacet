package qobuz

import (
	"context"
	"encoding/base64"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"regexp"
	"strings"
)

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
		return Status{}, errors.New("qobuz OAuth private_key was not found in web player bundle")
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
	err := c.postFormJSONWithToken(ctx, "/user/login", url.Values{"extra": {"partner"}}, token, &resp)
	if err != nil {
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
	switchedAccount := (c.authToken != "" && c.authToken != finalToken) || (c.userName != "" && name != "" && c.userName != name)
	c.authToken = finalToken
	c.userName = name
	c.subscription = resp.User.Subscription.Offer
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
	switchedAccount := (c.authToken != "" && c.authToken != resp.UserAuthToken) || (c.userName != "" && name != "" && c.userName != name)
	c.authToken = resp.UserAuthToken
	c.userName = name
	c.subscription = resp.User.Subscription.Offer
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

	return c.Status(), nil
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
	defer func() { _ = resp.Body.Close() }()
	if resp.StatusCode != http.StatusOK {
		return "", fmt.Errorf("GET %s returned %d", targetURL, resp.StatusCode)
	}
	b, err := io.ReadAll(io.LimitReader(resp.Body, 20<<20))
	if err != nil {
		return "", err
	}
	return string(b), nil
}
