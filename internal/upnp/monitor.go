package upnp

import (
	"bufio"
	"bytes"
	"context"
	"io"
	"log"
	"net"
	"net/http"
	"strings"
	"sync"
	"time"
)

const (
	defaultSweepInterval = 60 * time.Second
	defaultMissThreshold = 3
	defaultProbeTimeout  = 2 * time.Second
	ssdpMulticastAddr    = "239.255.255.250:1900"
)

// MonitorConfig configures automatic UPnP/DLNA renderer discovery and lifecycle
// tracking (periodic M-SEARCH sweeps, passive SSDP NOTIFY listening, and
// liveness-probed offline removal).
type MonitorConfig struct {
	SweepInterval time.Duration // defaults to 60s
	MissThreshold int           // consecutive missed sweeps before probing & removing (defaults to 3)
	ProbeTimeout  time.Duration // HTTP liveness probe timeout (defaults to 2s)
	DisableNotify bool          // disables UDP 239.255.255.250:1900 NOTIFY listener (useful in tests)

	// Optional overrides for unit testing.
	DiscoverFn func(ctx context.Context) ([]*Renderer, error)
	ConnectFn  func(ctx context.Context, location string) (*Renderer, error)
	ProbeFn    func(ctx context.Context, location string) bool
}

// MonitorCallbacks connects Monitor to the zone manager.
type MonitorCallbacks struct {
	ListUPnP  func() []RendererInfo
	IsActive  func(id string) bool
	OnFound   func(r *Renderer)
	OnRemoved func(id, name string)
}

// Monitor continuously tracks UPnP/DLNA renderers joining and leaving the LAN.
type Monitor struct {
	cfg MonitorConfig
	cb  MonitorCallbacks

	mu           sync.Mutex
	started      bool
	cancel       context.CancelFunc
	done         chan struct{}
	missed       map[string]int
	knownLocs    map[string]time.Time // recently fetched SSDP NOTIFY locations
	loggedRender map[string]string    // id -> last logged location
	connectSem   chan struct{}        // bounds concurrent SSDP NOTIFY ConnectFn fetches
}

// NewMonitor constructs a Monitor with sensible defaults.
func NewMonitor(cfg MonitorConfig, cb MonitorCallbacks) *Monitor {
	if cfg.SweepInterval <= 0 {
		cfg.SweepInterval = defaultSweepInterval
	}
	if cfg.MissThreshold <= 0 {
		cfg.MissThreshold = defaultMissThreshold
	}
	if cfg.ProbeTimeout <= 0 {
		cfg.ProbeTimeout = defaultProbeTimeout
	}
	if cfg.DiscoverFn == nil {
		cfg.DiscoverFn = Discover
	}
	if cfg.ConnectFn == nil {
		cfg.ConnectFn = ConnectByURL
	}
	if cfg.ProbeFn == nil {
		cfg.ProbeFn = ProbeLocation
	}
	return &Monitor{
		cfg:          cfg,
		cb:           cb,
		missed:       make(map[string]int),
		knownLocs:    make(map[string]time.Time),
		loggedRender: make(map[string]string),
		connectSem:   make(chan struct{}, 4),
	}
}

// ProbeLocation performs a fast HTTP GET to a renderer's UPnP description URL
// to verify whether the device is still reachable on the LAN before removing it.
func ProbeLocation(ctx context.Context, rawURL string) bool {
	if strings.TrimSpace(rawURL) == "" {
		return false
	}
	loc, err := validateRendererURL(ctx, rawURL)
	if err != nil {
		return false
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, loc.String(), nil)
	if err != nil {
		return false
	}
	client := newSOAPHTTPClient()
	resp, err := client.Do(req)
	if err != nil {
		return false
	}
	_, _ = io.Copy(io.Discard, io.LimitReader(resp.Body, 4096))
	_ = resp.Body.Close()
	return resp.StatusCode == http.StatusOK
}

// Start launches the initial discovery sweep, periodic M-SEARCH ticker, and
// passive SSDP NOTIFY multicast listener.
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

// Stop stops the monitor and waits for its background goroutines to exit.
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

	var wg sync.WaitGroup
	if !m.cfg.DisableNotify {
		wg.Add(1)
		go func() {
			defer wg.Done()
			m.listenSSDPNotify(ctx)
		}()
	}

	// Immediate initial sweep on startup.
	m.sweep(ctx, true)

	ticker := time.NewTicker(m.cfg.SweepInterval)
	defer ticker.Stop()

	for {
		select {
		case <-ctx.Done():
			wg.Wait()
			return
		case <-ticker.C:
			m.sweep(ctx, false)
		}
	}
}

func (m *Monitor) sweep(ctx context.Context, initial bool) {
	dctx, cancel := context.WithTimeout(ctx, 5*time.Second)
	devices, err := m.cfg.DiscoverFn(dctx)
	cancel()
	if err != nil {
		if ctx.Err() == nil && initial {
			log.Printf("SSDP discovery warning: %v", err)
		}
		return
	}

	seen := make(map[string]struct{}, len(devices))
	for _, d := range devices {
		info := d.Info()
		seen[info.ID] = struct{}{}
		m.recordFound(d)
	}
	if initial && len(devices) == 0 {
		log.Printf("no UPnP renderers discovered yet — monitoring network automatically")
	}

	// Check registered UPnP renderers that did not reply to this sweep.
	if m.cb.ListUPnP == nil {
		return
	}
	registered := m.cb.ListUPnP()
	for _, info := range registered {
		if ctx.Err() != nil {
			return
		}
		if _, ok := seen[info.ID]; ok {
			continue
		}
		// Never evict a renderer that is actively playing or buffering.
		if m.cb.IsActive != nil && m.cb.IsActive(info.ID) {
			m.mu.Lock()
			m.missed[info.ID] = 0
			m.mu.Unlock()
			continue
		}

		m.mu.Lock()
		m.missed[info.ID]++
		misses := m.missed[info.ID]
		m.mu.Unlock()

		if misses < m.cfg.MissThreshold {
			continue
		}

		// Confirm via direct HTTP probe before evicting (multicast UDP can drop packets).
		pctx, pcancel := context.WithTimeout(ctx, m.cfg.ProbeTimeout)
		alive := m.cfg.ProbeFn(pctx, info.Location)
		pcancel()
		if alive {
			m.mu.Lock()
			m.missed[info.ID] = 0
			m.mu.Unlock()
			continue
		}

		m.evict(info.ID, info.Name, "unreachable")
	}
}

func (m *Monitor) recordFound(r *Renderer) {
	info := r.Info()
	m.mu.Lock()
	m.missed[info.ID] = 0
	prevLoc, existed := m.loggedRender[info.ID]
	changed := !existed || prevLoc != info.Location
	m.loggedRender[info.ID] = info.Location
	if info.Location != "" {
		m.knownLocs[info.Location] = time.Now()
	}
	m.mu.Unlock()

	if m.cb.OnFound != nil {
		m.cb.OnFound(r)
	}
	if changed {
		log.Printf("discovered UPnP renderer: %s [%s] (%s, %d protocols, gapless=%v, events=%v)",
			info.Name, info.Model, info.Host, len(info.SinkProtocols), info.SupportsSetNext, info.SupportsEvents)
	}
}

func (m *Monitor) evict(id, name, reason string) {
	m.mu.Lock()
	delete(m.missed, id)
	if loc, ok := m.loggedRender[id]; ok {
		delete(m.knownLocs, loc)
		delete(m.loggedRender, id)
	}
	m.mu.Unlock()

	if m.cb.OnRemoved != nil {
		m.cb.OnRemoved(id, name)
	}
	log.Printf("UPnP renderer offline (%s): %s (%s)", reason, name, id)
}

func (m *Monitor) listenSSDPNotify(ctx context.Context) {
	gaddr, err := net.ResolveUDPAddr("udp4", ssdpMulticastAddr)
	if err != nil {
		return
	}
	conn, err := net.ListenMulticastUDP("udp4", nil, gaddr)
	if err != nil {
		log.Printf("SSDP NOTIFY multicast listener unavailable (periodic discovery still active): %v", err)
		return
	}
	defer func() { _ = conn.Close() }()

	go func() {
		<-ctx.Done()
		_ = conn.Close()
	}()

	buf := make([]byte, 8192)
	for {
		n, _, readErr := conn.ReadFromUDP(buf)
		if readErr != nil {
			return
		}
		ev, ok := parseSSDPNotify(buf[:n])
		if !ok {
			continue
		}
		m.handleSSDPNotify(ctx, ev)
	}
}

type ssdpNotifyEvent struct {
	nts      string // "ssdp:alive" or "ssdp:byebye"
	nt       string
	usn      string
	udn      string // extracted "uuid:..."
	location string
}

func parseSSDPNotify(pkt []byte) (ssdpNotifyEvent, bool) {
	sc := bufio.NewScanner(bytes.NewReader(pkt))
	if !sc.Scan() {
		return ssdpNotifyEvent{}, false
	}
	firstLine := strings.TrimSpace(sc.Text())
	if !strings.HasPrefix(strings.ToUpper(firstLine), "NOTIFY ") {
		return ssdpNotifyEvent{}, false
	}

	var ev ssdpNotifyEvent
	for sc.Scan() {
		line := strings.TrimSpace(sc.Text())
		if line == "" {
			break
		}
		k, v, ok := strings.Cut(line, ":")
		if !ok {
			continue
		}
		val := strings.TrimSpace(v)
		switch strings.ToLower(strings.TrimSpace(k)) {
		case "nts":
			ev.nts = strings.ToLower(val)
		case "nt":
			ev.nt = val
		case "usn":
			ev.usn = val
		case "location":
			ev.location = val
		}
	}

	if ev.nts != "ssdp:alive" && ev.nts != "ssdp:byebye" {
		return ssdpNotifyEvent{}, false
	}
	// Only react to MediaRenderer / AVTransport announcements to avoid fetching
	// XML from every router, printer, and Hue bridge on the LAN.
	ntLower := strings.ToLower(ev.nt)
	usnLower := strings.ToLower(ev.usn)
	if !strings.Contains(ntLower, "avtransport") &&
		!strings.Contains(ntLower, "mediarenderer") &&
		!strings.Contains(usnLower, "avtransport") &&
		!strings.Contains(usnLower, "mediarenderer") {
		return ssdpNotifyEvent{}, false
	}

	// Extract UDN ("uuid:...") from USN ("uuid:xxx::urn:schemas-upnp-org:...").
	if idx := strings.Index(ev.usn, "::"); idx > 0 {
		ev.udn = strings.TrimSpace(ev.usn[:idx])
	} else {
		ev.udn = strings.TrimSpace(ev.usn)
	}
	return ev, true
}

func (m *Monitor) handleSSDPNotify(ctx context.Context, ev ssdpNotifyEvent) {
	switch ev.nts {
	case "ssdp:alive":
		if ev.location == "" {
			return
		}
		// Dedupe bursts of ssdp:alive packets from the same speaker.
		m.mu.Lock()
		if last, ok := m.knownLocs[ev.location]; ok && time.Since(last) < 30*time.Second {
			if ev.udn != "" {
				m.missed[ev.udn] = 0
			}
			m.mu.Unlock()
			return
		}
		m.knownLocs[ev.location] = time.Now()
		m.mu.Unlock()

		select {
		case m.connectSem <- struct{}{}:
		default:
			// Concurrency cap reached; periodic sweep will pick up any remaining devices.
			return
		}
		go func(loc string) {
			defer func() { <-m.connectSem }()
			cctx, cancel := context.WithTimeout(ctx, 4*time.Second)
			defer cancel()
			r, err := m.cfg.ConnectFn(cctx, loc)
			if err != nil {
				return
			}
			m.recordFound(r)
		}(ev.location)

	case "ssdp:byebye":
		if ev.udn == "" || m.cb.ListUPnP == nil {
			return
		}
		var target *RendererInfo
		for _, info := range m.cb.ListUPnP() {
			if strings.EqualFold(info.ID, ev.udn) {
				cp := info
				target = &cp
				break
			}
		}
		if target == nil {
			return
		}
		if m.cb.IsActive != nil && m.cb.IsActive(target.ID) {
			return
		}
		go func(info RendererInfo) {
			pctx, cancel := context.WithTimeout(ctx, m.cfg.ProbeTimeout)
			alive := m.cfg.ProbeFn(pctx, info.Location)
			cancel()
			if !alive {
				m.evict(info.ID, info.Name, "ssdp:byebye")
			}
		}(*target)
	}
}
