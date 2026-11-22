// Package release allocates immutable build tags on main and support
// branches, publishes complete builds, and promotes them forward only
// (specs/0004-releases.md).
package release

import (
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"os/exec"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"time"

	"github.com/lanescape/browser/tools/internal/github"
	"github.com/lanescape/browser/tools/internal/r2"
	"github.com/lanescape/browser/tools/internal/version"
)

const (
	buildForm = `[0-9]{8}\.[1-9][0-9]*`
	// Attempts bounds the compare-and-swap's retries.
	Attempts = 5
	// LibchromiumReleases is where libchromium's releases are.
	LibchromiumReleases = "https://github.com/lanescape/libchromium/releases/tag"
)

var (
	// Targets are every target a published build has.
	Targets  = []string{"linux-amd64", "linux-arm64", "macos-arm64"}
	buildRe  = regexp.MustCompile(`^` + buildForm + `$`)
	tagRe    = regexp.MustCompile(`^(v[1-9][0-9]*)\.(` + buildForm + `)$`)
	branchRe = regexp.MustCompile(`^refs/heads/(main|support/v[1-9][0-9]*)$`)
	shaRe    = regexp.MustCompile(`^[0-9a-f]{40}$`)
)

// Refused is a promotion that would not move a major strictly forward to a
// complete build. Superseded means latest already names this build's commit
// or a later one: expected when an older build finishes last.
type Refused struct {
	Msg        string
	Superseded bool
}

func (r *Refused) Error() string { return r.Msg }

func refused(format string, args ...any) error { return &Refused{Msg: fmt.Sprintf(format, args...)} }

// Tag is a tag's name and the commit it names.
type Tag struct{ Name, SHA string }

// ParseTag is a build tag's major and build number.
func ParseTag(name string) (major, build string, ok bool) {
	m := tagRe.FindStringSubmatch(name)
	if m == nil {
		return "", "", false
	}
	return m[1], m[2], true
}

func buildKey(build string) (int, int) {
	day, number, _ := strings.Cut(build, ".")
	d, _ := strconv.Atoi(day)
	n, _ := strconv.Atoi(number)
	return d, n
}

func later(a, b string) bool {
	ad, an := buildKey(a)
	bd, bn := buildKey(b)
	return ad > bd || (ad == bd && an > bn)
}

// Holder is a commit with a build tag for a number, and the tag's major.
type Holder struct{ SHA, Major string }

// Holders are the commits, with their tags' majors, tagged for build.
func Holders(tags []Tag, build string) []Holder {
	seen := map[Holder]bool{}
	var out []Holder
	for _, t := range tags {
		if major, b, ok := ParseTag(t.Name); ok && b == build {
			h := Holder{t.SHA, major}
			if !seen[h] {
				seen[h] = true
				out = append(out, h)
			}
		}
	}
	sort.Slice(out, func(i, j int) bool {
		if out[i].SHA != out[j].SHA {
			return out[i].SHA < out[j].SHA
		}
		return out[i].Major < out[j].Major
	})
	return out
}

// ownBuild is sha's highest build number, if no other commit has a tag for
// that number.
func ownBuild(tags []Tag, sha string) string {
	best := ""
	for _, t := range tags {
		if _, b, ok := ParseTag(t.Name); ok && t.SHA == sha && (best == "" || later(b, best)) {
			best = b
		}
	}
	if best == "" {
		return ""
	}
	for _, h := range Holders(tags, best) {
		if h.SHA != sha {
			return ""
		}
	}
	return best
}

// Remote is GitHub's refs, as reservation reads and writes them.
type Remote interface {
	// MatchingRefs lists refs under a namespace, such as tags or
	// heads/support.
	MatchingRefs(namespace string) ([]Tag, error)
	// CreateTag creates a tag, and returns the commit it names: another one
	// if it existed already.
	CreateTag(name, sha string) (string, error)
}

// GitHubRemote is Remote on GitHub's API.
type GitHubRemote struct{ API *github.Client }

type ref struct {
	Ref    string `json:"ref"`
	Object struct {
		SHA string `json:"sha"`
	} `json:"object"`
}

// MatchingRefs lists refs under a namespace.
func (g GitHubRemote) MatchingRefs(namespace string) ([]Tag, error) {
	repo, err := g.API.Repository()
	if err != nil {
		return nil, err
	}
	refs, err := github.Paginate[ref](g.API, "repos/"+repo+"/git/matching-refs/"+namespace+"/")
	if err != nil {
		return nil, err
	}
	out := make([]Tag, 0, len(refs))
	for _, r := range refs {
		out = append(out, Tag{Name: r.Ref, SHA: r.Object.SHA})
	}
	return out, nil
}

// CreateTag creates a tag through the API.
func (g GitHubRemote) CreateTag(name, sha string) (string, error) {
	repo, err := g.API.Repository()
	if err != nil {
		return "", err
	}
	_, err = g.API.Request(http.MethodPost, "repos/"+repo+"/git/refs", map[string]string{"ref": "refs/tags/" + name, "sha": sha}, nil)
	if err == nil {
		return sha, nil
	}
	// GitHub refuses a ref that exists with 422.
	var e *github.Error
	if !errors.As(err, &e) || e.Status != 422 {
		return "", err
	}
	var existing ref
	if _, err := g.API.Request(http.MethodGet, "repos/"+repo+"/git/ref/tags/"+name, nil, &existing); err != nil {
		return "", errors.New("tag reservation failed without a collision")
	}
	return existing.Object.SHA, nil
}

func remoteTags(r Remote) ([]Tag, error) {
	refs, err := r.MatchingRefs("tags")
	if err != nil {
		return nil, err
	}
	for i := range refs {
		refs[i].Name = strings.TrimPrefix(refs[i].Name, "refs/tags/")
	}
	return refs, nil
}

// Today is the UTC day, YYYYMMDD.
func Today() string { return time.Now().UTC().Format("20060102") }

// Reserve tags sha v<major>.YYYYMMDD.N, on gitRef, a main or support/vN push,
// and returns YYYYMMDD.N, which is unique across all majors.
func Reserve(r Remote, gitRef, major, today, sha string) (string, error) {
	branch := branchRe.FindStringSubmatch(gitRef)
	if branch == nil {
		return "", errors.New("build numbers only exist on main and support/vN")
	}
	if branch[1] == "main" {
		// A major moves to a support branch only after main advances its
		// package major.
		support, err := r.MatchingRefs("heads/support")
		if err != nil {
			return "", err
		}
		for _, s := range support {
			if s.Name == "refs/heads/support/"+major {
				return "", fmt.Errorf("main must advance its package major beyond support/%s", major)
			}
		}
	} else if branch[1] != "support/"+major {
		return "", errors.New("support branch does not match package major")
	}
	number := 0
	for range 100 {
		// Another major can take the same number at once under its own tag
		// name, and protected tags are never deleted. A number is kept only
		// if, listed after this commit's tag for it exists, no other commit
		// has a tag for it. Two commits cannot both keep one: whichever
		// lists later sees the other's tag. A contested number is given up
		// for the next, even by a rerun of a reservation that kept it
		// before.
		tags, err := remoteTags(r)
		if err != nil {
			return "", err
		}
		if b := ownBuild(tags, sha); b != "" {
			return b, nil
		}
		for _, t := range tags {
			if _, b, ok := ParseTag(t.Name); ok && strings.HasPrefix(b, today+".") {
				if _, n := buildKey(b); n > number {
					number = n
				}
			}
		}
		number++
		if _, err := r.CreateTag(fmt.Sprintf("%s.%s.%d", major, today, number), sha); err != nil {
			return "", err
		}
	}
	return "", errors.New("too many tag collisions")
}

// Repo is the local clone; promotion needs full history and origin's
// branches.
type Repo interface {
	Show(sha, path string) (string, error)
	BranchContains(branch, sha string) (bool, error)
	IsAncestor(ancestor, descendant string) (bool, error)
	BuildTags() ([]Tag, error)
}

// GitRepo is Repo on git, in Dir.
type GitRepo struct{ Dir string }

func (g GitRepo) git(args ...string) *exec.Cmd {
	cmd := exec.Command("git", args...)
	cmd.Dir = g.Dir
	return cmd
}

// Show is a file at a commit.
func (g GitRepo) Show(sha, path string) (string, error) {
	out, err := g.git("show", sha+":"+path).Output()
	if err != nil {
		return "", fmt.Errorf("git show %s:%s: %w", sha, path, err)
	}
	return string(out), nil
}

// BranchContains is whether origin's branch holds sha.
func (g GitRepo) BranchContains(branch, sha string) (bool, error) {
	ref := "refs/remotes/origin/" + branch
	if g.git("rev-parse", "--verify", "--quiet", ref).Run() != nil {
		return false, nil
	}
	return g.IsAncestor(sha, ref)
}

// fetchIfMissing: latest can name a commit pushed after this checkout was
// made, as when a newer build is promoted while an older one publishes;
// fetch it so the comparison can still be made.
func (g GitRepo) fetchIfMissing(rev string) error {
	if !shaRe.MatchString(rev) {
		return nil
	}
	if g.git("cat-file", "-e", rev+"^{commit}").Run() == nil {
		return nil
	}
	if out, err := g.git("fetch", "--quiet", "origin", rev).CombinedOutput(); err != nil {
		return fmt.Errorf("git fetch %s: %w: %s", rev, err, out)
	}
	return nil
}

// IsAncestor is whether ancestor is an ancestor of descendant.
func (g GitRepo) IsAncestor(ancestor, descendant string) (bool, error) {
	for _, rev := range []string{ancestor, descendant} {
		if err := g.fetchIfMissing(rev); err != nil {
			return false, err
		}
	}
	err := g.git("merge-base", "--is-ancestor", ancestor, descendant).Run()
	if err == nil {
		return true, nil
	}
	var exit *exec.ExitError
	if errors.As(err, &exit) && exit.ExitCode() == 1 {
		return false, nil
	}
	return false, fmt.Errorf("cannot compare %s with %s", ancestor, descendant)
}

// BuildTags are the clone's tags and the commits they name.
func (g GitRepo) BuildTags() ([]Tag, error) {
	out, err := g.git("for-each-ref", "--format=%(refname:strip=2) %(objectname) %(*objectname)", "refs/tags").Output()
	if err != nil {
		return nil, err
	}
	var tags []Tag
	for _, line := range strings.Split(strings.TrimSpace(string(out)), "\n") {
		fields := strings.Fields(line)
		if len(fields) >= 2 {
			tags = append(tags, Tag{Name: fields[0], SHA: fields[len(fields)-1]})
		}
	}
	return tags, nil
}

// Bucket is what promotion reads and writes of the release bucket.
type Bucket interface {
	// Read is an object's text; found is false if there's none.
	Read(key string) (text string, found bool, err error)
	// Digest is an object's size and SHA-256; found is false if there's
	// none.
	Digest(key string) (size int64, sum string, found bool, err error)
	// ReadLatest is <major>/latest and the ETag it was read at, from one
	// request; found is false if there's none.
	ReadLatest(major string) (text, etag string, found bool, err error)
	// WriteLatest replaces <major>/latest only if it's still at etag, or,
	// when exists is false, makes it only if there's none. It's false on a
	// failed precondition. It's never unconditional.
	WriteLatest(major, build, etag string, exists bool) (bool, error)
}

// R2Bucket is Bucket on the release bucket.
type R2Bucket struct{ R2 *r2.Client }

// Read is an object's text.
func (b R2Bucket) Read(key string) (string, bool, error) {
	data, _, found, err := b.R2.Get(key)
	return string(data), found, err
}

// Digest is an object's size and SHA-256.
func (b R2Bucket) Digest(key string) (int64, string, bool, error) { return b.R2.Digest(key) }

// ReadLatest is <major>/latest and its ETag.
func (b R2Bucket) ReadLatest(major string) (string, string, bool, error) {
	data, etag, found, err := b.R2.Get(major + "/latest")
	if err != nil || !found {
		return "", "", false, err
	}
	if etag == "" {
		return "", "", false, fmt.Errorf("R2 gave %s/latest without an ETag; nothing can be swapped for it", major)
	}
	return string(data), etag, true, nil
}

// WriteLatest swaps <major>/latest.
func (b R2Bucket) WriteLatest(major, build, etag string, exists bool) (bool, error) {
	if exists && etag == "" {
		return false, errors.New("an empty ETag")
	}
	o := r2.PutOptions{IfNoneMatch: !exists}
	if exists {
		o.IfMatch = etag
	}
	_, err := b.R2.Put(major+"/latest", []byte(build+"\n"), "text/plain; charset=utf-8", r2.NoStore, o)
	if errors.Is(err, r2.ErrPreconditionFailed) {
		return false, nil
	}
	return err == nil, err
}

type candidate struct{ SHA, Major string }

// candidates are the commits and package majors tagged with a build number.
func candidates(build string, repo Repo) ([]candidate, error) {
	if !buildRe.MatchString(build) {
		return nil, errors.New("invalid build number")
	}
	tags, err := repo.BuildTags()
	if err != nil {
		return nil, err
	}
	var found []candidate
	for _, h := range Holders(tags, build) {
		cargo, err := repo.Show(h.SHA, "Cargo.toml")
		if err != nil {
			return nil, err
		}
		major, err := version.ReleaseMajor(cargo)
		if err != nil {
			return nil, err
		}
		if h.Major != major {
			return nil, refused("build tag %s.%s does not match package major %s", h.Major, build, major)
		}
		c := candidate{h.SHA, major}
		dup := false
		for _, f := range found {
			dup = dup || f == c
		}
		if !dup {
			found = append(found, c)
		}
	}
	return found, nil
}

// PromotionGroup is the major(s) a build number is tagged under: the
// Promote build workflow checks, before anyone approves, that the number
// names a build. Normally one major; a contested number can name two, and
// promotion settles the commit from the bucket.
func PromotionGroup(build string, repo Repo) (string, error) {
	found, err := candidates(build, repo)
	if err != nil {
		return "", err
	}
	set := map[string]bool{}
	for _, c := range found {
		set[c.Major] = true
	}
	if len(set) == 0 {
		return "", refused("no build tag for %s", build)
	}
	majors := make([]string, 0, len(set))
	for m := range set {
		majors = append(majors, m)
	}
	sort.Strings(majors)
	return strings.Join(majors, ","), nil
}

// Identify is the commit and package major of a build number. A number
// contested in a reservation race is tagged on more than one commit; only
// the commit that kept it was published under it, so with a bucket the
// published manifest settles it.
func Identify(build string, repo Repo, bucket Bucket) (sha, major string, err error) {
	found, err := candidates(build, repo)
	if err != nil {
		return "", "", err
	}
	if len(found) > 1 && bucket != nil {
		var kept []candidate
		for _, c := range found {
			text, ok, err := bucket.Read(c.Major + "/" + build + "/manifest.json")
			if err != nil {
				return "", "", err
			}
			var m struct {
				Commit string `json:"commit"`
			}
			if ok {
				json.Unmarshal([]byte(text), &m)
			}
			if m.Commit == c.SHA {
				kept = append(kept, c)
			}
		}
		found = kept
	}
	if len(found) != 1 {
		return "", "", refused("expected exactly one commit tagged with build number %s, found %d", build, len(found))
	}
	return found[0].SHA, found[0].Major, nil
}

// Package is a manifest's package.
type Package struct {
	Name   string `json:"name"`
	Bytes  int64  `json:"bytes"`
	SHA256 string `json:"sha256"`
}

// Libchromium is what a manifest records of the libchromium version a build
// was made with, whose GitHub release publishes Chromium's corresponding
// source (third_party/README.md).
type Libchromium struct {
	Version string `json:"version"`
	Release string `json:"release"`
}

// LibchromiumOf is a version's record.
func LibchromiumOf(version string) Libchromium {
	return Libchromium{Version: version, Release: LibchromiumReleases + "/v" + version}
}

// Manifest is a build's manifest.
type Manifest struct {
	Major       string             `json:"major"`
	Build       string             `json:"build"`
	Commit      string             `json:"commit"`
	Packages    map[string]Package `json:"packages"`
	Libchromium *Libchromium       `json:"libchromium,omitempty"`
}

func readManifest(build, major string, bucket Bucket) (Manifest, error) {
	text, found, err := bucket.Read(major + "/" + build + "/manifest.json")
	if err != nil {
		return Manifest{}, err
	}
	if !found {
		return Manifest{}, refused("%s/%s has no manifest; its publication is not complete", major, build)
	}
	var m Manifest
	if err := json.Unmarshal([]byte(text), &m); err != nil {
		return Manifest{}, refused("%s/%s's manifest is not readable", major, build)
	}
	return m, nil
}

// Sources checks that a libchromium version's release publishes its
// corresponding source (CheckSources).
type Sources func(version, manifestSHA256 string, pinnedAssets map[string]string) error

// Promoter promotes builds.
type Promoter struct {
	Repo    Repo
	Bucket  Bucket
	Sources Sources
}

// verify refuses a build that is not complete, intact and published from
// its major's branch.
func (p Promoter) verify(build, sha, major string) error {
	on := false
	for _, branch := range []string{"main", "support/" + major} {
		ok, err := p.Repo.BranchContains(branch, sha)
		if err != nil {
			return err
		}
		on = on || ok
	}
	if !on {
		return refused("%s is not on main or support/%s", build, major)
	}
	m, err := readManifest(build, major, p.Bucket)
	if err != nil {
		return err
	}
	if m.Commit != sha || m.Build != build || m.Major != major {
		return refused("manifest identity mismatch")
	}
	if len(m.Packages) != len(Targets) {
		return refused("incomplete platform set")
	}
	for _, target := range Targets {
		pkg, ok := m.Packages[target]
		if !ok {
			return refused("incomplete platform set")
		}
		if pkg.Name != "lanescape-browser-"+target+".tar.gz" {
			return refused("package target mismatch")
		}
		size, sum, found, err := p.Bucket.Digest(major + "/" + build + "/" + pkg.Name)
		if err != nil {
			return err
		}
		if !found || size != pkg.Bytes || sum != pkg.SHA256 {
			return refused("package integrity mismatch")
		}
	}
	// The corresponding source is published with the libchromium version the
	// build was made with.
	text, err := p.Repo.Show(sha, "engine/libchromium.json")
	if err != nil {
		return err
	}
	var pin struct {
		Version  string            `json:"version"`
		Manifest string            `json:"manifest"`
		Assets   map[string]string `json:"assets"`
	}
	if err := json.Unmarshal([]byte(text), &pin); err != nil {
		return err
	}
	if m.Libchromium == nil || *m.Libchromium != LibchromiumOf(pin.Version) {
		return refused("corresponding source mismatch: the manifest must name the pinned libchromium release")
	}
	return p.Sources(pin.Version, pin.Manifest, pin.Assets)
}

// Promote points <major>/latest at build if that moves it strictly forward;
// every promotion goes here. It returns "promoted", or "current" if latest
// already names build. The write is a compare-and-swap on the ETag read, so
// concurrent promotions cannot move latest backwards in any order.
func (p Promoter) Promote(build string) (string, error) {
	sha, major, err := Identify(build, p.Repo, p.Bucket)
	if err != nil {
		return "", err
	}
	// Published objects are immutable, so this holds for every attempt.
	if err := p.verify(build, sha, major); err != nil {
		return "", err
	}
	for range Attempts {
		text, etag, exists, err := p.Bucket.ReadLatest(major)
		if err != nil {
			return "", err
		}
		if exists {
			current := strings.TrimSpace(text)
			if current == build {
				return "current", nil
			}
			if !buildRe.MatchString(current) {
				return "", refused("%s/latest does not hold a build number", major)
			}
			m, err := readManifest(current, major, p.Bucket)
			if err != nil {
				return "", err
			}
			if m.Commit == "" {
				return "", refused("%s/latest names %s, whose manifest has no commit", major, current)
			}
			newer := m.Commit == sha
			if !newer {
				if newer, err = p.Repo.IsAncestor(sha, m.Commit); err != nil {
					return "", err
				}
			}
			if newer {
				return "", &Refused{Msg: fmt.Sprintf("%s is not newer than %s/latest %s", build, major, current), Superseded: true}
			}
			descends, err := p.Repo.IsAncestor(m.Commit, sha)
			if err != nil {
				return "", err
			}
			if !descends {
				return "", refused("%s does not descend from %s/latest %s", build, major, current)
			}
		}
		ok, err := p.Bucket.WriteLatest(major, build, etag, exists)
		if err != nil {
			return "", err
		}
		if ok {
			return "promoted", nil
		}
		// Another promotion replaced latest after it was read: check again
		// against the new value.
	}
	return "", fmt.Errorf("%s/latest kept changing; promotion of %s gave up", major, build)
}

// PromoteIfUnstable promotes build only if its major is flagged unstable at
// its own commit; "stable" if it isn't.
func (p Promoter) PromoteIfUnstable(build string) (string, error) {
	sha, _, err := Identify(build, p.Repo, p.Bucket)
	if err != nil {
		return "", err
	}
	cargo, err := p.Repo.Show(sha, "Cargo.toml")
	if err != nil {
		return "", err
	}
	unstable, err := version.Unstable(cargo)
	if err != nil {
		return "", err
	}
	if !unstable {
		return "stable", nil
	}
	return p.Promote(build)
}
