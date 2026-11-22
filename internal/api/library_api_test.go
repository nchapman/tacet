package api

import (
	"bytes"
	"context"
	"encoding/binary"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"tacet/internal/library"
	"tacet/internal/qobuz"
	"tacet/internal/queue"
)

type apiFakeRemote struct {
	provider string
	albums   []library.Album
	tracks   []library.Track
}

func (f *apiFakeRemote) ProviderID() string { return f.provider }
func (f *apiFakeRemote) IsConnected() bool  { return true }
func (f *apiFakeRemote) SyncCollection(_ context.Context) (library.RemoteCollection, error) {
	return library.RemoteCollection{Albums: f.albums, Tracks: f.tracks}, nil
}
func (f *apiFakeRemote) GetAlbum(_ context.Context, externalID string) (library.Album, error) {
	for _, a := range f.albums {
		if a.ExternalID == externalID {
			cp := a
			for _, tr := range f.tracks {
				if tr.AlbumID == a.ID {
					cp.Tracks = append(cp.Tracks, tr)
				}
			}
			return cp, nil
		}
	}
	return library.Album{}, library.ErrNotFound
}
func (f *apiFakeRemote) GetTrack(_ context.Context, externalID string) (library.Track, error) {
	for _, tr := range f.tracks {
		if tr.ExternalID == externalID {
			return tr, nil
		}
	}
	return library.Track{}, library.ErrNotFound
}
func (f *apiFakeRemote) AddFavorite(_ context.Context, _, _ string) error    { return nil }
func (f *apiFakeRemote) RemoveFavorite(_ context.Context, _, _ string) error { return nil }

func TestUnifiedLibraryAPI(t *testing.T) {
	musicDir := t.TempDir()
	albumDir := filepath.Join(musicDir, "Miles Davis", "Kind of Blue")
	if err := os.MkdirAll(albumDir, 0o755); err != nil {
		t.Fatalf("mkdir: %v", err)
	}
	if err := os.WriteFile(filepath.Join(albumDir, "01 - So What.flac"), makeTestFLACBytes(44100, 16, map[string]string{
		"ARTIST": "Miles Davis",
		"ALBUM":  "Kind of Blue",
		"TITLE":  "So What",
		"GENRE":  "Jazz",
	}), 0o644); err != nil {
		t.Fatalf("write flac: %v", err)
	}

	cat, err := library.NewCatalogWithDB(":memory:", musicDir)
	if err != nil {
		t.Fatalf("NewCatalogWithDB: %v", err)
	}
	defer func() { _ = cat.Close() }()
	if _, scanErr := cat.Scan(); scanErr != nil {
		t.Fatalf("Scan: %v", scanErr)
	}

	fakeRemote := &apiFakeRemote{
		provider: "qobuz",
		albums: []library.Album{
			{
				ID:            "qobuz:kob-hi",
				Source:        library.SourceQobuz,
				SourceType:    library.SourceTypeRemote,
				Provider:      "qobuz",
				ExternalID:    "kob-hi",
				Title:         "Kind of Blue (Remastered)",
				Artist:        "Miles Davis",
				Genre:         "Jazz",
				Year:          1959,
				TrackCount:    1,
				PrimaryFormat: "FLAC",
				MaxBit:        24,
				MaxRate:       192000,
			},
		},
		tracks: []library.Track{
			{
				ID:           "qobuz:5001",
				AlbumID:      "qobuz:kob-hi",
				Source:       library.SourceQobuz,
				SourceType:   library.SourceTypeRemote,
				Provider:     "qobuz",
				ExternalID:   "5001",
				QobuzTrackID: 5001,
				Title:        "So What",
				Artist:       "Miles Davis",
				AlbumArtist:  "Miles Davis",
				Album:        "Kind of Blue (Remastered)",
				Genre:        "Jazz",
				TrackNumber:  1,
				DurationSec:  562,
				BitDepth:     24,
				SampleRate:   192000,
				MIME:         "audio/flac",
				Ext:          ".flac",
			},
		},
	}
	cat.RegisterRemoteSource(fakeRemote)

	qb := qobuz.NewClient(qobuz.Config{})
	mgr := queue.NewManager(8080, "", qb)
	defer mgr.Close()
	srv := NewServer(cat, qb, mgr)
	defer srv.Close()
	h := srv.Handler()

	// 1. Add remote album via POST /api/v1/library/remote/albums
	req := httptest.NewRequest(http.MethodPost, "/api/v1/library/remote/albums",
		bytes.NewBufferString(`{"provider":"qobuz","externalId":"kob-hi"}`))
	req.Header.Set("Content-Type", "application/json")
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	if rec.Code != http.StatusOK {
		t.Fatalf("POST remote/albums = %d: %s", rec.Code, rec.Body.String())
	}

	// 2. GET /api/v1/library/albums?groupVersions=true collapses the 2 versions to 1 primary (24/192 Qobuz).
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/api/v1/library/albums?groupVersions=true", nil))
	if rec.Code != http.StatusOK {
		t.Fatalf("GET albums = %d: %s", rec.Code, rec.Body.String())
	}
	var albPage struct {
		Items []library.Album `json:"items"`
		Total int             `json:"total"`
	}
	if err := json.Unmarshal(rec.Body.Bytes(), &albPage); err != nil {
		t.Fatalf("unmarshal albums: %v", err)
	}
	if albPage.Total != 1 || albPage.Items[0].ID != "qobuz:kob-hi" || albPage.Items[0].VersionCount != 2 {
		t.Fatalf("grouped albums = %+v (total=%d)", albPage.Items, albPage.Total)
	}

	// 3. ETag 304 Not Modified on conditional GET.
	etag := rec.Header().Get("ETag")
	condReq := httptest.NewRequest(http.MethodGet, "/api/v1/library/albums?groupVersions=true", nil)
	condReq.Header.Set("If-None-Match", etag)
	condRec := httptest.NewRecorder()
	h.ServeHTTP(condRec, condReq)
	if condRec.Code != http.StatusNotModified {
		t.Fatalf("conditional GET = %d, want 304", condRec.Code)
	}

	// 4. GET /api/v1/library/artists & /api/v1/library/genres
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/api/v1/library/artists", nil))
	if rec.Code != http.StatusOK {
		t.Fatalf("GET artists = %d: %s", rec.Code, rec.Body.String())
	}
	var artistPage struct {
		Items []library.ArtistSummary `json:"items"`
		Total int                     `json:"total"`
	}
	if err := json.Unmarshal(rec.Body.Bytes(), &artistPage); err != nil {
		t.Fatalf("unmarshal artists: %v", err)
	}
	if artistPage.Total != 1 || artistPage.Items[0].Name != "Miles Davis" {
		t.Fatalf("artists = %+v", artistPage)
	}

	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/api/v1/library/genres", nil))
	if rec.Code != http.StatusOK {
		t.Fatalf("GET genres = %d: %s", rec.Code, rec.Body.String())
	}
	var genreResp struct {
		Items []library.GenreSummary `json:"items"`
	}
	if err := json.Unmarshal(rec.Body.Bytes(), &genreResp); err != nil {
		t.Fatalf("unmarshal genres: %v", err)
	}
	if len(genreResp.Items) != 1 || genreResp.Items[0].Name != "Jazz" {
		t.Fatalf("genres = %+v", genreResp)
	}

	// 5. Record play + verify GET /api/v1/library/heavy-rotation
	cat.RecordPlay(fakeRemote.tracks[0])
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/api/v1/library/heavy-rotation?days=30", nil))
	if rec.Code != http.StatusOK {
		t.Fatalf("GET heavy-rotation = %d: %s", rec.Code, rec.Body.String())
	}
	var rotResp struct {
		Items []library.HeavyRotationAlbum `json:"items"`
	}
	if err := json.Unmarshal(rec.Body.Bytes(), &rotResp); err != nil {
		t.Fatalf("unmarshal heavy-rotation: %v", err)
	}
	if len(rotResp.Items) != 1 || rotResp.Items[0].RecentPlays != 1 {
		t.Fatalf("heavy-rotation = %+v", rotResp)
	}

	// 6. Remove remote album via DELETE /api/v1/library/remote/albums/{id}
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodDelete, "/api/v1/library/remote/albums/qobuz:kob-hi", nil))
	if rec.Code != http.StatusOK {
		t.Fatalf("DELETE remote album = %d: %s", rec.Code, rec.Body.String())
	}
	if cat.HasAlbum("qobuz:kob-hi") {
		t.Fatalf("expected qobuz:kob-hi removed from catalog")
	}
}

func TestPlayerCookiePathMatchesEncodeURIComponent(t *testing.T) {
	got := playerCookiePath("player:994f6d155ad4")
	want := "/api/v1/players/player%3A994f6d155ad4"
	if got != want {
		t.Fatalf("playerCookiePath = %q, want %q", got, want)
	}
}

func TestLibrarySearchAndQobuzArtistEndpoints(t *testing.T) {
	qSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/artist/search":
			_ = json.NewEncoder(w).Encode(map[string]any{
				"artists": map[string]any{
					"items": []map[string]any{{"id": 77, "name": "Miles Davis", "albums_count": 40}},
				},
			})
		case "/artist/get":
			_ = json.NewEncoder(w).Encode(map[string]any{
				"id":   77,
				"name": "Miles Davis",
				"albums": map[string]any{
					"items": []map[string]any{
						{
							"id":                    "q-bitches-brew",
							"title":                 "Bitches Brew",
							"artist":                map[string]any{"id": 77, "name": "Miles Davis"},
							"tracks_count":          6,
							"release_date_original": "1970-03-30",
							"maximum_bit_depth":     24,
							"maximum_sampling_rate": 96.0,
						},
					},
				},
			})
		default:
			http.NotFound(w, r)
		}
	}))
	defer qSrv.Close()

	dir := t.TempDir()
	flac := makeTestFLACBytes(96000, 24, map[string]string{
		"TITLE":       "So What",
		"ARTIST":      "Miles Davis",
		"ALBUMARTIST": "Miles Davis",
		"ALBUM":       "Kind of Blue",
	})
	if err := os.WriteFile(filepath.Join(dir, "01.flac"), flac, 0o600); err != nil {
		t.Fatalf("write flac: %v", err)
	}
	cat, err := library.NewCatalogWithDB(":memory:", dir)
	if err != nil {
		t.Fatalf("NewCatalogWithDB: %v", err)
	}
	defer func() { _ = cat.Close() }()
	if _, err := cat.Scan(); err != nil {
		t.Fatalf("Scan: %v", err)
	}

	qc := qobuz.NewClient(qobuz.Config{
		BaseURL:   qSrv.URL,
		AppID:     "app",
		AppSecret: "sec",
		AuthToken: "tok",
	})
	mgr := queue.NewManager(8080, "", qc)
	defer mgr.Close()
	srv := NewServer(cat, qc, mgr)
	defer srv.Close()
	h := srv.Handler()

	// 1. Unified library search
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/api/v1/library/search?q=Miles+Davis", nil))
	if rec.Code != http.StatusOK {
		t.Fatalf("GET /library/search status = %d: %s", rec.Code, rec.Body.String())
	}
	var sr library.UnifiedSearchResult
	if err := json.Unmarshal(rec.Body.Bytes(), &sr); err != nil {
		t.Fatalf("unmarshal search: %v", err)
	}
	if sr.TopResult == nil || sr.TopResult.Type != "artist" || sr.TopResult.Artist.Name != "Miles Davis" {
		t.Fatalf("TopResult = %+v, want artist Miles Davis", sr.TopResult)
	}

	// 2. Qobuz artist by name
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodGet, "/api/v1/qobuz/artist?name=Miles+Davis", nil))
	if rec.Code != http.StatusOK {
		t.Fatalf("GET /qobuz/artist status = %d: %s", rec.Code, rec.Body.String())
	}
	var ad qobuz.ArtistDetail
	if err := json.Unmarshal(rec.Body.Bytes(), &ad); err != nil {
		t.Fatalf("unmarshal artist: %v", err)
	}
	if ad.Artist.ID != 77 || len(ad.Albums) != 1 || ad.Albums[0].Title != "Bitches Brew" {
		t.Fatalf("ArtistDetail = %+v", ad)
	}
}

func TestSettingsAndQobuzCredentialsPersistence(t *testing.T) {
	ctx := context.Background()
	qSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/user/login":
			_ = json.NewEncoder(w).Encode(map[string]any{
				"user": map[string]any{
					"display_name": "Nolan",
					"subscription": map[string]any{"offer": "Studio Premier"},
				},
				"user_auth_token": "tok-xyz",
			})
		case "/favorite/getUserFavorites":
			_ = json.NewEncoder(w).Encode(map[string]any{
				"albums": map[string]any{"items": []any{}, "total": 0},
				"tracks": map[string]any{"items": []any{}, "total": 0},
			})
		default:
			http.NotFound(w, r)
		}
	}))
	defer qSrv.Close()

	cat, err := library.NewCatalogWithDB(":memory:")
	if err != nil {
		t.Fatalf("NewCatalogWithDB: %v", err)
	}
	defer func() { _ = cat.Close() }()

	qb := qobuz.NewClient(qobuz.Config{BaseURL: qSrv.URL, AppID: "app1", AppSecret: "sec1"})
	cat.RegisterRemoteSource(qb.AsRemoteSource())
	mgr := queue.NewManager(8080, "http://127.0.0.1:8080", qb)
	defer mgr.Close()
	srv := NewServer(cat, qb, mgr)
	defer srv.Close()
	h := srv.Handler()

	// 1. PATCH /api/v1/settings persists stream_mode and qobuz_max_quality into app_prefs.
	req := httptest.NewRequest(http.MethodPatch, "/api/v1/settings", strings.NewReader(`{"streamMode":"relay","qobuzMaxQuality":"flac_24_96"}`))
	req.Header.Set("Content-Type", "application/json")
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	if rec.Code != http.StatusOK {
		t.Fatalf("PATCH /api/v1/settings = %d: %s", rec.Code, rec.Body.String())
	}
	if mode, ok, err := cat.GetPref(ctx, library.PrefStreamMode); err != nil || !ok || mode != "relay" {
		t.Fatalf("persisted stream_mode = (%q, %v, %v)", mode, ok, err)
	}
	if maxQ, ok, err := cat.GetPref(ctx, library.PrefQobuzMaxQuality); err != nil || !ok || maxQ != "flac_24_96" {
		t.Fatalf("persisted qobuz_max_quality = (%q, %v, %v)", maxQ, ok, err)
	}

	// 2. POST /api/v1/qobuz/login persists credentials into provider_credentials.
	req = httptest.NewRequest(http.MethodPost, "/api/v1/qobuz/login", strings.NewReader(`{"authToken":"tok-xyz"}`))
	req.Header.Set("Content-Type", "application/json")
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	if rec.Code != http.StatusOK {
		t.Fatalf("POST /api/v1/qobuz/login = %d: %s", rec.Code, rec.Body.String())
	}
	var saved qobuz.PersistedCredentials
	if ok, err := cat.GetProviderCredentials(ctx, "qobuz", &saved); err != nil || !ok {
		t.Fatalf("GetProviderCredentials = (%v, %v)", ok, err)
	}
	if saved.AuthToken != "tok-xyz" || saved.UserName != "Nolan" {
		t.Fatalf("saved credentials = %+v", saved)
	}

	// 3. DELETE /api/v1/qobuz/session wipes persisted credentials.
	rec = httptest.NewRecorder()
	h.ServeHTTP(rec, httptest.NewRequest(http.MethodDelete, "/api/v1/qobuz/session", nil))
	if rec.Code != http.StatusOK {
		t.Fatalf("DELETE /api/v1/qobuz/session = %d: %s", rec.Code, rec.Body.String())
	}
	if ok, err := cat.GetProviderCredentials(ctx, "qobuz", &saved); err != nil || ok {
		t.Fatalf("expected cleared credentials after logout, got ok=%v err=%v", ok, err)
	}
}

func makeTestFLACBytes(sampleRate, bitDepth int, tags map[string]string) []byte {
	// Valid 42-byte FLAC header + STREAMINFO block (2 channels, 180s), plus optional Vorbis Comment block.
	buf := make([]byte, 42)
	copy(buf[0:4], "fLaC")
	buf[4] = 0x80 // last metadata block, type 0 (STREAMINFO)
	buf[7] = 34   // length = 34
	totalSamples := uint64(sampleRate) * 180
	packed := (uint64(sampleRate) << 44) | (uint64(1) << 41) | (uint64(bitDepth-1) << 36) | (totalSamples & ((1 << 36) - 1))
	for i := range 8 {
		buf[18+i] = byte(packed >> (56 - i*8))
	}
	if len(tags) > 0 {
		buf[4] &^= 0x80
		var vc bytes.Buffer
		_ = binary.Write(&vc, binary.LittleEndian, uint32(5))
		vc.WriteString("tacet")
		_ = binary.Write(&vc, binary.LittleEndian, uint32(len(tags)))
		for k, v := range tags {
			kv := k + "=" + v
			_ = binary.Write(&vc, binary.LittleEndian, uint32(len(kv)))
			vc.WriteString(kv)
		}
		vb := vc.Bytes()
		buf = append(buf, 0x84, byte(len(vb)>>16), byte(len(vb)>>8), byte(len(vb)))
		buf = append(buf, vb...)
	}
	return buf
}
