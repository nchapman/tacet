package api

import (
	"context"
	"errors"
	"net/http"
	"strconv"
	"strings"

	"tacet/internal/library"
	"tacet/internal/qobuz"
)

func parseGenreIDsParam(raw string) []int64 {
	raw = strings.TrimSpace(raw)
	if raw == "" {
		return nil
	}
	parts := strings.Split(raw, ",")
	out := make([]int64, 0, len(parts))
	for _, p := range parts {
		id, err := strconv.ParseInt(strings.TrimSpace(p), 10, 64)
		if err == nil && id > 0 {
			out = append(out, id)
		}
	}
	return out
}

func (s *Server) markAlbumsInLibrary(ctx context.Context, albums []library.Album) []library.Album {
	if s.catalog == nil || len(albums) == 0 {
		return albums
	}
	return s.catalog.PreferLocalAlbums(ctx, albums)
}

func (s *Server) handleDiscover(w http.ResponseWriter, r *http.Request) {
	type discoverResp struct {
		Connected bool                  `json:"connected"`
		Genres    []qobuz.DiscoverGenre `json:"genres"`
		Shelves   []qobuz.DiscoverShelf `json:"shelves"`
	}
	if s.qobuz == nil || !s.qobuz.IsConnected() {
		writeJSON(w, http.StatusOK, discoverResp{
			Connected: false,
			Genres:    []qobuz.DiscoverGenre{},
			Shelves:   []qobuz.DiscoverShelf{},
		})
		return
	}

	genreIDs := parseGenreIDsParam(r.URL.Query().Get("genre_ids"))
	feed, err := s.qobuz.Discover(r.Context(), genreIDs)
	if err != nil {
		writeDomainError(w, err)
		return
	}

	shelves := make([]qobuz.DiscoverShelf, len(feed.Shelves))
	for i, sh := range feed.Shelves {
		sh.Items = s.markAlbumsInLibrary(r.Context(), sh.Items)
		shelves[i] = sh
	}
	genres := feed.Genres
	if genres == nil {
		genres = []qobuz.DiscoverGenre{}
	}
	writeJSON(w, http.StatusOK, discoverResp{
		Connected: true,
		Genres:    genres,
		Shelves:   shelves,
	})
}

func (s *Server) handleDiscoverShelf(w http.ResponseWriter, r *http.Request) {
	if s.qobuz == nil {
		writeDomainError(w, ErrQobuzUnavailable)
		return
	}
	slug := r.PathValue("slug")
	genreIDs := parseGenreIDsParam(r.URL.Query().Get("genre_ids"))
	limit, offset, err := parsePageParams(r, 24, 100)
	if err != nil {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
		return
	}

	page, err := s.qobuz.DiscoverShelf(r.Context(), slug, genreIDs, limit, offset)
	if err != nil {
		writeDomainError(w, err)
		return
	}
	page.Items = s.markAlbumsInLibrary(r.Context(), page.Items)
	writeJSON(w, http.StatusOK, page)
}

func (s *Server) handleSimilarAlbums(w http.ResponseWriter, r *http.Request) {
	type similarResp struct {
		Items []library.Album `json:"items"`
	}
	if s.qobuz == nil || !s.qobuz.IsConnected() {
		writeJSON(w, http.StatusOK, similarResp{Items: []library.Album{}})
		return
	}
	id := strings.TrimSpace(r.PathValue("id"))
	if id == "" {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", "album id is required", false)
		return
	}
	// Resolve Qobuz ID either directly ("qobuz:<ext>") or via sibling version in the library.
	qobuzID := ""
	if strings.HasPrefix(id, "qobuz:") {
		qobuzID = strings.TrimPrefix(id, "qobuz:")
	} else if s.catalog != nil {
		if alb, ok := s.catalog.Album(id); ok {
			for _, v := range alb.Versions {
				if strings.HasPrefix(v.ID, "qobuz:") {
					qobuzID = strings.TrimPrefix(v.ID, "qobuz:")
					break
				}
			}
		}
	}
	if qobuzID == "" {
		writeJSON(w, http.StatusOK, similarResp{Items: []library.Album{}})
		return
	}

	limit, _, err := parsePageParams(r, 12, 50)
	if err != nil {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
		return
	}
	items, err := s.qobuz.SimilarAlbums(r.Context(), qobuzID, limit)
	if err != nil {
		// Contextual suggestions are best-effort; return empty on 404 so album pages never break.
		if errors.Is(err, qobuz.ErrNotFound) {
			writeJSON(w, http.StatusOK, similarResp{Items: []library.Album{}})
			return
		}
		writeDomainError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, similarResp{Items: s.markAlbumsInLibrary(r.Context(), items)})
}

func (s *Server) handleSimilarArtists(w http.ResponseWriter, r *http.Request) {
	type similarResp struct {
		Items []qobuz.Artist `json:"items"`
	}
	if s.qobuz == nil || !s.qobuz.IsConnected() {
		writeJSON(w, http.StatusOK, similarResp{Items: []qobuz.Artist{}})
		return
	}
	artistID, err := strconv.ParseInt(strings.TrimSpace(r.PathValue("id")), 10, 64)
	if err != nil || artistID <= 0 {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", "artist id must be a positive integer", false)
		return
	}
	limit, _, err := parsePageParams(r, 12, 50)
	if err != nil {
		writeAPIError(w, http.StatusBadRequest, "INVALID_REQUEST", err.Error(), false)
		return
	}
	items, err := s.qobuz.SimilarArtists(r.Context(), artistID, limit)
	if err != nil {
		if errors.Is(err, qobuz.ErrNotFound) {
			writeJSON(w, http.StatusOK, similarResp{Items: []qobuz.Artist{}})
			return
		}
		writeDomainError(w, err)
		return
	}
	writeJSON(w, http.StatusOK, similarResp{Items: items})
}
