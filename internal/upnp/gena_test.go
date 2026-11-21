package upnp

import (
	"context"
	"fmt"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/huin/goupnp/soap"
)

func TestParseNotifyBody_AVTransportLastChange(t *testing.T) {
	payload := `<?xml version="1.0" encoding="utf-8"?>
<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0">
  <e:property>
    <LastChange>&lt;Event xmlns="urn:schemas-upnp-org:metadata-1-0/AVT/"&gt;
      &lt;InstanceID val="0"&gt;
        &lt;TransportState val="PLAYING"/&gt;
        &lt;AVTransportURI val="http://192.168.1.10:8080/media/local/t1.flac"/&gt;
        &lt;CurrentTrackURI val="http://192.168.1.10:8080/media/local/t2.flac"/&gt;
        &lt;CurrentTrackDuration val="0:04:15"/&gt;
      &lt;/InstanceID&gt;
    &lt;/Event&gt;</LastChange>
  </e:property>
</e:propertyset>`

	ev, err := ParseNotifyBody(ServiceAVTransport, strings.NewReader(payload))
	if err != nil {
		t.Fatalf("ParseNotifyBody error: %v", err)
	}
	if !ev.HasState || ev.State != "PLAYING" {
		t.Fatalf("expected State=PLAYING, got %+v", ev)
	}
	if !ev.HasTrackURI || ev.TrackURI != "http://192.168.1.10:8080/media/local/t2.flac" {
		t.Fatalf("expected CurrentTrackURI preferred over AVTransportURI, got %+v", ev)
	}
	if !ev.HasDuration || ev.Duration != 4*time.Minute+15*time.Second {
		t.Fatalf("expected Duration=4m15s, got %+v", ev)
	}
}

func TestParseNotifyBody_RenderingControlLastChange(t *testing.T) {
	payload := `<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0">
  <e:property>
    <LastChange>&lt;Event xmlns="urn:schemas-upnp-org:metadata-1-0/RCS/"&gt;
      &lt;InstanceID val="0"&gt;
        &lt;Volume channel="LF" val="99"/&gt;
        &lt;Volume channel="Master" val="42"/&gt;
        &lt;Volume channel="RF" val="88"/&gt;
        &lt;Mute channel="Master" val="1"/&gt;
        &lt;Mute channel="LF" val="0"/&gt;
      &lt;/InstanceID&gt;
    &lt;/Event&gt;</LastChange>
  </e:property>
</e:propertyset>`

	ev, err := ParseNotifyBody(ServiceRenderingControl, strings.NewReader(payload))
	if err != nil {
		t.Fatalf("ParseNotifyBody error: %v", err)
	}
	if !ev.HasVolume || ev.Volume != 42 {
		t.Fatalf("expected Master Volume=42, got %+v", ev)
	}
	if !ev.HasMute || !ev.Muted {
		t.Fatalf("expected Master Muted=true, got %+v", ev)
	}
}

func TestIsActionUnsupported(t *testing.T) {
	fault401 := &soap.SOAPFaultError{}
	fault401.Detail.UPnPError.Errorcode = 401
	if !IsActionUnsupported(fault401) {
		t.Fatal("expected UPnP 401 to be unsupported")
	}

	fault602 := &soap.SOAPFaultError{}
	fault602.Detail.UPnPError.Errorcode = 602
	if !IsActionUnsupported(fault602) {
		t.Fatal("expected UPnP 602 to be unsupported")
	}

	fault501 := &soap.SOAPFaultError{}
	fault501.Detail.UPnPError.Errorcode = 501
	if IsActionUnsupported(fault501) {
		t.Fatal("expected UPnP 501 (transient action failed) NOT to be permanently unsupported")
	}

	if !IsActionUnsupported(fmt.Errorf("wrapped: %w", ErrActionUnsupported)) {
		t.Fatal("expected wrapped ErrActionUnsupported to be true")
	}
}

func TestGENAClient_SubscribeNotifyAndUnsubscribe(t *testing.T) {
	received := make(chan DeviceEvent, 4)
	gena := NewGENAClient(func(ev DeviceEvent) {
		received <- ev
	})

	// Callback HTTP server receiving NOTIFY requests from the fake renderer.
	cbSrv := httptest.NewServer(gena.Handler())
	defer cbSrv.Close()

	var mu sync.Mutex
	var gotCallback string
	var gotNT string
	var unsubscribed bool
	subDone := make(chan struct{}, 1)
	unsubDone := make(chan struct{}, 1)

	// Fake UPnP renderer event endpoint.
	devSrv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.Method {
		case "SUBSCRIBE":
			mu.Lock()
			gotCallback = r.Header.Get("CALLBACK")
			gotNT = r.Header.Get("NT")
			mu.Unlock()
			w.Header().Set("SID", "uuid:test-sid-123")
			w.Header().Set("TIMEOUT", "Second-120")
			w.WriteHeader(http.StatusOK)
			select {
			case subDone <- struct{}{}:
			default:
			}
		case "UNSUBSCRIBE":
			if r.Header.Get("SID") == "uuid:test-sid-123" {
				mu.Lock()
				unsubscribed = true
				mu.Unlock()
				select {
				case unsubDone <- struct{}{}:
				default:
				}
			}
			w.WriteHeader(http.StatusOK)
		default:
			http.Error(w, "unexpected method", http.StatusMethodNotAllowed)
		}
	}))
	defer devSrv.Close()

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	gena.WatchRenderer(ctx, "uuid:wiim-1", cbSrv.URL, devSrv.URL+"/event/av", "")

	select {
	case <-subDone:
	case <-time.After(2 * time.Second):
		t.Fatal("timed out waiting for SUBSCRIBE")
	}

	// Wait briefly for runSub to mark active=true after receiving SUBSCRIBE response.
	deadline := time.Now().Add(time.Second)
	for !gena.IsSubscribed("uuid:wiim-1") && time.Now().Before(deadline) {
		time.Sleep(10 * time.Millisecond)
	}
	if !gena.IsSubscribed("uuid:wiim-1") {
		t.Fatal("expected IsSubscribed=true after SUBSCRIBE")
	}

	mu.Lock()
	cbURL := strings.Trim(gotCallback, "<>")
	nt := gotNT
	mu.Unlock()
	if nt != "upnp:event" {
		t.Fatalf("expected NT=upnp:event, got %q", nt)
	}
	if !strings.HasPrefix(cbURL, cbSrv.URL+"/upnp/event/") {
		t.Fatalf("unexpected callback URL %q", cbURL)
	}

	// Wrong SID should be rejected with 412 Precondition Failed.
	badReq, _ := http.NewRequest("NOTIFY", cbURL, strings.NewReader(`<e:propertyset/>`))
	badReq.Header.Set("SID", "uuid:wrong-sid")
	badResp, err := http.DefaultClient.Do(badReq)
	if err != nil {
		t.Fatal(err)
	}
	badResp.Body.Close()
	if badResp.StatusCode != http.StatusPreconditionFailed {
		t.Fatalf("expected 412 for mismatched SID, got %d", badResp.StatusCode)
	}

	// Valid NOTIFY should return 200 OK and dispatch DeviceEvent.
	body := `<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0"><e:property><LastChange>&lt;Event&gt;&lt;InstanceID val="0"&gt;&lt;TransportState val="PAUSED_PLAYBACK"/&gt;&lt;/InstanceID&gt;&lt;/Event&gt;</LastChange></e:property></e:propertyset>`
	notifyReq, _ := http.NewRequest("NOTIFY", cbURL, strings.NewReader(body))
	notifyReq.Header.Set("SID", "uuid:test-sid-123")
	notifyReq.Header.Set("SEQ", "1")
	notifyResp, err := http.DefaultClient.Do(notifyReq)
	if err != nil {
		t.Fatal(err)
	}
	notifyResp.Body.Close()
	if notifyResp.StatusCode != http.StatusOK {
		t.Fatalf("expected 200 OK for valid NOTIFY, got %d", notifyResp.StatusCode)
	}

	select {
	case ev := <-received:
		if ev.RendererID != "uuid:wiim-1" || ev.State != "PAUSED_PLAYBACK" || ev.Seq != 1 {
			t.Fatalf("unexpected DeviceEvent: %+v", ev)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("timed out waiting for DeviceEvent")
	}

	// Close should send UNSUBSCRIBE with the active SID.
	gena.Close(context.Background())
	select {
	case <-unsubDone:
	case <-time.After(2 * time.Second):
		t.Fatal("timed out waiting for UNSUBSCRIBE on Close")
	}
	mu.Lock()
	wasUnsub := unsubscribed
	mu.Unlock()
	if !wasUnsub {
		t.Fatal("expected UNSUBSCRIBE to be sent")
	}
}
