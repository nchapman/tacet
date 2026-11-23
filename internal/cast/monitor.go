package cast

import (
	"context"
	"log"
	"net"
	"strings"
	"sync"
	"time"

	"github.com/vishen/go-chromecast/dns"

	"tacet/internal/upnp"
)

const (
	defaultSweepInterval = 60 * time.Second
	defaultBrowseTimeout = 5 * time.Second
	defaultMissThreshold = 3
)

// DiscoveredDevice is a lightweight mDNS discovery record for a Google Cast device
// or Google Cast speaker group.
type DiscoveredDevice struct {
	UUID  string
	Name  string
	Model string
	Host  string
	Port  int
}

// IsGroup reports whether this Cast endpoint is a multi-room Google Cast Group.
func (d DiscoveredDevice) IsGroup() bool {
	return strings.EqualFold(strings.TrimSpace(d.Model), "Google Cast Group") || (d.Port != 0 && d.Port != 8009)
}

// Discover scans the local network via mDNS (_googlecast._tcp) for Google Cast
// devices and speaker groups.
func Discover(ctx context.Context) ([]DiscoveredDevice, error) {
	ch, err := dns.DiscoverCastDNSEntries(ctx, nil)
	if err != nil {
		return nil, err
	}
	byID := make(map[string]DiscoveredDevice)
	for {
		select {
		case <-ctx.Done():
			out := make([]DiscoveredDevice, 0, len(byID))
			for _, d := range byID {
				out = append(out, d)
			}
			return out, nil
		case entry, ok := <-ch:
			if !ok {
				out := make([]DiscoveredDevice, 0, len(byID))
				for _, d := range byID {
					out = append(out, d)
				}
				return out, nil
			}
			uuid := strings.TrimSpace(entry.GetUUID())
			addr := strings.TrimSpace(entry.GetAddr())
			port := entry.GetPort()
			if uuid == "" || addr == "" || port < 1024 || port > 65535 {
				continue
			}
			// Only accept private RFC1918 IPv4 LAN addresses (blocks loopback & link-local metadata IPs).
			ip := net.ParseIP(addr)
			if ip == nil || ip.To4() == nil || !ip.IsPrivate() {
				continue
			}
			name := strings.TrimSpace(entry.GetName())
			if name == "" {
				name = strings.TrimSpace(entry.DeviceName)
			}
			if name == "" {
				name = addr
			}
			byID[uuid] = DiscoveredDevice{
				UUID:  uuid,
				Name:  name,
				Model: strings.TrimSpace(entry.Device),
				Host:  ip.To4().String(),
				Port:  port,
			}
		}
	}
}

// MonitorConfig configures continuous mDNS Google Cast discovery.
type MonitorConfig struct {
	SweepInterval time.Duration // defaults to 60s
	BrowseTimeout time.Duration // defaults to 5s
	MissThreshold int           // consecutive missed sweeps before evicting (defaults to 3)

	// Optional override for unit tests.
	DiscoverFn func(ctx context.Context) ([]DiscoveredDevice, error)
}

// MonitorCallbacks connects Monitor to the zone manager.
type MonitorCallbacks struct {
	// ListUPnPHosts returns the set of IP hosts currently served by UPnP renderers
	// so single Cast devices on the same host are skipped in favor of UPnP.
	// Cast speaker groups are always kept.
	ListUPnPHosts func() map[string]struct{}
	ListCast      func() []upnp.RendererInfo
	IsActive      func(id string) bool
	OnFound       func(r *Renderer)
	OnRemoved     func(id, name string)
}

// Monitor continuously discovers Google Cast devices and speaker groups on the LAN.
type Monitor struct {
	cfg MonitorConfig
	cb  MonitorCallbacks

	mu       sync.Mutex
	started  bool
	cancel   context.CancelFunc
	done     chan struct{}
	missed   map[string]int
	loggedID map[string]string // id -> last logged location
}

// NewMonitor constructs a Cast Monitor with sensible defaults.
func NewMonitor(cfg MonitorConfig, cb MonitorCallbacks) *Monitor {
	if cfg.SweepInterval <= 0 {
		cfg.SweepInterval = defaultSweepInterval
	}
	if cfg.BrowseTimeout <= 0 {
		cfg.BrowseTimeout = defaultBrowseTimeout
	}
	if cfg.MissThreshold <= 0 {
		cfg.MissThreshold = defaultMissThreshold
	}
	if cfg.DiscoverFn == nil {
		cfg.DiscoverFn = Discover
	}
	return &Monitor{
		cfg:      cfg,
		cb:       cb,
		missed:   make(map[string]int),
		loggedID: make(map[string]string),
	}
}

// Start begins background mDNS discovery. Calling Start more than once is a no-op.
func (m *Monitor) Start(ctx context.Context) {
	m.mu.Lock()
	if m.started {
		m.mu.Unlock()
		return
	}
	runCtx, cancel := context.WithCancel(ctx)
	m.started = true
	m.cancel = cancel
	m.done = make(chan struct{})
	m.mu.Unlock()

	go m.run(runCtx)
}

// Stop stops background discovery and waits for clean shutdown.
func (m *Monitor) Stop() {
	m.mu.Lock()
	if !m.started {
		m.mu.Unlock()
		return
	}
	cancel := m.cancel
	done := m.done
	m.mu.Unlock()

	cancel()
	<-done
}

func (m *Monitor) run(ctx context.Context) {
	defer close(m.done)

	// Short delay on startup so UPnP SSDP discovery registers dual-protocol devices
	// (like WiiM Ultra) first, allowing UPnP-over-Cast preference to kick in cleanly.
	select {
	case <-ctx.Done():
		return
	case <-time.After(3 * time.Second):
	}

	m.sweep(ctx)

	ticker := time.NewTicker(m.cfg.SweepInterval)
	defer ticker.Stop()

	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			m.sweep(ctx)
		}
	}
}

func (m *Monitor) sweep(ctx context.Context) {
	bctx, cancel := context.WithTimeout(ctx, m.cfg.BrowseTimeout)
	devices, err := m.cfg.DiscoverFn(bctx)
	cancel()
	if err != nil {
		if ctx.Err() == nil {
			log.Printf("Google Cast mDNS discovery warning: %v", err)
		}
		return
	}

	var upnpHosts map[string]struct{}
	if m.cb.ListUPnPHosts != nil {
		upnpHosts = m.cb.ListUPnPHosts()
	}

	seen := make(map[string]struct{}, len(devices))
	for _, dev := range devices {
		uuid := strings.TrimSpace(dev.UUID)
		if uuid == "" {
			continue
		}
		id := "cast:" + uuid
		// Prefer UPnP over Cast when the same single device exposes both,
		// but always keep Google Cast multi-room speaker groups.
		if !dev.IsGroup() {
			if _, hasUPnP := upnpHosts[dev.Host]; hasUPnP {
				continue
			}
		}
		seen[id] = struct{}{}
		r := NewRenderer(uuid, dev.Name, dev.Model, dev.Host, dev.Port)
		m.recordFound(r)
	}

	if m.cb.ListCast == nil {
		return
	}
	for _, info := range m.cb.ListCast() {
		if ctx.Err() != nil {
			return
		}
		if _, ok := seen[info.ID]; ok {
			continue
		}
		// Never evict an actively playing or buffering Cast zone.
		if m.cb.IsActive != nil && m.cb.IsActive(info.ID) {
			m.mu.Lock()
			m.missed[info.ID] = 0
			m.mu.Unlock()
			continue
		}

		// If a UPnP renderer appeared on the same host (and this isn't a Cast group),
		// immediately drop the duplicate Cast zone in favor of UPnP.
		isGroup := strings.EqualFold(info.Model, "Google Cast Group") || !strings.HasSuffix(info.Location, ":8009")
		if !isGroup {
			if _, hasUPnP := upnpHosts[info.Host]; hasUPnP {
				m.evict(info.ID, info.Name, "replaced by UPnP")
				continue
			}
		}

		m.mu.Lock()
		m.missed[info.ID]++
		misses := m.missed[info.ID]
		m.mu.Unlock()

		if misses < m.cfg.MissThreshold {
			continue
		}
		m.evict(info.ID, info.Name, "offline")
	}
}

func (m *Monitor) recordFound(r *Renderer) {
	info := r.Info()
	m.mu.Lock()
	m.missed[info.ID] = 0
	prevLoc, existed := m.loggedID[info.ID]
	changed := !existed || prevLoc != info.Location
	m.loggedID[info.ID] = info.Location
	m.mu.Unlock()

	if m.cb.OnFound != nil {
		m.cb.OnFound(r)
	}
	if changed {
		log.Printf("discovered Google Cast device: %s [%s] (%s)", info.Name, info.Model, info.Location)
	}
}

func (m *Monitor) evict(id, name, reason string) {
	m.mu.Lock()
	delete(m.missed, id)
	delete(m.loggedID, id)
	m.mu.Unlock()

	if m.cb.OnRemoved != nil {
		m.cb.OnRemoved(id, name)
	}
	log.Printf("Google Cast device removed (%s): %s (%s)", reason, name, id)
}
