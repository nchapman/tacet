package qobuz

import (
	"context"
	"encoding/json"
	"fmt"
	"net/url"
	"slices"
	"strconv"
	"strings"
	"time"

	"tacet/internal/cache"
	"tacet/internal/library"
)

const (
	discoverGenresTTL  = 24 * time.Hour
	discoverFeedTTL    = 15 * time.Minute
	discoverSimilarTTL = time.Hour
)

// DiscoverGenre is a top-level Qobuz editorial genre used to filter discovery shelves.
type DiscoverGenre struct {
	ID   int64  `json:"id"`
	Name string `json:"name"`
}

// DiscoverShelf is a single editorial row of albums on the discovery feed.
type DiscoverShelf struct {
	Slug     string          `json:"slug"`
	Title    string          `json:"title"`
	Subtitle string          `json:"subtitle,omitempty"`
	Items    []library.Album `json:"items"`
	HasMore  bool            `json:"hasMore"`
}

// DiscoverFeed is the top-level discovery payload returned for Home.
type DiscoverFeed struct {
	Genres  []DiscoverGenre `json:"genres"`
	Shelves []DiscoverShelf `json:"shelves"`
}

// DiscoverShelfPage is a paginated slice of a single discovery shelf.
type DiscoverShelfPage struct {
	Slug       string          `json:"slug"`
	Title      string          `json:"title"`
	Subtitle   string          `json:"subtitle,omitempty"`
	Items      []library.Album `json:"items"`
	HasMore    bool            `json:"hasMore"`
	Total      int             `json:"total,omitempty"`
	Offset     int             `json:"offset"`
	NextOffset int             `json:"nextOffset,omitempty"`
	Limit      int             `json:"limit"`
}

type shelfDef struct {
	slug         string
	title        string
	subtitle     string
	indexKey     string // key inside discover/index containers
	discoverPath string // non-empty when backed by discover/* pagination
	featuredType string // non-empty when backed by album/getFeatured
}

var discoverShelfDefs = []shelfDef{
	{
		slug:         "new-releases",
		title:        "New Releases",
		subtitle:     "Fresh studio masters and this week's arrivals",
		indexKey:     "new_releases",
		discoverPath: "/discover/newReleases",
	},
	{
		slug:         "most-streamed",
		title:        "Most Streamed",
		subtitle:     "Popular albums listeners are spinning right now",
		indexKey:     "most_streamed",
		featuredType: "most-streamed",
	},
	{
		slug:         "album-of-the-week",
		title:        "Album of the Week",
		subtitle:     "Weekly editorial spotlights",
		indexKey:     "album_of_the_week",
		discoverPath: "/discover/albumOfTheWeek",
	},
	{
		slug:         "qobuzissims",
		title:        "Qobuzissime",
		subtitle:     "Standout recordings singled out by Qobuz editors",
		indexKey:     "qobuzissims",
		featuredType: "qobuzissims",
	},
	{
		slug:         "press-awards",
		title:        "Press Awards",
		subtitle:     "Critically acclaimed releases and award winners",
		indexKey:     "press_awards",
		featuredType: "press-awards",
	},
	{
		slug:         "ideal-discography",
		title:        "Ideal Discography",
		subtitle:     "Essential albums worth knowing inside and out",
		indexKey:     "ideal_discography",
		discoverPath: "/discover/idealDiscography",
	},
}

func lookupShelfDef(slug string) (shelfDef, bool) {
	norm := strings.ToLower(strings.TrimSpace(slug))
	for _, def := range discoverShelfDefs {
		if def.slug == norm {
			return def, true
		}
	}
	return shelfDef{}, false
}

func normalizeGenreIDs(ids []int64) string {
	if len(ids) == 0 {
		return ""
	}
	cp := make([]int64, 0, len(ids))
	for _, id := range ids {
		if id > 0 {
			cp = append(cp, id)
		}
	}
	if len(cp) == 0 {
		return ""
	}
	slices.Sort(cp)
	cp = slices.Compact(cp)
	parts := make([]string, len(cp))
	for i, id := range cp {
		parts[i] = strconv.FormatInt(id, 10)
	}
	return strings.Join(parts, ",")
}

// Genres returns Qobuz's top-level editorial genres for discovery filtering.
func (c *Client) Genres(ctx context.Context) ([]DiscoverGenre, error) {
	if err := c.requireAuth(); err != nil {
		return nil, err
	}
	if cached, ok, err := cache.GetJSON[[]DiscoverGenre](c.kv, "genres"); err == nil && ok {
		return cached, nil
	}

	var resp struct {
		Genres struct {
			Items []struct {
				ID   int64  `json:"id"`
				Name string `json:"name"`
			} `json:"items"`
		} `json:"genres"`
	}
	if err := c.getJSON(ctx, "/genre/list", nil, true, &resp); err != nil {
		return nil, err
	}

	out := make([]DiscoverGenre, 0, len(resp.Genres.Items))
	for _, g := range resp.Genres.Items {
		name := strings.TrimSpace(g.Name)
		if g.ID <= 0 || name == "" {
			continue
		}
		out = append(out, DiscoverGenre{ID: g.ID, Name: name})
	}
	_ = cache.SetJSON(c.kv, "genres", out, discoverGenresTTL)
	return out, nil
}

// Discover fetches the editorial discovery feed (optionally filtered by genre IDs).
func (c *Client) Discover(ctx context.Context, genreIDs []int64) (DiscoverFeed, error) {
	if err := c.requireAuth(); err != nil {
		return DiscoverFeed{}, err
	}
	genreParam := normalizeGenreIDs(genreIDs)
	cacheKey := "discover:" + genreParam
	if cached, ok, err := cache.GetJSON[DiscoverFeed](c.kv, cacheKey); err == nil && ok {
		return cached, nil
	}

	params := url.Values{}
	if genreParam != "" {
		params.Set("genre_ids", genreParam)
	}
	var resp struct {
		Containers map[string]struct {
			Data struct {
				HasMore bool            `json:"has_more"`
				Items   json.RawMessage `json:"items"`
			} `json:"data"`
		} `json:"containers"`
	}
	if err := c.getJSON(ctx, "/discover/index", params, true, &resp); err != nil {
		return DiscoverFeed{}, err
	}

	// Best-effort genres fetch; do not fail the whole feed if genre/list hiccups.
	genres, _ := c.Genres(ctx)
	if genres == nil {
		genres = []DiscoverGenre{}
	}

	shelves := make([]DiscoverShelf, 0, len(discoverShelfDefs))
	for _, def := range discoverShelfDefs {
		container, ok := resp.Containers[def.indexKey]
		if !ok || len(container.Data.Items) == 0 {
			continue
		}
		var rawItems []qDiscoverAlbum
		if err := json.Unmarshal(container.Data.Items, &rawItems); err != nil {
			continue
		}
		albums := convertDiscoverAlbums(rawItems)
		if len(albums) == 0 {
			continue
		}
		shelves = append(shelves, DiscoverShelf{
			Slug:     def.slug,
			Title:    def.title,
			Subtitle: def.subtitle,
			Items:    albums,
			HasMore:  container.Data.HasMore || len(albums) >= 10,
		})
	}

	feed := DiscoverFeed{Genres: genres, Shelves: shelves}
	_ = cache.SetJSON(c.kv, cacheKey, feed, discoverFeedTTL)
	return feed, nil
}

// DiscoverShelf fetches a paginated editorial shelf by slug (e.g. "new-releases", "qobuzissims").
func (c *Client) DiscoverShelf(ctx context.Context, slug string, genreIDs []int64, limit, offset int) (DiscoverShelfPage, error) {
	if err := c.requireAuth(); err != nil {
		return DiscoverShelfPage{}, err
	}
	def, ok := lookupShelfDef(slug)
	if !ok {
		return DiscoverShelfPage{}, ErrNotFound
	}
	if limit <= 0 {
		limit = 24
	}
	if limit > 100 {
		limit = 100
	}
	if offset < 0 {
		offset = 0
	}

	genreParam := normalizeGenreIDs(genreIDs)
	cacheKey := fmt.Sprintf("shelf:%s:%s:%d:%d", def.slug, genreParam, limit, offset)
	if cached, ok, err := cache.GetJSON[DiscoverShelfPage](c.kv, cacheKey); err == nil && ok {
		return cached, nil
	}

	params := url.Values{
		"limit":  {strconv.Itoa(limit)},
		"offset": {strconv.Itoa(offset)},
	}
	if genreParam != "" {
		params.Set("genre_ids", genreParam)
	}

	var page DiscoverShelfPage
	if def.discoverPath != "" {
		var resp struct {
			HasMore bool             `json:"has_more"`
			Items   []qDiscoverAlbum `json:"items"`
		}
		if err := c.getJSON(ctx, def.discoverPath, params, true, &resp); err != nil {
			return DiscoverShelfPage{}, err
		}
		rawCount := len(resp.Items)
		hasMore := resp.HasMore && rawCount > 0
		nextOffset := 0
		if hasMore {
			nextOffset = offset + rawCount
		}
		page = DiscoverShelfPage{
			Slug:       def.slug,
			Title:      def.title,
			Subtitle:   def.subtitle,
			Items:      convertDiscoverAlbums(resp.Items),
			HasMore:    hasMore,
			Offset:     offset,
			NextOffset: nextOffset,
			Limit:      limit,
		}
	} else {
		params.Set("type", def.featuredType)
		var resp struct {
			Albums struct {
				Total int      `json:"total"`
				Items []qAlbum `json:"items"`
			} `json:"albums"`
		}
		if err := c.getJSON(ctx, "/album/getFeatured", params, true, &resp); err != nil {
			return DiscoverShelfPage{}, err
		}
		items := make([]library.Album, 0, len(resp.Albums.Items))
		for _, qa := range resp.Albums.Items {
			if qa.Streamable != nil && !*qa.Streamable {
				continue
			}
			items = append(items, qa.toLibraryAlbum(false))
		}
		rawCount := len(resp.Albums.Items)
		hasMore := rawCount > 0 && offset+rawCount < resp.Albums.Total
		nextOffset := 0
		if hasMore {
			nextOffset = offset + rawCount
		}
		page = DiscoverShelfPage{
			Slug:       def.slug,
			Title:      def.title,
			Subtitle:   def.subtitle,
			Items:      items,
			Total:      resp.Albums.Total,
			HasMore:    hasMore,
			Offset:     offset,
			NextOffset: nextOffset,
			Limit:      limit,
		}
	}

	_ = cache.SetJSON(c.kv, cacheKey, page, discoverFeedTTL)
	return page, nil
}

// SimilarAlbums returns Qobuz's recommended/similar albums for the given album ID.
func (c *Client) SimilarAlbums(ctx context.Context, albumID string, limit int) ([]library.Album, error) {
	if err := c.requireAuth(); err != nil {
		return nil, err
	}
	rawID := strings.TrimSpace(strings.TrimPrefix(albumID, "qobuz:"))
	if rawID == "" {
		return nil, ErrNotFound
	}
	if limit <= 0 {
		limit = 12
	}
	if limit > 50 {
		limit = 50
	}

	cacheKey := fmt.Sprintf("similar-albums:%s:%d", rawID, limit)
	if cached, ok, err := cache.GetJSON[[]library.Album](c.kv, cacheKey); err == nil && ok {
		return cached, nil
	}

	var resp struct {
		Albums struct {
			Items []qDiscoverAlbum `json:"items"`
		} `json:"albums"`
	}
	params := url.Values{
		"album_id": {rawID},
		"limit":    {strconv.Itoa(limit)},
	}
	if err := c.getJSON(ctx, "/album/suggest", params, true, &resp); err != nil {
		return nil, err
	}

	out := convertDiscoverAlbums(resp.Albums.Items)
	if len(out) > limit {
		out = out[:limit]
	}
	_ = cache.SetJSON(c.kv, cacheKey, out, discoverSimilarTTL)
	return out, nil
}

// SimilarArtists returns artists similar to the given Qobuz artist ID.
func (c *Client) SimilarArtists(ctx context.Context, artistID int64, limit int) ([]Artist, error) {
	if err := c.requireAuth(); err != nil {
		return nil, err
	}
	if artistID <= 0 {
		return nil, ErrNotFound
	}
	if limit <= 0 {
		limit = 12
	}
	if limit > 50 {
		limit = 50
	}

	cacheKey := fmt.Sprintf("similar-artists:%d:%d", artistID, limit)
	if cached, ok, err := cache.GetJSON[[]Artist](c.kv, cacheKey); err == nil && ok {
		return cached, nil
	}

	var resp struct {
		Artists struct {
			Items []qArtist `json:"items"`
		} `json:"artists"`
	}
	params := url.Values{
		"artist_id": {strconv.FormatInt(artistID, 10)},
		"limit":     {strconv.Itoa(limit)},
	}
	if err := c.getJSON(ctx, "/artist/getSimilarArtists", params, true, &resp); err != nil {
		return nil, err
	}

	out := make([]Artist, 0, len(resp.Artists.Items))
	for _, qa := range resp.Artists.Items {
		art := qa.toArtist()
		if art.ID <= 0 || art.Name == "" {
			continue
		}
		out = append(out, art)
	}
	_ = cache.SetJSON(c.kv, cacheKey, out, discoverSimilarTTL)
	return out, nil
}

type qDiscoverAlbum struct {
	ID         string `json:"id"`
	Title      string `json:"title"`
	Version    string `json:"version"`
	TrackCount int    `json:"track_count"`
	Image      qImage `json:"image"`
	Artists    []struct {
		ID    int64    `json:"id"`
		Name  string   `json:"name"`
		Roles []string `json:"roles"`
	} `json:"artists"`
	Genre struct {
		ID   int64  `json:"id"`
		Name string `json:"name"`
	} `json:"genre"`
	Dates struct {
		Original string `json:"original"`
		Stream   string `json:"stream"`
	} `json:"dates"`
	AudioInfo struct {
		MaximumSamplingRate float64 `json:"maximum_sampling_rate"`
		MaximumBitDepth     int     `json:"maximum_bit_depth"`
	} `json:"audio_info"`
	Rights struct {
		Streamable *bool `json:"streamable"`
	} `json:"rights"`
	AIGenerated bool `json:"ai_generated"`
}

func (qa qDiscoverAlbum) primaryArtist() string {
	for _, a := range qa.Artists {
		if slices.Contains(a.Roles, "main-artist") && strings.TrimSpace(a.Name) != "" {
			return strings.TrimSpace(a.Name)
		}
	}
	for _, a := range qa.Artists {
		if strings.TrimSpace(a.Name) != "" {
			return strings.TrimSpace(a.Name)
		}
	}
	return ""
}

func (qa qDiscoverAlbum) toLibraryAlbum() library.Album {
	dateStr := qa.Dates.Original
	if dateStr == "" {
		dateStr = qa.Dates.Stream
	}
	var year int
	if len(dateStr) >= 4 {
		year, _ = strconv.Atoi(dateStr[:4])
	}
	return library.Album{
		ID:            "qobuz:" + qa.ID,
		Source:        library.SourceQobuz,
		SourceType:    library.SourceTypeRemote,
		Provider:      string(library.SourceQobuz),
		ExternalID:    qa.ID,
		Title:         strings.TrimSpace(qa.Title),
		Artist:        qa.primaryArtist(),
		Genre:         strings.TrimSpace(qa.Genre.Name),
		Year:          year,
		TrackCount:    qa.TrackCount,
		ArtworkURL:    qa.Image.best(),
		PrimaryFormat: "FLAC",
		MaxBit:        qa.AudioInfo.MaximumBitDepth,
		MaxRate:       normalizeSampleRateHz(qa.AudioInfo.MaximumSamplingRate),
		Available:     true,
	}
}

func convertDiscoverAlbums(items []qDiscoverAlbum) []library.Album {
	out := make([]library.Album, 0, len(items))
	seen := make(map[string]struct{}, len(items))
	for _, qa := range items {
		if qa.ID == "" || qa.AIGenerated {
			continue
		}
		if qa.Rights.Streamable != nil && !*qa.Rights.Streamable {
			continue
		}
		if _, dup := seen[qa.ID]; dup {
			continue
		}
		seen[qa.ID] = struct{}{}
		out = append(out, qa.toLibraryAlbum())
	}
	return out
}
