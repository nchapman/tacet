package qobuz

import (
	"context"
	"encoding/json"
	"net/url"
	"strings"
	"time"
	"unicode"

	"tacet/internal/cache"
)

const (
	albumCacheTTL          = 7 * 24 * time.Hour
	albumMatchCacheTTL     = 7 * 24 * time.Hour
	albumMatchMissCacheTTL = 24 * time.Hour
	artistCacheTTL         = 7 * 24 * time.Hour
	artistNameCacheTTL     = 7 * 24 * time.Hour
	artistNameMissCacheTTL = 24 * time.Hour
	searchCacheTTL         = time.Hour
)

type cachedArtistName struct {
	ID int64 `json:"id"`
}

type cachedAlbumMatch struct {
	AlbumID string `json:"albumId"`
}

// rawCacheKey builds a deterministic cache key for a raw upstream Qobuz HTTP JSON
// response. url.Values.Encode sorts keys alphabetically, and app_id is excluded so
// keys remain stable across app credential refreshes.
func rawCacheKey(path string, params url.Values) string {
	if len(params) == 0 {
		return "raw:" + path
	}
	clean := make(url.Values, len(params))
	for k, vals := range params {
		if k == "app_id" {
			continue
		}
		if k == "query" {
			norm := make([]string, len(vals))
			for i, v := range vals {
				norm[i] = strings.ToLower(strings.TrimSpace(v))
			}
			clean[k] = norm
			continue
		}
		clean[k] = vals
	}
	if len(clean) == 0 {
		return "raw:" + path
	}
	return "raw:" + path + "?" + clean.Encode()
}

// getCachedJSON returns a cached raw Qobuz JSON response unmarshaled into dst,
// or fetches, unmarshals, and caches the raw upstream JSON on a miss.
// Caching the raw upstream payload means parser/model additions (e.g. new fields
// or improved filtering) take effect immediately without cache invalidation.
func (c *Client) getCachedJSON(ctx context.Context, path string, params url.Values, ttl time.Duration, dst any) error {
	return c.getCachedJSONValid(ctx, path, params, ttl, dst, nil)
}

// getCachedJSONValid behaves like getCachedJSON, but only persists the raw body
// when valid returns true (protecting long-TTL caches from transient degraded
// or empty 200 OK upstream payloads).
func (c *Client) getCachedJSONValid(ctx context.Context, path string, params url.Values, ttl time.Duration, dst any, valid func() bool) error {
	if c.kv == nil || ttl <= 0 {
		return c.getJSON(ctx, path, params, true, dst)
	}
	key := rawCacheKey(path, params)
	if raw, ok, err := c.kv.Get(key); err == nil && ok {
		if err := json.Unmarshal(raw, dst); err == nil {
			if valid == nil || valid() {
				return nil
			}
		}
		// Corrupt or semantically empty cached payload: fall through to re-fetch and heal.
	}
	gen := c.authGen.Load()
	body, err := c.getRawJSON(ctx, path, params, true)
	if err != nil {
		return err
	}
	if err := json.Unmarshal(body, dst); err != nil {
		return err
	}
	if c.authGen.Load() == gen && (valid == nil || valid()) {
		_ = c.kv.Set(key, body, ttl)
	}
	return nil
}

func (c *Client) hasCachedJSON(path string, params url.Values) bool {
	if c.kv == nil {
		return false
	}
	_, ok, err := c.kv.Get(rawCacheKey(path, params))
	return err == nil && ok
}

func albumMatchCacheKey(artist, title string) string {
	return "album-match:" + normMatchKey(artist) + "|" + normMatchKey(title)
}

func artistNameCacheKey(name string) string {
	return "artist-name:" + normMatchKey(name)
}

func (c *Client) getCachedAlbumMatch(artist, title string) (string, bool) {
	key := albumMatchCacheKey(artist, title)
	if key == "album-match:|" {
		return "", false
	}
	entry, ok, err := cache.GetJSON[cachedAlbumMatch](c.kv, key)
	if err != nil || !ok {
		return "", false
	}
	return entry.AlbumID, true
}

func (c *Client) putCachedAlbumMatch(artist, title, qobuzAlbumID string) {
	key := albumMatchCacheKey(artist, title)
	if key == "album-match:|" {
		return
	}
	ttl := albumMatchCacheTTL
	if qobuzAlbumID == "" {
		ttl = albumMatchMissCacheTTL
	}
	_ = cache.SetJSON(c.kv, key, cachedAlbumMatch{AlbumID: qobuzAlbumID}, ttl)
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
