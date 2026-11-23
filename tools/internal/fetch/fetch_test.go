package fetch

import (
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"
)

// What a failed request says never holds the URL it was redirected to.
func TestFailuresNeverNameTheirURL(t *testing.T) {
	const secret = "X-Amz-Signature=deadbeefsecret"
	stall := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		time.Sleep(200 * time.Millisecond)
	}))
	defer stall.Close()
	closed := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {}))
	closed.Close()
	for name, location := range map[string]string{
		"timed out":         stall.URL + "/asset?" + secret,
		"connection failed": closed.URL + "/asset?" + secret,
		"request failed":    "http://%zz/asset?" + secret, // a malformed Location
	} {
		redirect := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			w.Header().Set("Location", location)
			w.WriteHeader(http.StatusFound)
		}))
		client := &http.Client{Timeout: 100 * time.Millisecond}
		_, err := client.Get(redirect.URL + "/download")
		redirect.Close()
		if err == nil {
			t.Fatalf("%s: no error", name)
		}
		if !strings.Contains(err.Error(), "deadbeef") {
			t.Fatalf("%s: Go's own error no longer holds the URL, so this test proves nothing: %v", name, err)
		}
		got := Error("downloading manifest.json", err).Error()
		if strings.Contains(got, "deadbeef") || strings.Contains(got, "http") {
			t.Fatalf("%s: %q", name, got)
		}
		if got != "downloading manifest.json: "+name {
			t.Errorf("got %q, want %q", got, name)
		}
	}
}

// A redirect after a signed URL doesn't tell the next host that URL.
func TestRedirectsSendNoReferer(t *testing.T) {
	var referer string
	last := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		referer = r.Header.Get("Referer")
	}))
	defer last.Close()
	signed := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		http.Redirect(w, r, last.URL+"/next", http.StatusFound)
	}))
	defer signed.Close()
	first := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		http.Redirect(w, r, signed.URL+"/asset?X-Amz-Signature=deadbeef", http.StatusFound)
	}))
	defer first.Close()
	for _, check := range []func(*http.Request, []*http.Request) error{nil, NoReferer} {
		referer = ""
		resp, err := (&http.Client{CheckRedirect: check}).Get(first.URL + "/download")
		if err != nil {
			t.Fatal(err)
		}
		resp.Body.Close()
		if check == nil && !strings.Contains(referer, "deadbeef") {
			t.Fatalf("Go no longer sends the Referer, so this test proves nothing: %q", referer)
		}
		if check != nil && referer != "" {
			t.Fatalf("Referer %q", referer)
		}
	}
}
