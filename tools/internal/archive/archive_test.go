package archive

import (
	"archive/tar"
	"bytes"
	"compress/gzip"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"sort"
	"strings"
	"testing"
)

// Packages stay standard gzip tarballs with the expected layout and modes.
func TestRoundTripKeepsLayoutModesAndGzipFormat(t *testing.T) {
	if _, err := exec.LookPath("pigz"); err != nil {
		t.Skip("pigz is installed by the machines' setup")
	}
	root := t.TempDir()
	source := filepath.Join(root, "bundle")
	os.MkdirAll(filepath.Join(source, "lib"), 0o755)
	os.WriteFile(filepath.Join(source, "lanescape-browser"), []byte("engine"), 0o755)
	data := bytes.Repeat([]byte{0, 1, 2, 3, 250, 251, 252, 253}, 4096*32)
	os.WriteFile(filepath.Join(source, "lib", "data.bin"), data, 0o644)
	pkg := filepath.Join(root, "lanescape-browser-test.tar.gz")
	if err := Pack(source, pkg, "lanescape-browser"); err != nil {
		t.Fatal(err)
	}
	f, _ := os.Open(pkg)
	defer f.Close()
	gz, err := gzip.NewReader(f)
	if err != nil {
		t.Fatal(err)
	}
	tr := tar.NewReader(gz)
	var names []string
	for {
		h, err := tr.Next()
		if err != nil {
			break
		}
		names = append(names, strings.TrimSuffix(h.Name, "/"))
	}
	sort.Strings(names)
	if want := []string{"lanescape-browser", "lanescape-browser/lanescape-browser", "lanescape-browser/lib", "lanescape-browser/lib/data.bin"}; !slices.Equal(names, want) {
		t.Fatalf("names %v", names)
	}
	out := filepath.Join(root, "out")
	if err := Unpack(pkg, out); err != nil {
		t.Fatal(err)
	}
	if st, _ := os.Stat(filepath.Join(out, "lanescape-browser", "lanescape-browser")); st.Mode().Perm() != 0o755 {
		t.Fatalf("mode %v", st.Mode())
	}
	if got, _ := os.ReadFile(filepath.Join(out, "lanescape-browser", "lib", "data.bin")); !bytes.Equal(got, data) {
		t.Fatal("contents changed")
	}
}

// Extraction refuses what a package must never hold.
func TestExtractRefusesWhatLeavesTheDestination(t *testing.T) {
	for _, h := range []*tar.Header{
		{Name: "../escape", Typeflag: tar.TypeReg, Mode: 0o644},
		{Name: "/abs", Typeflag: tar.TypeReg, Mode: 0o644},
		{Name: "a/../../escape", Typeflag: tar.TypeReg, Mode: 0o644},
		{Name: "link", Typeflag: tar.TypeSymlink, Linkname: "../../etc/passwd"},
		{Name: "link", Typeflag: tar.TypeSymlink, Linkname: "/etc/passwd"},
		{Name: "dev", Typeflag: tar.TypeChar},
	} {
		var buf bytes.Buffer
		tw := tar.NewWriter(&buf)
		tw.WriteHeader(h)
		tw.Close()
		if err := Extract(&buf, t.TempDir()); err == nil {
			t.Errorf("%s (%c) extracted", h.Name, h.Typeflag)
		}
	}
	// Setuid and the like are dropped.
	var buf bytes.Buffer
	tw := tar.NewWriter(&buf)
	tw.WriteHeader(&tar.Header{Name: "tool", Typeflag: tar.TypeReg, Mode: 0o4755, Size: 1})
	tw.Write([]byte("x"))
	tw.Close()
	dir := t.TempDir()
	if err := Extract(&buf, dir); err != nil {
		t.Fatal(err)
	}
	if st, _ := os.Stat(filepath.Join(dir, "tool")); st.Mode()&os.ModeSetuid != 0 || st.Mode().Perm() != 0o755 {
		t.Fatalf("mode %v", st.Mode())
	}
}

// member is one tar member for a test archive.
type member struct {
	name, link string
	kind       byte
	mode       int64
	body       string
}

func tarOf(t *testing.T, members ...member) *bytes.Buffer {
	t.Helper()
	var buf bytes.Buffer
	tw := tar.NewWriter(&buf)
	for _, m := range members {
		h := &tar.Header{Name: m.name, Linkname: m.link, Typeflag: m.kind, Mode: m.mode, Size: int64(len(m.body))}
		if m.kind != tar.TypeReg {
			h.Size = 0
		}
		if err := tw.WriteHeader(h); err != nil {
			t.Fatal(err)
		}
		if h.Size > 0 {
			tw.Write([]byte(m.body))
		}
	}
	tw.Close()
	return &buf
}

// Links are judged by where they resolve through the tree, not by their
// spelling, including links left by an earlier extraction into the same
// destination, as CI's package and tests share one.
func TestExtractFollowsLinksThroughTheTree(t *testing.T) {
	cases := map[string][]member{
		"a link through a link to the parent": {
			{name: "here", link: ".", kind: tar.TypeSymlink},
			{name: "escape", link: "here/..", kind: tar.TypeSymlink},
		},
		"a file through a link to the parent": {
			{name: "here", link: ".", kind: tar.TypeSymlink},
			{name: "up", link: "here/..", kind: tar.TypeSymlink},
		},
		"a file written through a chain": {
			{name: "sub/", kind: tar.TypeDir, mode: 0o755},
			{name: "sub/top", link: "..", kind: tar.TypeSymlink},
			{name: "sub/top/../escape", kind: tar.TypeReg, mode: 0o644, body: "x"},
		},
		"a hard link outside": {
			{name: "hard", link: "../outside", kind: tar.TypeLink},
		},
		"a link through a link replaced later": {
			{name: "d/", kind: tar.TypeDir, mode: 0o755},
			{name: "x", link: "d", kind: tar.TypeSymlink},
			{name: "l", link: "x/..", kind: tar.TypeSymlink},
			{name: "x", link: "d/..", kind: tar.TypeSymlink},
		},
		"a hard link through a link": {
			{name: "here", link: ".", kind: tar.TypeSymlink},
			{name: "hard", link: "here/../outside", kind: tar.TypeLink},
		},
	}
	for name, members := range cases {
		t.Run(name, func(t *testing.T) {
			parent := t.TempDir()
			os.WriteFile(filepath.Join(parent, "outside"), []byte("keep"), 0o644)
			dest := filepath.Join(parent, "dest")
			if err := Extract(tarOf(t, members...), dest); err == nil {
				t.Fatal("extracted")
			}
			if _, err := os.Lstat(filepath.Join(parent, "escape")); err == nil {
				t.Fatal("wrote outside the destination")
			}
		})
	}

	// A link to outside the destination that is already there, from an
	// earlier extraction or otherwise, is never written through.
	parent := t.TempDir()
	outside := filepath.Join(parent, "outside")
	os.Mkdir(outside, 0o755)
	dest := filepath.Join(parent, "dest")
	os.Mkdir(dest, 0o755)
	os.Symlink(outside, filepath.Join(dest, "abs"))
	os.Symlink("../outside", filepath.Join(dest, "rel"))
	for _, m := range []member{
		{name: "abs/file", kind: tar.TypeReg, mode: 0o644, body: "x"},
		{name: "rel/file", kind: tar.TypeReg, mode: 0o644, body: "x"},
		{name: "rel/dir/", kind: tar.TypeDir, mode: 0o755},
		{name: "rel/link", link: "x", kind: tar.TypeSymlink},
	} {
		if err := Extract(tarOf(t, m), dest); err == nil {
			t.Errorf("%s extracted", m.name)
		}
	}
	if entries, _ := os.ReadDir(outside); len(entries) != 0 {
		t.Fatalf("wrote outside the destination: %v", entries)
	}

	// Links that stay inside work across two extractions.
	dest = filepath.Join(t.TempDir(), "dest")
	if err := Extract(tarOf(t,
		member{name: "pkg/", kind: tar.TypeDir, mode: 0o755},
		member{name: "pkg/lib/", kind: tar.TypeDir, mode: 0o755},
		member{name: "pkg/current", link: "lib", kind: tar.TypeSymlink},
	), dest); err != nil {
		t.Fatal(err)
	}
	if err := Extract(tarOf(t,
		member{name: "pkg/current/data", kind: tar.TypeReg, mode: 0o644, body: "data"},
		member{name: "tests", link: "pkg/current", kind: tar.TypeSymlink},
	), dest); err != nil {
		t.Fatal(err)
	}
	if got, _ := os.ReadFile(filepath.Join(dest, "tests", "data")); string(got) != "data" {
		t.Fatalf("read %q through the links", got)
	}
}

// Modes follow Python's data filter.
func TestExtractGivesTheDataFiltersModes(t *testing.T) {
	dest := t.TempDir()
	err := Extract(tarOf(t,
		member{name: "open", kind: tar.TypeReg, mode: 0o777, body: "x"},
		member{name: "none", kind: tar.TypeReg, mode: 0o000, body: "x"},
		member{name: "group", kind: tar.TypeReg, mode: 0o070, body: "x"},
		member{name: "plain", kind: tar.TypeReg, mode: 0o644, body: "x"},
		member{name: "sticky", kind: tar.TypeReg, mode: 0o7755, body: "x"},
		member{name: "locked/", kind: tar.TypeDir, mode: 0o500},
		member{name: "locked/inside", kind: tar.TypeReg, mode: 0o644, body: "x"},
		member{name: "hard", link: "plain", kind: tar.TypeLink, mode: 0o755},
	), dest)
	if err != nil {
		t.Fatal(err)
	}
	for name, want := range map[string]os.FileMode{
		"open": 0o755, "none": 0o600, "group": 0o640, "sticky": 0o755,
		"locked/inside": 0o644, "hard": 0o755,
	} {
		st, err := os.Stat(filepath.Join(dest, name))
		if err != nil {
			t.Fatal(err)
		}
		if st.Mode() != want {
			t.Errorf("%s: mode %v, want %v", name, st.Mode(), want)
		}
	}
	// An archived directory's mode is ignored: the owner can still write.
	if err := os.WriteFile(filepath.Join(dest, "locked", "new"), nil, 0o644); err != nil {
		t.Fatal(err)
	}
	// A contained hard link shares its target.
	a, _ := os.Stat(filepath.Join(dest, "plain"))
	b, _ := os.Stat(filepath.Join(dest, "hard"))
	if !os.SameFile(a, b) {
		t.Fatal("the hard link is a copy")
	}
}

// A link is written exactly as checked, and a hard link to a link links
// the file it leads to.
func TestLinksAreWrittenAsChecked(t *testing.T) {
	dest := filepath.Join(t.TempDir(), "dest")
	err := Extract(tarOf(t,
		member{name: "a/b/", kind: tar.TypeDir, mode: 0o755},
		member{name: "a/b/file", kind: tar.TypeReg, mode: 0o644, body: "deep"},
		member{name: "file", kind: tar.TypeReg, mode: 0o644, body: "top"},
		member{name: "up", link: "a/b", kind: tar.TypeSymlink},
		// Through up, ".." is a/: up/../b/file is a/b/file. Cleaned, it
		// would be b/file, which doesn't exist.
		member{name: "through", link: "up/../b/file", kind: tar.TypeSymlink},
		member{name: "sub/", kind: tar.TypeDir, mode: 0o755},
		member{name: "rel", link: "file", kind: tar.TypeSymlink},
		member{name: "sub/hard", link: "rel", kind: tar.TypeLink},
	), dest)
	if err != nil {
		t.Fatal(err)
	}
	if got, _ := os.Readlink(filepath.Join(dest, "through")); got != "up/../b/file" {
		t.Fatalf("link written as %q", got)
	}
	if got, _ := os.ReadFile(filepath.Join(dest, "through")); string(got) != "deep" {
		t.Fatalf("link reads %q", got)
	}
	info, _ := os.Lstat(filepath.Join(dest, "sub", "hard"))
	if got, _ := os.ReadFile(filepath.Join(dest, "sub", "hard")); !info.Mode().IsRegular() || string(got) != "top" {
		t.Fatalf("hard link is %v reading %q", info.Mode(), got)
	}
	// A hard link to a directory is refused.
	if err := Extract(tarOf(t, member{name: "hd", link: "a", kind: tar.TypeLink}), dest); err == nil {
		t.Fatal("hard-linked a directory")
	}
}
