package api

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"log"
	"net"
	"net/http"
	"net/netip"
	"net/url"
	"strconv"
	"strings"
	"time"

	"tacet/internal/library"
	"tacet/internal/qobuz"
)

const oauthStateCookieName = "tacet_qobuz_oauth"

func (s *Server) handleLibraryStatus(w http.ResponseWriter, _ *http.Request) {
	writeJSON(w, http.StatusOK, s.catalog.Status())
}

func (s *Server) handleLibraryAlbums(w http.ResponseWriter, r *http.Request) {
	limit, offset, err := parsePageParams(r, 200, 500)
	if err != nil {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
		return
	}
	qv := r.URL.Query()
	q := strings.TrimSpace(qv.Get("q"))
	sortBy := strings.TrimSpace(qv.Get("sort"))
	sourceType := strings.TrimSpace(qv.Get("source"))
	provider := strings.TrimSpace(qv.Get("provider"))
	artist := strings.TrimSpace(qv.Get("artist"))
	genre := strings.TrimSpace(qv.Get("genre"))
	groupVersions := qv.Get("groupVersions") == "true"
	includeUnavailable := qv.Get("includeUnavailable") == "true"

	if q == "" && artist == "" && genre == "" {
		baseTag := strings.Trim(s.catalog.ETag(), `"`)
		etag := fmt.Sprintf(`"%s-%s-%s-%s-%v-%v-%d-%d"`, baseTag, sortBy, sourceType, provider, groupVersions, includeUnavailable, limit, offset)
		w.Header().Set("ETag", etag)
		if match := strings.TrimSpace(r.Header.Get("If-None-Match")); match == etag {
			w.WriteHeader(http.StatusNotModified)
			return
		}
	}

	items, total, err := s.catalog.QueryAlbums(library.AlbumFilter{
		Query:              q,
		SourceType:         sourceType,
		Provider:           provider,
		Artist:             artist,
		Genre:              genre,
		Sort:               sortBy,
		GroupVersions:      groupVersions,
		IncludeUnavailable: includeUnavailable,
		Limit:              limit,
		Offset:             offset,
	})
	if err != nil {
		writeAPIError(w, http.StatusInternalServerError, "LIBRARY_ERROR", err.Error(), true)
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"items":  items,
		"total":  total,
		"limit":  limit,
		"offset": offset,
	})
}

func (s *Server) handleLibraryAlbum(w http.ResponseWriter, r *http.Request) {
	id := r.PathValue("id")
	alb, ok := s.catalog.Album(id)
	if !ok {
		writeDomainError(w, fmt.Errorf("%w: %q", ErrAlbumNotFound, id))
		return
	}
	// Best-effort enrich in-library Qobuz albums with cached editorial metadata.
	if alb.Source == library.SourceQobuz && alb.ExternalID != "" && s.qobuz != nil && s.qobuz.Status().Authenticated {
		if qAlb, err := s.qobuz.GetAlbum(r.Context(), alb.ExternalID); err == nil {
			if alb.Label == "" {
				alb.Label = qAlb.Label
			}
			if alb.Description == "" {
				alb.Description = qAlb.Description
			}
			if alb.Copyright == "" {
				alb.Copyright = qAlb.Copyright
			}
		}
	}
	writeJSON(w, http.StatusOK, alb)
}

func (s *Server) handleLibraryTracks(w http.ResponseWriter, r *http.Request) {
	limit, offset, err := parsePageParams(r, 200, 1000)
	if err != nil {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
		return
	}
	qv := r.URL.Query()
	items, total, err := s.catalog.QueryTracks(library.TrackFilter{
		Query:              strings.TrimSpace(qv.Get("q")),
		SourceType:         strings.TrimSpace(qv.Get("source")),
		Provider:           strings.TrimSpace(qv.Get("provider")),
		Artist:             strings.TrimSpace(qv.Get("artist")),
		AlbumID:            strings.TrimSpace(qv.Get("albumId")),
		Album:              strings.TrimSpace(qv.Get("album")),
		Genre:              strings.TrimSpace(qv.Get("genre")),
		Sort:               strings.TrimSpace(qv.Get("sort")),
		SortDir:            strings.TrimSpace(qv.Get("dir")),
		IncludeUnavailable: qv.Get("includeUnavailable") == "true",
		Limit:              limit,
		Offset:             offset,
	})
	if err != nil {
		writeAPIError(w, http.StatusInternalServerError, "LIBRARY_ERROR", err.Error(), true)
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"items":  items,
		"total":  total,
		"limit":  limit,
		"offset": offset,
	})
}

func (s *Server) handleLibraryArtists(w http.ResponseWriter, r *http.Request) {
	limit, offset, err := parsePageParams(r, 500, 1000)
	if err != nil {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
		return
	}
	qv := r.URL.Query()
	items, total, err := s.catalog.ListArtists(library.FacetFilter{
		Query:              strings.TrimSpace(qv.Get("q")),
		SourceType:         strings.TrimSpace(qv.Get("source")),
		Provider:           strings.TrimSpace(qv.Get("provider")),
		Genre:              strings.TrimSpace(qv.Get("genre")),
		IncludeUnavailable: qv.Get("includeUnavailable") == "true",
		Limit:              limit,
		Offset:             offset,
	})
	if err != nil {
		writeAPIError(w, http.StatusInternalServerError, "LIBRARY_ERROR", err.Error(), true)
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"items":  items,
		"total":  total,
		"limit":  limit,
		"offset": offset,
	})
}

func (s *Server) handleLibraryGenres(w http.ResponseWriter, r *http.Request) {
	qv := r.URL.Query()
	items, err := s.catalog.ListGenres(library.FacetFilter{
		SourceType:         strings.TrimSpace(qv.Get("source")),
		Provider:           strings.TrimSpace(qv.Get("provider")),
		IncludeUnavailable: qv.Get("includeUnavailable") == "true",
	})
	if err != nil {
		writeAPIError(w, http.StatusInternalServerError, "LIBRARY_ERROR", err.Error(), true)
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"items": items,
	})
}

func (s *Server) handleHeavyRotation(w http.ResponseWriter, r *http.Request) {
	days := 30
	if raw := strings.TrimSpace(r.URL.Query().Get("days")); raw != "" {
		if v, err := strconv.Atoi(raw); err == nil && v > 0 && v <= 365 {
			days = v
		}
	}
	limit := 20
	if raw := strings.TrimSpace(r.URL.Query().Get("limit")); raw != "" {
		if v, err := strconv.Atoi(raw); err == nil && v > 0 && v <= 100 {
			limit = v
		}
	}
	items, err := s.catalog.HeavyRotation(days, limit)
	if err != nil {
		writeAPIError(w, http.StatusInternalServerError, "LIBRARY_ERROR", err.Error(), true)
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"days":  days,
		"items": items,
	})
}

func (s *Server) handleLibraryRescan(w http.ResponseWriter, r *http.Request) {
	wait := r.URL.Query().Get("wait") == "true"
	if wait {
		if _, err := s.catalog.Scan(); err != nil {
			writeAPIError(w, http.StatusInternalServerError, "SCAN_FAILED", err.Error(), true)
			return
		}
		st := s.catalog.Status()
		s.manager.Publish("library.updated", st)
		s.PrewarmArt(context.Background())
		s.PrewarmArtists(context.Background())
		writeJSON(w, http.StatusOK, st)
		return
	}

	go func() {
		defer func() {
			s.manager.Publish("library.updated", s.catalog.Status())
		}()
		if _, err := s.catalog.Scan(); err != nil {
			log.Printf("async library rescan failed: %v", err)
			return
		}
		s.PrewarmArt(context.Background())
		s.PrewarmArtists(context.Background())
	}()

	st := s.catalog.Status()
	st.Scanning = true
	writeJSON(w, http.StatusAccepted, st)
}

func (s *Server) handleLibrarySync(w http.ResponseWriter, r *http.Request) {
	provider := strings.ToLower(strings.TrimSpace(r.URL.Query().Get("provider")))
	wait := r.URL.Query().Get("wait") == "true"

	runSync := func(ctx context.Context) error {
		if provider == "" || provider == "all" || provider == "local" {
			if len(s.catalog.Roots()) > 0 {
				if _, err := s.catalog.ScanContext(ctx); err != nil && provider == "local" {
					return err
				}
				s.PrewarmArt(context.Background())
			}
		}
		if provider == "" || provider == "all" || provider == "qobuz" {
			if s.qobuz != nil && s.qobuz.IsConnected() {
				if _, err := s.catalog.SyncRemote(ctx, "qobuz"); err != nil && provider == "qobuz" {
					return err
				}
			} else if provider == "qobuz" {
				return library.ErrProviderOffline
			}
		} else if provider != "local" {
			if _, err := s.catalog.SyncRemote(ctx, provider); err != nil {
				return err
			}
		}
		return nil
	}

	if wait {
		if err := runSync(r.Context()); err != nil {
			writeDomainError(w, err)
			return
		}
		writeJSON(w, http.StatusOK, s.catalog.Status())
		return
	}

	go func() {
		sctx, cancel := context.WithTimeout(context.WithoutCancel(r.Context()), 3*time.Minute)
		defer cancel()
		if err := runSync(sctx); err != nil {
			log.Printf("async library sync (%s) warning: %v", provider, err)
		}
		s.manager.Publish("library.updated", s.catalog.Status())
	}()

	st := s.catalog.Status()
	st.Scanning = true
	writeJSON(w, http.StatusAccepted, st)
}

func (s *Server) handleAddRemoteAlbum(w http.ResponseWriter, r *http.Request) {
	var req struct {
		Provider   string `json:"provider"`
		ExternalID string `json:"externalId"`
	}
	if err := decodeJSON(w, r, &req); err != nil {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
		return
	}
	if strings.TrimSpace(req.Provider) == "" {
		req.Provider = "qobuz"
	}
	alb, err := s.catalog.AddRemoteAlbum(r.Context(), req.Provider, req.ExternalID)
	if err != nil {
		writeDomainError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, alb)
}

func (s *Server) handleRemoveRemoteAlbum(w http.ResponseWriter, r *http.Request) {
	id := r.PathValue("id")
	if err := s.catalog.RemoveRemoteAlbum(r.Context(), id); err != nil {
		writeDomainError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"removed": true, "id": id})
}

func (s *Server) handleAddRemoteTrack(w http.ResponseWriter, r *http.Request) {
	var req struct {
		Provider   string `json:"provider"`
		ExternalID string `json:"externalId"`
	}
	if err := decodeJSON(w, r, &req); err != nil {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
		return
	}
	if strings.TrimSpace(req.Provider) == "" {
		req.Provider = "qobuz"
	}
	tr, err := s.catalog.AddRemoteTrack(r.Context(), req.Provider, req.ExternalID)
	if err != nil {
		writeDomainError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, tr)
}

func (s *Server) handleRemoveRemoteTrack(w http.ResponseWriter, r *http.Request) {
	id := r.PathValue("id")
	if err := s.catalog.RemoveRemoteTrack(r.Context(), id); err != nil {
		writeDomainError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"removed": true, "id": id})
}

func (s *Server) handleSetPrimaryVersion(w http.ResponseWriter, r *http.Request) {
	id := r.PathValue("id")
	alb, err := s.catalog.SetPrimaryVersion(id)
	if err != nil {
		writeDomainError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, alb)
}

func (s *Server) handleGroupVersions(w http.ResponseWriter, r *http.Request) {
	var req struct {
		AlbumIDs []string `json:"albumIds"`
	}
	if err := decodeJSON(w, r, &req); err != nil {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
		return
	}
	gid, err := s.catalog.GroupVersions(req.AlbumIDs)
	if err != nil {
		writeDomainError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{"versionGroupId": gid})
}

func (s *Server) handleUngroupVersion(w http.ResponseWriter, r *http.Request) {
	id := r.PathValue("id")
	alb, err := s.catalog.UngroupVersion(id)
	if err != nil {
		writeDomainError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, alb)
}

func (s *Server) triggerQobuzLibrarySyncAsync() {
	go func() {
		sctx, cancel := context.WithTimeout(context.Background(), 2*time.Minute)
		defer cancel()
		if _, err := s.catalog.SyncRemote(sctx, "qobuz"); err != nil {
			log.Printf("auto Qobuz favorites sync warning: %v", err)
		}
		s.PrewarmArtists(context.Background())
	}()
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

var cgnatPrefix = netip.MustParsePrefix("100.64.0.0/10")

var localOAuthSuffixes = []string{
	".localhost",
	".local",
	".lan",
	".home",
	".home.arpa",
	".internal",
	".localdomain",
	".ts.net",
}

func isValidDNSLabel(label string) bool {
	if len(label) == 0 || len(label) > 63 {
		return false
	}
	if label[0] == '-' || label[len(label)-1] == '-' {
		return false
	}
	for i := 0; i < len(label); i++ {
		c := label[i]
		isAlpha := (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z')
		isDigit := c >= '0' && c <= '9'
		if !isAlpha && !isDigit && c != '-' {
			return false
		}
	}
	return true
}

func isValidHostname(host string) bool {
	if len(host) == 0 || len(host) > 253 {
		return false
	}
	labels := strings.Split(host, ".")
	for _, label := range labels {
		if !isValidDNSLabel(label) {
			return false
		}
	}
	return true
}

// isLocalOAuthHost returns true for loopback, private/CGNAT LAN IPs, single-label
// LAN/MagicDNS hostnames (e.g. "kamek"), and local-only DNS suffixes (e.g. ".local",
// ".lan", ".ts.net") while rejecting public internet domains and public IPs.
func isLocalOAuthHost(host string) bool {
	host = strings.TrimSuffix(strings.ToLower(strings.TrimSpace(host)), ".")
	if host == "" {
		return false
	}
	if host == "localhost" {
		return true
	}
	if addr, err := netip.ParseAddr(host); err == nil {
		addr = addr.Unmap()
		return addr.IsLoopback() || addr.IsPrivate() || addr.IsLinkLocalUnicast() || cgnatPrefix.Contains(addr)
	}
	if !isValidHostname(host) {
		return false
	}
	// Single-label hostnames (e.g. "kamek", "nas") are never publicly routable.
	if !strings.Contains(host, ".") {
		return true
	}
	for _, suffix := range localOAuthSuffixes {
		if strings.HasSuffix(host, suffix) && len(host) > len(suffix) {
			return true
		}
	}
	return false
}

func isRequestHTTPS(r *http.Request) bool {
	return r.TLS != nil || strings.EqualFold(r.Header.Get("X-Forwarded-Proto"), "https")
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
	if h, portStr, err := net.SplitHostPort(hostport); err == nil {
		if p, pErr := strconv.Atoi(portStr); pErr != nil || p < 1 || p > 65535 {
			return "", fmt.Errorf("invalid OAuth callback port in Host %q", hostport)
		}
		host = h
	}
	host = strings.Trim(host, "[]")
	if !isLocalOAuthHost(host) {
		return "", fmt.Errorf("OAuth callback host %q is not a local/LAN host; set --public-url (TACET_PUBLIC_URL) to allow custom hosts", host)
	}
	scheme := "http"
	if isRequestHTTPS(r) {
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
		Secure:   isRequestHTTPS(r),
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
		Secure:   isRequestHTTPS(r),
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
	if s.catalog != nil {
		if saveErr := s.catalog.SetProviderCredentials(r.Context(), "qobuz", s.qobuz.ExportCredentials()); saveErr != nil {
			log.Printf("warning: failed to persist Qobuz OAuth credentials: %v", saveErr)
		}
	}
	s.manager.Publish("qobuz.status", st)
	s.triggerQobuzLibrarySyncAsync()
	http.Redirect(w, r, "/qobuz", http.StatusFound)
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
		if s.catalog != nil {
			if saveErr := s.catalog.SetProviderCredentials(r.Context(), "qobuz", s.qobuz.ExportCredentials()); saveErr != nil {
				log.Printf("warning: failed to persist Qobuz token credentials: %v", saveErr)
			}
		}
		s.manager.Publish("qobuz.status", st)
		s.triggerQobuzLibrarySyncAsync()
		writeJSON(w, http.StatusOK, st)
		return
	}
	if req.Email != "" && req.Password != "" {
		st, err := s.qobuz.Login(r.Context(), req.Email, req.Password)
		if err != nil {
			writeAPIError(w, http.StatusUnauthorized, "QOBUZ_AUTH_FAILED", err.Error(), false)
			return
		}
		if s.catalog != nil {
			if saveErr := s.catalog.SetProviderCredentials(r.Context(), "qobuz", s.qobuz.ExportCredentials()); saveErr != nil {
				log.Printf("warning: failed to persist Qobuz login credentials: %v", saveErr)
			}
		}
		s.manager.Publish("qobuz.status", st)
		s.triggerQobuzLibrarySyncAsync()
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

func (s *Server) handleQobuzLogout(w http.ResponseWriter, r *http.Request) {
	if s.qobuz == nil {
		writeDomainError(w, ErrQobuzUnavailable)
		return
	}
	st := s.qobuz.Logout()
	if s.catalog != nil {
		_ = s.catalog.DeleteProviderCredentials(r.Context(), "qobuz")
		_ = s.catalog.SetProviderAvailable(r.Context(), "qobuz", false)
	}
	s.manager.Publish("qobuz.status", st)
	writeJSON(w, http.StatusOK, st)
}

func (s *Server) handleLibrarySearch(w http.ResponseWriter, r *http.Request) {
	q := strings.TrimSpace(r.URL.Query().Get("q"))
	if q == "" {
		writeJSON(w, http.StatusOK, library.UnifiedSearchResult{
			Query:   "",
			Artists: make([]library.ArtistSummary, 0),
			Albums:  make([]library.Album, 0),
			Tracks:  make([]library.Track, 0),
		})
		return
	}
	limit, _, err := parsePageParams(r, 8, 50)
	if err != nil {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
		return
	}
	res, err := s.catalog.UnifiedSearch(r.Context(), q, limit)
	if err != nil {
		writeDomainError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, res)
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
	for i := range res.Albums {
		res.Albums[i].InLibrary = s.catalog.HasAlbum(res.Albums[i].ID)
	}
	for i := range res.Tracks {
		tr, inLib := s.catalog.Track(res.Tracks[i].ID)
		res.Tracks[i].InLibrary = inLib && tr.Available
	}
	writeJSON(w, http.StatusOK, res)
}

func (s *Server) handleQobuzArtist(w http.ResponseWriter, r *http.Request) {
	if s.qobuz == nil {
		writeDomainError(w, ErrQobuzUnavailable)
		return
	}
	limit, _, err := parsePageParams(r, 50, 200)
	if err != nil {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
		return
	}

	rawID := strings.TrimSpace(r.URL.Query().Get("id"))
	name := strings.TrimSpace(r.URL.Query().Get("name"))
	if rawID == "" && name == "" {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", "id or name query parameter is required", false)
		return
	}

	var detail qobuz.ArtistDetail
	if rawID != "" {
		id, parseErr := strconv.ParseInt(rawID, 10, 64)
		if parseErr != nil || id <= 0 {
			writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", "id must be a positive integer", false)
			return
		}
		detail, err = s.qobuz.GetArtist(r.Context(), id, limit)
	} else {
		detail, err = s.qobuz.GetArtistByName(r.Context(), name, limit)
	}
	if err != nil {
		writeDomainError(w, err)
		return
	}
	for i := range detail.Albums {
		detail.Albums[i].InLibrary = s.catalog.HasAlbum(detail.Albums[i].ID)
	}
	for i := range detail.Tracks {
		if libTr, ok := s.catalog.Track(detail.Tracks[i].ID); ok {
			detail.Tracks[i].InLibrary = libTr.Available
			detail.Tracks[i].PlayCount = libTr.PlayCount
			detail.Tracks[i].LastPlayedAt = libTr.LastPlayedAt
		}
	}
	writeJSON(w, http.StatusOK, detail)
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
	// Enrich with library membership and sibling versions so the unified Album
	// Landing Page renders identically for in-library and out-of-library albums.
	if libAlb, ok := s.catalog.Album(alb.ID); ok {
		if libAlb.Available {
			alb.IsPartial = libAlb.IsPartial
			alb.InLibrary = !libAlb.IsPartial
			alb.VersionGroupID = libAlb.VersionGroupID
			alb.VersionCount = libAlb.VersionCount
			alb.IsPrimary = libAlb.IsPrimary
			alb.Versions = libAlb.Versions
			alb.AddedAt = libAlb.AddedAt
		}
		alb.PlayCount = libAlb.PlayCount
	}
	for i := range alb.Tracks {
		if libTr, ok := s.catalog.Track(alb.Tracks[i].ID); ok {
			alb.Tracks[i].InLibrary = libTr.Available
			alb.Tracks[i].PlayCount = libTr.PlayCount
			alb.Tracks[i].LastPlayedAt = libTr.LastPlayedAt
		}
	}
	writeJSON(w, http.StatusOK, alb)
}
