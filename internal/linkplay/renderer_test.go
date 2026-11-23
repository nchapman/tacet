package linkplay

import (
	"context"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"

	"tacet/internal/upnp"
)

func TestBuildQueueContextXML(t *testing.T) {
	xmlStr := buildQueueContextXML("tacet", []upnp.MediaItem{
		{
			ID:          "local:1",
			URL:         "http://192.168.1.10:4320/media/local/local:1.flac",
			Title:       "So What",
			Artist:      "Miles Davis",
			Album:       "Kind of Blue",
			AlbumArtURL: "http://192.168.1.10:4320/media/art/a1",
			MIME:        "audio/flac",
			Duration:    562 * time.Second,
		},
		{
			ID:       "qobuz:2",
			URL:      "http://192.168.1.10:4320/media/qobuz/2.flac?handoff=1",
			Title:    "Freddie Freeloader",
			Artist:   "Miles Davis",
			Album:    "Kind of Blue",
			MIME:     "audio/flac",
			Duration: 586 * time.Second,
		},
	}, nil)
	for _, want := range []string{
		"<ListName>tacet</ListName>",
		"<TrackNumber>2</TrackNumber>",
		"<Track1>",
		"<Track2>",
		"http://192.168.1.10:4320/media/local/local:1.flac",
		"http://192.168.1.10:4320/media/qobuz/2.flac?handoff=1",
		"So What",
		"Freddie Freeloader",
	} {
		if !strings.Contains(xmlStr, want) {
			t.Fatalf("expected %q in queue context XML:\n%s", want, xmlStr)
		}
	}
}

func TestParseGetQueueIndex(t *testing.T) {
	resp := `<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:GetQueueIndexResponse xmlns:u="urn:schemas-wiimu-com:service:PlayQueue:1"><CurrentIndex>3</CurrentIndex><CurrentPage>1</CurrentPage></u:GetQueueIndexResponse></s:Body></s:Envelope>`
	idx, ok := parseGetQueueIndex(resp)
	if !ok || idx != 2 {
		t.Fatalf("got (%d, %v), want (2, true)", idx, ok)
	}
}

func TestLinkplaySOAPActions(t *testing.T) {
	var (
		mu      sync.Mutex
		actions []string
		bodies  []string
	)
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, _ := io.ReadAll(r.Body)
		mu.Lock()
		actions = append(actions, r.Header.Get("SOAPACTION"))
		bodies = append(bodies, string(body))
		mu.Unlock()
		w.Header().Set("Content-Type", "text/xml")
		_, _ = w.Write([]byte(`<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:OKResponse xmlns:u="urn:schemas-wiimu-com:service:PlayQueue:1"/></s:Body></s:Envelope>`))
	}))
	defer srv.Close()

	lp := NewRenderer(nil, srv.URL)
	ctx := context.Background()

	if err := lp.ReplaceQueue(ctx, []upnp.MediaItem{
		{ID: "1", URL: "http://example/1.flac", Title: "One", MIME: "audio/flac"},
		{ID: "2", URL: "http://example/2.flac", Title: "Two", MIME: "audio/flac"},
	}, 1); err != nil {
		t.Fatalf("ReplaceQueue: %v", err)
	}
	if err := lp.AppendQueue(ctx, []upnp.MediaItem{{ID: "3", URL: "http://example/3.flac", Title: "Three", MIME: "audio/flac"}}); err != nil {
		t.Fatalf("AppendQueue: %v", err)
	}
	if err := lp.RemoveQueueRange(ctx, 1, 1); err != nil {
		t.Fatalf("RemoveQueueRange: %v", err)
	}
	if err := lp.MoveQueueItem(ctx, 2, 0); err != nil {
		t.Fatalf("MoveQueueItem: %v", err)
	}
	if err := lp.PlayIndex(ctx, 0); err != nil {
		t.Fatalf("PlayIndex: %v", err)
	}
	if err := lp.SetLoopMode(ctx, "all", false); err != nil {
		t.Fatalf("SetLoopMode: %v", err)
	}

	mu.Lock()
	defer mu.Unlock()
	wantActions := []string{
		`"urn:schemas-wiimu-com:service:PlayQueue:1#CreateQueue"`,
		`"urn:schemas-wiimu-com:service:PlayQueue:1#PlayQueueWithIndex"`,
		`"urn:schemas-wiimu-com:service:PlayQueue:1#AppendTracksInQueue"`,
		`"urn:schemas-wiimu-com:service:PlayQueue:1#RemoveTracksInQueue"`,
		`"urn:schemas-wiimu-com:service:PlayQueue:1#MoveTracksInQueue"`,
		`"urn:schemas-wiimu-com:service:PlayQueue:1#PlayQueueWithIndex"`,
		`"urn:schemas-wiimu-com:service:PlayQueue:1#SetQueueLoopMode"`,
	}
	if len(actions) != len(wantActions) {
		t.Fatalf("got %d actions %v, want %d", len(actions), actions, len(wantActions))
	}
	for i, want := range wantActions {
		if actions[i] != want {
			t.Fatalf("action[%d] = %q, want %q", i, actions[i], want)
		}
	}
	// Verify 1-based conversion on PlayQueueWithIndex(startIndex=1 -> Index=2) and Move(2->0 => 3->1).
	if !strings.Contains(bodies[1], "<Index>2</Index>") {
		t.Fatalf("expected 1-based Index=2 in %s", bodies[1])
	}
	if !strings.Contains(bodies[4], "<IndexList>3</IndexList><ToIndex>1</ToIndex>") {
		t.Fatalf("expected 1-based move 3->1 in %s", bodies[4])
	}
	if !strings.Contains(bodies[6], "<LoopMode>0</LoopMode>") {
		t.Fatalf("expected LoopMode=0 for repeat=all in %s", bodies[6])
	}
}

func TestLinkplayReplaceQueue_BatchesOver50Tracks(t *testing.T) {
	var (
		mu      sync.Mutex
		actions []string
	)
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		mu.Lock()
		actions = append(actions, r.Header.Get("SOAPACTION"))
		mu.Unlock()
		w.Header().Set("Content-Type", "text/xml")
		_, _ = w.Write([]byte(`<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:OKResponse xmlns:u="urn:schemas-wiimu-com:service:PlayQueue:1"/></s:Body></s:Envelope>`))
	}))
	defer srv.Close()

	lp := NewRenderer(nil, srv.URL)
	items := make([]upnp.MediaItem, 55)
	for i := range items {
		items[i] = upnp.MediaItem{
			ID:    "t",
			URL:   "http://example/t.flac",
			Title: "Track",
			MIME:  "audio/flac",
		}
	}
	if err := lp.ReplaceQueue(context.Background(), items, 2); err != nil {
		t.Fatalf("ReplaceQueue(55): %v", err)
	}

	mu.Lock()
	defer mu.Unlock()
	want := []string{
		`"urn:schemas-wiimu-com:service:PlayQueue:1#CreateQueue"`,
		`"urn:schemas-wiimu-com:service:PlayQueue:1#AppendTracksInQueue"`,
		`"urn:schemas-wiimu-com:service:PlayQueue:1#PlayQueueWithIndex"`,
	}
	if len(actions) != len(want) {
		t.Fatalf("got %d actions %v, want %d", len(actions), actions, len(want))
	}
	for i := range want {
		if actions[i] != want[i] {
			t.Fatalf("action[%d] = %q, want %q", i, actions[i], want[i])
		}
	}
}

func TestLinkplayGetQueueIndex(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "text/xml")
		_, _ = w.Write([]byte(`<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:GetQueueIndexResponse xmlns:u="urn:schemas-wiimu-com:service:PlayQueue:1"><CurrentIndex>4</CurrentIndex></u:GetQueueIndexResponse></s:Body></s:Envelope>`))
	}))
	defer srv.Close()

	lp := NewRenderer(nil, srv.URL)
	idx, ok := lp.getQueueIndex(context.Background())
	if !ok || idx != 3 {
		t.Fatalf("getQueueIndex = (%d, %v), want (3, true)", idx, ok)
	}
}
