package cast

import (
	"context"
	"sync"
	"testing"
	"time"

	castproto "github.com/vishen/go-chromecast/cast"

	"tacet/internal/upnp"
)

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
