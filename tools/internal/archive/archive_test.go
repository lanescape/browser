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
