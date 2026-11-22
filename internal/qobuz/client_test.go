package qobuz

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"testing"
)

func TestSignFileURL(t *testing.T) {
	// Verify deterministic MD5 of "trackgetFileUrlformat_id27intentstreamtrack_id1234561700000000secret"
	got := SignFileURL(123456, 27, "stream", 1700000000, "secret")
	want := md5Hex("trackgetFileUrlformat_id27intentstreamtrack_id1234561700000000secret")
	if got != want {
		t.Fatalf("SignFileURL = %q, want %q", got, want)
	}
}

func TestClient_SearchAndStreamURL(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Header.Get("X-App-Id") != "app123" {
			http.Error(w, "missing app id", http.StatusBadRequest)
			return
		}
		if r.Header.Get("X-User-Auth-Token") != "tok456" {
			http.Error(w, "missing auth token", http.StatusUnauthorized)
			return
		}

		switch r.URL.Path {
		case "/catalog/search":
			_ = json.NewEncoder(w).Encode(map[string]any{
				"tracks": map[string]any{
					"items": []map[string]any{
						{
							"id":                    999,
							"title":                 "Blue in Green",
							"track_number":          3,
							"duration":              337,
							"maximum_bit_depth":     24,
							"maximum_sampling_rate": 192.0,
							"performer":             map[string]any{"name": "Miles Davis"},
							"album": map[string]any{
								"id":    "alb1",
								"title": "Kind of Blue",
								"image": map[string]any{"large": "https://static.qobuz.com/cover.jpg"},
							},
						},
					},
				},
				"albums": map[string]any{"items": []any{}},
			})
		case "/track/getFileUrl":
			q := r.URL.Query()
			if q.Get("format_id") != "27" || q.Get("track_id") != "999" || q.Get("request_sig") == "" {
				http.Error(w, "bad params", http.StatusBadRequest)
				return
			}
			_ = json.NewEncoder(w).Encode(map[string]any{
				"url":           "https://streaming-qobuz-std.akamaized.net/file.flac",
				"mime_type":     "audio/flac",
				"bit_depth":     24,
				"sampling_rate": 192.0,
				"duration":      337,
				"format_id":     27,
			})
		default:
			http.NotFound(w, r)
		}
	}))
	defer srv.Close()

	c := NewClient(Config{
		BaseURL:   srv.URL,
		AppID:     "app123",
		AppSecret: "sec789",
		AuthToken: "tok456",
	})

	res, err := c.Search(context.Background(), "miles", 10)
	if err != nil {
		t.Fatalf("Search failed: %v", err)
	}
	if len(res.Tracks) != 1 || res.Tracks[0].SampleRate != 192000 || res.Tracks[0].BitDepth != 24 {
		t.Fatalf("unexpected search tracks: %+v", res.Tracks)
	}

	stream, err := c.StreamURL(context.Background(), 999)
	if err != nil {
		t.Fatalf("StreamURL failed: %v", err)
	}
	if stream.URL != "https://streaming-qobuz-std.akamaized.net/file.flac" || stream.SampleRate != 192000 || stream.BitDepth != 24 {
		t.Fatalf("unexpected stream info: %+v", stream)
	}
}

func TestClient_OAuthExchangeAndTokenLogin(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/oauth/callback":
			if r.URL.Query().Get("code") != "auth-code-xyz" || r.URL.Query().Get("private_key") != "pk123" {
				http.Error(w, "bad oauth params", http.StatusBadRequest)
				return
			}
			_ = json.NewEncoder(w).Encode(map[string]any{
				"token":   "fresh-user-token",
				"user_id": 4242,
			})
		case "/user/login":
			if r.Method != http.MethodPost || r.Header.Get("X-User-Auth-Token") != "fresh-user-token" {
				http.Error(w, "unauthorized", http.StatusUnauthorized)
				return
			}
			_ = json.NewEncoder(w).Encode(map[string]any{
				"user": map[string]any{
					"display_name": "Hi-Res Listener",
					"subscription": map[string]any{"offer": "studio"},
				},
				"user_auth_token": "fresh-user-token",
			})
		default:
			http.NotFound(w, r)
		}
	}))
	defer srv.Close()

	c := NewClient(Config{
		BaseURL:   srv.URL,
		AppID:     "app123",
		AppSecret: "sec789",
	})
	c.privateKey = "pk123"

	st, err := c.ExchangeOAuthCode(context.Background(), "auth-code-xyz")
	if err != nil {
		t.Fatalf("ExchangeOAuthCode failed: %v", err)
	}
	if !st.Authenticated || st.UserName != "Hi-Res Listener" || st.Subscription != "studio" {
		t.Fatalf("unexpected status after OAuth: %+v", st)
	}
}

func TestClient_SecretFailoverCacheAndGetAlbum(t *testing.T) {
	fileURLCalls := 0
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/track/getFileUrl":
			fileURLCalls++
			// Reject first secret with 400 Invalid Request Signature; accept second secret.
			ts := r.URL.Query().Get("request_ts")
			var tsInt int64
			for _, ch := range ts {
				tsInt = tsInt*10 + int64(ch-'0')
			}
			wantSig := SignFileURL(777, FormatFLAC192, "stream", tsInt, "good-secret")
			if r.URL.Query().Get("request_sig") != wantSig {
				http.Error(w, `{"message":"Invalid Request Signature"}`, http.StatusBadRequest)
				return
			}
			_ = json.NewEncoder(w).Encode(map[string]any{
				"url":           "https://cdn.example/777.flac",
				"mime_type":     "audio/x-flac",
				"bit_depth":     24,
				"sampling_rate": 96.0,
				"duration":      240,
				"format_id":     27,
			})
		case "/album/get":
			_ = json.NewEncoder(w).Encode(map[string]any{
				"id":                    "alb-1",
				"title":                 "A Love Supreme",
				"artist":                map[string]any{"name": "John Coltrane"},
				"image":                 map[string]any{"large": "https://img/coltrane.jpg"},
				"tracks_count":          1,
				"release_date_original": "1965-01-01",
				"maximum_bit_depth":     24,
				"maximum_sampling_rate": 192.0,
				"tracks": map[string]any{
					"items": []map[string]any{
						{
							"id":                    777,
							"title":                 "Acknowledgement",
							"track_number":          1,
							"media_number":          1,
							"duration":              462,
							"maximum_bit_depth":     24,
							"maximum_sampling_rate": 192.0,
							"performer":             map[string]any{"name": "John Coltrane"},
						},
					},
				},
			})
		default:
			http.NotFound(w, r)
		}
	}))
	defer srv.Close()

	c := NewClient(Config{
		BaseURL:   srv.URL,
		AppID:     "app1",
		AppSecret: "bad-secret",
		AuthToken: "tok1",
	})
	c.altSecrets = []string{"bad-secret", "good-secret"}

	info, err := c.StreamURL(context.Background(), 777)
	if err != nil {
		t.Fatalf("StreamURL with secret failover failed: %v", err)
	}
	if info.MIME != "audio/flac" || info.SampleRate != 96000 || info.BitDepth != 24 {
		t.Fatalf("unexpected StreamInfo: %+v", info)
	}
	if fileURLCalls != 2 {
		t.Fatalf("expected 2 calls (bad then good secret), got %d", fileURLCalls)
	}

	// Second call should hit cache without hitting HTTP server.
	if _, err = c.StreamURL(context.Background(), 777); err != nil {
		t.Fatalf("cached StreamURL failed: %v", err)
	}
	if fileURLCalls != 2 {
		t.Fatalf("expected cached call not to hit server, got %d calls", fileURLCalls)
	}

	// GetAlbum should map album and track metadata accurately.
	alb, err := c.GetAlbum(context.Background(), "alb-1")
	if err != nil {
		t.Fatalf("GetAlbum failed: %v", err)
	}
	if alb.Title != "A Love Supreme" || alb.Year != 1965 || len(alb.Tracks) != 1 || alb.Tracks[0].SampleRate != 192000 {
		t.Fatalf("unexpected GetAlbum result: %+v", alb)
	}
}

func TestRichEditorialMetadataAndPopularTracks(t *testing.T) {
	if got := cleanQobuzText("<p>A <i>classic</i>&nbsp;record.<br/>Second line &amp; more.</p>"); got != "A classic\u00a0record.\nSecond line & more." {
		t.Fatalf("unexpected cleanQobuzText output: %q", got)
	}

	var sawArtistPage bool
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/album/get":
			_ = json.NewEncoder(w).Encode(map[string]any{
				"id":                    "alb-rich",
				"title":                 "Kind of Blue",
				"artist":                map[string]any{"id": 42, "name": "Miles Davis"},
				"genre":                 map[string]any{"name": "Jazz"},
				"label":                 map[string]any{"name": "Columbia Legacy"},
				"description":           "<p>Landmark modal jazz.<br/>Recorded in 1959.</p>",
				"copyright":             "℗ 1959 Sony Music Entertainment",
				"release_date_original": "1959-08-17",
				"tracks_count":          1,
				"maximum_bit_depth":     24,
				"maximum_sampling_rate": 192,
				"tracks":                map[string]any{"items": []any{}},
			})
		case "/artist/get":
			_ = json.NewEncoder(w).Encode(map[string]any{
				"id":           42,
				"name":         "Miles Davis",
				"albums_count": 120,
				"biography": map[string]any{
					"summary": "Short bio",
					"content": "<p>Trumpeter &amp; bandleader.<br/>Icon of jazz.</p>",
				},
				"albums": map[string]any{"items": []any{}},
			})
		case "/artist/page":
			sawArtistPage = true
			_ = json.NewEncoder(w).Encode(map[string]any{
				"top_tracks": []any{
					map[string]any{
						"id":       901,
						"title":    "So What",
						"duration": 562,
						"audio_info": map[string]any{
							"maximum_bit_depth":     24,
							"maximum_sampling_rate": 192,
						},
						"rights": map[string]any{"streamable": true},
						"artist": map[string]any{"name": map[string]any{"display": "Miles Davis"}},
						"album": map[string]any{
							"id":    "alb-rich",
							"title": "Kind of Blue",
						},
					},
					map[string]any{
						"id":     902,
						"title":  "Unavailable Track",
						"rights": map[string]any{"streamable": false},
					},
				},
			})
		default:
			http.NotFound(w, r)
		}
	}))
	defer srv.Close()

	c := NewClient(Config{BaseURL: srv.URL, AppID: "123456"})
	c.authToken = "tok"

	alb, err := c.GetAlbum(context.Background(), "alb-rich")
	if err != nil {
		t.Fatalf("GetAlbum failed: %v", err)
	}
	if alb.Label != "Columbia Legacy" {
		t.Fatalf("expected label 'Columbia Legacy', got %q", alb.Label)
	}
	if alb.Description != "Landmark modal jazz.\nRecorded in 1959." {
		t.Fatalf("unexpected description: %q", alb.Description)
	}
	if alb.Copyright != "℗ 1959 Sony Music Entertainment" {
		t.Fatalf("unexpected copyright: %q", alb.Copyright)
	}

	detail, err := c.GetArtist(context.Background(), 42, 20)
	if err != nil {
		t.Fatalf("GetArtist failed: %v", err)
	}
	if !sawArtistPage {
		t.Fatalf("expected /artist/page to be queried for top_tracks")
	}
	if detail.Artist.Biography != "Trumpeter & bandleader.\nIcon of jazz." {
		t.Fatalf("unexpected artist biography: %q", detail.Artist.Biography)
	}
	if len(detail.Tracks) != 1 || detail.Tracks[0].Title != "So What" || detail.Tracks[0].QobuzTrackID != 901 {
		t.Fatalf("expected 1 streamable popular track 'So What', got %+v", detail.Tracks)
	}
	if _, err := c.ResolveTrack(context.Background(), 901); err != nil {
		t.Fatalf("expected popular track 901 to be cached in trackCache: %v", err)
	}
}
