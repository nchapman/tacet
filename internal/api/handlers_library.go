package api

import (
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"net"
	"net/http"
	"net/netip"
	"net/url"
	"strings"
	"time"

	"tacet/internal/library"
	"tacet/internal/qobuz"
)

const oauthStateCookieName = "tacet_qobuz_oauth"

func (s *Server) handleLocalAlbums(w http.ResponseWriter, r *http.Request) {
	writeJSON(w, http.StatusOK, map[string]any{
		"roots":  s.catalog.Roots(),
		"albums": s.catalog.Albums(),
	})
}

func (s *Server) handleLocalAlbum(w http.ResponseWriter, r *http.Request) {
	id := r.PathValue("id")
	alb, ok := s.catalog.Album(id)
	if !ok {
		writeError(w, http.StatusNotFound, fmt.Errorf("album %q not found", id))
		return
	}
	writeJSON(w, http.StatusOK, alb)
}

func (s *Server) handleLocalTracks(w http.ResponseWriter, r *http.Request) {
	q := strings.TrimSpace(r.URL.Query().Get("q"))
	var tracks []library.Track
	if q != "" {
		tracks = s.catalog.Search(q, 100)
	} else {
		tracks = s.catalog.Tracks()
	}
	writeJSON(w, http.StatusOK, map[string]any{"tracks": tracks})
}

func (s *Server) handleLocalRescan(w http.ResponseWriter, r *http.Request) {
	count, err := s.catalog.Scan()
	if err != nil {
		writeError(w, http.StatusInternalServerError, err)
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"tracks": count,
		"albums": len(s.catalog.Albums()),
	})
}

func (s *Server) handleQobuzStatus(w http.ResponseWriter, r *http.Request) {
	if s.qobuz == nil {
		writeJSON(w, http.StatusOK, qobuz.Status{})
		return
	}
	writeJSON(w, http.StatusOK, s.qobuz.Status())
}

func (s *Server) resolveOAuthCallbackURL(r *http.Request) (string, error) {
	if s.manager != nil {
		if base := strings.TrimRight(s.manager.PublicBaseURL(), "/"); base != "" {
			return base + "/api/qobuz/oauth/callback", nil
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
	return fmt.Sprintf("%s://%s/api/qobuz/oauth/callback", scheme, hostport), nil
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
		http.Error(w, "qobuz client unavailable", http.StatusServiceUnavailable)
		return
	}
	callbackURL, err := s.resolveOAuthCallbackURL(r)
	if err != nil {
		http.Error(w, err.Error(), http.StatusBadRequest)
		return
	}
	authURL, err := s.qobuz.OAuthURL(r.Context(), callbackURL)
	if err != nil {
		http.Error(w, "failed to prepare Qobuz OAuth: "+err.Error(), http.StatusBadGateway)
		return
	}
	parsed, err := url.Parse(authURL)
	if err != nil || parsed.Scheme != "https" || parsed.Hostname() != "www.qobuz.com" {
		http.Error(w, "invalid Qobuz OAuth redirect target", http.StatusBadGateway)
		return
	}
	stateTok, err := s.issueOAuthState()
	if err != nil {
		http.Error(w, "failed to initialize OAuth session", http.StatusInternalServerError)
		return
	}
	http.SetCookie(w, &http.Cookie{ //nolint:gosec // Secure enabled when TLS is active; LAN control point commonly runs over plain HTTP
		Name:     oauthStateCookieName,
		Value:    stateTok,
		Path:     "/api/qobuz/oauth/callback",
		MaxAge:   600,
		HttpOnly: true,
		Secure:   r.TLS != nil,
		SameSite: http.SameSiteLaxMode,
	})
	http.Redirect(w, r, parsed.String(), http.StatusFound) //nolint:gosec // validated https://www.qobuz.com OAuth URL
}

func (s *Server) handleQobuzOAuthCallback(w http.ResponseWriter, r *http.Request) {
	if s.qobuz == nil {
		http.Error(w, "qobuz client unavailable", http.StatusServiceUnavailable)
		return
	}
	cookie, err := r.Cookie(oauthStateCookieName)
	http.SetCookie(w, &http.Cookie{ //nolint:gosec // Secure enabled when TLS is active; LAN control point commonly runs over plain HTTP
		Name:     oauthStateCookieName,
		Value:    "",
		Path:     "/api/qobuz/oauth/callback",
		MaxAge:   -1,
		HttpOnly: true,
		Secure:   r.TLS != nil,
		SameSite: http.SameSiteLaxMode,
	})
	if err != nil || !s.consumeOAuthState(cookie.Value) {
		http.Error(w, "missing or expired Qobuz OAuth session — start sign-in from the web remote", http.StatusForbidden)
		return
	}
	code := strings.TrimSpace(r.URL.Query().Get("code_autorisation"))
	if code == "" {
		code = strings.TrimSpace(r.URL.Query().Get("code"))
	}
	if code == "" {
		http.Error(w, "missing Qobuz OAuth code in callback URL", http.StatusBadRequest)
		return
	}
	if _, err := s.qobuz.ExchangeOAuthCode(r.Context(), code); err != nil {
		http.Error(w, "Qobuz sign-in failed: "+err.Error(), http.StatusUnauthorized)
		return
	}
	http.Redirect(w, r, "/?tab=qobuz", http.StatusFound)
}

func (s *Server) handleQobuzLogin(w http.ResponseWriter, r *http.Request) {
	if s.qobuz == nil {
		writeError(w, http.StatusServiceUnavailable, errors.New("qobuz client unavailable"))
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
		writeError(w, http.StatusBadRequest, err)
		return
	}

	if req.AppID != "" || req.AppSecret != "" {
		s.qobuz.SetCredentials(req.AppID, req.AppSecret, "")
	}
	if strings.TrimSpace(req.AuthToken) != "" {
		st, err := s.qobuz.LoginWithToken(r.Context(), req.AuthToken)
		if err != nil {
			writeError(w, http.StatusUnauthorized, err)
			return
		}
		writeJSON(w, http.StatusOK, st)
		return
	}
	if req.Email != "" && req.Password != "" {
		st, err := s.qobuz.Login(r.Context(), req.Email, req.Password)
		if err != nil {
			writeError(w, http.StatusUnauthorized, err)
			return
		}
		writeJSON(w, http.StatusOK, st)
		return
	}
	st := s.qobuz.Status()
	if !st.Authenticated {
		writeError(w, http.StatusBadRequest, errors.New("sign in via Qobuz OAuth or provide a user_auth_token"))
		return
	}
	writeJSON(w, http.StatusOK, st)
}

func (s *Server) handleQobuzSearch(w http.ResponseWriter, r *http.Request) {
	if s.qobuz == nil {
		writeError(w, http.StatusServiceUnavailable, errors.New("qobuz unavailable"))
		return
	}
	q := strings.TrimSpace(r.URL.Query().Get("q"))
	res, err := s.qobuz.Search(r.Context(), q, 30)
	if err != nil {
		writeError(w, http.StatusBadGateway, err)
		return
	}
	writeJSON(w, http.StatusOK, res)
}

func (s *Server) handleQobuzAlbum(w http.ResponseWriter, r *http.Request) {
	if s.qobuz == nil {
		writeError(w, http.StatusServiceUnavailable, errors.New("qobuz unavailable"))
		return
	}
	id := strings.TrimPrefix(r.PathValue("id"), "qobuz:")
	alb, err := s.qobuz.GetAlbum(r.Context(), id)
	if err != nil {
		writeError(w, http.StatusBadGateway, err)
		return
	}
	writeJSON(w, http.StatusOK, alb)
}
