package upnp

import (
	"bytes"
	"context"
	"crypto/rand"
	"encoding/hex"
	"fmt"
	"io"
	"log"
	"net/http"
	"strconv"
	"strings"
	"sync"
	"time"
)

const (
	// ServiceAVTransport identifies AVTransport:1 GENA events.
	ServiceAVTransport = "AVTransport"
	// ServiceRenderingControl identifies RenderingControl:1 GENA events.
	ServiceRenderingControl = "RenderingControl"

	defaultGENATimeoutSec = 300
	maxGENABodyBytes      = 512 << 10 // 512 KiB
)

// DeviceEvent is a parsed UPnP GENA NOTIFY update from a renderer.
type DeviceEvent struct {
	RendererID  string
	Service     string
	Seq         uint32
	HasState    bool
	State       string
	HasTrackURI bool
	TrackURI    string
	HasDuration bool
	Duration    time.Duration
	HasVolume   bool
	Volume      int
	HasMute     bool
	Muted       bool
}

// HasAnyUpdate reports whether the parsed event carried at least one recognized field.
func (e DeviceEvent) HasAnyUpdate() bool {
	return e.HasState || e.HasTrackURI || e.HasDuration || e.HasVolume || e.HasMute
}

type genaSub struct {
	token           string
	rendererID      string
	service         string
	eventSubURL     string
	callbackBaseURL string

	ctx        context.Context
	cancelFunc context.CancelFunc

	mu     sync.RWMutex
	sid    string
	active bool
}

// GENAClient manages UPnP GENA SUBSCRIBE/renew/UNSUBSCRIBE lifecycles and
// handles incoming HTTP NOTIFY callbacks from renderers.
type GENAClient struct {
	httpClient *http.Client
	onEvent    func(DeviceEvent)

	mu         sync.RWMutex
	byToken    map[string]*genaSub
	byRenderer map[string][]*genaSub // rendererID -> active service subscriptions
	closed     bool

	eventCh  chan DeviceEvent
	stopOnce sync.Once
	stopCh   chan struct{}
}

// NewGENAClient creates a GENA subscription manager and NOTIFY callback handler.
// onEvent is invoked sequentially on a background worker after each NOTIFY
// receives an immediate HTTP 200 OK.
func NewGENAClient(onEvent func(DeviceEvent)) *GENAClient {
	c := newSOAPHTTPClient()
	g := &GENAClient{
		httpClient: &c,
		onEvent:    onEvent,
		byToken:    make(map[string]*genaSub),
		byRenderer: make(map[string][]*genaSub),
		eventCh:    make(chan DeviceEvent, 128),
		stopCh:     make(chan struct{}),
	}
	go g.dispatchLoop()
	return g
}

func (g *GENAClient) dispatchLoop() {
	for {
		select {
		case <-g.stopCh:
			return
		case ev := <-g.eventCh:
			if g.onEvent != nil {
				g.onEvent(ev)
			}
		}
	}
}

// Handler returns an http.Handler to mount at "/upnp/event/".
func (g *GENAClient) Handler() http.Handler {
	return http.HandlerFunc(g.handleNotify)
}

// WatchRenderer starts (or refreshes) GENA event subscriptions for a renderer's
// AVTransport and RenderingControl services. It is safe to call repeatedly on
// re-discovery; duplicate subscriptions for unchanged endpoints are ignored.
func (g *GENAClient) WatchRenderer(ctx context.Context, rendererID, callbackBaseURL, avEventURL, rcEventURL string) {
	callbackBaseURL = strings.TrimRight(strings.TrimSpace(callbackBaseURL), "/")
	if rendererID == "" || callbackBaseURL == "" {
		return
	}

	g.mu.Lock()
	if g.closed {
		g.mu.Unlock()
		return
	}
	existing := g.byRenderer[rendererID]
	if sameEndpoints(existing, callbackBaseURL, avEventURL, rcEventURL) {
		g.mu.Unlock()
		return
	}
	// Tear down stale subscriptions if endpoints changed.
	delete(g.byRenderer, rendererID)
	for _, old := range existing {
		delete(g.byToken, old.token)
	}
	g.mu.Unlock()

	for _, old := range existing {
		if old.cancelFunc != nil {
			old.cancelFunc()
		}
		go g.unsubscribeOneCtx(context.WithoutCancel(ctx), old)
	}

	var subs []*genaSub
	if avEventURL != "" {
		subs = append(subs, g.newSub(ctx, rendererID, ServiceAVTransport, avEventURL, callbackBaseURL))
	}
	if rcEventURL != "" {
		subs = append(subs, g.newSub(ctx, rendererID, ServiceRenderingControl, rcEventURL, callbackBaseURL))
	}
	if len(subs) == 0 {
		return
	}

	g.mu.Lock()
	if g.closed {
		g.mu.Unlock()
		for _, sub := range subs {
			if sub.cancelFunc != nil {
				sub.cancelFunc()
			}
		}
		return
	}
	g.byRenderer[rendererID] = subs
	for _, sub := range subs {
		g.byToken[sub.token] = sub
	}
	g.mu.Unlock()

	for _, sub := range subs {
		go g.runSub(sub)
	}
}

func sameEndpoints(existing []*genaSub, callbackBaseURL, avEventURL, rcEventURL string) bool {
	want := 0
	if avEventURL != "" {
		want++
	}
	if rcEventURL != "" {
		want++
	}
	if len(existing) != want {
		return false
	}
	for _, sub := range existing {
		if sub.callbackBaseURL != callbackBaseURL {
			return false
		}
		if sub.service == ServiceAVTransport && sub.eventSubURL != avEventURL {
			return false
		}
		if sub.service == ServiceRenderingControl && sub.eventSubURL != rcEventURL {
			return false
		}
	}
	return true
}

func (g *GENAClient) newSub(parent context.Context, rendererID, service, eventSubURL, callbackBaseURL string) *genaSub {
	subCtx, cancel := context.WithCancel(parent)
	return &genaSub{
		token:           randomToken(),
		rendererID:      rendererID,
		service:         service,
		eventSubURL:     eventSubURL,
		callbackBaseURL: callbackBaseURL,
		ctx:             subCtx,
		cancelFunc:      cancel,
	}
}

// IsSubscribed reports whether the renderer currently has an active AVTransport
// GENA subscription.
func (g *GENAClient) IsSubscribed(rendererID string) bool {
	g.mu.RLock()
	subs := g.byRenderer[rendererID]
	g.mu.RUnlock()
	for _, sub := range subs {
		if sub.service == ServiceAVTransport {
			sub.mu.RLock()
			ok := sub.active
			sub.mu.RUnlock()
			return ok
		}
	}
	return false
}

// Close cancels all renewal loops and best-effort unsubscribes all active SIDs.
func (g *GENAClient) Close(ctx context.Context) {
	g.mu.Lock()
	if g.closed {
		g.mu.Unlock()
		return
	}
	g.closed = true
	var all []*genaSub
	for _, subs := range g.byRenderer {
		all = append(all, subs...)
	}
	g.byRenderer = make(map[string][]*genaSub)
	g.byToken = make(map[string]*genaSub)
	g.mu.Unlock()

	g.stopOnce.Do(func() { close(g.stopCh) })

	var wg sync.WaitGroup
	for _, sub := range all {
		if sub.cancelFunc != nil {
			sub.cancelFunc()
		}
		wg.Add(1)
		go func(s *genaSub) {
			defer wg.Done()
			g.unsubscribeOneCtx(ctx, s)
		}(sub)
	}
	wg.Wait()
}

func (g *GENAClient) runSub(sub *genaSub) {
	ctx := sub.ctx
	defer sub.cancelFunc()

	backoff := 5 * time.Second
	for {
		select {
		case <-ctx.Done():
			return
		case <-g.stopCh:
			return
		default:
		}

		sid, timeout, err := g.subscribe(ctx, sub)
		if err != nil {
			sub.mu.Lock()
			sub.active = false
			sub.sid = ""
			sub.mu.Unlock()

			select {
			case <-time.After(backoff):
			case <-ctx.Done():
				return
			case <-g.stopCh:
				return
			}
			if backoff < 60*time.Second {
				backoff *= 2
				if backoff > 60*time.Second {
					backoff = 60 * time.Second
				}
			}
			continue
		}

		backoff = 5 * time.Second
		sub.mu.Lock()
		sub.sid = sid
		sub.active = true
		sub.mu.Unlock()

		// Renew loop until renewal fails or context cancels.
		for {
			renewIn := timeout / 2
			if renewIn < 15*time.Second {
				renewIn = 15 * time.Second
			}
			timer := time.NewTimer(renewIn)
			select {
			case <-ctx.Done():
				timer.Stop()
				return
			case <-g.stopCh:
				timer.Stop()
				return
			case <-timer.C:
			}

			newTimeout, err := g.renew(ctx, sub, sid)
			if err != nil {
				sub.mu.Lock()
				sub.active = false
				sub.sid = ""
				sub.mu.Unlock()
				break // fall back to fresh SUBSCRIBE
			}
			timeout = newTimeout
		}
	}
}

func (g *GENAClient) subscribe(ctx context.Context, sub *genaSub) (string, time.Duration, error) {
	reqCtx, cancel := context.WithTimeout(ctx, 5*time.Second)
	defer cancel()

	req, err := http.NewRequestWithContext(reqCtx, "SUBSCRIBE", sub.eventSubURL, nil)
	if err != nil {
		return "", 0, err
	}
	callbackURL := fmt.Sprintf("%s/upnp/event/%s", sub.callbackBaseURL, sub.token)
	// Use exact uppercase UPnP GENA header names for strict embedded libupnp stacks.
	req.Header["CALLBACK"] = []string{"<" + callbackURL + ">"}
	req.Header["NT"] = []string{"upnp:event"}
	req.Header["TIMEOUT"] = []string{fmt.Sprintf("Second-%d", defaultGENATimeoutSec)}

	resp, err := g.httpClient.Do(req)
	if err != nil {
		return "", 0, err
	}
	defer func() { _ = resp.Body.Close() }()
	_, _ = io.Copy(io.Discard, io.LimitReader(resp.Body, 4096))

	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return "", 0, fmt.Errorf("GENA SUBSCRIBE %s returned HTTP %d", sub.eventSubURL, resp.StatusCode)
	}
	sid := strings.TrimSpace(resp.Header.Get("SID"))
	if sid == "" {
		return "", 0, fmt.Errorf("GENA SUBSCRIBE %s returned empty SID", sub.eventSubURL)
	}
	timeout := parseGENATimeout(resp.Header.Get("TIMEOUT"))
	return sid, timeout, nil
}

func (g *GENAClient) renew(ctx context.Context, sub *genaSub, sid string) (time.Duration, error) {
	reqCtx, cancel := context.WithTimeout(ctx, 5*time.Second)
	defer cancel()

	req, err := http.NewRequestWithContext(reqCtx, "SUBSCRIBE", sub.eventSubURL, nil)
	if err != nil {
		return 0, err
	}
	req.Header["SID"] = []string{sid}
	req.Header["TIMEOUT"] = []string{fmt.Sprintf("Second-%d", defaultGENATimeoutSec)}

	resp, err := g.httpClient.Do(req)
	if err != nil {
		return 0, err
	}
	defer func() { _ = resp.Body.Close() }()
	_, _ = io.Copy(io.Discard, io.LimitReader(resp.Body, 4096))

	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return 0, fmt.Errorf("GENA renew %s returned HTTP %d", sub.eventSubURL, resp.StatusCode)
	}
	return parseGENATimeout(resp.Header.Get("TIMEOUT")), nil
}

func (g *GENAClient) unsubscribeOneCtx(ctx context.Context, sub *genaSub) {
	sub.mu.Lock()
	sid := sub.sid
	sub.sid = ""
	sub.active = false
	sub.mu.Unlock()
	if sid == "" {
		return
	}

	reqCtx, cancel := context.WithTimeout(ctx, 2*time.Second)
	defer cancel()
	req, err := http.NewRequestWithContext(reqCtx, "UNSUBSCRIBE", sub.eventSubURL, nil)
	if err != nil {
		return
	}
	req.Header["SID"] = []string{sid}
	resp, err := g.httpClient.Do(req)
	if err != nil {
		return
	}
	defer func() { _ = resp.Body.Close() }()
	_, _ = io.Copy(io.Discard, io.LimitReader(resp.Body, 4096))
}

func (g *GENAClient) handleNotify(w http.ResponseWriter, r *http.Request) {
	if r.Method != "NOTIFY" {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	token := strings.TrimPrefix(r.URL.Path, "/upnp/event/")
	token = strings.Trim(token, "/")
	if token == "" || strings.Contains(token, "/") {
		http.Error(w, "invalid event token", http.StatusPreconditionFailed)
		return
	}

	g.mu.RLock()
	sub, ok := g.byToken[token]
	g.mu.RUnlock()
	if !ok {
		http.Error(w, "unknown subscription", http.StatusPreconditionFailed)
		return
	}

	incomingSID := strings.TrimSpace(r.Header.Get("SID"))
	sub.mu.RLock()
	expectedSID := sub.sid
	sub.mu.RUnlock()
	// Allow SEQ=0 before SUBSCRIBE response returns, otherwise require exact SID match.
	if expectedSID != "" && incomingSID != expectedSID {
		http.Error(w, "SID mismatch", http.StatusPreconditionFailed)
		return
	}

	body, err := io.ReadAll(io.LimitReader(r.Body, maxGENABodyBytes))
	if err != nil {
		http.Error(w, "read failed", http.StatusBadRequest)
		return
	}

	// Acknowledge immediately so embedded libupnp renderers never block/deadlock
	// while we process state transitions or issue follow-up SOAP calls.
	w.WriteHeader(http.StatusOK)

	ev, err := ParseNotifyBody(sub.service, bytes.NewReader(body))
	if err != nil {
		log.Printf("GENA notify parse warning (%s %s): %v", sub.rendererID, sub.service, err)
		return
	}
	ev.RendererID = sub.rendererID
	if seqStr := strings.TrimSpace(r.Header.Get("SEQ")); seqStr != "" {
		if seq, err := strconv.ParseUint(seqStr, 10, 32); err == nil {
			ev.Seq = uint32(seq)
		}
	}
	if !ev.HasAnyUpdate() {
		return
	}

	select {
	case g.eventCh <- ev:
	default:
		// Drop if queue is saturated; background poll will reconcile.
	}
}

func randomToken() string {
	var b [16]byte
	_, _ = rand.Read(b[:])
	return hex.EncodeToString(b[:])
}
