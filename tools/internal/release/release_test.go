package release

import (
	"errors"
	"maps"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

func refusedWith(t *testing.T, err error, want string) *Refused {
	t.Helper()
	var r *Refused
	if !errors.As(err, &r) || !strings.Contains(err.Error(), want) {
		t.Fatalf("err = %v, want a refusal mentioning %q", err, want)
	}
	return r
}

// --- Build tags ---

func TestBuildTagsCarryTheirMajor(t *testing.T) {
	for name, want := range map[string][2]string{"v1.20261107.1": {"v1", "20261107.1"}, "v12.20261107.30": {"v12", "20261107.30"}} {
		major, build, ok := ParseTag(name)
		if !ok || major != want[0] || build != want[1] {
			t.Errorf("ParseTag(%q) = %s %s %v", name, major, build, ok)
		}
	}
	for _, name := range []string{"20261107.1", "v0.20261107.1", "v1", "v1.20261107.0", "v1.2026110.1", "v01.20261107.1"} {
		if _, _, ok := ParseTag(name); ok {
			t.Errorf("ParseTag(%q) accepted", name)
		}
	}
}

func TestBuildLookupFindsTheTaggedCommit(t *testing.T) {
	r, _ := history()
	if sha, major, err := Identify("20261106.1", r, nil); sha != "o" || major != "v1" || err != nil {
		t.Fatalf("%s %s %v", sha, major, err)
	}
	if sha, _, _ := Identify("20261107.2", r, nil); sha != "b" {
		t.Fatal(sha)
	}
	_, _, err := Identify("20261107.9", r, nil)
	refusedWith(t, err, "exactly one")
	// A tag without a major names no build.
	r.tags = append(r.tags, Tag{"20261107.8", "b"})
	_, _, err = Identify("20261107.8", r, nil)
	refusedWith(t, err, "exactly one")
	for _, build := range []string{"2026110.1", "20261102.0", "v1", "v1.20261107.1", "20261102.1/../x", ""} {
		if _, _, err := Identify(build, r, nil); err == nil || !strings.Contains(err.Error(), "build number") {
			t.Errorf("Identify(%q) = %v", build, err)
		}
	}
}

func TestBuildTagMajorMustMatchPackageMajor(t *testing.T) {
	r := newFakeRepo()
	r.commit("abc", "", 1, nil)
	r.tags = []Tag{{"v2.20261107.1", "abc"}}
	_, _, err := Identify("20261107.1", r, nil)
	refusedWith(t, err, "does not match package major v1")
	r.commit("def", "", 2, nil)
	r.tags = []Tag{{"v2.20261107.2", "def"}}
	if sha, major, err := Identify("20261107.2", r, nil); sha != "def" || major != "v2" || err != nil {
		t.Fatalf("%s %s %v", sha, major, err)
	}
}

func TestAContestedNumberBelongsToThePublishedCommit(t *testing.T) {
	// x was published as 20261107.1; y's reservation created v2.20261107.1
	// from a stale listing and was cancelled before moving on.
	r, b := newFakeRepo(), newFakeBucket()
	r.commit("x", "", 1, nil)
	r.commit("y", "x", 2, nil)
	r.branches = map[string]string{"main": "y", "support/v1": "x"}
	publish(b, r, "x", "20261107.1", publishOptions{})
	r.tags = append(r.tags, Tag{"v2.20261107.1", "y"})
	_, _, err := Identify("20261107.1", r, nil)
	refusedWith(t, err, "found 2")
	if sha, major, err := Identify("20261107.1", r, b); sha != "x" || major != "v1" || err != nil {
		t.Fatalf("%s %s %v", sha, major, err)
	}
	// The Promote build workflow's check has no bucket and still lets
	// promotion run.
	if g, err := PromotionGroup("20261107.1", r); g != "v1,v2" || err != nil {
		t.Fatalf("%s %v", g, err)
	}
	if got, err := promoter(r, b).Promote("20261107.1"); got != "promoted" || err != nil {
		t.Fatalf("%s %v", got, err)
	}
	_, err = PromotionGroup("20261107.9", r)
	refusedWith(t, err, "no build tag")
	// A manifest there but unreadable resolves nothing.
	for _, bad := range []string{"{not json", "null", "{}"} {
		b.put("v2/20261107.1/manifest.json", []byte(bad), "")
		_, _, err = Identify("20261107.1", r, b)
		refusedWith(t, err, "v2/20261107.1's manifest is not readable")
	}
}

// latest's manifest must name the commit latest's build was tagged for:
// one naming an older commit can't make an older build look newer.
func TestLatestsManifestMustNameItsTaggedCommit(t *testing.T) {
	r, b := history()
	promoter(r, b).Promote("20261107.2")
	b.put("v1/20261107.2/manifest.json", []byte(`{"major": "v1", "build": "20261107.2", "commit": "a"}`), "")
	_, err := promoter(r, b).Promote("20261107.1")
	refusedWith(t, err, "not the one tagged v1.20261107.2")
}

// latest's manifest must be its build's: null, or another build's, isn't.
func TestLatestsManifestMustBeItsOwn(t *testing.T) {
	for _, bad := range []string{"null", `{"major": "v1", "build": "20261107.9", "commit": "a"}`} {
		r, b := history()
		promoter(r, b).Promote("20261107.1")
		b.put("v1/20261107.1/manifest.json", []byte(bad), "")
		_, err := promoter(r, b).Promote("20261107.2")
		refusedWith(t, err, "manifest is not readable")
	}
}

// Git compares commits only: a manifest naming a branch is refused.
func TestGitComparesOnlyCommits(t *testing.T) {
	g := GitRepo{Dir: t.TempDir()}
	sha := strings.Repeat("a", 40)
	for _, pair := range [][2]string{{"main", sha}, {sha, "refs/heads/main"}, {"HEAD", sha}} {
		if _, err := g.IsAncestor(pair[0], pair[1]); err == nil || !strings.Contains(err.Error(), "not a commit") {
			t.Errorf("IsAncestor(%s, %s): %v", pair[0], pair[1], err)
		}
	}
	if _, err := g.BranchContains("main", "main"); err == nil {
		t.Error("BranchContains took a branch for a commit")
	}
}

// --- Reservation ---

func TestReservationReusesTheExactCommitsTag(t *testing.T) {
	f := &fakeRemote{tags: []Tag{{"v1.20261031.1", "abc"}}}
	if b, err := Reserve(f, "refs/heads/main", "v1", "20261107", "abc"); b != "20261031.1" || err != nil {
		t.Fatalf("%s %v", b, err)
	}
	if len(f.created) != 0 {
		t.Fatal(f.created)
	}
}

// A sequence past any machine integer still advances, and orders by value.
func TestSequencesHaveNoLimit(t *testing.T) {
	f := &fakeRemote{tags: []Tag{{"v1.20261107.9223372036854775807", "one"}, {"v1.20261107.99", "two"}}}
	if b, err := Reserve(f, "refs/heads/main", "v1", "20261107", "abc"); b != "20261107.9223372036854775808" || err != nil {
		t.Fatalf("%s %v", b, err)
	}
	if !later("20261107.10", "20261107.9") || later("20261107.9", "20261107.10") || !later("20261108.1", "20261107.99999999999999999999") {
		t.Fatal("builds don't order by value")
	}
}

func TestReservationCountsEveryMajorsTagsAndCreatesAMajorTag(t *testing.T) {
	// Tags that aren't build tags, such as one without a major, count for
	// nothing.
	f := &fakeRemote{tags: []Tag{{"v1.20261107.2", "one"}, {"v2.20261107.4", "other"}, {"v1.20261106.9", "yesterday"},
		{"20261107.7", "unrelated"}, {"20261107.1", "abc"}, {"v1", "unrelated"}}}
	if b, err := Reserve(f, "refs/heads/main", "v1", "20261107", "abc"); b != "20261107.5" || err != nil {
		t.Fatalf("%s %v", b, err)
	}
	if !slices.Equal(f.created, []string{"v1.20261107.5"}) {
		t.Fatal(f.created)
	}
}

func TestSameMajorCollisionTakesTheNextNumber(t *testing.T) {
	f := &fakeRemote{}
	f.onCreate = func(name, sha string) {
		f.onCreate = nil
		f.mu.Lock()
		f.tags = append(f.tags, Tag{name, "other"})
		f.mu.Unlock()
	}
	if b, err := Reserve(f, "refs/heads/main", "v1", "20261107", "abc"); b != "20261107.2" || err != nil {
		t.Fatalf("%s %v", b, err)
	}
	if !slices.Equal(f.created, []string{"v1.20261107.1", "v1.20261107.2"}) {
		t.Fatal(f.created)
	}
}

func TestCrossMajorRaceKeepsTheNumberUnique(t *testing.T) {
	// main (v2) takes the same number at the same moment under its own tag.
	f := &fakeRemote{}
	f.onCreate = func(name, sha string) {
		f.onCreate = nil
		f.mu.Lock()
		f.tags = append(f.tags, Tag{"v2.20261107.1", "main"})
		f.mu.Unlock()
	}
	if b, err := Reserve(f, "refs/heads/support/v1", "v1", "20261107", "abc"); b != "20261107.2" || err != nil {
		t.Fatalf("%s %v", b, err)
	}
	if !slices.Equal(f.created, []string{"v1.20261107.1", "v1.20261107.2"}) {
		t.Fatal(f.created)
	}
}

func TestAStaleReservationReturnsTheNumberItsCommitAlreadyKept(t *testing.T) {
	// While this run collides on .1, another run for the same commit keeps .2.
	f := &fakeRemote{}
	f.onCreate = func(name, sha string) {
		f.onCreate = nil
		f.mu.Lock()
		f.tags = append(f.tags, Tag{name, "other"}, Tag{"v1.20261107.2", "abc"})
		f.mu.Unlock()
	}
	if b, err := Reserve(f, "refs/heads/main", "v1", "20261107", "abc"); b != "20261107.2" || err != nil {
		t.Fatalf("%s %v", b, err)
	}
	if !slices.Equal(f.created, []string{"v1.20261107.1"}) {
		t.Fatal(f.created)
	}
}

func TestRerunsGiveUpAContestedNumber(t *testing.T) {
	// x kept .1; y created v2.20261107.1 from a stale listing and was
	// cancelled. Reruns of either never fail and never reuse .1.
	f := &fakeRemote{tags: []Tag{{"v1.20261107.1", "x"}, {"v2.20261107.1", "y"}}}
	for _, step := range []struct{ major, sha, want string }{{"v1", "x", "20261107.2"}, {"v2", "y", "20261107.3"}, {"v1", "x", "20261107.2"}} {
		if b, err := Reserve(f, "refs/heads/main", step.major, "20261107", step.sha); b != step.want || err != nil {
			t.Fatalf("%s: %s %v", step.sha, b, err)
		}
	}
	if !slices.Equal(f.created, []string{"v1.20261107.2", "v2.20261107.3"}) {
		t.Fatal(f.created)
	}
}

// x (v1) and y (v2) interleave their listings and tag creations in every
// order.
func TestConcurrentReservationsOfTwoMajorsNeverShareANumber(t *testing.T) {
	majors := map[string]string{"x": "v1", "y": "v2"}
	for _, order := range orders("xy", 6) {
		f := &fakeRemote{}
		s := newSchedule(order)
		f.gate = s.turn
		results := s.run(t, map[string]func() (string, error){
			"x": func() (string, error) { return Reserve(f.as("x"), "refs/heads/main", "v1", "20261107", "x") },
			"y": func() (string, error) { return Reserve(f.as("y"), "refs/heads/main", "v2", "20261107", "y") },
		})
		// The commit that gave a number up may keep its tag for it, but never
		// returns it.
		if results["x"] == results["y"] {
			t.Fatalf("%s: both got %s", order, results["x"])
		}
		for sha, build := range results {
			if !slices.Contains(Holders(f.tags, build), Holder{sha, majors[sha]}) {
				t.Fatalf("%s: %s got %s without holding it", order, sha, build)
			}
		}
	}
}

// orders is every string of n letters from alphabet.
func orders(alphabet string, n int) []string {
	out := []string{""}
	for range n {
		var next []string
		for _, o := range out {
			for _, c := range alphabet {
				next = append(next, o+string(c))
			}
		}
		out = next
	}
	return out
}

func TestOnlyMainAndSupportBranchesReserve(t *testing.T) {
	if _, err := Reserve(&fakeRemote{}, "refs/pull/1/merge", "v1", "20261107", "abc"); err == nil {
		t.Fatal("a pull request reserved a number")
	}
	f := &fakeRemote{tags: []Tag{{"v1.20261031.1", "abc"}}}
	if b, err := Reserve(f, "refs/heads/support/v1", "v1", "20261107", "abc"); b != "20261031.1" || err != nil {
		t.Fatalf("%s %v", b, err)
	}
	for _, ref := range []string{"refs/heads/support/v2", "refs/heads/support/v1/x", "refs/heads/support/v01", "refs/heads/feature"} {
		if _, err := Reserve(f, ref, "v1", "20261107", "abc"); err == nil {
			t.Errorf("%s reserved a number", ref)
		}
	}
}

func TestMainMustAdvanceBeyondSupportMajors(t *testing.T) {
	f := &fakeRemote{tags: []Tag{{"v2.20261031.1", "abc"}}, support: []string{"v1"}}
	if _, err := Reserve(f, "refs/heads/main", "v1", "20261107", "abc"); err == nil || !strings.Contains(err.Error(), "advance") {
		t.Fatalf("err = %v", err)
	}
	if b, err := Reserve(f, "refs/heads/main", "v2", "20261107", "abc"); b != "20261031.1" || err != nil {
		t.Fatalf("%s %v", b, err)
	}
}

// --- Forward-only promotion ---

func TestFirstPromotionCreatesLatest(t *testing.T) {
	r, b := history()
	if got, err := promoter(r, b).Promote("20261106.1"); got != "promoted" || err != nil {
		t.Fatalf("%s %v", got, err)
	}
	if b.latest("v1") != "20261106.1\n" || b.objects["v1/latest"].cacheControl != "no-store" {
		t.Fatal(b.latest("v1"))
	}
}

func TestPromotionMovesToDescendants(t *testing.T) {
	r, b := history()
	for _, build := range []string{"20261106.1", "20261107.1", "20261107.2"} {
		if got, err := promoter(r, b).Promote(build); got != "promoted" || err != nil {
			t.Fatalf("%s: %s %v", build, got, err)
		}
	}
	if !slices.Equal(b.writes, []string{"20261106.1", "20261107.1", "20261107.2"}) {
		t.Fatal(b.writes)
	}
	// A newer build may skip builds in between.
	r, b = history()
	promoter(r, b).Promote("20261106.1")
	if got, err := promoter(r, b).Promote("20261107.2"); got != "promoted" || err != nil {
		t.Fatalf("%s %v", got, err)
	}
}

func TestPromotingLatestAgainChangesNothing(t *testing.T) {
	r, b := history()
	promoter(r, b).Promote("20261107.1")
	etag := b.objects["v1/latest"].etag
	if got, err := promoter(r, b).Promote("20261107.1"); got != "current" || err != nil {
		t.Fatalf("%s %v", got, err)
	}
	if b.objects["v1/latest"].etag != etag {
		t.Fatal("latest was written again")
	}
}

func TestOlderAndSiblingBuildsAreRefused(t *testing.T) {
	for _, tc := range []struct{ latest, build, reason string }{
		{"20261107.2", "20261107.1", "not newer"},
		{"20261107.2", "20261106.1", "not newer"},
		{"20261107.1", "20261107.3", "does not descend"},
		{"20261107.3", "20261107.2", "does not descend"},
	} {
		r, b := history()
		promoter(r, b).Promote(tc.latest)
		_, err := promoter(r, b).Promote(tc.build)
		refused := refusedWith(t, err, tc.reason)
		if b.latest("v1") != tc.latest+"\n" {
			t.Fatalf("latest moved to %s", b.latest("v1"))
		}
		// Only an older build is "superseded", which automatic promotion
		// treats as nothing to do; a build off the line of latest is always
		// an error.
		if refused.Superseded != (tc.reason == "not newer") {
			t.Fatalf("%s over %s: superseded %v", tc.build, tc.latest, refused.Superseded)
		}
	}
}

func TestAnotherBuildOfTheLatestCommitIsRefused(t *testing.T) {
	r, b := history()
	promoter(r, b).Promote("20261107.2")
	// b holds both tags; latest's must still be there, or latest's
	// manifest couldn't be trusted to name b.
	publish(b, r, "b", "20261107.5", publishOptions{})
	_, err := promoter(r, b).Promote("20261107.5")
	refusedWith(t, err, "not newer")
}

func TestIncompleteOrCorruptBuildsAreRefused(t *testing.T) {
	for _, tc := range []struct {
		name   string
		sha    string
		o      publishOptions
		reason string
	}{
		{"no manifest", "c", publishOptions{noManifest: true}, "no manifest"},
		{"missing platform", "c", publishOptions{targets: Targets[:2]}, "platform"},
		{"other commit", "a", publishOptions{}, "identity"},
	} {
		r, b := history()
		r.commit("c", "b", 1, nil)
		r.branches["main"] = "c"
		publish(b, r, tc.sha, "20261107.9", tc.o)
		r.tags[len(r.tags)-1] = Tag{"v1.20261107.9", "c"}
		_, err := promoter(r, b).Promote("20261107.9")
		refusedWith(t, err, tc.reason)
		if len(b.writes) != 0 {
			t.Fatalf("%s: wrote %v", tc.name, b.writes)
		}
	}
	r, b := history()
	b.objects["v1/20261107.2/lanescape-browser-linux-arm64.tar.gz"].data = append(b.objects["v1/20261107.2/lanescape-browser-linux-arm64.tar.gz"].data, '!')
	_, err := promoter(r, b).Promote("20261107.2")
	refusedWith(t, err, "integrity")
	r, b = history()
	delete(b.objects, "v1/20261107.2/lanescape-browser-macos-arm64.tar.gz")
	_, err = promoter(r, b).Promote("20261107.2")
	refusedWith(t, err, "integrity")
	// The manifest must name the libchromium release the commit pins, whose
	// release has the corresponding source.
	for _, lib := range []any{"none", LibchromiumOf("156.0.1.1-1"), Libchromium{Version: libchromiumPin.Version, Release: "https://example.com/elsewhere"}} {
		r, b := history()
		r.commit("c", "b", 1, nil)
		r.branches["main"] = "c"
		publish(b, r, "c", "20261107.9", publishOptions{libchromium: lib})
		_, err := promoter(r, b).Promote("20261107.9")
		refusedWith(t, err, "corresponding source")
		if len(b.writes) != 0 {
			t.Fatalf("%v: wrote %v", lib, b.writes)
		}
	}
}

func TestBuildsOffTheirMajorsBranchesOrMajorAreRefused(t *testing.T) {
	r, b := history()
	r.commit("feature", "b", 1, nil)
	publish(b, r, "feature", "20261107.8", publishOptions{})
	_, err := promoter(r, b).Promote("20261107.8")
	refusedWith(t, err, "not on main or support/v1")
	// A v2 commit on support/v1 is not on any v2 branch.
	r.commit("t", "s", 2, nil)
	r.branches["support/v1"] = "t"
	publish(b, r, "t", "20261107.9", publishOptions{major: "v2"})
	_, err = promoter(r, b).Promote("20261107.9")
	refusedWith(t, err, "not on main or support/v2")
	// Published under v1 but tagged by a v2 package.
	r, b = history()
	r.commit("u", "b", 2, nil)
	r.branches["main"] = "u"
	publish(b, r, "u", "20261107.9", publishOptions{tag: "v1.20261107.9"})
	_, err = promoter(r, b).Promote("20261107.9")
	refusedWith(t, err, "does not match package major v2")
	if len(b.writes) != 0 {
		t.Fatal(b.writes)
	}
}

func TestLatestWithoutAKnownBuildIsRefused(t *testing.T) {
	r, b := history()
	b.put("v1/latest", []byte("20261101.1\n"), "")
	_, err := promoter(r, b).Promote("20261107.2")
	refusedWith(t, err, "no manifest")
	b.put("v1/latest", []byte("garbage\n"), "")
	_, err = promoter(r, b).Promote("20261107.2")
	refusedWith(t, err, "build number")
}

func TestFailedPreconditionRechecksAgainstTheNewLatest(t *testing.T) {
	// Another promotion lands between this one's read and write: newer than
	// ours, so refuse.
	r, b := history()
	promoter(r, b).Promote("20261106.1")
	b.beforeWrite = func(string, string) { b.beforeWrite = nil; promoter(r, b).Promote("20261107.2") }
	_, err := promoter(r, b).Promote("20261107.1")
	refusedWith(t, err, "not newer")
	if b.latest("v1") != "20261107.2\n" {
		t.Fatal(b.latest("v1"))
	}
	// Older than ours: retry and land on top of it.
	r, b = history()
	promoter(r, b).Promote("20261106.1")
	b.beforeWrite = func(string, string) { b.beforeWrite = nil; promoter(r, b).Promote("20261107.1") }
	if got, err := promoter(r, b).Promote("20261107.2"); got != "promoted" || err != nil {
		t.Fatalf("%s %v", got, err)
	}
	if !slices.Equal(b.writes, []string{"20261106.1", "20261107.1", "20261107.2"}) {
		t.Fatal(b.writes)
	}
}

func TestRetriesAreBounded(t *testing.T) {
	r, b := history()
	b.beforeWrite = func(string, string) { b.mu.Lock(); b.put("v1/latest", []byte("20261106.1\n"), ""); b.mu.Unlock() }
	if _, err := promoter(r, b).Promote("20261107.2"); err == nil || !strings.Contains(err.Error(), "kept changing") {
		t.Fatalf("err = %v", err)
	}
	if len(b.writes) != 0 {
		t.Fatal(b.writes)
	}
}

// A (older) and B (newer) interleave their reads and compare-and-swap
// writes in every order, including retries after a failed precondition;
// latest always ends at B.
func TestConcurrentPromotionsNeverMoveLatestBackwards(t *testing.T) {
	older, newer := "20261107.1", "20261107.2"
	for _, start := range []string{"", "20261106.1"} {
		for _, order := range orders("AB", 6) {
			r, b := history()
			if start != "" {
				promoter(r, b).Promote(start)
				b.writes = nil
			}
			s := newSchedule(order)
			b.gate = s.turn
			results := s.run(t, map[string]func() (string, error){
				"A": func() (string, error) { return promoter(r, b.as("A")).Promote(older) },
				"B": func() (string, error) { return promoter(r, b.as("B")).Promote(newer) },
			})
			if b.latest("v1") != newer+"\n" || results["B"] != "promoted" ||
				(results["A"] != "promoted" && results["A"] != "not newer") || b.writes[len(b.writes)-1] != newer {
				t.Fatalf("%s %s: latest %q, results %v, writes %v", start, order, b.latest("v1"), results, b.writes)
			}
			n := 0
			for _, w := range b.writes {
				if w == older {
					n++
				}
			}
			if (n == 1) != (results["A"] == "promoted") || n > 1 {
				t.Fatalf("%s %s: %d writes of the older build, A %s", start, order, n, results["A"])
			}
		}
	}
}

// --- Unstable auto-promotion ---

func TestOnlyUnstableMajorsPromoteAutomatically(t *testing.T) {
	for _, tc := range []struct {
		flag *bool
		want string
	}{{boolp(true), "promoted"}, {boolp(false), "stable"}, {nil, "stable"}} {
		r, b := history()
		r.commit("c", "b", 1, tc.flag)
		r.branches["main"] = "c"
		publish(b, r, "c", "20261107.9", publishOptions{})
		if got, err := promoter(r, b).PromoteIfUnstable("20261107.9"); got != tc.want || err != nil {
			t.Fatalf("%v: %s %v", tc.flag, got, err)
		}
		if want := tc.want == "promoted"; (len(b.writes) == 1) != want {
			t.Fatalf("%v: writes %v", tc.flag, b.writes)
		}
	}
}

func TestUnstablePromotionIsStillForwardOnly(t *testing.T) {
	r, b := history()
	r.commit("c", "b", 1, boolp(true))
	r.branches["main"] = "c"
	publish(b, r, "c", "20261107.9", publishOptions{})
	promoter(r, b).Promote("20261107.9")
	r.files["a"] = r.files["c"]
	_, err := promoter(r, b).PromoteIfUnstable("20261107.1")
	refusedWith(t, err, "not newer")
}

func TestPromotionRefusedForMissingSourceLeavesLatest(t *testing.T) {
	r, b := history()
	promoter(r, b).Promote("20261107.1")
	var version, manifest string
	var assets map[string]string
	p := Promoter{Repo: r, Bucket: b, Sources: func(v, m string, a map[string]string) error {
		version, manifest, assets = v, m, a
		return &Refused{Msg: "corresponding source missing"}
	}}
	_, err := p.Promote("20261107.2")
	refusedWith(t, err, "corresponding source missing")
	if version != libchromiumPin.Version || manifest != libchromiumPin.Manifest || !maps.Equal(assets, libchromiumPin.Assets) {
		t.Fatalf("called with %s %s %v", version, manifest, assets)
	}
	if b.latest("v1") != "20261107.1\n" {
		t.Fatal(b.latest("v1"))
	}
}

// --- Real git ---

// The comparison promotion makes, against real git rather than the fake: a
// commit pushed after the checkout is fetched before comparing.
func TestACommitPushedAfterTheCheckoutIsFetchedBeforeComparing(t *testing.T) {
	tmp := t.TempDir()
	git := func(dir string, args ...string) string {
		t.Helper()
		out, err := exec.Command("git", append([]string{"-C", dir, "-c", "user.name=t", "-c", "user.email=t@t"}, args...)...).CombinedOutput()
		if err != nil {
			t.Fatalf("git %v: %v %s", args, err, out)
		}
		return strings.TrimSpace(string(out))
	}
	origin, clone := filepath.Join(tmp, "origin"), filepath.Join(tmp, "clone")
	git(tmp, "init", "-q", "-b", "main", origin)
	git(origin, "commit", "-q", "--allow-empty", "-m", "older")
	older := git(origin, "rev-parse", "HEAD")
	git(tmp, "clone", "-q", origin, clone)
	// A newer build lands and is promoted while the older one is still
	// publishing.
	git(origin, "commit", "-q", "--allow-empty", "-m", "newer")
	newer := git(origin, "rev-parse", "HEAD")
	repo := GitRepo{Dir: clone}
	if ok, err := repo.IsAncestor(older, newer); !ok || err != nil {
		t.Fatalf("older before newer: %v %v", ok, err)
	}
	if ok, err := repo.IsAncestor(newer, older); ok || err != nil {
		t.Fatalf("newer before older: %v %v", ok, err)
	}
}
