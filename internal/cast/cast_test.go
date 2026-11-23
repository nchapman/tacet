package cast

import (
	"context"
	"encoding/json"
	"sync"
	"testing"
	"time"

	castproto "github.com/vishen/go-chromecast/cast"

	"tacet/internal/upnp"
)

func TestBuildCastLoadPayload(t *testing.T) {
	cmd := &castproto.LoadMediaCommand{
		PayloadHeader: castproto.LoadHeader,
		CurrentTime:   0,
		Autoplay:      true,
		Media: castproto.MediaItem{
			ContentId:   "http://192.168.1.10:4320/media/local/local:1.flac",
			StreamType:  "BUFFERED",
			ContentType: "audio/flac",
		},
	}
	payload := buildCastLoadPayload(cmd, upnp.MediaItem{
		Title:       "All Blues",
		Artist:      "Miles Davis",
		Album:       "Kind of Blue",
		AlbumArtURL: "http://192.168.1.10:4320/media/art/album1",
		TrackNumber: 4,
		Duration:    693 * time.Second,
	})
	raw, err := json.Marshal(payload)
	if err != nil {
		t.Fatalf("marshal payload: %v", err)
	}
	var got struct {
		Type     string `json:"type"`
		Autoplay bool   `json:"autoplay"`
		Media    struct {
			ContentID   string  `json:"contentId"`
			ContentType string  `json:"contentType"`
			Duration    float64 `json:"duration"`
			Metadata    struct {
				MetadataType int    `json:"metadataType"`
				Title        string `json:"title"`
				Artist       string `json:"artist"`
				AlbumName    string `json:"albumName"`
				TrackNumber  int    `json:"trackNumber"`
				Images       []struct {
					URL string `json:"url"`
				} `json:"images"`
			} `json:"metadata"`
		} `json:"media"`
	}
	if err := json.Unmarshal(raw, &got); err != nil {
		t.Fatalf("unmarshal payload: %v", err)
	}
	if got.Type != "LOAD" || !got.Autoplay || got.Media.Duration != 693 {
		t.Fatalf("unexpected load envelope: %+v", got)
	}
	m := got.Media.Metadata
	if m.MetadataType != 3 || m.Title != "All Blues" || m.Artist != "Miles Davis" || m.AlbumName != "Kind of Blue" || m.TrackNumber != 4 {
		t.Fatalf("unexpected music metadata: %+v", m)
	}
	if len(m.Images) != 1 || m.Images[0].URL != "http://192.168.1.10:4320/media/art/album1" {
		t.Fatalf("unexpected metadata images: %+v", m.Images)
	}
}

func TestBuildCastQueueLoadPayload(t *testing.T) {
	var _ upnp.QueueController = (*Renderer)(nil)
	p := buildCastQueueLoadPayload([]upnp.MediaItem{
		{URL: "http://example/1.flac", Title: "One", Artist: "A", Album: "Alb", MIME: "audio/flac"},
		{URL: "http://example/2.flac", Title: "Two", Artist: "A", Album: "Alb", MIME: "audio/flac"},
	}, 1)
	raw, err := json.Marshal(p)
	if err != nil {
		t.Fatalf("marshal queue payload: %v", err)
	}
	var got struct {
		Type      string `json:"type"`
		Autoplay  bool   `json:"autoplay"`
		QueueData struct {
			StartIndex int    `json:"startIndex"`
			RepeatMode string `json:"repeatMode"`
			Items      []struct {
				Autoplay    bool    `json:"autoplay"`
				PreloadTime float64 `json:"preloadTime"`
				Media       struct {
					ContentID string `json:"contentId"`
					Metadata  struct {
						MetadataType int    `json:"metadataType"`
						Title        string `json:"title"`
					} `json:"metadata"`
				} `json:"media"`
			} `json:"items"`
		} `json:"queueData"`
	}
	if err := json.Unmarshal(raw, &got); err != nil {
		t.Fatalf("unmarshal queue payload: %v", err)
	}
	if got.Type != "LOAD" || !got.Autoplay || got.QueueData.StartIndex != 1 || got.QueueData.RepeatMode != "REPEAT_OFF" || len(got.QueueData.Items) != 2 {
		t.Fatalf("unexpected queue load payload: %+v", got)
	}
	if got.QueueData.Items[1].Media.Metadata.MetadataType != 3 || got.QueueData.Items[1].Media.Metadata.Title != "Two" {
		t.Fatalf("unexpected second queue item metadata: %+v", got.QueueData.Items[1])
	}
}

func TestMapCastStatus(t *testing.T) {
	st := mapCastStatus(
		&castproto.Media{
			PlayerState: "PLAYING",
			CurrentTime: 42.5,
			Media: castproto.MediaItem{
				ContentId: "http://192.168.1.10:4320/media/local/local:1.flac",
				Duration:  240,
			},
		},
		&castproto.Volume{Level: 0.65, Muted: false},
		"",
	)
	if st.State != "PLAYING" || !st.HasPosition || st.Position != 42500*time.Millisecond {
		t.Fatalf("unexpected playback status: %+v", st)
	}
	if st.Duration != 240*time.Second || !st.HasVolValue || st.Volume != 65 || st.Muted {
		t.Fatalf("unexpected duration/volume: %+v", st)
	}

	paused := mapCastStatus(&castproto.Media{PlayerState: "PAUSED"}, &castproto.Volume{Level: 0.2, Muted: true}, "")
	if paused.State != "PAUSED_PLAYBACK" || !paused.Muted || paused.Volume != 20 {
		t.Fatalf("unexpected paused status: %+v", paused)
	}
}

func TestMonitor_PrefersUPnPButKeepsCastGroups(t *testing.T) {
	var (
		mu         sync.Mutex
		registered = map[string]upnp.RendererInfo{}
	)
	upnpHosts := map[string]struct{}{
		"192.168.1.51": {}, // WiiM Ultra already discovered via UPnP
	}

	mon := NewMonitor(MonitorConfig{
		SweepInterval: 30 * time.Millisecond,
		BrowseTimeout: 50 * time.Millisecond,
		MissThreshold: 2,
		DiscoverFn: func(_ context.Context) ([]DiscoveredDevice, error) {
			return []DiscoveredDevice{
				// Duplicate single device on same host as UPnP -> must be skipped!
				{UUID: "wiim-cast", Name: "Living Room Wiim", Model: "WiiM Ultra", Host: "192.168.1.51", Port: 8009},
				// Cast speaker group hosted on same IP (non-8009 port) -> must be KEPT!
				{UUID: "group-1", Name: "Whole Home", Model: "Google Cast Group", Host: "192.168.1.51", Port: 32123},
				// Pure Cast speaker -> must be KEPT!
				{UUID: "nest-1", Name: "Kitchen Nest", Model: "Nest Audio", Host: "192.168.1.77", Port: 8009},
			}, nil
		},
	}, MonitorCallbacks{
		ListUPnPHosts: func() map[string]struct{} { return upnpHosts },
		ListCast: func() []upnp.RendererInfo {
			mu.Lock()
			defer mu.Unlock()
			out := make([]upnp.RendererInfo, 0, len(registered))
			for _, info := range registered {
				out = append(out, info)
			}
			return out
		},
		IsActive: func(_ string) bool { return false },
		OnFound: func(r *Renderer) {
			mu.Lock()
			registered[r.Info().ID] = r.Info()
			mu.Unlock()
		},
		OnRemoved: func(id, _ string) {
			mu.Lock()
			delete(registered, id)
			mu.Unlock()
		},
	})

	mon.sweep(context.Background())

	mu.Lock()
	defer mu.Unlock()
	if _, hasDup := registered["cast:wiim-cast"]; hasDup {
		t.Fatalf("expected cast:wiim-cast to be skipped in favor of UPnP")
	}
	if _, hasGroup := registered["cast:group-1"]; !hasGroup {
		t.Fatalf("expected Cast speaker group cast:group-1 to be kept")
	}
	if _, hasNest := registered["cast:nest-1"]; !hasNest {
		t.Fatalf("expected pure Cast speaker cast:nest-1 to be kept")
	}
}
