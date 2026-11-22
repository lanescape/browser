// Package r2 is Cloudflare R2, through its S3 API, with the standard library
// alone (specs/0004-releases.md, Publishing).
//
// Requests are signed with AWS Signature Version 4 in an Authorization
// header, with R2's region, auto, and the payload's SHA-256, which R2
// checks. Writes can be conditional: If-None-Match: * creates an object only
// if there is none, and If-Match: <ETag> replaces one only if it is still
// what was read; R2 evaluates both atomically and answers 412 when one
// fails. Credentials and signatures are never printed.
package r2

import (
	"crypto/hmac"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/lanescape/browser/tools/internal/fetch"
)

const (
	// Region is R2's only region for SigV4.
	Region = "auto"
	// EmptySHA256 is the payload hash of a request without a body.
	EmptySHA256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
	// Immutable is the caching of packages and manifests, which never change.
	Immutable = "public, max-age=31536000, immutable"
	// NoStore is vN/latest's, which does change.
	NoStore = "no-store"
	// Attempts is how many times a request is tried.
	Attempts = 3
)

// Error is an answer R2 shouldn't have given: a refusal, a server error, or
// one the caller doesn't expect.
type Error struct {
	Method, Key string
	Status      int
	Code        string
}

func (e *Error) Error() string {
	return fmt.Sprintf("R2 answered %s %s with %d %s", e.Method, e.Key, e.Status, e.Code)
}

// ErrPreconditionFailed is a conditional write whose condition didn't hold:
// the object exists, or isn't what was read.
var ErrPreconditionFailed = errors.New("R2: precondition failed")

// Identity is a file's size and SHA-256.
type Identity struct {
	Bytes  int64  `json:"bytes"`
	SHA256 string `json:"sha256"`
}

// IdentityOf reads a file's identity.
func IdentityOf(path string) (Identity, error) {
	f, err := os.Open(path)
	if err != nil {
		return Identity{}, err
	}
	defer f.Close()
	h := sha256.New()
	n, err := io.Copy(h, f)
	if err != nil {
		return Identity{}, err
	}
	return Identity{Bytes: n, SHA256: hex.EncodeToString(h.Sum(nil))}, nil
}

func hmacSHA256(key []byte, text string) []byte {
	m := hmac.New(sha256.New, key)
	m.Write([]byte(text))
	return m.Sum(nil)
}

// uriEncode is SigV4's UriEncode: every byte but A-Z a-z 0-9 - . _ ~ as %XX,
// in capitals, and '/' kept unless encodeSlash.
func uriEncode(s string, encodeSlash bool) string {
	var b strings.Builder
	for i := 0; i < len(s); i++ {
		c := s[i]
		switch {
		case 'A' <= c && c <= 'Z', 'a' <= c && c <= 'z', '0' <= c && c <= '9', c == '-', c == '.', c == '_', c == '~':
			b.WriteByte(c)
		case c == '/' && !encodeSlash:
			b.WriteByte(c)
		default:
			fmt.Fprintf(&b, "%%%02X", c)
		}
	}
	return b.String()
}

// Authorization is the headers that sign a request: x-amz-date,
// x-amz-content-sha256 and Authorization, over host and every header given.
// path is the object's path, unescaped.
func Authorization(method, host, path string, query, headers map[string]string, payloadSHA256, keyID, secret string, when time.Time, region string) map[string]string {
	amzDate := when.UTC().Format("20060102T150405Z")
	signed := map[string]string{}
	for k, v := range headers {
		signed[strings.ToLower(k)] = strings.Join(strings.Fields(v), " ")
	}
	signed["host"] = host
	signed["x-amz-content-sha256"] = payloadSHA256
	signed["x-amz-date"] = amzDate
	names := make([]string, 0, len(signed))
	for n := range signed {
		names = append(names, n)
	}
	sort.Strings(names)
	pairs := make([]string, 0, len(query))
	for k, v := range query {
		pairs = append(pairs, uriEncode(k, true)+"="+uriEncode(v, true))
	}
	sort.Strings(pairs)
	var canonicalHeaders strings.Builder
	for _, n := range names {
		canonicalHeaders.WriteString(n + ":" + signed[n] + "\n")
	}
	canonical := strings.Join([]string{
		method, uriEncode(path, false), strings.Join(pairs, "&"), canonicalHeaders.String(),
		strings.Join(names, ";"), payloadSHA256,
	}, "\n")
	scope := amzDate[:8] + "/" + region + "/s3/aws4_request"
	sum := sha256.Sum256([]byte(canonical))
	toSign := strings.Join([]string{"AWS4-HMAC-SHA256", amzDate, scope, hex.EncodeToString(sum[:])}, "\n")
	key := []byte("AWS4" + secret)
	for _, part := range strings.Split(scope, "/") {
		key = hmacSHA256(key, part)
	}
	signature := hex.EncodeToString(hmacSHA256(key, toSign))
	return map[string]string{
		"x-amz-date":           amzDate,
		"x-amz-content-sha256": payloadSHA256,
		"Authorization": fmt.Sprintf("AWS4-HMAC-SHA256 Credential=%s/%s, SignedHeaders=%s, Signature=%s",
			keyID, scope, strings.Join(names, ";"), signature),
	}
}

var errorCode = regexp.MustCompile(`<Code>([A-Za-z]{1,64})</Code>`)

func codeOf(body []byte) string {
	if m := errorCode.FindSubmatch(body); m != nil {
		return string(m[1])
	}
	return "no error code"
}

var (
	keyIDForm  = regexp.MustCompile(`^[A-Za-z0-9]{1,128}$`)
	secretForm = regexp.MustCompile(`^[!-~]{1,256}$`)
	strongETag = regexp.MustCompile(`^"[^"\s]+"$`)
)

// Client is one bucket of an R2 account.
type Client struct {
	scheme string
	// Host is the Host header sent, exactly the one signed: without a port
	// that is the scheme's default.
	Host   string
	Bucket string
	keyID  string
	secret string
	// Clock is time.Now unless set; Sleep is time.Sleep.
	Clock func() time.Time
	Sleep func(time.Duration)
	// HTTP is the client requests go through.
	HTTP *http.Client
	// Dial, if set, replaces where connections go, as tests do to send a
	// default port's requests to their fake. Set it, and HTTP, before the
	// first request.
	Dial func(network, address string) (net.Conn, error)

	once sync.Once
}

// New is a bucket of an account, signed for with an R2 API token's access
// key ID and secret access key. endpoint, if set, replaces the account's,
// https://<account ID>.r2.cloudflarestorage.com.
func New(accountID, bucket, keyID, secret, endpoint string) (*Client, error) {
	// Checked before anything is signed or sent. Neither message holds a
	// value.
	if !keyIDForm.MatchString(keyID) {
		return nil, errors.New("the R2 access key ID must be letters and digits")
	}
	if !secretForm.MatchString(secret) {
		return nil, errors.New("the R2 secret access key must be printable characters without spaces")
	}
	if endpoint == "" {
		endpoint = "https://" + accountID + ".r2.cloudflarestorage.com"
	}
	u, err := url.Parse(endpoint)
	if err != nil || (u.Scheme != "https" && u.Scheme != "http") || u.Hostname() == "" ||
		(u.Path != "" && u.Path != "/") || u.RawQuery != "" || u.User != nil {
		return nil, errors.New("an R2 endpoint is a scheme and a host")
	}
	// Go would send an internationalized name as punycode, not as signed.
	for _, r := range u.Hostname() {
		if r > 0x7e || r <= ' ' {
			return nil, errors.New("an R2 endpoint's host must be ASCII")
		}
	}
	port := u.Port()
	if (u.Scheme == "https" && port == "443") || (u.Scheme == "http" && port == "80") {
		port = ""
	}
	host := u.Hostname()
	if port != "" {
		host = net.JoinHostPort(host, port)
	} else if strings.Contains(host, ":") {
		host = "[" + host + "]"
	}
	return &Client{scheme: u.Scheme, Host: host, Bucket: bucket, keyID: keyID, secret: secret}, nil
}

// Config is .github/release.json: the bucket and its account, which aren't
// secret.
type Config struct {
	R2 struct {
		AccountID string `json:"account_id"`
		Bucket    string `json:"bucket"`
	} `json:"r2"`
	Downloads string `json:"downloads"`
}

// FromEnvironment is the release bucket of root's .github/release.json,
// signed for with R2_ACCESS_KEY_ID and R2_SECRET_ACCESS_KEY.
// LANESCAPE_BROWSER_R2_ENDPOINT replaces the account's endpoint, for tests
// only.
func FromEnvironment(root string, getenv func(string) string) (*Client, error) {
	data, err := os.ReadFile(filepath.Join(root, ".github", "release.json"))
	if err != nil {
		return nil, err
	}
	var cfg Config
	if err := json.Unmarshal(data, &cfg); err != nil {
		return nil, fmt.Errorf(".github/release.json: %w", err)
	}
	var missing []string
	for _, name := range []string{"R2_ACCESS_KEY_ID", "R2_SECRET_ACCESS_KEY"} {
		if getenv(name) == "" {
			missing = append(missing, name)
		}
	}
	if len(missing) > 0 {
		return nil, fmt.Errorf("%s must be set: the publish and promote environments hold them", strings.Join(missing, " and "))
	}
	return New(cfg.R2.AccountID, cfg.R2.Bucket, getenv("R2_ACCESS_KEY_ID"), getenv("R2_SECRET_ACCESS_KEY"),
		getenv("LANESCAPE_BROWSER_R2_ENDPOINT"))
}

func (c *Client) now() time.Time {
	if c.Clock != nil {
		return c.Clock()
	}
	return time.Now()
}

func (c *Client) sleep(d time.Duration) {
	if c.Sleep != nil {
		c.Sleep(d)
		return
	}
	time.Sleep(d)
}

// http is the client requests go through, made once: straight to the
// endpoint, never through a proxy from the environment, and never following
// a redirect, which would resend a signed request elsewhere or turn a PUT
// into a GET. A redirect is answered as the error it is.
func (c *Client) http() *http.Client {
	c.once.Do(func() {
		if c.HTTP != nil {
			return
		}
		transport := http.DefaultTransport.(*http.Transport).Clone()
		transport.Proxy = nil
		if c.Dial != nil {
			transport.DialContext = nil
			transport.Dial = c.Dial
		}
		c.HTTP = &http.Client{
			Timeout:   5 * time.Minute,
			Transport: transport,
			CheckRedirect: func(*http.Request, []*http.Request) error {
				return http.ErrUseLastResponse
			},
		}
	})
	return c.HTTP
}

func (c *Client) path(key string) (string, error) {
	if key == "" || strings.HasPrefix(key, "/") {
		return "", fmt.Errorf("not an object key: %q", key)
	}
	for _, part := range strings.Split(key, "/") {
		if part == "" || part == "." || part == ".." {
			return "", fmt.Errorf("not an object key: %q", key)
		}
	}
	return "/" + c.Bucket + "/" + key, nil
}

// body is what a request sends: bytes, or a file reopened for each attempt.
type body struct {
	data []byte
	file string
	size int64
}

func (b *body) open() (io.ReadCloser, error) {
	if b.file != "" {
		return os.Open(b.file)
	}
	return io.NopCloser(strings.NewReader(string(b.data))), nil
}

// request sends a request, retrying connection failures, 429 and 5xx. It
// returns the response for 200, 404 or 412, with the body unread; the
// caller closes it.
func (c *Client) request(method, key string, headers map[string]string, b *body, payload string) (*http.Response, error) {
	path, err := c.path(key)
	if err != nil {
		return nil, err
	}
	for attempt := 0; attempt < Attempts; attempt++ {
		sent := map[string]string{}
		for k, v := range headers {
			sent[k] = v
		}
		if b != nil {
			sent["Content-Length"] = strconv.FormatInt(b.size, 10)
		}
		signed := Authorization(method, c.Host, path, nil, sent, payload, c.keyID, c.secret, c.now(), Region)
		var reader io.ReadCloser
		if b != nil && b.size > 0 {
			if reader, err = b.open(); err != nil {
				return nil, err
			}
		} else if b != nil {
			// Any other empty body is one of unknown length to Go, sent
			// chunked, without the Content-Length signed.
			reader = http.NoBody
		}
		req, err := http.NewRequest(method, c.scheme+"://"+c.Host+uriEncode(path, false), reader)
		if err != nil {
			if reader != nil {
				reader.Close()
			}
			return nil, err
		}
		req.Host = c.Host
		if b != nil {
			req.ContentLength = b.size
		}
		for k, v := range sent {
			if k != "Content-Length" {
				req.Header.Set(k, v)
			}
		}
		for k, v := range signed {
			req.Header.Set(k, v)
		}
		// Not Go's transparent gzip: what's read is the object as stored.
		req.Header.Set("Accept-Encoding", "identity")
		resp, err := c.http().Do(req)
		if err != nil {
			if attempt+1 == Attempts {
				return nil, fetch.Error("R2 "+method+" "+key, err)
			}
			c.sleep(time.Duration(1<<attempt) * time.Second)
			continue
		}
		if resp.StatusCode == 200 || resp.StatusCode == 404 || resp.StatusCode == 412 {
			return resp, nil
		}
		// The body only names the error; reading it is best-effort, and the
		// status decides what happens.
		var errBody []byte
		if method != http.MethodHead {
			errBody, _ = io.ReadAll(io.LimitReader(resp.Body, 64<<10))
		}
		resp.Body.Close()
		if (resp.StatusCode == 429 || resp.StatusCode >= 500) && attempt+1 < Attempts {
			c.sleep(time.Duration(1<<attempt) * time.Second)
			continue
		}
		return nil, &Error{Method: method, Key: key, Status: resp.StatusCode, Code: codeOf(errBody)}
	}
	panic("unreachable")
}

var errNotFound = errors.New("no object")

// read is consume(response) for a read, or errNotFound if there's no
// object. A read that fails partway, after its status, is tried again from
// the start, as a failed request is.
func (c *Client) read(method, key string, consume func(*http.Response) error) error {
	for attempt := 0; attempt < Attempts; attempt++ {
		resp, err := c.request(method, key, nil, nil, EmptySHA256)
		if err != nil {
			return err
		}
		if resp.StatusCode == 404 {
			io.Copy(io.Discard, resp.Body)
			resp.Body.Close()
			return errNotFound
		}
		if resp.StatusCode == 412 {
			resp.Body.Close()
			return &Error{Method: method, Key: key, Status: 412, Code: "PreconditionFailed"}
		}
		err = consume(resp)
		resp.Body.Close()
		if err == nil {
			return nil
		}
		if attempt+1 == Attempts {
			return fetch.Error("R2 "+method+" "+key, err)
		}
		c.sleep(time.Duration(1<<attempt) * time.Second)
	}
	panic("unreachable")
}

// Head is an object's size, ETag, recorded SHA-256 and caching.
type Head struct {
	Bytes        int64
	ETag         string
	SHA256       string
	CacheControl string
}

// Head reads an object's head; found is false if there's none.
func (c *Client) Head(key string) (h Head, found bool, err error) {
	err = c.read(http.MethodHead, key, func(resp *http.Response) error {
		io.Copy(io.Discard, resp.Body)
		h = Head{Bytes: resp.ContentLength, ETag: resp.Header.Get("ETag"), SHA256: resp.Header.Get("x-amz-meta-sha256"),
			CacheControl: resp.Header.Get("Cache-Control")}
		return nil
	})
	if errors.Is(err, errNotFound) {
		return Head{}, false, nil
	}
	return h, err == nil, err
}

// Get is an object's contents and ETag, read in one request; found is false
// if there's none.
func (c *Client) Get(key string) (data []byte, etag string, found bool, err error) {
	err = c.read(http.MethodGet, key, func(resp *http.Response) error {
		d, err := io.ReadAll(resp.Body)
		if err != nil {
			return err
		}
		if resp.ContentLength >= 0 && int64(len(d)) != resp.ContentLength {
			return io.ErrUnexpectedEOF
		}
		data, etag = d, resp.Header.Get("ETag")
		return nil
	})
	if errors.Is(err, errNotFound) {
		return nil, "", false, nil
	}
	return data, etag, err == nil, err
}

// Digest is an object's size and SHA-256, streamed; found is false if
// there's none.
func (c *Client) Digest(key string) (size int64, sum string, found bool, err error) {
	err = c.read(http.MethodGet, key, func(resp *http.Response) error {
		h := sha256.New()
		n, err := io.Copy(h, resp.Body)
		if err != nil {
			return err
		}
		if resp.ContentLength >= 0 && n != resp.ContentLength {
			return io.ErrUnexpectedEOF
		}
		size, sum = n, hex.EncodeToString(h.Sum(nil))
		return nil
	})
	if errors.Is(err, errNotFound) {
		return 0, "", false, nil
	}
	return size, sum, err == nil, err
}

// PutOptions are a write's conditions.
type PutOptions struct {
	// SHA256 is the body's, computed if empty.
	SHA256 string
	// IfNoneMatch creates the object only if there's none.
	IfNoneMatch bool
	// IfMatch, if set, replaces the object only if its ETag is still this
	// one: one strong, quoted ETag.
	IfMatch string
}

// Put writes data and returns its ETag. A failed condition is
// ErrPreconditionFailed.
func (c *Client) Put(key string, data []byte, contentType, cacheControl string, o PutOptions) (string, error) {
	sum := o.SHA256
	if sum == "" {
		s := sha256.Sum256(data)
		sum = hex.EncodeToString(s[:])
	}
	return c.put(key, &body{data: data, size: int64(len(data))}, contentType, cacheControl, sum, o)
}

// PutFile writes a file, streamed, and returns its ETag.
func (c *Client) PutFile(key, path, contentType, cacheControl string, o PutOptions) (string, error) {
	sum := o.SHA256
	id, err := IdentityOf(path)
	if err != nil {
		return "", err
	}
	if sum == "" {
		sum = id.SHA256
	}
	return c.put(key, &body{file: path, size: id.Bytes}, contentType, cacheControl, sum, o)
}

func (c *Client) put(key string, b *body, contentType, cacheControl, sum string, o PutOptions) (string, error) {
	headers := map[string]string{"Content-Type": contentType, "Cache-Control": cacheControl, "x-amz-meta-sha256": sum}
	if o.IfNoneMatch {
		headers["If-None-Match"] = "*"
	}
	if o.IfMatch != "" {
		// One strong, quoted ETag, as R2 gives: anything else would make the
		// write unconditional or never match.
		if !strongETag.MatchString(o.IfMatch) {
			return "", errors.New("If-Match needs one strong, quoted ETag")
		}
		headers["If-Match"] = o.IfMatch
	}
	resp, err := c.request(http.MethodPut, key, headers, b, sum)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	// A write's outcome is its status; its body, which may be cut short,
	// says nothing more.
	io.Copy(io.Discard, resp.Body)
	switch resp.StatusCode {
	case 412:
		return "", ErrPreconditionFailed
	case 404:
		return "", &Error{Method: http.MethodPut, Key: key, Status: 404, Code: "NoSuchBucket"}
	}
	return resp.Header.Get("ETag"), nil
}

// PutImmutable uploads a file that must never change. A retry finds the
// same object, by hashing what is stored, and accepts it; a different one
// is a conflict.
func PutImmutable(c *Client, path, key, contentType string) (Identity, error) {
	expected, err := IdentityOf(path)
	if err != nil {
		return Identity{}, err
	}
	_, err = c.PutFile(key, path, contentType, Immutable, PutOptions{SHA256: expected.SHA256, IfNoneMatch: true})
	if err == nil {
		return expected, nil
	}
	if !errors.Is(err, ErrPreconditionFailed) {
		return Identity{}, err
	}
	// The object's own bytes decide, not the SHA-256 its metadata claims.
	size, sum, found, err := c.Digest(key)
	if err != nil {
		return Identity{}, err
	}
	if !found || size != expected.Bytes || sum != expected.SHA256 {
		return Identity{}, fmt.Errorf("immutable object conflict: %s", key)
	}
	return expected, nil
}
