package release

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"maps"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"

	"github.com/lanescape/browser/tools/internal/github"
	"github.com/lanescape/browser/tools/internal/r2"
	"github.com/lanescape/browser/tools/internal/r2/r2test"
)

func env(m map[string]string) func(string) string { return func(k string) string { return m[k] } }

// gitHub is a fake of GitHub's API: the tag refs, and calls it records.
func gitHub(t *testing.T, handle func(w http.ResponseWriter, r *http.Request, body map[string]string)) *github.Client {
	t.Helper()
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var body map[string]string
		if r.Body != nil {
			json.NewDecoder(r.Body).Decode(&body)
		}
		handle(w, r, body)
	}))
	t.Cleanup(srv.Close)
	return &github.Client{Base: srv.URL, Getenv: env(map[string]string{"GITHUB_TOKEN": "t", "GITHUB_REPOSITORY": "lanescape/browser"})}
}

// abcSHA is a commit's full SHA, as GitHub lists them.
var abcSHA = strings.Repeat("abc", 13) + "a"

// A listing with anything but refs under its namespace is incomplete, and
// reservation stops rather than reuse a number.
func TestAnIncompleteRefListingStopsReservation(t *testing.T) {
	for _, page := range []string{`[null]`, `[{"ref": "refs/tags/v1.20261107.4"}]`, `[{"ref": "refs/heads/x", "object": {"sha": "` + abcSHA + `"}}]`} {
		api := gitHub(t, func(w http.ResponseWriter, r *http.Request, _ map[string]string) {
			if strings.HasSuffix(r.URL.Path, "/support/") {
				w.Write([]byte("[]"))
				return
			}
			if r.Method != http.MethodGet {
				t.Errorf("%s with %s", r.Method, page)
			}
			w.Write([]byte(page))
		})
		if b, err := Reserve(GitHubRemote{API: api}, "refs/heads/main", "v1", "20261107", abcSHA); err == nil {
			t.Errorf("%s: reserved %s", page, b)
		}
	}
}

func TestANewTagIsCreatedThroughTheAPI(t *testing.T) {
	var tags []Tag
	var posted map[string]string
	api := gitHub(t, func(w http.ResponseWriter, r *http.Request, body map[string]string) {
		switch {
		case r.Method == http.MethodGet && strings.HasPrefix(r.URL.Path, "/repos/lanescape/browser/git/matching-refs/"):
			out := []map[string]any{}
			if strings.HasSuffix(r.URL.Path, "/tags/") {
				for _, tg := range tags {
					out = append(out, map[string]any{"ref": "refs/tags/" + tg.Name, "object": map[string]string{"sha": tg.SHA, "type": "commit"}})
				}
			}
			json.NewEncoder(w).Encode(out)
		case r.Method == http.MethodPost && r.URL.Path == "/repos/lanescape/browser/git/refs":
			posted = body
			tags = append(tags, Tag{strings.TrimPrefix(body["ref"], "refs/tags/"), body["sha"]})
			w.WriteHeader(201)
		default:
			t.Errorf("unexpected %s %s", r.Method, r.URL.Path)
			w.WriteHeader(500)
		}
	})
	if b, err := Reserve(GitHubRemote{API: api}, "refs/heads/main", "v1", "20261107", abcSHA); b != "20261107.1" || err != nil {
		t.Fatalf("%s %v", b, err)
	}
	if posted["ref"] != "refs/tags/v1.20261107.1" || posted["sha"] != abcSHA {
		t.Fatal(posted)
	}
}

func TestAConcurrentRunOfTheSameCommitGetsTheSameBuild(t *testing.T) {
	var tags []Tag
	api := gitHub(t, func(w http.ResponseWriter, r *http.Request, body map[string]string) {
		switch {
		case r.Method == http.MethodGet && strings.HasSuffix(r.URL.Path, "/matching-refs/tags/"):
			out := []map[string]any{}
			for _, tg := range tags {
				out = append(out, map[string]any{"ref": "refs/tags/" + tg.Name, "object": map[string]string{"sha": tg.SHA, "type": "commit"}})
			}
			json.NewEncoder(w).Encode(out)
		case r.Method == http.MethodGet && strings.HasSuffix(r.URL.Path, "/matching-refs/heads/support/"):
			w.Write([]byte("[]"))
		case r.Method == http.MethodPost:
			// The other run got there first.
			tags = append(tags, Tag{"v1.20261107.1", abcSHA})
			w.WriteHeader(422)
		case r.URL.Path == "/repos/lanescape/browser/git/ref/tags/v1.20261107.1":
			w.Write([]byte(`{"ref": "refs/tags/v1.20261107.1", "object": {"sha": "` + abcSHA + `", "type": "commit"}}`))
		default:
			w.WriteHeader(500)
		}
	})
	if b, err := Reserve(GitHubRemote{API: api}, "refs/heads/main", "v1", "20261107", abcSHA); b != "20261107.1" || err != nil {
		t.Fatalf("%s %v", b, err)
	}
}

func TestAnAPIFailureIsNotACollision(t *testing.T) {
	api := gitHub(t, func(w http.ResponseWriter, r *http.Request, _ map[string]string) {
		if r.Method == http.MethodGet {
			w.Write([]byte("[]"))
			return
		}
		w.WriteHeader(403)
	})
	_, err := Reserve(GitHubRemote{API: api}, "refs/heads/main", "v1", "20261107", "abc")
	var e *github.Error
	if !errors.As(err, &e) || e.Status != 403 {
		t.Fatalf("err = %v", err)
	}
}

// --- The bucket on R2 ---

func r2Bucket(t *testing.T) (*r2test.Fake, R2Bucket) {
	f := r2test.New(t)
	return f, R2Bucket{R2: f.Client(t)}
}

func TestLatestIsReadWithItsETagAndSwappedOnlyAtIt(t *testing.T) {
	f, b := r2Bucket(t)
	if _, _, found, err := b.ReadLatest("v1"); found || err != nil {
		t.Fatal(found, err)
	}
	if ok, err := b.WriteLatest("v1", "20261107.1", "", false); !ok || err != nil {
		t.Fatal(ok, err)
	}
	// Making it again fails: it exists.
	if ok, err := b.WriteLatest("v1", "20261107.9", "", false); ok || err != nil {
		t.Fatal(ok, err)
	}
	text, etag, _, _ := b.ReadLatest("v1")
	if text != "20261107.1\n" {
		t.Fatal(text)
	}
	if ok, err := b.WriteLatest("v1", "20261107.2", etag, true); !ok || err != nil {
		t.Fatal(ok, err)
	}
	// The ETag read before that write no longer holds.
	if ok, err := b.WriteLatest("v1", "20261107.3", etag, true); ok || err != nil {
		t.Fatal(ok, err)
	}
	if text, _, _, _ := b.ReadLatest("v1"); text != "20261107.2\n" {
		t.Fatal(text)
	}
	if f.Get("lanescape-browser/v1/latest").Headers["cache-control"] != "no-store" {
		t.Fatal(f.Get("lanescape-browser/v1/latest").Headers)
	}
}

func TestReadsAndDigests(t *testing.T) {
	f, b := r2Bucket(t)
	if _, found, err := b.Read("v1/none"); found || err != nil {
		t.Fatal(found, err)
	}
	if _, _, found, err := b.Digest("v1/none"); found || err != nil {
		t.Fatal(found, err)
	}
	f.Put("lanescape-browser/v1/x", &r2test.Object{Body: []byte("package"), ETag: `"1"`, Headers: map[string]string{}})
	if text, _, _ := b.Read("v1/x"); text != "package" {
		t.Fatal(text)
	}
	s := sha256.Sum256([]byte("package"))
	if n, sum, _, _ := b.Digest("v1/x"); n != 7 || sum != hex.EncodeToString(s[:]) {
		t.Fatal(n, sum)
	}
}

func TestARefusalIsNotAMissingObject(t *testing.T) {
	f, b := r2Bucket(t)
	f.Refuse = 403
	if _, _, err := b.Read("v1/x"); err == nil || !strings.Contains(err.Error(), "403") {
		t.Fatal(err)
	}
	if _, err := b.WriteLatest("v1", "20261107.1", "", false); err == nil || !strings.Contains(err.Error(), "403") {
		t.Fatal(err)
	}
}

func TestLatestIsNeverWrittenUnconditionally(t *testing.T) {
	f, b := r2Bucket(t)
	for _, etag := range []string{"", "unquoted", `W/"1"`, `"1", "2"`} {
		if _, err := b.WriteLatest("v1", "20261107.1", etag, true); err == nil {
			t.Errorf("ETag %q accepted", etag)
		}
	}
	if f.Get("lanescape-browser/v1/latest") != nil {
		t.Fatal("latest was written")
	}
	// latest without an ETag can't be swapped for.
	f.Put("lanescape-browser/v1/latest", &r2test.Object{Body: []byte("20261107.1\n"), Headers: map[string]string{}})
	if _, _, _, err := b.ReadLatest("v1"); err == nil || !strings.Contains(err.Error(), "without an ETag") {
		t.Fatal(err)
	}
}

// --- Corresponding source ---

const (
	sourceVersion = "157.0.8103.39-4"
	tarballPath   = "/chromium-browser-official/chromium-157.0.8103.39.tar.xz"
)

type sourceCase struct {
	assets        map[string]map[string]any
	manifest      map[string]any
	status        int
	pinned        *string
	tarballStatus int
	pinnedAssets  map[string]string
	// redirect, if set, is where the manifest's download is sent; extra
	// follows the manifest served, but not the one pinned.
	redirect string
	extra    []byte
}

func sourceManifest(change map[string]any) map[string]any {
	m := map[string]any{"version": sourceVersion, "source": map[string]any{"chromium": map[string]any{"tarball": "TARBALL"}},
		"assets": []map[string]any{
			{"name": "a-source-blink.tar.zst", "part": "source", "size": 10, "sha256": strings.Repeat("b", 64)},
			{"name": "a-linux-amd64-lib.tar.zst", "part": "lib", "size": 5, "sha256": strings.Repeat("c", 64)}}}
	for k, v := range change {
		m[k] = v
	}
	return m
}

var blink = map[string]any{"size": 10, "digest": "sha256:" + strings.Repeat("b", 64)}

func with(m map[string]any, k string, v any) map[string]any {
	out := map[string]any{}
	for a, b := range m {
		out[a] = b
	}
	if v == nil {
		delete(out, k)
	} else {
		out[k] = v
	}
	return out
}

func (c sourceCase) check(t *testing.T) error {
	t.Helper()
	var srv *httptest.Server
	srv = httptest.NewTLSServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch {
		case r.URL.Path == "/repos/lanescape/libchromium/releases/tags/v"+sourceVersion:
			if c.status != 0 && c.status != 200 {
				w.WriteHeader(c.status)
				return
			}
			var assets []map[string]any
			for name, a := range c.assets {
				entry := map[string]any{"name": name, "browser_download_url": srv.URL + "/dl/" + name, "state": "uploaded"}
				for k, v := range a {
					entry[k] = v
				}
				assets = append(assets, entry)
			}
			json.NewEncoder(w).Encode(map[string]any{"assets": assets})
		case r.URL.Path == "/dl/manifest.json":
			if c.redirect != "" {
				http.Redirect(w, r, c.redirect, http.StatusFound)
				return
			}
			w.Write(append(c.raw(srv.URL), c.extra...))
		case r.URL.Path == tarballPath && r.Method == http.MethodHead:
			if c.tarballStatus == http.StatusFound {
				http.Redirect(w, r, "/elsewhere", http.StatusFound)
				return
			}
			if c.tarballStatus != 0 {
				w.WriteHeader(c.tarballStatus)
			}
		default:
			t.Errorf("unexpected %s %s", r.Method, r.URL.Path)
			w.WriteHeader(500)
		}
	}))
	defer srv.Close()
	raw := c.raw(srv.URL)
	if c.extra != nil {
		// The pin is the manifest without the extra, at exactly the limit.
		maxManifest = len(raw)
	}
	pinned := ""
	if c.pinned != nil {
		pinned = *c.pinned
	} else {
		s := sha256.Sum256(raw)
		pinned = hex.EncodeToString(s[:])
	}
	pinnedAssets := c.pinnedAssets
	if pinnedAssets == nil {
		pinnedAssets = map[string]string{"a-linux-amd64-lib.tar.zst": strings.Repeat("c", 64)}
	}
	s := SourceChecker{API: &github.Client{Base: srv.URL, HTTP: srv.Client(), Getenv: env(map[string]string{"GITHUB_TOKEN": "t"})}, HTTP: srv.Client(),
		TarballBase: srv.URL + "/chromium-browser-official/"}
	return s.Check(sourceVersion, pinned, pinnedAssets)
}

// raw is the manifest as served, with the tarball at the test server.
func (c sourceCase) raw(base string) []byte {
	data, _ := json.Marshal(c.manifest)
	return []byte(strings.ReplaceAll(string(data), "TARBALL", base+tarballPath))
}

func TestAReleaseWithEverySourceArchivePasses(t *testing.T) {
	c := sourceCase{assets: map[string]map[string]any{"manifest.json": {"size": 1}, "a-source-blink.tar.zst": blink, "a-linux-amd64-lib.tar.zst": {"size": 5}},
		manifest: sourceManifest(nil)}
	if err := c.check(t); err != nil {
		t.Fatal(err)
	}
}

func TestMissingShortReplacedOrUnfinishedSourceIsRefused(t *testing.T) {
	for _, assets := range []map[string]map[string]any{
		{"manifest.json": {"size": 1}},
		{"manifest.json": {"size": 1}, "a-source-blink.tar.zst": with(blink, "size", 9)},
		// The same size, other bytes.
		{"manifest.json": {"size": 1}, "a-source-blink.tar.zst": with(blink, "digest", "sha256:"+strings.Repeat("e", 64))},
		{"manifest.json": {"size": 1}, "a-source-blink.tar.zst": {"size": 10}},
		{"manifest.json": {"size": 1}, "a-source-blink.tar.zst": with(blink, "state", "starter")},
	} {
		refusedWith(t, sourceCase{assets: assets, manifest: sourceManifest(nil)}.check(t), "lacks a-source-blink")
	}
	refusedWith(t, sourceCase{assets: map[string]map[string]any{"a-source-blink.tar.zst": blink}, manifest: sourceManifest(nil)}.check(t), "no manifest")
	good := map[string]map[string]any{"manifest.json": {"size": 1}, "a-source-blink.tar.zst": blink}
	refusedWith(t, sourceCase{assets: good, manifest: sourceManifest(map[string]any{"assets": []map[string]any{
		{"name": "a-linux-amd64-lib.tar.zst", "part": "lib", "size": 5, "sha256": strings.Repeat("c", 64)}}})}.check(t), "lists no source")
	refusedWith(t, sourceCase{assets: good, manifest: sourceManifest(map[string]any{"version": "other"})}.check(t), "lists no source")
	refusedWith(t, sourceCase{status: 404, manifest: sourceManifest(nil)}.check(t), "no release")
}

func TestTheManifestMustBeThePinnedOne(t *testing.T) {
	// A manifest that lists less source, even consistently with the release,
	// isn't the one reviewed at the pin.
	good := map[string]map[string]any{"manifest.json": {"size": 1}, "a-source-blink.tar.zst": blink}
	for _, pinned := range []string{strings.Repeat("d", 64), ""} {
		refusedWith(t, sourceCase{assets: good, manifest: sourceManifest(nil), pinned: &pinned}.check(t), "not the pinned one")
	}
}

func TestThePinnedSDKPartsMustBeTheManifests(t *testing.T) {
	good := map[string]map[string]any{"manifest.json": {"size": 1}, "a-source-blink.tar.zst": blink}
	for _, pinned := range []map[string]string{{"a-linux-amd64-lib.tar.zst": strings.Repeat("d", 64)}, {"a-other.tar.zst": strings.Repeat("c", 64)}, {}} {
		refusedWith(t, sourceCase{assets: good, manifest: sourceManifest(nil), pinnedAssets: pinned}.check(t), "other SDK parts")
	}
}

// A failed download never names the signed URL it was redirected to.
func TestAFailedManifestDownloadHidesWhereItWent(t *testing.T) {
	closed := httptest.NewServer(http.NotFoundHandler())
	closed.Close()
	good := map[string]map[string]any{"manifest.json": {"size": 1}, "a-source-blink.tar.zst": blink}
	for _, to := range []string{closed.URL + "/m?X-Amz-Signature=deadbeef", "http://%zz/m?X-Amz-Signature=deadbeef"} {
		err := sourceCase{assets: good, manifest: sourceManifest(nil), redirect: to}.check(t)
		if err == nil || strings.Contains(err.Error(), "deadbeef") || strings.Contains(err.Error(), "http") {
			t.Fatalf("err = %v", err)
		}
	}
}

// The whole manifest is hashed: the pinned one with more after it isn't
// the pinned one.
func TestAManifestLongerThanTheLimitIsRefused(t *testing.T) {
	good := map[string]map[string]any{"manifest.json": {"size": 1}, "a-source-blink.tar.zst": blink}
	c := sourceCase{assets: good, manifest: sourceManifest(nil), extra: []byte(" ")}
	defer func(was int) { maxManifest = was }(maxManifest)
	refusedWith(t, c.check(t), "not the pinned one")
}

func TestChromiumsTarballMustBeThere(t *testing.T) {
	good := map[string]map[string]any{"manifest.json": {"size": 1}, "a-source-blink.tar.zst": blink}
	refusedWith(t, sourceCase{assets: good, manifest: sourceManifest(nil), tarballStatus: 404}.check(t), "is not there")
	refusedWith(t, sourceCase{assets: good, manifest: sourceManifest(map[string]any{"source": map[string]any{}})}.check(t), "names no Chromium tarball")
	// A redirect, even to something that answers, isn't the tarball.
	refusedWith(t, sourceCase{assets: good, manifest: sourceManifest(nil), tarballStatus: http.StatusFound}.check(t), "is not there")
	// Another version's tarball, or any other page, isn't this one's.
	for _, other := range []string{"https://www.chromium.org/", "TARBALL.html",
		"https://commondatastorage.googleapis.com/chromium-browser-official/chromium-157.0.8103.38.tar.xz"} {
		m := sourceManifest(map[string]any{"source": map[string]any{"chromium": map[string]any{"tarball": other}}})
		refusedWith(t, sourceCase{assets: good, manifest: m}.check(t), "names no Chromium tarball")
	}
}

// The repository's own pin, live: run with LANESCAPE_BROWSER_LIVE=1 and a
// GITHUB_TOKEN.
func TestThePinnedReleaseLive(t *testing.T) {
	if os.Getenv("LANESCAPE_BROWSER_LIVE") == "" {
		t.Skip("LANESCAPE_BROWSER_LIVE isn't set")
	}
	s := SourceChecker{API: &github.Client{}}
	if err := s.Check(libchromiumPin.Version, libchromiumPin.Manifest, libchromiumPin.Assets); err != nil {
		t.Fatal(err)
	}
	refusedWith(t, s.Check("157.0.8103.39-99", libchromiumPin.Manifest, libchromiumPin.Assets), "no release")
}

// --- Publishing ---

// repoWith is a root with a Cargo.toml of a major, the real pin, and
// packages for targets.
func repoWith(t *testing.T, major string, targets []string) string {
	return repoWithFlag(t, major, targets, false)
}

// repoWithFlag is repoWith, its major marked unstable or not.
func repoWithFlag(t *testing.T, major string, targets []string, unstable bool) string {
	t.Helper()
	root := t.TempDir()
	os.MkdirAll(filepath.Join(root, "dist"), 0o755)
	os.MkdirAll(filepath.Join(root, "engine"), 0o755)
	os.WriteFile(filepath.Join(root, "engine", "libchromium.json"), []byte(libchromiumText), 0o644)
	cargo := "[package]\nversion=\"" + major[1:] + ".0.0\"\n"
	if unstable {
		cargo += "\n[package.metadata.lanebrowser]\nunstable = true\n"
	}
	os.WriteFile(filepath.Join(root, "Cargo.toml"), []byte(cargo), 0o644)
	for _, target := range targets {
		os.WriteFile(filepath.Join(root, "dist", "lanescape-browser-"+target+".tar.gz"), []byte("package "+target), 0o644)
	}
	return root
}

func TestPublicationUsesThePackageMajorAndUploadsTheManifestLast(t *testing.T) {
	for _, tc := range []struct {
		major    string
		unstable bool
	}{{"v1", false}, {"v1", true}, {"v2", false}, {"v3", true}} {
		major := tc.major
		root := repoWithFlag(t, major, Targets, tc.unstable)
		f := r2test.New(t)
		var calledVersion, calledManifest string
		var calledAssets map[string]string
		p := Publisher{Root: root, TagCommit: tagged(abcSHA), R2: func() (*r2.Client, error) { return f.Client(t), nil },
			Sources: func(v, m string, a map[string]string) error {
				calledVersion, calledManifest, calledAssets = v, m, a
				return nil
			}}
		if err := p.Publish("20261031.2", abcSHA); err != nil {
			t.Fatal(err)
		}
		if calledVersion != libchromiumPin.Version || calledManifest != libchromiumPin.Manifest || !maps.Equal(calledAssets, libchromiumPin.Assets) {
			t.Fatal(calledVersion, calledManifest, calledAssets)
		}
		// The bucket layout keeps the bare build number; the manifest goes last.
		var keys []string
		for _, target := range Targets {
			keys = append(keys, "lanescape-browser/"+major+"/20261031.2/lanescape-browser-"+target+".tar.gz")
		}
		keys = append(keys, "lanescape-browser/"+major+"/20261031.2/manifest.json")
		for _, k := range keys {
			if f.Get(k) == nil {
				t.Fatalf("%s wasn't uploaded", k)
			}
		}
		if order := f.PutOrder(); !slices.Equal(order, keys) {
			t.Fatalf("uploads %v, want %v", order, keys)
		}
		var m Manifest
		if err := json.Unmarshal(f.Get(keys[3]).Body, &m); err != nil {
			t.Fatal(err)
		}
		if m.Build != "20261031.2" || m.Major != major || m.Commit != abcSHA || *m.Libchromium != LibchromiumOf(libchromiumPin.Version) {
			t.Fatalf("%+v", m)
		}
		if raw := string(f.Get(keys[3]).Body); strings.Contains(raw, "unstable") {
			t.Fatal(raw)
		}
		// A rerun finds what's there and accepts it.
		if err := p.Publish("20261031.2", abcSHA); err != nil {
			t.Fatalf("rerun: %v", err)
		}
	}
}

// tagged is a TagCommit for which every build's tag names sha.
func tagged(sha string) func(string) (string, bool, error) {
	return func(string) (string, bool, error) { return sha, true, nil }
}

// A build is published only for the commit its tag names, checked before
// anything is uploaded.
func TestPublicationNeedsTheTaggedCommit(t *testing.T) {
	root := repoWith(t, "v1", Targets)
	for name, tc := range map[string]struct {
		build, commit string
		tag           func(string) (string, bool, error)
	}{
		"another commit": {"20261031.2", abcSHA, tagged(strings.Repeat("d", 40))},
		"no tag":         {"20261031.2", abcSHA, func(string) (string, bool, error) { return "", false, nil }},
		"a short commit": {"20261031.2", "abc", tagged("abc")},
		"a branch":       {"20261031.2", "main", tagged("main")},
		"not a build":    {"latest", abcSHA, tagged(abcSHA)},
	} {
		made := false
		p := Publisher{Root: root, Sources: noSources, TagCommit: tc.tag,
			R2: func() (*r2.Client, error) { made = true; return nil, errors.New("no") }}
		if err := p.Publish(tc.build, tc.commit); err == nil || made || strings.Contains(err.Error(), "main") {
			t.Errorf("%s: err = %v, R2 made %v", name, err, made)
		}
	}
	var asked string
	p := Publisher{Root: root, Sources: noSources, TagCommit: func(name string) (string, bool, error) { asked = name; return "", false, nil }}
	p.Publish("20261031.2", abcSHA)
	if asked != "v1.20261031.2" {
		t.Fatalf("asked for tag %q", asked)
	}
}

// pythonManifest is the manifest the Python tooling published for v1's
// 20261031.2 of commit abc (replaced by abcSHA in use), with repoWith's packages; PIN is the pinned
// libchromium version.
const pythonManifest = `{"build": "20261031.2", "commit": "abc", "libchromium": {"release": "https://github.com/lanescape/libchromium/releases/tag/vPIN", "version": "PIN"}, "major": "v1", "packages": {"linux-amd64": {"bytes": 19, "name": "lanescape-browser-linux-amd64.tar.gz", "sha256": "3a646d1e103e3fe04b4bb88e47aa901f712053774b9d13a6765d29b92295d024"}, "linux-arm64": {"bytes": 19, "name": "lanescape-browser-linux-arm64.tar.gz", "sha256": "fb1030ede9d1c1248e6e795c6de3aa756002047fea9e880114f24cb3c4fa0a22"}, "macos-arm64": {"bytes": 19, "name": "lanescape-browser-macos-arm64.tar.gz", "sha256": "ebdaf48b2edb86b7ff0a38595c7a259200a506c96d59b70039901feaaeae6ee0"}}}` + "\n"

// Publishing again a build the Python tooling published finds the same
// manifest, byte for byte, and accepts it.
func TestPublishingAgainABuildPythonPublishedAcceptsIt(t *testing.T) {
	root := repoWith(t, "v1", Targets)
	f := r2test.New(t)
	c := f.Client(t)
	manifest := filepath.Join(t.TempDir(), "manifest.json")
	os.WriteFile(manifest, []byte(strings.NewReplacer("PIN", libchromiumPin.Version, `"commit": "abc"`, `"commit": "`+abcSHA+`"`).Replace(pythonManifest)), 0o644)
	for _, target := range Targets {
		name := "lanescape-browser-" + target + ".tar.gz"
		if _, err := r2.PutImmutable(c, filepath.Join(root, "dist", name), "v1/20261031.2/"+name, "application/gzip"); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := r2.PutImmutable(c, manifest, "v1/20261031.2/manifest.json", "application/json"); err != nil {
		t.Fatal(err)
	}
	p := Publisher{Root: root, TagCommit: tagged(abcSHA), Sources: noSources, R2: func() (*r2.Client, error) { return f.Client(t), nil }}
	if err := p.Publish("20261031.2", abcSHA); err != nil {
		t.Fatal(err)
	}
}

func TestAMissingPlatformOrSourcePublishesNothing(t *testing.T) {
	root := repoWith(t, "v1", Targets[:2])
	made := false
	p := Publisher{Root: root, TagCommit: tagged(abcSHA), Sources: noSources, R2: func() (*r2.Client, error) { made = true; return nil, errors.New("no") }}
	if err := p.Publish("20261031.2", abcSHA); err == nil || !strings.Contains(err.Error(), "macos-arm64") || made {
		t.Fatalf("err = %v, R2 made %v", err, made)
	}
	root = repoWith(t, "v1", Targets)
	p = Publisher{Root: root, TagCommit: tagged(abcSHA), Sources: func(string, string, map[string]string) error { return &Refused{Msg: "no source"} },
		R2: func() (*r2.Client, error) { made = true; return nil, errors.New("no") }}
	if err := p.Publish("20261031.2", abcSHA); err == nil || !strings.Contains(err.Error(), "no source") || made {
		t.Fatalf("err = %v, R2 made %v", err, made)
	}
}

// The commit a tag names, through the API.
func TestTagCommitThroughTheAPI(t *testing.T) {
	api := gitHub(t, func(w http.ResponseWriter, r *http.Request, _ map[string]string) {
		switch r.URL.Path {
		case "/repos/lanescape/browser/git/ref/tags/v1.20261107.1":
			w.Write([]byte(`{"ref": "refs/tags/v1.20261107.1", "object": {"sha": "` + abcSHA + `", "type": "commit"}}`))
		case "/repos/lanescape/browser/git/ref/tags/v1.20261107.2":
			w.Write([]byte(`{"ref": "refs/tags/v1.20261107.2", "object": {"sha": "abc", "type": "commit"}}`))
		case "/repos/lanescape/browser/git/ref/tags/v1.20261107.3":
			w.Write([]byte(`{"ref": "refs/tags/v1.20261107.3", "object": {"sha": "` + strings.Repeat("e", 40) + `", "type": "tag"}}`))
		case "/repos/lanescape/browser/git/tags/" + strings.Repeat("e", 40):
			w.Write([]byte(`{"sha": "` + strings.Repeat("e", 40) + `", "object": {"sha": "` + abcSHA + `", "type": "commit"}}`))
		case "/repos/lanescape/browser/git/ref/tags/v1.20261107.5":
			w.Write([]byte(`{"ref": "refs/tags/v1.20261107.5", "object": {"sha": "` + strings.Repeat("f", 40) + `", "type": "tag"}}`))
		case "/repos/lanescape/browser/git/tags/" + strings.Repeat("f", 40):
			// A tag of a tag.
			w.Write([]byte(`{"sha": "` + strings.Repeat("f", 40) + `", "object": {"sha": "` + strings.Repeat("e", 40) + `", "type": "tag"}}`))
		case "/repos/lanescape/browser/git/ref/tags/v1.20261107.6":
			w.Write([]byte(`{"ref": "refs/tags/v1.20261107.6", "object": {"sha": "` + strings.Repeat("0", 40) + `", "type": "tag"}}`))
		case "/repos/lanescape/browser/git/tags/" + strings.Repeat("0", 40):
			// Another tag object's answer.
			w.Write([]byte(`{"sha": "` + strings.Repeat("e", 40) + `", "object": {"sha": "` + abcSHA + `", "type": "commit"}}`))
		case "/repos/lanescape/browser/git/ref/tags/v1.20261107.4":
			w.Write([]byte(`{"ref": "refs/tags/v1.20261107.4", "object": {"sha": "` + abcSHA + `", "type": "tree"}}`))
		default:
			w.WriteHeader(404)
		}
	})
	g := GitHubRemote{API: api}
	if sha, found, err := g.TagCommit("v1.20261107.1"); sha != abcSHA || !found || err != nil {
		t.Fatalf("%s %v %v", sha, found, err)
	}
	if _, found, err := g.TagCommit("v1.20261107.9"); found || err != nil {
		t.Fatalf("missing: %v %v", found, err)
	}
	if _, _, err := g.TagCommit("v1.20261107.2"); err == nil {
		t.Fatal("a short SHA was accepted")
	}
	// An annotated tag names the commit it tags.
	if sha, found, err := g.TagCommit("v1.20261107.3"); sha != abcSHA || !found || err != nil {
		t.Fatalf("annotated: %s %v %v", sha, found, err)
	}
	if _, _, err := g.TagCommit("v1.20261107.4"); err == nil {
		t.Fatal("a tag of a tree was accepted")
	}
	if sha, found, err := g.TagCommit("v1.20261107.5"); sha != abcSHA || !found || err != nil {
		t.Fatalf("a tag of a tag: %s %v %v", sha, found, err)
	}
	if _, _, err := g.TagCommit("v1.20261107.6"); err == nil {
		t.Fatal("another tag object's answer was accepted")
	}
}

// An annotated build tag, such as one made by hand, names the commit it
// tags for reservation too, as it does for publication, promotion and the
// clone: a rerun for that commit reuses its number rather than taking a
// second one.
func TestReservationPeelsAnnotatedTags(t *testing.T) {
	tagObject := strings.Repeat("e", 40)
	var created []string
	api := gitHub(t, func(w http.ResponseWriter, r *http.Request, body map[string]string) {
		switch {
		case strings.HasSuffix(r.URL.Path, "/matching-refs/heads/support/"):
			w.Write([]byte("[]"))
		case strings.HasSuffix(r.URL.Path, "/matching-refs/tags/"):
			refs := `{"ref": "refs/tags/v1.20261107.3", "object": {"sha": "` + tagObject + `", "type": "tag"}}`
			for _, c := range created {
				refs += `, {"ref": "` + c + `", "object": {"sha": "` + strings.Repeat("d", 40) + `", "type": "commit"}}`
			}
			w.Write([]byte("[" + refs + "]"))
		case r.URL.Path == "/repos/lanescape/browser/git/tags/"+tagObject:
			w.Write([]byte(`{"sha": "` + tagObject + `", "object": {"sha": "` + abcSHA + `", "type": "commit"}}`))
		case r.Method == http.MethodPost:
			created = append(created, body["ref"])
			w.WriteHeader(201)
		default:
			t.Errorf("unexpected %s %s", r.Method, r.URL.Path)
			w.WriteHeader(500)
		}
	})
	if b, err := Reserve(GitHubRemote{API: api}, "refs/heads/main", "v1", "20261107", abcSHA); b != "20261107.3" || err != nil {
		t.Fatalf("%s %v", b, err)
	}
	if len(created) != 0 {
		t.Fatalf("created %v", created)
	}
	// Another commit's annotated tag still holds its number.
	other := strings.Repeat("d", 40)
	if b, err := Reserve(GitHubRemote{API: api}, "refs/heads/main", "v1", "20261107", other); b != "20261107.4" || err != nil {
		t.Fatalf("%s %v", b, err)
	}
}
