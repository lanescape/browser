// Package r2test is a fake of what the tools use of R2's S3 API, for tests.
package r2test

import (
	"crypto/md5"
	"crypto/sha256"
	"encoding/hex"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"net/url"
	"regexp"
	"strconv"
	"strings"
	"sync"
	"testing"

	"github.com/lanescape/browser/tools/internal/r2"
	"time"
)

// fake is what the client uses of R2's S3 API, path-style, in memory: GET,
// HEAD and PUT, each checked against its signature and its payload's
// SHA-256, and If-Match and If-None-Match evaluated atomically.
type Fake struct {
	bucket, keyID, secret string

	Mu             sync.Mutex
	objects        map[string]*Object
	Refuse         int
	FailNext       int
	TruncateNext   int
	TruncateErrors int
	Hosts          []string
	puts           []string
	Srv            *httptest.Server
}

type Object struct {
	Body    []byte
	ETag    string
	Headers map[string]string
}

func New(t *testing.T) *Fake {
	f := &Fake{bucket: "lanescape-browser", keyID: "id", secret: "secret", objects: map[string]*Object{}}
	f.Srv = httptest.NewServer(http.HandlerFunc(f.serve))
	t.Cleanup(f.Srv.Close)
	return f
}

func (f *Fake) Client(t *testing.T) *r2.Client {
	t.Helper()
	c, err := r2.New("0123", f.bucket, f.keyID, f.secret, f.Srv.URL)
	if err != nil {
		t.Fatal(err)
	}
	c.Sleep = func(time.Duration) {}
	return c
}

func (f *Fake) Port() string {
	u, _ := url.Parse(f.Srv.URL)
	return u.Port()
}

func (f *Fake) Put(key string, o *Object) {
	f.Mu.Lock()
	defer f.Mu.Unlock()
	f.objects[key] = o
}

func (f *Fake) Get(key string) *Object {
	f.Mu.Lock()
	defer f.Mu.Unlock()
	return f.objects[key]
}

func (f *Fake) answer(w http.ResponseWriter, r *http.Request, status int, headers map[string]string, body []byte) {
	for k, v := range headers {
		w.Header().Set(k, v)
	}
	w.Header().Set("Content-Length", strconv.Itoa(len(body)))
	f.Mu.Lock()
	cut := f.TruncateNext > 0 && status == 200 && len(body) > 0 && r.Method != http.MethodHead
	if cut {
		f.TruncateNext--
	}
	f.Mu.Unlock()
	w.WriteHeader(status)
	if r.Method == http.MethodHead {
		return
	}
	if cut {
		// The connection drops partway through the body.
		w.Write(body[:len(body)/2])
		w.(http.Flusher).Flush()
		panic(http.ErrAbortHandler)
	}
	w.Write(body)
}

func (f *Fake) error(w http.ResponseWriter, r *http.Request, status int, code string) {
	body := []byte("<Error><Code>" + code + "</Code></Error>")
	f.Mu.Lock()
	cut := f.TruncateErrors > 0 && r.Method != http.MethodHead
	if cut {
		f.TruncateErrors--
	}
	f.Mu.Unlock()
	if cut {
		// The error's body is cut short.
		w.Header().Set("Content-Length", strconv.Itoa(len(body)))
		w.WriteHeader(status)
		w.Write(body[:5])
		w.(http.Flusher).Flush()
		panic(http.ErrAbortHandler)
	}
	f.answer(w, r, status, map[string]string{"Content-Type": "application/xml"}, body)
}

func (f *Fake) serve(w http.ResponseWriter, r *http.Request) {
	body, _ := io.ReadAll(r.Body)
	f.Mu.Lock()
	f.Hosts = append(f.Hosts, r.Host)
	fail, refuse := f.FailNext > 0, f.Refuse
	if fail {
		f.FailNext--
	}
	f.Mu.Unlock()
	if fail {
		f.error(w, r, 503, "ServiceUnavailable")
		return
	}
	if refuse != 0 {
		f.error(w, r, refuse, "AccessDenied")
		return
	}
	if !f.verify(r, body) {
		f.error(w, r, 403, "SignatureDoesNotMatch")
		return
	}
	key := strings.TrimPrefix(r.URL.Path, "/")
	f.Mu.Lock()
	found := f.objects[key]
	if r.Method == http.MethodGet || r.Method == http.MethodHead {
		f.Mu.Unlock()
		if found == nil {
			f.error(w, r, 404, "NoSuchKey")
			return
		}
		h := map[string]string{"ETag": found.ETag}
		for k, v := range found.Headers {
			h[k] = v
		}
		f.answer(w, r, 200, h, found.Body)
		return
	}
	match, none := r.Header.Get("If-Match"), r.Header.Get("If-None-Match")
	if (match != "" && (found == nil || (match != "*" && match != found.ETag))) ||
		(none != "" && found != nil && (none == "*" || none == found.ETag)) {
		f.Mu.Unlock()
		f.error(w, r, 412, "PreconditionFailed")
		return
	}
	sum := md5.Sum(body)
	etag := `"` + hex.EncodeToString(sum[:]) + `"`
	kept := map[string]string{}
	for k := range r.Header {
		lk := strings.ToLower(k)
		if lk == "content-type" || lk == "cache-control" || strings.HasPrefix(lk, "x-amz-meta-") {
			kept[lk] = r.Header.Get(k)
		}
	}
	f.objects[key] = &Object{Body: body, ETag: etag, Headers: kept}
	f.puts = append(f.puts, key)
	f.Mu.Unlock()
	f.answer(w, r, 200, map[string]string{"ETag": etag}, nil)
}

var authForm = regexp.MustCompile(`^AWS4-HMAC-SHA256 Credential=([^/]+)/(\d{8})/auto/s3/aws4_request, SignedHeaders=([a-z0-9;-]+), Signature=[0-9a-f]{64}$`)

// verify recomputes the signature from what was sent, and checks the
// payload's SHA-256.
func (f *Fake) verify(r *http.Request, body []byte) bool {
	auth := r.Header.Get("Authorization")
	m := authForm.FindStringSubmatch(auth)
	if m == nil || m[1] != f.keyID {
		return false
	}
	payload := r.Header.Get("x-amz-content-sha256")
	if r.Method == http.MethodPut {
		sum := sha256.Sum256(body)
		if payload != hex.EncodeToString(sum[:]) {
			return false
		}
		// R2 needs a PUT's length, sent as signed: never chunked.
		if !strings.Contains(";"+m[3]+";", ";content-length;") || len(r.TransferEncoding) != 0 ||
			r.Header.Get("Content-Length") != strconv.Itoa(len(body)) {
			return false
		}
	}
	headers := map[string]string{}
	for _, n := range strings.Split(m[3], ";") {
		switch n {
		case "host", "x-amz-date", "x-amz-content-sha256":
			continue
		case "content-length":
			headers[n] = strconv.FormatInt(r.ContentLength, 10)
			continue
		}
		v := r.Header.Get(n)
		if v == "" {
			return false
		}
		headers[n] = v
	}
	when, err := time.Parse("20060102T150405Z", r.Header.Get("x-amz-date"))
	if err != nil {
		return false
	}
	path, err := url.PathUnescape(r.URL.EscapedPath())
	if err != nil {
		return false
	}
	want := r2.Authorization(r.Method, r.Host, path, nil, headers, payload, f.keyID, f.secret, when, r2.Region)
	return want["Authorization"] == auth
}

// dialTo sends every connection to the fake, whatever address it's for.
func DialTo(port string) func(string, string) (net.Conn, error) {
	return func(network, _ string) (net.Conn, error) {
		return net.Dial(network, net.JoinHostPort("127.0.0.1", port))
	}
}

// PutOrder is the keys written through the API, in order.
func (f *Fake) PutOrder() []string {
	f.Mu.Lock()
	defer f.Mu.Unlock()
	return append([]string(nil), f.puts...)
}
