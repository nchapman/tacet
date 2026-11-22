package qobuz

import (
	"fmt"
	"strconv"
	"strings"
	"time"
	"unicode"

	"tacet/internal/cache"
	"tacet/internal/library"
)

const (
	albumCacheTTL          = 7 * 24 * time.Hour
	artistCacheTTL         = 7 * 24 * time.Hour
	artistNameCacheTTL     = 7 * 24 * time.Hour
	artistNameMissCacheTTL = 24 * time.Hour
	searchCacheTTL         = time.Hour
)

type cachedArtistName struct {
	ID int64 `json:"id"`
}

func albumCacheKey(rawID string) string {
	return "album:" + strings.TrimSpace(rawID)
}

func artistCacheKey(artistID int64, limit int) string {
	return fmt.Sprintf("artist:%d:%d", artistID, limit)
}

func artistNameCacheKey(name string) string {
	return "artist-name:" + normMatchKey(name)
}

func searchCacheKey(query string, limit, offset int) string {
	return fmt.Sprintf("search:%s:%d:%d", strings.ToLower(strings.TrimSpace(query)), limit, offset)
}

func (c *Client) getCachedAlbum(rawID string) (library.Album, bool) {
	alb, ok, err := cache.GetJSON[library.Album](c.kv, albumCacheKey(rawID))
	if err != nil || !ok {
		return library.Album{}, false
	}
	rehydrateTracks(alb.Tracks)
	return alb, true
}

func (c *Client) putCachedAlbum(rawID string, alb library.Album) {
	if strings.TrimSpace(rawID) == "" || len(alb.Tracks) == 0 {
		return
	}
	_ = cache.SetJSON(c.kv, albumCacheKey(rawID), alb, albumCacheTTL)
}

func (c *Client) getCachedArtist(artistID int64, limit int) (ArtistDetail, bool) {
	detail, ok, err := cache.GetJSON[ArtistDetail](c.kv, artistCacheKey(artistID, limit))
	if err != nil || !ok {
		return ArtistDetail{}, false
	}
	rehydrateTracks(detail.Tracks)
	for i := range detail.Albums {
		rehydrateTracks(detail.Albums[i].Tracks)
	}
	return detail, true
}

func (c *Client) putCachedArtist(artistID int64, limit int, detail ArtistDetail) {
	if artistID <= 0 {
		return
	}
	_ = cache.SetJSON(c.kv, artistCacheKey(artistID, limit), detail, artistCacheTTL)
}

func (c *Client) getCachedArtistName(name string) (int64, bool) {
	key := artistNameCacheKey(name)
	if key == "artist-name:" {
		return 0, false
	}
	entry, ok, err := cache.GetJSON[cachedArtistName](c.kv, key)
	if err != nil || !ok {
		return 0, false
	}
	return entry.ID, true
}

func (c *Client) putCachedArtistName(name string, id int64, overwrite bool) {
	key := artistNameCacheKey(name)
	if key == "artist-name:" {
		return
	}
	if !overwrite {
		// Preserve an existing positive artist ID, but allow upgrading a cached miss (ID 0).
		if existingID, ok := c.getCachedArtistName(name); ok && existingID > 0 {
			return
		}
	}
	ttl := artistNameCacheTTL
	if id == 0 {
		ttl = artistNameMissCacheTTL
	}
	_ = cache.SetJSON(c.kv, key, cachedArtistName{ID: id}, ttl)
}

func (c *Client) getCachedSearch(query string, limit, offset int) (SearchResult, bool) {
	res, ok, err := cache.GetJSON[SearchResult](c.kv, searchCacheKey(query, limit, offset))
	if err != nil || !ok {
		return SearchResult{}, false
	}
	rehydrateTracks(res.Tracks)
	for i := range res.Albums {
		rehydrateTracks(res.Albums[i].Tracks)
	}
	return res, true
}

func (c *Client) putCachedSearch(query string, limit, offset int, res SearchResult) {
	if strings.TrimSpace(query) == "" {
		return
	}
	_ = cache.SetJSON(c.kv, searchCacheKey(query, limit, offset), res, searchCacheTTL)
}

func normMatchKey(s string) string {
	var b strings.Builder
	b.Grow(len(s))
	for _, r := range strings.ToLower(strings.TrimSpace(s)) {
		if unicode.IsLetter(r) || unicode.IsDigit(r) {
			b.WriteRune(r)
		}
	}
	return b.String()
}

func rehydrateTracks(tracks []library.Track) {
	for i := range tracks {
		rehydrateTrackQobuzID(&tracks[i])
	}
}

// rehydrateTrackQobuzID restores QobuzTrackID (which is json:"-" on library.Track)
// from ID ("qobuz:<id>") after JSON deserialization from the cache.
func rehydrateTrackQobuzID(t *library.Track) {
	if t == nil || t.QobuzTrackID > 0 {
		return
	}
	raw, ok := strings.CutPrefix(strings.TrimSpace(t.ID), "qobuz:")
	if !ok {
		return
	}
	if id, err := strconv.ParseInt(raw, 10, 64); err == nil && id > 0 {
		t.QobuzTrackID = id
	}
}
