package main

import (
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/lanescape/browser/tools/internal/release"
	"github.com/lanescape/browser/tools/internal/repo"
)

// cliRepo is two commits on main: b, a stable v1 build, and c, an unstable
// one after it.
type cliRepo struct{ files map[string]map[string]string }

func (r cliRepo) Show(sha, path string) (string, error) { return r.files[sha][path], nil }
func (r cliRepo) BranchContains(branch, sha string) (bool, error) {
	return branch == "main", nil
}
func (r cliRepo) IsAncestor(a, d string) (bool, error) { return a == d || (a == "b" && d == "c"), nil }
func (r cliRepo) BuildTags() ([]release.Tag, error) {
	return []release.Tag{{Name: "v1.20261107.2", SHA: "b"}, {Name: "v1.20261107.9", SHA: "c"}}, nil
}

type cliBucket struct {
	objects map[string][]byte
	etag    int
	writes  []string
}

func (b *cliBucket) Read(key string) (string, bool, error) {
	o, ok := b.objects[key]
	return string(o), ok, nil
}
func (b *cliBucket) Digest(key string) (int64, string, bool, error) {
	o, ok := b.objects[key]
	s := sha256.Sum256(o)
	return int64(len(o)), hex.EncodeToString(s[:]), ok, nil
}
func (b *cliBucket) ReadLatest(major string) (string, string, bool, error) {
	o, ok := b.objects[major+"/latest"]
	return string(o), fmt.Sprintf(`"%d"`, b.etag), ok, nil
}
func (b *cliBucket) WriteLatest(major, build, etag string, exists bool) (bool, error) {
	_, ok := b.objects[major+"/latest"]
	if ok != exists || (ok && etag != fmt.Sprintf(`"%d"`, b.etag)) {
		return false, nil
	}
	b.etag++
	b.objects[major+"/latest"] = []byte(build + "\n")
	b.writes = append(b.writes, build)
	return true, nil
}

func TestTheReleaseCommands(t *testing.T) {
	root, _ := filepath.Abs("../..")
	pinText, _ := os.ReadFile(filepath.Join(root, "engine", "libchromium.json"))
	pin, _ := repo.ReadPin(pinText)
	r := cliRepo{files: map[string]map[string]string{
		"b": {"Cargo.toml": "[package]\nversion=\"1.0.0\"\n", "engine/libchromium.json": string(pinText)},
		"c": {"Cargo.toml": "[package]\nversion=\"1.0.0\"\n[package.metadata.lanescape-browser]\nunstable = true\n", "engine/libchromium.json": string(pinText)},
	}}
	b := &cliBucket{objects: map[string][]byte{}}
	for sha, build := range map[string]string{"b": "20261107.2", "c": "20261107.9"} {
		packages := map[string]release.Package{}
		for _, target := range release.Targets {
			data := []byte(target + build)
			s := sha256.Sum256(data)
			name := "lanescape-browser-" + target + ".tar.gz"
			packages[target] = release.Package{Name: name, Bytes: int64(len(data)), SHA256: hex.EncodeToString(s[:])}
			b.objects["v1/"+build+"/"+name] = data
		}
		lib := release.LibchromiumOf(pin.Version)
		m, _ := json.Marshal(release.Manifest{Major: "v1", Build: build, Commit: sha, Packages: packages, Libchromium: &lib})
		b.objects["v1/"+build+"/manifest.json"] = m
	}
	made := 0
	d := deps{
		getenv: func(string) string { return "" },
		promoter: func(string) (release.Promoter, error) {
			made++
			return release.Promoter{Repo: r, Bucket: b, Sources: func(string, string, map[string]string) error { return nil }}, nil
		},
		repo: func(string) release.Repo { return r },
	}
	var out bytes.Buffer
	for _, args := range [][]string{
		{"release", "promote", "--build", "20261107.9", "--if-unstable"},
		{"release", "promote", "--build", "20261107.2", "--if-unstable"},
		{"release", "major", "--build", "20261107.9"},
	} {
		if err := run(root, args, &out, d); err != nil {
			t.Fatalf("%v: %v", args, err)
		}
	}
	want := "promoted 20261107.9\nnot promoting 20261107.2: its major is stable; promote it with the Promote build workflow\nv1\n"
	if out.String() != want {
		t.Fatalf("printed:\n%s", out.String())
	}
	// Promotion uses the release bucket, with the environment's keys.
	if made != 2 {
		t.Fatalf("made %d promoters", made)
	}
	err := run(root, []string{"release", "promote", "--build", "20261107.2"}, &out, d)
	if err == nil || !strings.Contains(err.Error(), "refused: 20261107.2 is not newer") {
		t.Fatalf("err = %v", err)
	}
	if len(b.writes) != 1 || b.writes[0] != "20261107.9" {
		t.Fatal(b.writes)
	}
	// Bad arguments are refused before anything is done.
	for _, args := range [][]string{{"release", "promote"}, {"release", "reserve"}, {"build", "--target", "nowhere"}, {"pin"}, {"publish", "x"}} {
		if err := run(root, args, &out, d); err == nil {
			t.Errorf("%v accepted", args)
		}
	}
}
