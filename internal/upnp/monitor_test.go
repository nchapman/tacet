package upnp

import (
	"context"
	"sync"
	"sync/atomic"
	"testing"
	"time"
)

func TestParseSSDPNotify(t *testing.T) {
	alivePkt := []byte("NOTIFY * HTTP/1.1\r\n" +
		"HOST: 239.255.255.250:1900\r\n" +
		"NT: urn:schemas-upnp-org:service:AVTransport:1\r\n" +
		"NTS: ssdp:alive\r\n" +
		"LOCATION: http://192.168.1.50:49152/description.xml\r\n" +
		"USN: uuid:wiim-1234::urn:schemas-upnp-org:service:AVTransport:1\r\n\r\n")

	ev, ok := parseSSDPNotify(alivePkt)
	if !ok {
		t.Fatalf("expected valid AVTransport alive notify")
	}
	if ev.nts != "ssdp:alive" || ev.udn != "uuid:wiim-1234" || ev.location != "http://192.168.1.50:49152/description.xml" {
		t.Fatalf("unexpected parsed notify: %+v", ev)
	}

	byebyePkt := []byte("NOTIFY * HTTP/1.1\r\n" +
		"NT: urn:schemas-upnp-org:device:MediaRenderer:1\r\n" +
		"NTS: ssdp:byebye\r\n" +
		"USN: uuid:wiim-1234::urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n")
	ev2, ok := parseSSDPNotify(byebyePkt)
	if !ok || ev2.nts != "ssdp:byebye" || ev2.udn != "uuid:wiim-1234" {
		t.Fatalf("unexpected parsed byebye: %+v ok=%v", ev2, ok)
	}

	// Non-renderer UPnP devices (e.g. InternetGatewayDevice) should be ignored.
	routerPkt := []byte("NOTIFY * HTTP/1.1\r\n" +
		"NT: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n" +
		"NTS: ssdp:alive\r\n" +
		"LOCATION: http://192.168.1.1:5000/rootDesc.xml\r\n" +
		"USN: uuid:router-1::urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\r\n")
	if _, ok := parseSSDPNotify(routerPkt); ok {
		t.Fatalf("expected router notify to be ignored")
	}
}

func TestMonitor_EvictsOfflineAfterMissThresholdAndProbeFailure(t *testing.T) {
	var (
		mu          sync.Mutex
		discoverSet = []*Renderer{{info: RendererInfo{ID: "uuid:spk-1", Name: "Living Room", Location: "http://192.168.1.50:49152/desc.xml"}}}
		registered  = map[string]RendererInfo{}
		removed     []string
	)
	var probeAlive atomic.Bool
	probeAlive.Store(true)

	mon := NewMonitor(MonitorConfig{
		SweepInterval: 30 * time.Millisecond,
		MissThreshold: 2,
		ProbeTimeout:  200 * time.Millisecond,
		DisableNotify: true,
		DiscoverFn: func(_ context.Context) ([]*Renderer, error) {
			mu.Lock()
			defer mu.Unlock()
			cp := make([]*Renderer, len(discoverSet))
			copy(cp, discoverSet)
			return cp, nil
		},
		ProbeFn: func(_ context.Context, _ string) bool {
			return probeAlive.Load()
		},
	}, MonitorCallbacks{
		ListUPnP: func() []RendererInfo {
			mu.Lock()
			defer mu.Unlock()
			out := make([]RendererInfo, 0, len(registered))
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
			removed = append(removed, id)
			mu.Unlock()
		},
	})

	mon.Start(context.Background())
	defer mon.Stop()

	// Wait for initial discovery to register spk-1.
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		mu.Lock()
		_, ok := registered["uuid:spk-1"]
		mu.Unlock()
		if ok {
			break
		}
		time.Sleep(15 * time.Millisecond)
	}

	// Simulate dropped multicast SSDP while direct HTTP probe still succeeds:
	// speaker must NOT be evicted.
	mu.Lock()
	discoverSet = nil
	mu.Unlock()
	time.Sleep(120 * time.Millisecond)
	mu.Lock()
	if _, ok := registered["uuid:spk-1"]; !ok {
		mu.Unlock()
		t.Fatalf("speaker was evicted even though direct HTTP probe succeeded")
	}
	mu.Unlock()

	// Now simulate speaker powered off (direct HTTP probe fails): must be evicted.
	probeAlive.Store(false)
	deadline = time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		mu.Lock()
		n := len(removed)
		mu.Unlock()
		if n > 0 {
			break
		}
		time.Sleep(20 * time.Millisecond)
	}
	mu.Lock()
	defer mu.Unlock()
	if len(removed) != 1 || removed[0] != "uuid:spk-1" {
		t.Fatalf("expected uuid:spk-1 to be evicted, removed=%v", removed)
	}
}
