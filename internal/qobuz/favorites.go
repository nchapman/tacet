package qobuz

import (
	"context"
	"errors"
	"fmt"
	"log"
	"net/url"
	"strconv"
	"strings"
	"sync"

	"tacet/internal/library"
)

type remoteSourceAdapter struct {
	c *Client
}

// AsRemoteSource exposes this Qobuz client as a generic library.RemoteSource.
func (c *Client) AsRemoteSource() library.RemoteSource {
	return &remoteSourceAdapter{c: c}
}

func (a *remoteSourceAdapter) ProviderID() string {
	return string(library.SourceQobuz)
}

func (a *remoteSourceAdapter) IsConnected() bool {
	return a.c.IsConnected()
}

func (a *remoteSourceAdapter) SyncCollection(ctx context.Context) (library.RemoteCollection, error) {
	return a.c.SyncFavorites(ctx)
}

func (a *remoteSourceAdapter) GetAlbum(ctx context.Context, externalID string) (library.Album, error) {
	return a.c.GetAlbum(ctx, externalID)
}

func (a *remoteSourceAdapter) GetTrack(ctx context.Context, externalID string) (library.Track, error) {
	clean := strings.TrimPrefix(strings.TrimSpace(externalID), "qobuz:")
	id, err := strconv.ParseInt(clean, 10, 64)
	if err != nil || id <= 0 {
		return library.Track{}, fmt.Errorf("invalid qobuz track id %q", externalID)
	}
	return a.c.GetTrack(ctx, id)
}

func (a *remoteSourceAdapter) AddFavorite(ctx context.Context, kind, externalID string) error {
	return a.c.AddFavorite(ctx, kind, externalID)
}

func (a *remoteSourceAdapter) RemoveFavorite(ctx context.Context, kind, externalID string) error {
	return a.c.RemoveFavorite(ctx, kind, externalID)
}

// IsConnected reports whether Qobuz currently holds an authenticated user session.
func (c *Client) IsConnected() bool {
	c.mu.RLock()
	defer c.mu.RUnlock()
	return c.authToken != "" && c.appID != ""
}

type qFavoritesResp struct {
	Albums struct {
		Total  int      `json:"total"`
		Limit  int      `json:"limit"`
		Offset int      `json:"offset"`
		Items  []qAlbum `json:"items"`
	} `json:"albums"`
	Tracks struct {
		Total  int      `json:"total"`
		Limit  int      `json:"limit"`
		Offset int      `json:"offset"`
		Items  []qTrack `json:"items"`
	} `json:"tracks"`
}

// SyncFavorites fetches the authenticated user's Qobuz favorite albums (with full tracklists)
// and favorite standalone tracks for unified library syncing.
func (c *Client) SyncFavorites(ctx context.Context) (library.RemoteCollection, error) {
	if err := c.requireAuth(); err != nil {
		return library.RemoteCollection{}, err
	}

	const pageSize = 500
	var favAlbums []qAlbum
	for offset := 0; ; offset += pageSize {
		params := url.Values{}
		params.Set("type", "albums")
		params.Set("limit", strconv.Itoa(pageSize))
		params.Set("offset", strconv.Itoa(offset))

		var resp qFavoritesResp
		if err := c.getJSON(ctx, "/favorite/getUserFavorites", params, true, &resp); err != nil {
			return library.RemoteCollection{}, fmt.Errorf("fetch qobuz favorite albums: %w", err)
		}
		favAlbums = append(favAlbums, resp.Albums.Items...)
		if len(resp.Albums.Items) < pageSize || (resp.Albums.Total > 0 && len(favAlbums) >= resp.Albums.Total) {
			break
		}
	}

	var favTracks []library.Track
	for offset := 0; ; offset += pageSize {
		params := url.Values{}
		params.Set("type", "tracks")
		params.Set("limit", strconv.Itoa(pageSize))
		params.Set("offset", strconv.Itoa(offset))

		var resp qFavoritesResp
		if err := c.getJSON(ctx, "/favorite/getUserFavorites", params, true, &resp); err != nil {
			return library.RemoteCollection{}, fmt.Errorf("fetch qobuz favorite tracks: %w", err)
		}
		for _, qt := range resp.Tracks.Items {
			tr := qt.toLibraryTrack(nil)
			c.cacheTrack(tr)
			favTracks = append(favTracks, tr)
		}
		if len(resp.Tracks.Items) < pageSize || (resp.Tracks.Total > 0 && len(favTracks) >= resp.Tracks.Total) {
			break
		}
	}

	// Hydrate full tracklists for favorited albums with bounded concurrency (4 workers)
	// so every synced album is searchable by track title and immediately playable.
	albums := make([]library.Album, len(favAlbums))
	sem := make(chan struct{}, 4)
	var wg sync.WaitGroup
	for i, qa := range favAlbums {
		wg.Add(1)
		go func(idx int, stub qAlbum) {
			defer wg.Done()
			select {
			case sem <- struct{}{}:
				defer func() { <-sem }()
			case <-ctx.Done():
				albums[idx] = stub.toLibraryAlbum(false)
				return
			}
			full, err := c.GetAlbum(ctx, stub.ID)
			if err != nil {
				log.Printf("qobuz favorites sync: album %s tracklist fetch failed (%v); using summary", stub.ID, err)
				summary := stub.toLibraryAlbum(false)
				if errors.Is(err, ErrNotFound) {
					summary.Available = false
				}
				albums[idx] = summary
				return
			}
			albums[idx] = full
		}(i, qa)
	}
	wg.Wait()

	return library.RemoteCollection{
		Albums: albums,
		Tracks: favTracks,
	}, nil
}

// AddFavorite marks an album or track as a favorite in the user's Qobuz account.
func (c *Client) AddFavorite(ctx context.Context, kind, externalID string) error {
	return c.mutateFavorite(ctx, "/favorite/create", kind, externalID)
}

// RemoveFavorite removes an album or track from the user's Qobuz favorites.
func (c *Client) RemoveFavorite(ctx context.Context, kind, externalID string) error {
	return c.mutateFavorite(ctx, "/favorite/delete", kind, externalID)
}

func (c *Client) mutateFavorite(ctx context.Context, endpoint, kind, externalID string) error {
	if err := c.requireAuth(); err != nil {
		return err
	}
	clean := strings.TrimPrefix(strings.TrimSpace(externalID), "qobuz:")
	if clean == "" {
		return fmt.Errorf("qobuz favorite %s: empty id", kind)
	}
	form := url.Values{}
	switch strings.ToLower(strings.TrimSpace(kind)) {
	case "album":
		form.Set("album_ids", clean)
	case "track":
		form.Set("track_ids", clean)
	default:
		return fmt.Errorf("unsupported qobuz favorite kind %q", kind)
	}
	var resp map[string]any
	return c.postFormJSON(ctx, endpoint, form, true, &resp)
}
