package release

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/lanescape/browser/tools/internal/repo"
)

var (
	libchromiumText = mustRead("../../../engine/libchromium.json")
	libchromiumPin  = func() repo.Pin { p, _ := repo.ReadPin([]byte(libchromiumText)); return p }()
)

func mustRead(path string) string {
	b, err := os.ReadFile(path)
	if err != nil {
		panic(err)
	}
	return string(b)
}

// fakeRepo is commits with one parent each, origin branches and build tags.
type fakeRepo struct {
	parents  map[string]string
	files    map[string]map[string]string
	tags     []Tag
	branches map[string]string
}

func newFakeRepo() *fakeRepo {
	return &fakeRepo{parents: map[string]string{}, files: map[string]map[string]string{}, branches: map[string]string{}}
}

// commit adds sha on parent; unstable is nil, true or false.
func (r *fakeRepo) commit(sha, parent string, major int, unstable *bool) {
	cargo := fmt.Sprintf("[package]\nversion=\"%d.0.0\"\n", major)
	if unstable != nil {
		cargo += fmt.Sprintf("\n[package.metadata.lanescape-browser]\nunstable = %v\n", *unstable)
	}
	r.parents[sha] = parent
	r.files[sha] = map[string]string{"Cargo.toml": cargo, "engine/libchromium.json": libchromiumText}
}

func (r *fakeRepo) Show(sha, path string) (string, error) { return r.files[sha][path], nil }

func (r *fakeRepo) BranchContains(branch, sha string) (bool, error) {
	head, ok := r.branches[branch]
	if !ok {
		return false, nil
	}
	return r.IsAncestor(sha, head)
}

func (r *fakeRepo) IsAncestor(ancestor, descendant string) (bool, error) {
	if _, ok := r.parents[ancestor]; !ok {
		return false, errors.New("unknown commit")
	}
	if _, ok := r.parents[descendant]; !ok {
		return false, errors.New("unknown commit")
	}
	for d := descendant; d != ""; d = r.parents[d] {
		if d == ancestor {
			return true, nil
		}
	}
	return false, nil
}

func (r *fakeRepo) BuildTags() ([]Tag, error) { return append([]Tag(nil), r.tags...), nil }

func (r *fakeRepo) removeTag(t Tag) {
	for i, x := range r.tags {
		if x == t {
			r.tags = append(r.tags[:i], r.tags[i+1:]...)
			return
		}
	}
}

type fakeObject struct {
	data         []byte
	etag         string
	cacheControl string
}

// fakeBucket is objects with ETags; latest supports only compare-and-swap
// writes, like R2's conditional writes. gate, if set, orders each caller's
// reads and writes of latest.
type fakeBucket struct {
	mu          sync.Mutex
	objects     map[string]*fakeObject
	version     int
	writes      []string
	gate        func(who string) func()
	beforeWrite func(major, build string)
}

func newFakeBucket() *fakeBucket { return &fakeBucket{objects: map[string]*fakeObject{}} }

func (b *fakeBucket) put(key string, data []byte, cacheControl string) {
	b.version++
	b.objects[key] = &fakeObject{data: data, etag: fmt.Sprintf(`"%d"`, b.version), cacheControl: cacheControl}
}

func (b *fakeBucket) Read(key string) (string, bool, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	o, ok := b.objects[key]
	if !ok {
		return "", false, nil
	}
	return string(o.data), true, nil
}

func (b *fakeBucket) Digest(key string) (int64, string, bool, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	o, ok := b.objects[key]
	if !ok {
		return 0, "", false, nil
	}
	s := sha256.Sum256(o.data)
	return int64(len(o.data)), hex.EncodeToString(s[:]), true, nil
}

// as is the bucket as one caller sees it, so the gate knows whose turn.
func (b *fakeBucket) as(who string) Bucket { return callerBucket{b, who} }

type callerBucket struct {
	*fakeBucket
	who string
}

func (c callerBucket) ReadLatest(major string) (string, string, bool, error) {
	if c.gate != nil {
		defer c.gate(c.who)()
	}
	return c.fakeBucket.readLatest(major)
}

func (c callerBucket) WriteLatest(major, build, etag string, exists bool) (bool, error) {
	if c.beforeWrite != nil {
		c.beforeWrite(major, build)
	}
	if c.gate != nil {
		defer c.gate(c.who)()
	}
	return c.fakeBucket.writeLatest(major, build, etag, exists)
}

func (b *fakeBucket) ReadLatest(major string) (string, string, bool, error) {
	return b.readLatest(major)
}

func (b *fakeBucket) WriteLatest(major, build, etag string, exists bool) (bool, error) {
	if b.beforeWrite != nil {
		b.beforeWrite(major, build)
	}
	return b.writeLatest(major, build, etag, exists)
}

func (b *fakeBucket) readLatest(major string) (string, string, bool, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	o, ok := b.objects[major+"/latest"]
	if !ok {
		return "", "", false, nil
	}
	return string(o.data), o.etag, true, nil
}

func (b *fakeBucket) writeLatest(major, build, etag string, exists bool) (bool, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	o, ok := b.objects[major+"/latest"]
	if ok != exists || (ok && o.etag != etag) {
		return false, nil
	}
	b.put(major+"/latest", []byte(build+"\n"), "no-store")
	b.writes = append(b.writes, build)
	return true, nil
}

func (b *fakeBucket) latest(major string) string {
	b.mu.Lock()
	defer b.mu.Unlock()
	if o, ok := b.objects[major+"/latest"]; ok {
		return string(o.data)
	}
	return ""
}

type publishOptions struct {
	major       string
	tag         string
	targets     []string
	noManifest  bool
	libchromium any // nil: the pinned one; "none": absent; else a Libchromium
}

// publish publishes a complete build of sha as CD does, manifest last.
func publish(b *fakeBucket, r *fakeRepo, sha, build string, o publishOptions) {
	if o.major == "" {
		o.major = "v1"
	}
	if o.targets == nil {
		o.targets = Targets
	}
	packages := map[string]Package{}
	for _, target := range o.targets {
		data := []byte(target + " " + build)
		s := sha256.Sum256(data)
		packages[target] = Package{Name: "lanescape-browser-" + target + ".tar.gz", Bytes: int64(len(data)), SHA256: hex.EncodeToString(s[:])}
		b.put(o.major+"/"+build+"/lanescape-browser-"+target+".tar.gz", data, "")
	}
	if !o.noManifest {
		m := map[string]any{"major": o.major, "build": build, "commit": sha, "packages": packages}
		switch l := o.libchromium.(type) {
		case nil:
			m["libchromium"] = LibchromiumOf(libchromiumPin.Version)
		case string:
		default:
			m["libchromium"] = l
		}
		data, _ := json.Marshal(m)
		b.put(o.major+"/"+build+"/manifest.json", data, "")
	}
	tag := o.tag
	if tag == "" {
		tag = o.major + "." + build
	}
	r.tags = append(r.tags, Tag{tag, sha})
}

// history: main o - a - b, with builds 20261106.1, 20261107.1 and .2;
// support/v1: o - s with build 20261107.3.
func history() (*fakeRepo, *fakeBucket) {
	r, b := newFakeRepo(), newFakeBucket()
	for _, c := range [][2]string{{"o", ""}, {"a", "o"}, {"b", "a"}, {"s", "o"}} {
		r.commit(c[0], c[1], 1, nil)
	}
	r.branches = map[string]string{"main": "b", "support/v1": "s"}
	publish(b, r, "o", "20261106.1", publishOptions{})
	publish(b, r, "a", "20261107.1", publishOptions{})
	publish(b, r, "b", "20261107.2", publishOptions{})
	publish(b, r, "s", "20261107.3", publishOptions{})
	return r, b
}

func noSources(string, string, map[string]string) error { return nil }

func promoter(r *fakeRepo, b Bucket) Promoter {
	return Promoter{Repo: r, Bucket: b, Sources: noSources}
}

// fakeRemote is GitHub's tag refs: creating an existing name fails and
// reports where it points.
type fakeRemote struct {
	mu       sync.Mutex
	tags     []Tag
	support  []string
	created  []string
	onCreate func(name, sha string)
	gate     func(who string) func()
}

// as is the remote as one caller sees it, so the gate knows whose turn.
func (f *fakeRemote) as(who string) Remote { return callerRemote{f, who} }

type callerRemote struct {
	*fakeRemote
	who string
}

func (c callerRemote) MatchingRefs(namespace string) ([]Tag, error) {
	if c.gate != nil {
		defer c.gate(c.who)()
	}
	return c.fakeRemote.MatchingRefs(namespace)
}

func (c callerRemote) CreateTag(name, sha string) (string, error) {
	if c.gate != nil {
		defer c.gate(c.who)()
	}
	return c.fakeRemote.CreateTag(name, sha)
}

func (f *fakeRemote) MatchingRefs(namespace string) ([]Tag, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	var out []Tag
	if namespace == "tags" {
		for _, t := range f.tags {
			out = append(out, Tag{"refs/tags/" + t.Name, t.SHA})
		}
		return out, nil
	}
	for _, major := range f.support {
		out = append(out, Tag{"refs/heads/support/" + major, "f"})
	}
	return out, nil
}

func (f *fakeRemote) CreateTag(name, sha string) (string, error) {
	f.mu.Lock()
	f.created = append(f.created, name)
	on := f.onCreate
	f.mu.Unlock()
	if on != nil {
		on(name, sha)
	}
	f.mu.Lock()
	defer f.mu.Unlock()
	for _, t := range f.tags {
		if t.Name == name {
			return t.SHA, nil
		}
	}
	f.tags = append(f.tags, Tag{name, sha})
	return sha, nil
}

// schedule grants each caller's turns in a fixed order, then in any order.
type schedule struct {
	mu    sync.Mutex
	cond  *sync.Cond
	order []string
	done  map[string]bool
}

func newSchedule(order string) *schedule {
	s := &schedule{done: map[string]bool{}}
	for _, c := range order {
		s.order = append(s.order, string(c))
	}
	s.cond = sync.NewCond(&s.mu)
	return s
}

func (s *schedule) turn(who string) func() {
	s.mu.Lock()
	deadline := time.Now().Add(5 * time.Second)
	for {
		for len(s.order) > 0 && s.done[s.order[0]] {
			s.order = s.order[1:]
		}
		if len(s.order) == 0 || s.order[0] == who {
			break
		}
		if time.Now().After(deadline) {
			s.mu.Unlock()
			panic("schedule stalled")
		}
		s.waitBriefly()
	}
	s.mu.Unlock()
	return func() {
		s.mu.Lock()
		if len(s.order) > 0 {
			s.order = s.order[1:]
		}
		s.cond.Broadcast()
		s.mu.Unlock()
	}
}

// waitBriefly waits for a broadcast, or a moment, with mu held.
func (s *schedule) waitBriefly() {
	t := time.AfterFunc(50*time.Millisecond, func() { s.mu.Lock(); s.cond.Broadcast(); s.mu.Unlock() })
	s.cond.Wait()
	t.Stop()
}

// run runs each job at once, one goroutine each, and returns their results.
func (s *schedule) run(t *testing.T, jobs map[string]func() (string, error)) map[string]string {
	t.Helper()
	var mu sync.Mutex
	results := map[string]string{}
	var wg sync.WaitGroup
	for name, job := range jobs {
		wg.Go(func() {
			result, err := job()
			if err != nil {
				var r *Refused
				if errors.As(err, &r) && strings.Contains(err.Error(), "not newer") {
					result = "not newer"
				} else {
					result = err.Error()
				}
			}
			mu.Lock()
			results[name] = result
			mu.Unlock()
			s.mu.Lock()
			s.done[name] = true
			s.cond.Broadcast()
			s.mu.Unlock()
		})
	}
	wg.Wait()
	return results
}

func boolp(b bool) *bool { return &b }
