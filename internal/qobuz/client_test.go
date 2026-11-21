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
