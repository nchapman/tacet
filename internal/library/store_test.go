package library

import (
	"bytes"
	"context"
	"encoding/binary"
	"os"
	"path/filepath"
	"sync"
	"testing"
	"time"
)

type fakeRemoteSource struct {
	mu           sync.Mutex
	provider     string
	connected    bool
	albums       []Album
	tracks       []Track
	addedAlbums  []string
	removedAlbs  []string
	addedTracks  []string
	removedTrks  []string
	albumByExtID map[string]*Album
	trackByExtID map[string]*Track
}

func (f *fakeRemoteSource) ProviderID() string { return f.provider }
func (f *fakeRemoteSource) IsConnected() bool {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.connected
}

func (f *fakeRemoteSource) SyncCollection(_ context.Context) (RemoteCollection, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if !f.connected {
		return RemoteCollection{}, ErrProviderOffline
	}
	return RemoteCollection{
		Albums: append([]Album(nil), f.albums...),
		Tracks: append([]Track(nil), f.tracks...),
	}, nil
}

func (f *fakeRemoteSource) GetAlbum(_ context.Context, externalID string) (Album, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if !f.connected {
		return Album{}, ErrProviderOffline
	}
	alb, ok := f.albumByExtID[externalID]
	if !ok {
		return Album{}, ErrNotFound
	}
	return *alb, nil
}

func (f *fakeRemoteSource) GetTrack(_ context.Context, externalID string) (Track, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	tr, ok := f.trackByExtID[externalID]
	if !ok {
		return Track{}, ErrNotFound
	}
	return *tr, nil
}

func (f *fakeRemoteSource) AddFavorite(_ context.Context, kind, externalID string) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	if kind == "album" {
		f.addedAlbums = append(f.addedAlbums, externalID)
	} else {
		f.addedTracks = append(f.addedTracks, externalID)
	}
	return nil
}

func (f *fakeRemoteSource) RemoveFavorite(_ context.Context, kind, externalID string) error {
	f.mu.Lock()
	defer f.mu.Unlock()
	if kind == "album" {
		f.removedAlbs = append(f.removedAlbs, externalID)
	} else {
		f.removedTrks = append(f.removedTrks, externalID)
	}
	return nil
}

func writeTaggedFLAC(t *testing.T, dir, relPath string, bitDepth, sampleRate int, tags map[string]string) string {
	t.Helper()
	full := filepath.Join(dir, relPath)
	if err := os.MkdirAll(filepath.Dir(full), 0o755); err != nil {
		t.Fatalf("mkdir %s: %v", full, err)
	}
	base := buildSyntheticFLAC(sampleRate, 2, bitDepth, uint64(sampleRate)*180)
	if len(tags) > 0 {
		// Clear last-block bit on STREAMINFO and append a Vorbis Comment block (type 4, last=1).
		base[4] &^= 0x80
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
		hdr := []byte{0x84, byte(len(vb) >> 16), byte(len(vb) >> 8), byte(len(vb))}
		base = append(base, hdr...)
		base = append(base, vb...)
	}
	if err := os.WriteFile(full, base, 0o644); err != nil {
		t.Fatalf("write flac %s: %v", full, err)
	}
	return full
}

func writeTestFLAC(t *testing.T, dir, relPath string, bitDepth, sampleRate int) string {
	return writeTaggedFLAC(t, dir, relPath, bitDepth, sampleRate, nil)
}

func TestMigrationsAndPersistence(t *testing.T) {
	dbPath := filepath.Join(t.TempDir(), "library.db")
	musicDir := t.TempDir()
	writeTestFLAC(t, musicDir, "Miles Davis/Kind of Blue/01 - So What.flac", 24, 96000)

	cat1, err := NewCatalogWithDB(dbPath, musicDir)
	if err != nil {
		t.Fatalf("NewCatalogWithDB(1): %v", err)
	}
	ver, err := cat1.SchemaVersion()
	if err != nil {
		t.Fatalf("SchemaVersion: %v", err)
	}
	if ver != len(migrations) {
		t.Fatalf("schema version = %d, want %d", ver, len(migrations))
	}
	n, err := cat1.Scan()
	if err != nil || n != 1 {
		t.Fatalf("Scan = (%d, %v), want (1, nil)", n, err)
	}
	etag1 := cat1.ETag()

	// Warm rescan with zero disk changes hits fast path and keeps ETag stable.
	n2, err := cat1.Scan()
	if err != nil || n2 != 1 {
		t.Fatalf("Scan(2) = (%d, %v), want (1, nil)", n2, err)
	}
	if cat1.ETag() != etag1 {
		t.Fatalf("unchanged rescan bumped ETag: %q -> %q", etag1, cat1.ETag())
	}
	if closeErr := cat1.Close(); closeErr != nil {
		t.Fatalf("Close(1): %v", closeErr)
	}

	// Reopening the persisted DB immediately serves cached tracks before scanning.
	cat2, err := NewCatalogWithDB(dbPath, musicDir)
	if err != nil {
		t.Fatalf("NewCatalogWithDB(2): %v", err)
	}
	defer func() { _ = cat2.Close() }()
	if st := cat2.Status(); st.Tracks != 1 || st.Albums != 1 {
		t.Fatalf("persisted status = %+v, want 1 track / 1 album", st)
	}
}

func TestSoftDeleteRestoreAndRetentionPurge(t *testing.T) {
	musicDir := t.TempDir()
	f1 := writeTestFLAC(t, musicDir, "Bill Evans/Waltz for Debby/01 - My Foolish Heart.flac", 24, 96000)
	f2 := writeTestFLAC(t, musicDir, "Bill Evans/Waltz for Debby/02 - Waltz for Debby.flac", 24, 96000)

	cat, err := NewCatalogWithDB(":memory:", musicDir)
	if err != nil {
		t.Fatalf("NewCatalogWithDB: %v", err)
	}
	defer func() { _ = cat.Close() }()

	now := time.Date(2026, 4, 1, 12, 0, 0, 0, time.UTC)
	cat.store.nowFn = func() time.Time { return now }
	cat.SetMissingRetention(30 * 24 * time.Hour)

	if n, scanErr := cat.Scan(); scanErr != nil || n != 2 {
		t.Fatalf("initial scan = (%d, %v), want (2, nil)", n, scanErr)
	}
	tracks, total, err := cat.QueryTracks(TrackFilter{})
	if err != nil || total != 2 {
		t.Fatalf("QueryTracks = (%d, %v), want (2, nil)", total, err)
	}
	firstTrack := tracks[0]
	cat.RecordPlay(firstTrack)

	// Simulate temporary NAS disconnect (remove file 1).
	if err := os.Remove(f1); err != nil {
		t.Fatalf("remove f1: %v", err)
	}
	if n, scanErr := cat.Scan(); scanErr != nil || n != 1 {
		t.Fatalf("scan after disconnect = (%d, %v), want (1, nil)", n, scanErr)
	}
	st := cat.Status()
	if st.Tracks != 1 || st.MissingTracks != 1 {
		t.Fatalf("status after soft-delete = %+v, want tracks=1 missing=1", st)
	}

	// Drive reconnects within 30 days: track restores with play count intact!
	now = now.Add(5 * 24 * time.Hour)
	writeTestFLAC(t, musicDir, "Bill Evans/Waltz for Debby/01 - My Foolish Heart.flac", 24, 96000)
	if n, scanErr := cat.Scan(); scanErr != nil || n != 2 {
		t.Fatalf("scan after reconnect = (%d, %v), want (2, nil)", n, scanErr)
	}
	restored, ok := cat.Track(firstTrack.ID)
	if !ok {
		t.Fatalf("restored track %s not found", firstTrack.ID)
	}
	if restored.PlayCount != 1 {
		t.Fatalf("restored track playCount = %d, want 1", restored.PlayCount)
	}

	// Permanently remove file 2 and advance past 30-day retention window -> purged.
	if err := os.Remove(f2); err != nil {
		t.Fatalf("remove f2: %v", err)
	}
	if _, scanErr := cat.Scan(); scanErr != nil {
		t.Fatalf("scan soft-delete f2: %v", scanErr)
	}
	if cat.Status().MissingTracks != 1 {
		t.Fatalf("expected 1 missing track before retention expiry")
	}

	now = now.Add(31 * 24 * time.Hour)
	if _, scanErr := cat.Scan(); scanErr != nil {
		t.Fatalf("scan after retention expiry: %v", scanErr)
	}
	if st := cat.Status(); st.MissingTracks != 0 || st.Tracks != 1 {
		t.Fatalf("status after retention purge = %+v, want tracks=1 missing=0", st)
	}
}

func TestVersionGroupingAndPrimarySelection(t *testing.T) {
	musicDir := t.TempDir()
	// Local copy: 16-bit / 44.1 kHz FLAC
	writeTaggedFLAC(t, musicDir, "Radiohead/In Rainbows/01 - 15 Step.flac", 16, 44100, map[string]string{
		"ARTIST": "Radiohead",
		"ALBUM":  "In Rainbows",
		"TITLE":  "15 Step",
		"GENRE":  "Alternative",
	})

	cat, err := NewCatalogWithDB(":memory:", musicDir)
	if err != nil {
		t.Fatalf("NewCatalogWithDB: %v", err)
	}
	defer func() { _ = cat.Close() }()
	if _, scanErr := cat.Scan(); scanErr != nil {
		t.Fatalf("Scan: %v", scanErr)
	}

	// Remote Qobuz copy: 24-bit / 192 kHz FLAC of same album (with deluxe edition suffix!)
	remote := &fakeRemoteSource{
		provider:  "qobuz",
		connected: true,
		albums: []Album{
			{
				ID:            "qobuz:alb-ir",
				Source:        SourceQobuz,
				SourceType:    SourceTypeRemote,
				Provider:      "qobuz",
				ExternalID:    "alb-ir",
				Title:         "In Rainbows (Deluxe Edition)",
				Artist:        "Radiohead",
				Genre:         "Alternative",
				Year:          2007,
				TrackCount:    1,
				PrimaryFormat: "FLAC",
				MaxBit:        24,
				MaxRate:       192000,
				Tracks: []Track{
					{
						ID:           "qobuz:trk-1",
						AlbumID:      "qobuz:alb-ir",
						Source:       SourceQobuz,
						SourceType:   SourceTypeRemote,
						Provider:     "qobuz",
						ExternalID:   "trk-1",
						QobuzTrackID: 101,
						Title:        "15 Step",
						Artist:       "Radiohead",
						AlbumArtist:  "Radiohead",
						Album:        "In Rainbows (Deluxe Edition)",
						Genre:        "Alternative",
						TrackNumber:  1,
						DurationSec:  237,
						BitDepth:     24,
						SampleRate:   192000,
						MIME:         "audio/flac",
						Ext:          ".flac",
					},
				},
			},
		},
	}
	cat.RegisterRemoteSource(remote)
	if n, syncErr := cat.SyncRemote(context.Background(), "qobuz"); syncErr != nil || n != 1 {
		t.Fatalf("SyncRemote = (%d, %v), want (1, nil)", n, syncErr)
	}

	// With GroupVersions=true, only 1 album appears, and the 24/192 Qobuz edition wins primary!
	grouped, total, err := cat.QueryAlbums(AlbumFilter{GroupVersions: true})
	if err != nil || total != 1 || len(grouped) != 1 {
		t.Fatalf("grouped albums = %d (total=%d, err=%v), want 1", len(grouped), total, err)
	}
	if grouped[0].ID != "qobuz:alb-ir" {
		t.Fatalf("auto primary = %q, want qobuz:alb-ir (higher resolution)", grouped[0].ID)
	}
	if grouped[0].VersionCount != 2 {
		t.Fatalf("versionCount = %d, want 2", grouped[0].VersionCount)
	}

	// Inspect album detail: both versions returned.
	detail, ok := cat.Album("qobuz:alb-ir")
	if !ok || len(detail.Versions) != 2 {
		t.Fatalf("GetAlbum versions = %d, want 2", len(detail.Versions))
	}
	var localID string
	for _, v := range detail.Versions {
		if v.SourceType == SourceTypeLocal {
			localID = v.ID
		}
	}
	if localID == "" {
		t.Fatalf("expected local version in detail.Versions")
	}

	// Pin local version as primary!
	if _, primErr := cat.SetPrimaryVersion(localID); primErr != nil {
		t.Fatalf("SetPrimaryVersion: %v", primErr)
	}
	grouped2, _, err := cat.QueryAlbums(AlbumFilter{GroupVersions: true})
	if err != nil || len(grouped2) != 1 || grouped2[0].ID != localID {
		t.Fatalf("pinned primary = %+v (err=%v), want %q", grouped2, err, localID)
	}

	// Ungroup local version -> both albums appear separately.
	if _, unErr := cat.UngroupVersion(localID); unErr != nil {
		t.Fatalf("UngroupVersion: %v", unErr)
	}
	ungrouped, totalUngrouped, err := cat.QueryAlbums(AlbumFilter{GroupVersions: true})
	if err != nil || totalUngrouped != 2 || len(ungrouped) != 2 {
		t.Fatalf("after UngroupVersion: got %d (total=%d, err=%v), want 2", len(ungrouped), totalUngrouped, err)
	}

	// Manually re-group them.
	gid, err := cat.GroupVersions([]string{localID, "qobuz:alb-ir"})
	if err != nil || gid == "" {
		t.Fatalf("GroupVersions = (%q, %v)", gid, err)
	}
	regrouped, totalRegrouped, err := cat.QueryAlbums(AlbumFilter{GroupVersions: true})
	if err != nil || totalRegrouped != 1 || len(regrouped) != 1 {
		t.Fatalf("after GroupVersions: got %d (total=%d, err=%v), want 1", len(regrouped), totalRegrouped, err)
	}
}

func TestRemoteAddRemoveHeavyRotationAndFTS(t *testing.T) {
	cat, err := NewCatalogWithDB(":memory:")
	if err != nil {
		t.Fatalf("NewCatalogWithDB: %v", err)
	}
	defer func() { _ = cat.Close() }()

	remote := &fakeRemoteSource{
		provider:  "qobuz",
		connected: true,
		albumByExtID: map[string]*Album{
			"ext-42": {
				ID:            "qobuz:ext-42",
				Source:        SourceQobuz,
				SourceType:    SourceTypeRemote,
				Provider:      "qobuz",
				ExternalID:    "ext-42",
				Title:         "A Love Supreme",
				Artist:        "John Coltrane",
				Genre:         "Jazz",
				Year:          1965,
				TrackCount:    1,
				PrimaryFormat: "FLAC",
				MaxBit:        24,
				MaxRate:       96000,
				Tracks: []Track{
					{
						ID:           "qobuz:trk-42",
						AlbumID:      "qobuz:ext-42",
						Source:       SourceQobuz,
						SourceType:   SourceTypeRemote,
						Provider:     "qobuz",
						ExternalID:   "trk-42",
						QobuzTrackID: 42,
						Title:        "Acknowledgement",
						Artist:       "John Coltrane",
						AlbumArtist:  "John Coltrane",
						Album:        "A Love Supreme",
						Genre:        "Jazz",
						TrackNumber:  1,
						DurationSec:  462,
						BitDepth:     24,
						SampleRate:   96000,
						MIME:         "audio/flac",
						Ext:          ".flac",
					},
				},
			},
		},
	}
	cat.RegisterRemoteSource(remote)

	// Add album via 2-way sync.
	alb, err := cat.AddRemoteAlbum(context.Background(), "qobuz", "ext-42")
	if err != nil {
		t.Fatalf("AddRemoteAlbum: %v", err)
	}
	if alb.ID != "qobuz:ext-42" || !cat.HasAlbum("qobuz:ext-42") {
		t.Fatalf("album not saved in library: %+v", alb)
	}

	// FTS5 search, artists, genres, and Heavy Rotation.
	found, total, err := cat.QueryTracks(TrackFilter{Query: "Coltrane Acknow"})
	if err != nil || total != 1 || len(found) != 1 || found[0].Title != "Acknowledgement" {
		t.Fatalf("FTS5 QueryTracks = (%+v, %d, %v), want 1 match", found, total, err)
	}
	genres, err := cat.ListGenres(FacetFilter{})
	if err != nil || len(genres) != 1 || genres[0].Name != "Jazz" {
		t.Fatalf("ListGenres = (%+v, %v), want Jazz", genres, err)
	}
	artists, artistTotal, err := cat.ListArtists(FacetFilter{Genre: "Jazz"})
	if err != nil || artistTotal != 1 || artists[0].Name != "John Coltrane" {
		t.Fatalf("ListArtists = (%+v, %d, %v), want John Coltrane", artists, artistTotal, err)
	}

	cat.RecordPlay(found[0])
	cat.RecordPlay(found[0])
	rot, err := cat.HeavyRotation(30, 10)
	if err != nil || len(rot) != 1 || rot[0].ID != "qobuz:ext-42" || rot[0].RecentPlays != 2 {
		t.Fatalf("HeavyRotation = (%+v, %v), want 1 album with 2 plays", rot, err)
	}

	// Sign out hides Qobuz albums without wiping play history.
	if err := cat.SetProviderAvailable(context.Background(), "qobuz", false); err != nil {
		t.Fatalf("SetProviderAvailable(false): %v", err)
	}
	if _, total, err := cat.QueryAlbums(AlbumFilter{}); err != nil || total != 0 {
		t.Fatalf("expected 0 available albums after provider sign-out, got %d (err=%v)", total, err)
	}
	if err := cat.SetProviderAvailable(context.Background(), "qobuz", true); err != nil {
		t.Fatalf("SetProviderAvailable(true): %v", err)
	}
	if rot2, err := cat.HeavyRotation(30, 10); err != nil || len(rot2) != 1 || rot2[0].RecentPlays != 2 {
		t.Fatalf("HeavyRotation after reconnect = (%+v, %v), want preserved play history", rot2, err)
	}

	// Remove album mirrors back to RemoteSource.
	if err := cat.RemoveRemoteAlbum(context.Background(), "qobuz:ext-42"); err != nil {
		t.Fatalf("RemoveRemoteAlbum: %v", err)
	}
	if cat.HasAlbum("qobuz:ext-42") {
		t.Fatalf("expected album deleted after RemoveRemoteAlbum")
	}
	if len(remote.removedAlbs) != 1 || remote.removedAlbs[0] != "ext-42" {
		t.Fatalf("remote.removedAlbs = %v, want [ext-42]", remote.removedAlbs)
	}
}

func TestUnifiedSearchIntentRanking(t *testing.T) {
	cat, err := NewCatalogWithDB(":memory:")
	if err != nil {
		t.Fatalf("NewCatalogWithDB: %v", err)
	}
	defer func() { _ = cat.Close() }()

	albs := []Album{
		{
			ID: "local:alb-al", Source: SourceLocal, SourceType: SourceTypeLocal, Provider: "local",
			Title: "Greatest Hits", Artist: "Al Green", Year: 1975, TrackCount: 1, Available: true,
		},
		{
			ID: "local:alb-miles", Source: SourceLocal, SourceType: SourceTypeLocal, Provider: "local",
			Title: "Kind of Blue", Artist: "Miles Davis", Year: 1959, TrackCount: 1, Available: true,
		},
	}
	trs := []Track{
		{
			ID: "local:t1", Source: SourceLocal, SourceType: SourceTypeLocal, Provider: "local",
			Title: "Tired of Being Alone", Artist: "Al Green", AlbumArtist: "Al Green", Album: "Greatest Hits",
			AlbumID: "local:alb-al", TrackNumber: 1, DurationSec: 163, Available: true,
		},
		{
			ID: "local:t2", Source: SourceLocal, SourceType: SourceTypeLocal, Provider: "local",
			Title: "So What", Artist: "Miles Davis", AlbumArtist: "Miles Davis", Album: "Kind of Blue",
			AlbumID: "local:alb-miles", TrackNumber: 1, DurationSec: 545, Available: true,
		},
	}
	if seedErr := cat.store.replaceProvider(context.Background(), SourceTypeLocal, "local", albs, trs, nil); seedErr != nil {
		t.Fatalf("seed: %v", seedErr)
	}

	// Artist query -> Artist TopResult
	rArtist, err := cat.UnifiedSearch(context.Background(), "Al Green", 8)
	if err != nil {
		t.Fatalf("UnifiedSearch artist: %v", err)
	}
	if rArtist.TopResult == nil || rArtist.TopResult.Type != "artist" || rArtist.TopResult.Artist.Name != "Al Green" {
		t.Fatalf("artist TopResult = %+v, want artist Al Green", rArtist.TopResult)
	}

	// Album query -> Album TopResult
	rAlbum, err := cat.UnifiedSearch(context.Background(), "Kind of Blue", 8)
	if err != nil {
		t.Fatalf("UnifiedSearch album: %v", err)
	}
	if rAlbum.TopResult == nil || rAlbum.TopResult.Type != "album" || rAlbum.TopResult.Album.Title != "Kind of Blue" {
		t.Fatalf("album TopResult = %+v, want album Kind of Blue", rAlbum.TopResult)
	}

	// Track query -> Track TopResult
	rTrack, err := cat.UnifiedSearch(context.Background(), "Tired of Being Alone", 8)
	if err != nil {
		t.Fatalf("UnifiedSearch track: %v", err)
	}
	if rTrack.TopResult == nil || rTrack.TopResult.Type != "track" || rTrack.TopResult.Track.Title != "Tired of Being Alone" {
		t.Fatalf("track TopResult = %+v, want track Tired of Being Alone", rTrack.TopResult)
	}
}

func TestPrefsAndProviderCredentialsPersistence(t *testing.T) {
	ctx := context.Background()
	dataDir := filepath.Join(t.TempDir(), "state")
	dbPath := filepath.Join(dataDir, "library.db")

	cat1, err := NewCatalogWithDB(dbPath)
	if err != nil {
		t.Fatalf("NewCatalogWithDB(1): %v", err)
	}

	// Verify tightened directory (0700) and DB file (0600) permissions.
	if info, statErr := os.Stat(dataDir); statErr != nil {
		t.Fatalf("stat dataDir: %v", statErr)
	} else if perm := info.Mode().Perm(); perm != 0o700 {
		t.Fatalf("dataDir perms = %o, want 700", perm)
	}
	if info, statErr := os.Stat(dbPath); statErr != nil {
		t.Fatalf("stat dbPath: %v", statErr)
	} else if perm := info.Mode().Perm(); perm != 0o600 {
		t.Fatalf("dbPath perms = %o, want 600", perm)
	}

	// Prefs CRUD.
	if _, ok, prefErr := cat1.GetPref(ctx, PrefStreamMode); prefErr != nil || ok {
		t.Fatalf("expected missing pref, got ok=%v err=%v", ok, prefErr)
	}
	if setErr := cat1.SetPref(ctx, PrefStreamMode, "relay"); setErr != nil {
		t.Fatalf("SetPref: %v", setErr)
	}
	if setErr := cat1.SetPref(ctx, PrefQobuzMaxQuality, "flac_24_96"); setErr != nil {
		t.Fatalf("SetPref quality: %v", setErr)
	}

	// Provider credentials CRUD (stored separately from app_prefs).
	type creds struct {
		AppID     string `json:"appId"`
		AuthToken string `json:"authToken"`
	}
	if credErr := cat1.SetProviderCredentials(ctx, "qobuz", creds{AppID: "123", AuthToken: "secret-tok"}); credErr != nil {
		t.Fatalf("SetProviderCredentials: %v", credErr)
	}
	prefs, err := cat1.ListPrefs(ctx)
	if err != nil {
		t.Fatalf("ListPrefs: %v", err)
	}
	if len(prefs) != 2 || prefs[PrefStreamMode] != "relay" {
		t.Fatalf("ListPrefs = %+v", prefs)
	}
	if closeErr := cat1.Close(); closeErr != nil {
		t.Fatalf("Close(1): %v", closeErr)
	}

	// Reopen DB to verify everything survived restart.
	cat2, err := NewCatalogWithDB(dbPath)
	if err != nil {
		t.Fatalf("NewCatalogWithDB(2): %v", err)
	}
	defer func() { _ = cat2.Close() }()

	if val, ok, getErr := cat2.GetPref(ctx, PrefStreamMode); getErr != nil || !ok || val != "relay" {
		t.Fatalf("reopened GetPref = (%q, %v, %v)", val, ok, getErr)
	}
	var gotCreds creds
	if ok, getErr := cat2.GetProviderCredentials(ctx, "qobuz", &gotCreds); getErr != nil || !ok {
		t.Fatalf("reopened GetProviderCredentials = (%v, %v)", ok, getErr)
	}
	if gotCreds.AppID != "123" || gotCreds.AuthToken != "secret-tok" {
		t.Fatalf("gotCreds = %+v", gotCreds)
	}

	// Delete pref & credentials.
	if delErr := cat2.DeletePref(ctx, PrefStreamMode); delErr != nil {
		t.Fatalf("DeletePref: %v", delErr)
	}
	if _, ok, getErr := cat2.GetPref(ctx, PrefStreamMode); getErr != nil || ok {
		t.Fatalf("expected deleted pref, got ok=%v err=%v", ok, getErr)
	}
	if delErr := cat2.DeleteProviderCredentials(ctx, "qobuz"); delErr != nil {
		t.Fatalf("DeleteProviderCredentials: %v", delErr)
	}
	if ok, getErr := cat2.GetProviderCredentials(ctx, "qobuz", &gotCreds); getErr != nil || ok {
		t.Fatalf("expected deleted creds, got ok=%v err=%v", ok, getErr)
	}
}

func TestHeavyRotationFrecencyRanking(t *testing.T) {
	cat, err := NewCatalogWithDB(":memory:")
	if err != nil {
		t.Fatalf("NewCatalogWithDB: %v", err)
	}
	defer func() { _ = cat.Close() }()

	ctx := context.Background()
	now := time.Now().UTC()
	mkAlb := func(extID, title string) Album {
		id := "qobuz:" + extID
		return Album{
			ID:         id,
			Source:     SourceQobuz,
			SourceType: SourceTypeRemote,
			Provider:   "qobuz",
			ExternalID: extID,
			Title:      title,
			Artist:     "Artist",
			Available:  true,
			Tracks: []Track{{
				ID: id + ":t1", AlbumID: id, Source: SourceQobuz, SourceType: SourceTypeRemote,
				Provider: "qobuz", ExternalID: extID + "-t1", Title: "Track 1", Artist: "Artist",
				Album: title, TrackNumber: 1, DurationSec: 180, MIME: "audio/flac", Ext: ".flac",
			}},
		}
	}
	remote := &fakeRemoteSource{
		provider:  "qobuz",
		connected: true,
		albums: []Album{
			mkAlb("recent", "Recent Spin"),
			mkAlb("frequent", "Sustained Favorite"),
			mkAlb("stale", "Old Favorite"),
			mkAlb("expired", "Expired"),
		},
	}
	cat.RegisterRemoteSource(remote)
	if _, syncErr := cat.SyncRemote(ctx, "qobuz"); syncErr != nil {
		t.Fatalf("SyncRemote: %v", syncErr)
	}

	insertPlays := func(albumID string, count int, daysAgo int) {
		ts := now.AddDate(0, 0, -daysAgo).Unix()
		for range count {
			if _, execErr := cat.store.db.ExecContext(ctx,
				`INSERT INTO play_events (track_id, album_id, provider, played_at) VALUES (?, ?, 'qobuz', ?)`,
				albumID+":t1", albumID, ts,
			); execErr != nil {
				t.Fatalf("insert play_event: %v", execErr)
			}
		}
	}

	// Sustained favorite: 6 plays 10 days ago => 6 * 70 = 420
	insertPlays("qobuz:frequent", 6, 10)
	// Recent spin: 2 plays 1 day ago => 2 * 100 = 200
	insertPlays("qobuz:recent", 2, 1)
	// Stale favorite: 4 plays 45 days ago => 4 * 20 = 80
	insertPlays("qobuz:stale", 4, 45)
	// Expired: 10 plays 100 days ago => outside 90-day window
	insertPlays("qobuz:expired", 10, 100)

	rot, err := cat.HeavyRotation(90, 10)
	if err != nil {
		t.Fatalf("HeavyRotation: %v", err)
	}
	if len(rot) != 3 {
		t.Fatalf("len(rot) = %d, want 3 (got %+v)", len(rot), rot)
	}
	wantOrder := []string{"qobuz:frequent", "qobuz:recent", "qobuz:stale"}
	for i, wantID := range wantOrder {
		if rot[i].ID != wantID {
			t.Errorf("rot[%d].ID = %q, want %q", i, rot[i].ID, wantID)
		}
	}
}

func TestPreferLocalAlbumsAndTracks(t *testing.T) {
	musicDir := t.TempDir()
	writeTaggedFLAC(t, musicDir, "Adele/19/04 - Chasing Pavements.flac", 16, 44100, map[string]string{
		"ARTIST":      "Adele",
		"ALBUMARTIST": "Adele",
		"ALBUM":       "19",
		"TITLE":       "Chasing Pavements",
	})

	cat, err := NewCatalogWithDB(":memory:", musicDir)
	if err != nil {
		t.Fatalf("NewCatalogWithDB: %v", err)
	}
	defer func() { _ = cat.Close() }()

	if n, scanErr := cat.Scan(); scanErr != nil || n != 1 {
		t.Fatalf("Scan = (%d, %v), want (1, nil)", n, scanErr)
	}
	localAlbs := cat.Albums()
	localTracks := cat.Tracks()
	if len(localAlbs) != 1 || len(localTracks) != 1 {
		t.Fatalf("expected 1 local album & track, got %d/%d", len(localAlbs), len(localTracks))
	}
	localAlbumID := localAlbs[0].ID
	localTrackID := localTracks[0].ID

	// Remote album matching local (including parenthetical edition suffix) resolves to local.
	gotAlbs := cat.PreferLocalAlbums([]Album{
		{ID: "qobuz:q19", Source: SourceQobuz, Artist: "Adele", Title: "19 (Deluxe Edition)"},
		{ID: "qobuz:q30", Source: SourceQobuz, Artist: "Adele", Title: "30"},
	})
	if len(gotAlbs) != 2 {
		t.Fatalf("len(gotAlbs) = %d, want 2", len(gotAlbs))
	}
	if gotAlbs[0].ID != localAlbumID || gotAlbs[0].Source != SourceLocal || !gotAlbs[0].InLibrary {
		t.Fatalf("expected local album substitution, got %+v", gotAlbs[0])
	}
	if gotAlbs[1].ID != "qobuz:q30" || gotAlbs[1].InLibrary {
		t.Fatalf("expected unmatched remote album untouched, got %+v", gotAlbs[1])
	}

	// Remote track matching local resolves to local (inheriting Qobuz art if local has none).
	gotTracks := cat.PreferLocalTracks([]Track{
		{ID: "qobuz:999", Source: SourceQobuz, Artist: "Adele", Title: "Chasing Pavements", Album: "19", ArtworkURL: "https://qobuz/art.jpg"},
		{ID: "qobuz:888", Source: SourceQobuz, Artist: "Adele", Title: "Skyfall", Album: "Skyfall"},
	})
	if gotTracks[0].ID != localTrackID || gotTracks[0].Source != SourceLocal || !gotTracks[0].InLibrary {
		t.Fatalf("expected local track substitution, got %+v", gotTracks[0])
	}
	if gotTracks[0].ArtworkURL != "https://qobuz/art.jpg" {
		t.Fatalf("expected fallback artwork preserved, got %q", gotTracks[0].ArtworkURL)
	}
	if gotTracks[1].ID != "qobuz:888" || gotTracks[1].Source != SourceQobuz {
		t.Fatalf("expected unmatched remote track untouched, got %+v", gotTracks[1])
	}
}
