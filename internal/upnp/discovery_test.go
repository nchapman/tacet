package upnp

import (
	"context"
	"net/http"
	"net/http/httptest"
	"testing"
)

func TestSOAPHTTPClient_BlocksRedirects(t *testing.T) {
	ts := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		http.Redirect(w, r, "http://127.0.0.1:1/secret", http.StatusFound)
	}))
	defer ts.Close()

	client := newSOAPHTTPClient()
	req, err := http.NewRequestWithContext(context.Background(), http.MethodGet, ts.URL, nil)
	if err != nil {
		t.Fatal(err)
	}
	resp, err := client.Do(req)
	if resp != nil {
		_ = resp.Body.Close()
	}
	if err == nil {
		t.Fatal("expected HTTP redirect to be refused by UPnP client")
	}
}
