package api

import (
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"log"
	"net"
	"net/http"
	"net/netip"
	"net/url"
	"strings"
	"time"

	"tacet/internal/qobuz"
)

const oauthStateCookieName = "tacet_qobuz_oauth"

func (s *Server) handleLibraryStatus(w http.ResponseWriter, _ *http.Request) {
	writeJSON(w, http.StatusOK, s.catalog.Status())
}

func (s *Server) handleLocalAlbums(w http.ResponseWriter, r *http.Request) {
	limit, offset, err := parsePageParams(r, 200, 500)
	if err != nil {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
		return
	}
	q := strings.TrimSpace(r.URL.Query().Get("q"))
	sortBy := strings.TrimSpace(r.URL.Query().Get("sort"))

	if q == "" {
		baseTag := strings.Trim(s.catalog.ETag(), `"`)
		etag := fmt.Sprintf(`"%s-%s-%d-%d"`, baseTag, sortBy, limit, offset)
		w.Header().Set("ETag", etag)
		if match := strings.TrimSpace(r.Header.Get("If-None-Match")); match == etag {
			w.WriteHeader(http.StatusNotModified)
			return
		}
	}

	items, total := s.catalog.AlbumsPage(q, sortBy, limit, offset)
	writeJSON(w, http.StatusOK, map[string]any{
		"items":  items,
		"total":  total,
		"limit":  limit,
		"offset": offset,
	})
}

func (s *Server) handleLocalAlbum(w http.ResponseWriter, r *http.Request) {
	id := r.PathValue("id")
	alb, ok := s.catalog.Album(id)
	if !ok {
		writeDomainError(w, fmt.Errorf("%w: %q", ErrAlbumNotFound, id))
		return
	}
	writeJSON(w, http.StatusOK, alb)
}

func (s *Server) handleLocalTracks(w http.ResponseWriter, r *http.Request) {
	limit, offset, err := parsePageParams(r, 100, 500)
	if err != nil {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
		return
	}
	q := strings.TrimSpace(r.URL.Query().Get("q"))
	items, total := s.catalog.TracksPage(q, limit, offset)
	writeJSON(w, http.StatusOK, map[string]any{
		"items":  items,
		"total":  total,
		"limit":  limit,
		"offset": offset,
	})
}

func (s *Server) handleLocalRescan(w http.ResponseWriter, r *http.Request) {
	wait := r.URL.Query().Get("wait") == "true"
	if wait {
		if _, err := s.catalog.Scan(); err != nil {
			writeAPIError(w, http.StatusInternalServerError, "SCAN_FAILED", err.Error(), true)
			return
		}
		st := s.catalog.Status()
		s.manager.Publish("library.updated", st)
		writeJSON(w, http.StatusOK, st)
		return
	}

	go func() {
		defer func() {
			s.manager.Publish("library.updated", s.catalog.Status())
		}()
		if _, err := s.catalog.Scan(); err != nil {
			log.Printf("async library rescan failed: %v", err)
		}
	}()

	st := s.catalog.Status()
	st.Scanning = true
	writeJSON(w, http.StatusAccepted, st)
}

func (s *Server) handleQobuzStatus(w http.ResponseWriter, _ *http.Request) {
	if s.qobuz == nil {
		writeJSON(w, http.StatusOK, qobuz.Status{
			MaxFormatID: qobuz.FormatFLAC192,
			MaxQuality:  qobuz.QualityLabel(qobuz.FormatFLAC192),
		})
		return
	}
	writeJSON(w, http.StatusOK, s.qobuz.Status())
}

func (s *Server) resolveOAuthCallbackURL(r *http.Request) (string, error) {
	if s.manager != nil {
		if base := strings.TrimRight(s.manager.PublicBaseURL(), "/"); base != "" {
			return base + "/api/v1/qobuz/oauth/callback", nil
		}
	}
	hostport := strings.TrimSpace(r.Host)
	if hostport == "" {
		return "", errors.New("missing Host header")
	}
	host := hostport
	if h, _, err := net.SplitHostPort(hostport); err == nil {
		host = h
	}
	host = strings.Trim(host, "[]")
	if !strings.EqualFold(host, "localhost") {
		addr, err := netip.ParseAddr(host)
		if err != nil || (!addr.IsLoopback() && !addr.IsPrivate()) {
			return "", fmt.Errorf("OAuth callback host %q is not localhost or a private LAN IP; set --public-url to allow custom hosts", host)
		}
	}
	scheme := "http"
	if r.TLS != nil {
		scheme = "https"
	}
	return fmt.Sprintf("%s://%s/api/v1/qobuz/oauth/callback", scheme, hostport), nil
}

func (s *Server) issueOAuthState() (string, error) {
	var raw [16]byte
	if _, err := rand.Read(raw[:]); err != nil {
		return "", err
	}
	tok := hex.EncodeToString(raw[:])
	now := time.Now()
	s.oauthMu.Lock()
	defer s.oauthMu.Unlock()
	for k, exp := range s.oauthState {
		if now.After(exp) {
			delete(s.oauthState, k)
		}
	}
	s.oauthState[tok] = now.Add(10 * time.Minute)
	return tok, nil
}

func (s *Server) consumeOAuthState(tok string) bool {
	if tok == "" {
		return false
	}
	now := time.Now()
	s.oauthMu.Lock()
	defer s.oauthMu.Unlock()
	exp, ok := s.oauthState[tok]
	delete(s.oauthState, tok)
	return ok && now.Before(exp)
}

func (s *Server) handleQobuzOAuthStart(w http.ResponseWriter, r *http.Request) {
	if s.qobuz == nil {
		writeDomainError(w, ErrQobuzUnavailable)
		return
	}
	callbackURL, err := s.resolveOAuthCallbackURL(r)
	if err != nil {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
		return
	}
	authURL, err := s.qobuz.OAuthURL(r.Context(), callbackURL)
	if err != nil {
		writeAPIError(w, http.StatusBadGateway, "UPSTREAM_ERROR", "failed to prepare Qobuz OAuth: "+err.Error(), true)
		return
	}
	parsed, err := url.Parse(authURL)
	if err != nil || parsed.Scheme != "https" || parsed.Hostname() != "www.qobuz.com" {
		writeAPIError(w, http.StatusBadGateway, "UPSTREAM_ERROR", "invalid Qobuz OAuth redirect target", false)
		return
	}
	stateTok, err := s.issueOAuthState()
	if err != nil {
		writeAPIError(w, http.StatusInternalServerError, "INTERNAL_ERROR", "failed to initialize OAuth session", true)
		return
	}
	http.SetCookie(w, &http.Cookie{ //nolint:gosec // Secure enabled when TLS is active; LAN control point commonly runs over plain HTTP
		Name:     oauthStateCookieName,
		Value:    stateTok,
		Path:     "/api/v1/qobuz/oauth/callback",
		MaxAge:   600,
		HttpOnly: true,
		Secure:   r.TLS != nil,
		SameSite: http.SameSiteLaxMode,
	})
	http.Redirect(w, r, parsed.String(), http.StatusFound) //nolint:gosec // validated https://www.qobuz.com OAuth URL
}

func (s *Server) handleQobuzOAuthCallback(w http.ResponseWriter, r *http.Request) {
	if s.qobuz == nil {
		writeDomainError(w, ErrQobuzUnavailable)
		return
	}
	cookie, err := r.Cookie(oauthStateCookieName)
	http.SetCookie(w, &http.Cookie{ //nolint:gosec // Secure enabled when TLS is active; LAN control point commonly runs over plain HTTP
		Name:     oauthStateCookieName,
		Value:    "",
		Path:     "/api/v1/qobuz/oauth/callback",
		MaxAge:   -1,
		HttpOnly: true,
		Secure:   r.TLS != nil,
		SameSite: http.SameSiteLaxMode,
	})
	if err != nil || !s.consumeOAuthState(cookie.Value) {
		writeAPIError(w, http.StatusForbidden, "OAUTH_STATE_INVALID", "missing or expired Qobuz OAuth session — start sign-in from the client", false)
		return
	}
	code := strings.TrimSpace(r.URL.Query().Get("code_autorisation"))
	if code == "" {
		code = strings.TrimSpace(r.URL.Query().Get("code"))
	}
	if code == "" {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", "missing Qobuz OAuth code in callback URL", false)
		return
	}
	st, err := s.qobuz.ExchangeOAuthCode(r.Context(), code)
	if err != nil {
		writeAPIError(w, http.StatusUnauthorized, "QOBUZ_AUTH_FAILED", "Qobuz sign-in failed: "+err.Error(), false)
		return
	}
	s.manager.Publish("qobuz.status", st)
	http.Redirect(w, r, "/?tab=qobuz", http.StatusFound)
}

func (s *Server) handleQobuzLogin(w http.ResponseWriter, r *http.Request) {
	if s.qobuz == nil {
		writeDomainError(w, ErrQobuzUnavailable)
		return
	}
	var req struct {
		Email     string `json:"email"`
		Password  string `json:"password"`
		AppID     string `json:"appId"`
		AppSecret string `json:"appSecret"`
		AuthToken string `json:"authToken"`
	}
	if err := decodeJSON(w, r, &req); err != nil {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
		return
	}

	if req.AppID != "" || req.AppSecret != "" {
		s.qobuz.SetCredentials(req.AppID, req.AppSecret, "")
	}
	if strings.TrimSpace(req.AuthToken) != "" {
		st, err := s.qobuz.LoginWithToken(r.Context(), req.AuthToken)
		if err != nil {
			writeAPIError(w, http.StatusUnauthorized, "QOBUZ_AUTH_FAILED", err.Error(), false)
			return
		}
		s.manager.Publish("qobuz.status", st)
		writeJSON(w, http.StatusOK, st)
		return
	}
	if req.Email != "" && req.Password != "" {
		st, err := s.qobuz.Login(r.Context(), req.Email, req.Password)
		if err != nil {
			writeAPIError(w, http.StatusUnauthorized, "QOBUZ_AUTH_FAILED", err.Error(), false)
			return
		}
		s.manager.Publish("qobuz.status", st)
		writeJSON(w, http.StatusOK, st)
		return
	}
	st := s.qobuz.Status()
	if !st.Authenticated {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", "sign in via Qobuz OAuth or provide an authToken", false)
		return
	}
	writeJSON(w, http.StatusOK, st)
}

func (s *Server) handleQobuzLogout(w http.ResponseWriter, _ *http.Request) {
	if s.qobuz == nil {
		writeDomainError(w, ErrQobuzUnavailable)
		return
	}
	st := s.qobuz.Logout()
	s.manager.Publish("qobuz.status", st)
	writeJSON(w, http.StatusOK, st)
}

func (s *Server) handleQobuzSearch(w http.ResponseWriter, r *http.Request) {
	if s.qobuz == nil {
		writeDomainError(w, ErrQobuzUnavailable)
		return
	}
	q := strings.TrimSpace(r.URL.Query().Get("q"))
	if q == "" {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", "q query parameter is required", false)
		return
	}
	limit, offset, err := parsePageParams(r, 30, 100)
	if err != nil {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
		return
	}
	res, err := s.qobuz.Search(r.Context(), q, limit, offset)
	if err != nil {
		writeDomainError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, res)
}

func (s *Server) handleQobuzAlbum(w http.ResponseWriter, r *http.Request) {
	if s.qobuz == nil {
		writeDomainError(w, ErrQobuzUnavailable)
		return
	}
	id := r.PathValue("id")
	alb, err := s.qobuz.GetAlbum(r.Context(), id)
	if err != nil {
		writeDomainError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, alb)
}
