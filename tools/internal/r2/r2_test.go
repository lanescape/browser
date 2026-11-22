package r2_test

import (
	"bytes"
	"compress/gzip"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/lanescape/browser/tools/internal/r2"
	"github.com/lanescape/browser/tools/internal/r2/r2test"
)

const (
	exampleKeyID  = "AKIAIOSFODNN7EXAMPLE"
	exampleSecret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
)

var exampleTime = time.Date(2013, 5, 24, 0, 0, 0, 0, time.UTC)

func hexSHA(b []byte) string {
	s := sha256.Sum256(b)
	return hex.EncodeToString(s[:])
}

// AWS's examples, from "Signature Calculations for the r2.Authorization
// Header" in the Amazon S3 API reference.
func TestAWSGetObjectExample(t *testing.T) {
	got := r2.Authorization("GET", "examplebucket.s3.amazonaws.com", "/test.txt", nil, map[string]string{"Range": "bytes=0-9"},
		r2.EmptySHA256, exampleKeyID, exampleSecret, exampleTime, "us-east-1")
	want := "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, " +
		"SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, " +
		"Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
	if got["Authorization"] != want || got["x-amz-date"] != "20130524T000000Z" {
		t.Fatalf("got %v", got)
	}
}

func TestAWSPutObjectExample(t *testing.T) {
	// The key, test$file.text, has a character the canonical URI escapes.
	got := r2.Authorization("PUT", "examplebucket.s3.amazonaws.com", "/test$file.text", nil,
		map[string]string{"Date": "Fri, 24 May 2013 00:00:00 GMT", "x-amz-storage-class": "REDUCED_REDUNDANCY"},
		hexSHA([]byte("Welcome to Amazon S3.")), exampleKeyID, exampleSecret, exampleTime, "us-east-1")
	if !strings.HasSuffix(got["Authorization"], "Signature=98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd") {
		t.Fatal(got["Authorization"])
	}
}

func TestPutGetHeadDigest(t *testing.T) {
	f := r2test.New(t)
	c := f.Client(t)
	etag, err := c.Put("v1/a.json", []byte("{}"), "application/json", r2.Immutable, r2.PutOptions{})
	if err != nil {
		t.Fatal(err)
	}
	stored := f.Get("lanescape-browser/v1/a.json")
	if stored.ETag != etag || stored.Headers["content-type"] != "application/json" || stored.Headers["cache-control"] != r2.Immutable ||
		stored.Headers["x-amz-meta-sha256"] != hexSHA([]byte("{}")) {
		t.Fatalf("stored %+v", stored)
	}
	if data, e, found, err := c.Get("v1/a.json"); err != nil || !found || string(data) != "{}" || e != etag {
		t.Fatalf("Get = %q %q %v %v", data, e, found, err)
	}
	if h, found, err := c.Head("v1/a.json"); err != nil || !found || h.SHA256 != hexSHA([]byte("{}")) || h.Bytes != 2 {
		t.Fatalf("Head = %+v %v %v", h, found, err)
	}
	if n, sum, found, err := c.Digest("v1/a.json"); err != nil || !found || n != 2 || sum != hexSHA([]byte("{}")) {
		t.Fatalf("Digest = %d %s %v %v", n, sum, found, err)
	}
	if _, _, found, err := c.Get("v1/none"); err != nil || found {
		t.Fatalf("Get none: %v %v", found, err)
	}
	if _, found, err := c.Head("v1/none"); err != nil || found {
		t.Fatalf("Head none: %v %v", found, err)
	}
	if _, _, found, err := c.Digest("v1/none"); err != nil || found {
		t.Fatalf("Digest none: %v %v", found, err)
	}
}

func TestConditionalWrites(t *testing.T) {
	f := r2test.New(t)
	c := f.Client(t)
	first, err := c.Put("v1/latest", []byte("1\n"), "text/plain", r2.NoStore, r2.PutOptions{IfNoneMatch: true})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := c.Put("v1/latest", []byte("2\n"), "text/plain", r2.NoStore, r2.PutOptions{IfNoneMatch: true}); !errors.Is(err, r2.ErrPreconditionFailed) {
		t.Fatalf("create over an existing one: %v", err)
	}
	second, err := c.Put("v1/latest", []byte("2\n"), "text/plain", r2.NoStore, r2.PutOptions{IfMatch: first})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := c.Put("v1/latest", []byte("3\n"), "text/plain", r2.NoStore, r2.PutOptions{IfMatch: first}); !errors.Is(err, r2.ErrPreconditionFailed) {
		t.Fatalf("a stale swap: %v", err)
	}
	if data, e, _, _ := c.Get("v1/latest"); string(data) != "2\n" || e != second {
		t.Fatalf("latest %q %q", data, e)
	}
	// Never unconditional: an ETag that isn't one strong, quoted one.
	for _, etag := range []string{"unquoted", `W/"1"`, `"1", "2"`} {
		if _, err := c.Put("v1/latest", []byte("4\n"), "text/plain", r2.NoStore, r2.PutOptions{IfMatch: etag}); err == nil {
			t.Errorf("If-Match %q accepted", etag)
		}
	}
}

func TestFilesStreamAndAreImmutable(t *testing.T) {
	f := r2test.New(t)
	c := f.Client(t)
	path := filepath.Join(t.TempDir(), "package.tar.gz")
	os.WriteFile(path, []byte(strings.Repeat("package", 100000)), 0o644)
	expected, _ := r2.IdentityOf(path)
	if got, err := r2.PutImmutable(c, path, "v1/1/p.tar.gz", "application/gzip"); err != nil || got != expected {
		t.Fatalf("r2.PutImmutable = %+v %v", got, err)
	}
	// A retry of the same file finds it and accepts it.
	if got, err := r2.PutImmutable(c, path, "v1/1/p.tar.gz", "application/gzip"); err != nil || got != expected {
		t.Fatalf("retry = %+v %v", got, err)
	}
	os.WriteFile(path, []byte("other"), 0o644)
	if _, err := r2.PutImmutable(c, path, "v1/1/p.tar.gz", "application/gzip"); err == nil || !strings.Contains(err.Error(), "immutable object conflict") {
		t.Fatalf("a different file: %v", err)
	}
	if n, sum, _, _ := c.Digest("v1/1/p.tar.gz"); n != expected.Bytes || sum != expected.SHA256 {
		t.Fatalf("stored %d %s", n, sum)
	}
}

func TestRetriesAndRefusals(t *testing.T) {
	f := r2test.New(t)
	c := f.Client(t)
	f.FailNext = 2
	if _, _, found, err := c.Get("v1/none"); err != nil || found {
		t.Fatalf("after two 503s: %v %v", found, err)
	}
	f.FailNext = 3
	if _, _, _, err := c.Get("v1/none"); err == nil || !strings.Contains(err.Error(), "503 ServiceUnavailable") {
		t.Fatalf("three 503s: %v", err)
	}
	f.FailNext, f.Refuse = 0, 403
	if _, _, _, err := c.Get("v1/none"); err == nil || !strings.Contains(err.Error(), "GET v1/none with 403 AccessDenied") {
		t.Fatalf("refused GET: %v", err)
	}
	// A HEAD's refusal has no body to name it.
	if _, _, err := c.Head("v1/none"); err == nil || !strings.Contains(err.Error(), "HEAD v1/none with 403") {
		t.Fatalf("refused HEAD: %v", err)
	}
	// A wrong secret is refused, not taken for a missing object.
	f.Refuse = 0
	wrong, _ := r2.New("0123", "lanescape-browser", "id", "not-the-secret", f.Srv.URL)
	wrong.Sleep = func(time.Duration) {}
	if _, _, _, err := wrong.Get("v1/none"); err == nil || !strings.Contains(err.Error(), "SignatureDoesNotMatch") {
		t.Fatalf("wrong secret: %v", err)
	}
}

func TestADownloadDroppedPartwayIsReadAgain(t *testing.T) {
	f := r2test.New(t)
	c := f.Client(t)
	data := []byte(strings.Repeat("x", 100000))
	if _, err := c.Put("v1/p", data, "application/gzip", r2.Immutable, r2.PutOptions{}); err != nil {
		t.Fatal(err)
	}
	f.TruncateNext = 1
	if n, sum, _, err := c.Digest("v1/p"); err != nil || n != 100000 || sum != hexSHA(data) {
		t.Fatalf("Digest after a drop: %d %v", n, err)
	}
	f.TruncateNext = 1
	if got, _, _, err := c.Get("v1/p"); err != nil || string(got) != string(data) {
		t.Fatalf("Get after a drop: %v", err)
	}
	f.TruncateNext = r2.Attempts
	if _, _, _, err := c.Digest("v1/p"); err == nil {
		t.Fatal("a download that always drops succeeded")
	}
}

// An endpoint with its scheme's default port signs and sends the host alone;
// the fake checks the signature against the Host it was sent.
func TestTheHostSignedIsTheHostSent(t *testing.T) {
	f := r2test.New(t)
	port := f.Port()
	for endpoint, host := range map[string]string{
		"http://127.0.0.1:80": "127.0.0.1", "http://127.0.0.1:" + port: "127.0.0.1:" + port,
		"http://[::1]:80": "[::1]", "http://[::1]": "[::1]", "http://[::1]:" + port: "[::1]:" + port,
	} {
		c, err := r2.New("0123", "lanescape-browser", "id", "secret", endpoint)
		if err != nil {
			t.Fatal(err)
		}
		c.Sleep = func(time.Duration) {}
		c.Dial = r2test.DialTo(port)
		f.Mu.Lock()
		f.Hosts = nil
		f.Mu.Unlock()
		if _, err := c.Put("v1/h", []byte("h"), "text/plain", r2.NoStore, r2.PutOptions{}); err != nil {
			t.Fatalf("%s: %v", endpoint, err)
		}
		if got, _, _, err := c.Get("v1/h"); err != nil || string(got) != "h" {
			t.Fatalf("%s: %v", endpoint, err)
		}
		if len(f.Hosts) != 2 || f.Hosts[0] != host || f.Hosts[1] != host {
			t.Fatalf("%s: hosts %v, want %s", endpoint, f.Hosts, host)
		}
	}
}

// A 503 whose body is cut short is still retried, and a 412's still means
// the condition failed.
func TestCutShortErrorBodies(t *testing.T) {
	f := r2test.New(t)
	c := f.Client(t)
	f.FailNext, f.TruncateErrors = 1, 1
	if _, _, found, err := c.Get("v1/none"); err != nil || found {
		t.Fatalf("after a cut-short 503: %v %v", found, err)
	}
	if _, err := c.Put("v1/l", []byte("1"), "text/plain", r2.NoStore, r2.PutOptions{}); err != nil {
		t.Fatal(err)
	}
	f.TruncateErrors = 1
	if _, err := c.Put("v1/l", []byte("2"), "text/plain", r2.NoStore, r2.PutOptions{IfNoneMatch: true}); !errors.Is(err, r2.ErrPreconditionFailed) {
		t.Fatalf("a cut-short 412: %v", err)
	}
}

func TestMalformedCredentialsAreRefusedWithoutTheirValues(t *testing.T) {
	for _, tc := range [][2]string{{"id\nX-Injected: 1", "secret"}, {"id", "sec ret"}, {"", "secret"}, {"id", ""}} {
		_, err := r2.New("0123", "b", tc[0], tc[1], "")
		if err == nil || strings.Contains(err.Error(), "Injected") || strings.Contains(err.Error(), "sec ret") {
			t.Errorf("New(%q, %q) = %v", tc[0], tc[1], err)
		}
	}
}

func TestKeysAndConfiguration(t *testing.T) {
	f := r2test.New(t)
	c := f.Client(t)
	for _, key := range []string{"", "/v1", "v1//x", "v1/../x", "v1/./x"} {
		if _, _, _, err := c.Get(key); err == nil {
			t.Errorf("key %q accepted", key)
		}
	}
	env := func(m map[string]string) func(string) string { return func(k string) string { return m[k] } }
	if _, err := r2.FromEnvironment("../../..", env(map[string]string{"R2_ACCESS_KEY_ID": "id"})); err == nil || !strings.Contains(err.Error(), "R2_SECRET_ACCESS_KEY") {
		t.Fatalf("without the secret: %v", err)
	}
	r, err := r2.FromEnvironment("../../..", env(map[string]string{"R2_ACCESS_KEY_ID": "id", "R2_SECRET_ACCESS_KEY": "secret"}))
	if err != nil {
		t.Fatal(err)
	}
	if r.Host != "6d39a82297745b856f8c09e7e0b91b3a.r2.cloudflarestorage.com" || r.Bucket != "lanescape-browser" {
		t.Fatalf("%+v", r)
	}
	for _, endpoint := range []string{"ftp://x", "https://", "https://x/path", "https://x?q=1", "https://u:p@x", "https://bücher.example"} {
		if _, err := r2.New("0123", "b", "id", "secret", endpoint); err == nil {
			t.Errorf("endpoint %q accepted", endpoint)
		}
	}
}

// An empty object is sent with its length, as signed, not chunked.
func TestEmptyObjectsSendTheirLength(t *testing.T) {
	f := r2test.New(t)
	c := f.Client(t)
	if _, err := c.Put("v1/empty", nil, "text/plain", r2.NoStore, r2.PutOptions{}); err != nil {
		t.Fatal(err)
	}
	empty := filepath.Join(t.TempDir(), "empty")
	os.WriteFile(empty, nil, 0o644)
	if _, err := c.PutFile("v1/empty-file", empty, "text/plain", r2.Immutable, r2.PutOptions{}); err != nil {
		t.Fatal(err)
	}
	for _, key := range []string{"v1/empty", "v1/empty-file"} {
		if o := f.Get("lanescape-browser/" + key); o == nil || len(o.Body) != 0 {
			t.Fatalf("%s: %+v", key, o)
		}
	}
}

// A redirect is an error: a signed request is never sent again elsewhere,
// and a PUT redirected to something that answers 200 isn't a write.
func TestRedirectsAreNeverFollowed(t *testing.T) {
	var elsewhere atomic.Int32
	other := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		elsewhere.Add(1)
		w.Header().Set("ETag", `"x"`)
	}))
	defer other.Close()
	for _, status := range []int{301, 302, 303, 307, 308} {
		redirect := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			http.Redirect(w, r, other.URL+r.URL.Path, status)
		}))
		c, err := r2.New("0123", "lanescape-browser", "id", "secret", redirect.URL)
		if err != nil {
			t.Fatal(err)
		}
		c.Sleep = func(time.Duration) {}
		if _, err := c.Put("v1/x", []byte("x"), "text/plain", r2.NoStore, r2.PutOptions{}); err == nil {
			t.Errorf("%d: a redirected PUT succeeded", status)
		}
		if _, _, _, err := c.Get("v1/x"); err == nil {
			t.Errorf("%d: a redirected GET succeeded", status)
		}
		redirect.Close()
	}
	if n := elsewhere.Load(); n != 0 {
		t.Fatalf("%d requests followed a redirect", n)
	}
}

// One client is safe to share from its first request on.
func TestAClientIsSharedFromItsFirstRequest(t *testing.T) {
	f := r2test.New(t)
	c := f.Client(t)
	var wg sync.WaitGroup
	for range 8 {
		wg.Add(1)
		go func() {
			defer wg.Done()
			if _, _, _, err := c.Get("v1/none"); err != nil {
				t.Error(err)
			}
		}()
	}
	wg.Wait()
}

// An object stored compressed is read as stored, not decompressed.
func TestObjectsAreReadAsStored(t *testing.T) {
	f := r2test.New(t)
	c := f.Client(t)
	var gz bytes.Buffer
	w := gzip.NewWriter(&gz)
	w.Write([]byte("plain"))
	w.Close()
	f.Put("lanescape-browser/v1/gz", &r2test.Object{Body: gz.Bytes(), ETag: `"gz"`, Headers: map[string]string{"Content-Encoding": "gzip"}})
	got, _, found, err := c.Get("v1/gz")
	if err != nil || !found || !bytes.Equal(got, gz.Bytes()) {
		t.Fatalf("read %q, %v", got, err)
	}
}
