package build

import (
	"archive/tar"
	"bytes"
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"sort"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/lanescape/browser/tools/internal/repo"
)

const repoRoot = "../../.."

func strp(s string) *string { return &s }

func linuxSDK() Description {
	var d Description
	json.Unmarshal([]byte(`{
		"version": "1-1", "target": "linux-arm64", "system": "linux", "sysroot": "{sdk}/sysroot",
		"toolchain": {"cc": "{sdk}/toolchain/bin/clang", "cxx": "{sdk}/toolchain/bin/clang++",
			"ar": "{sdk}/toolchain/bin/llvm-ar", "nm": "{sdk}/toolchain/bin/llvm-nm", "strip": "{sdk}/toolchain/bin/llvm-strip"},
		"defines": ["LIBCHROMIUM_EXPORTS"], "include_dirs": ["{sdk}/include", "{sdk}/gen"],
		"system_include_dirs": ["{sdk}/include/third_party/libc++/src/include"],
		"compile": {"c++": ["--target=aarch64-unknown-linux-gnu", "--sysroot={sdk}/sysroot", "-std=c++23"]},
		"link": {"flags": ["-fuse-ld=lld", "-Wl,-z,defs"], "libraries": ["{sdk}/lib/libchromium.so", "{sdk}/lib/libc++.so"],
			"system_libs": ["dl"], "frameworks": []}}`), &d)
	return d
}

func macSDK() Description {
	d := linuxSDK()
	d.Target, d.System, d.Sysroot = "macos-arm64", "mac", nil
	d.Compile = map[string][]string{"c++": {"--target=arm64-apple-macos", "-isysroot", "{apple_sdk}"},
		"objective-c++": {"--target=arm64-apple-macos", "-isysroot", "{apple_sdk}", "-fobjc-arc"}}
	d.Link.Flags, d.Link.Libraries, d.Link.SystemLibs, d.Link.Frameworks = []string{"-fuse-ld=lld"}, []string{"{sdk}/lib/libchromium.dylib"}, nil, []string{"IOSurface"}
	return d
}

func env(t *testing.T) *Env {
	t.Helper()
	root, _ := filepath.Abs(repoRoot)
	e, err := NewEnv(root)
	if err != nil {
		t.Fatal(err)
	}
	return e
}

// --- Pin ---

func pinIn(t *testing.T, manifest []byte) (string, error) {
	t.Helper()
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/v1-2/manifest.json" {
			t.Errorf("fetched %s", r.URL.Path)
		}
		w.Write(manifest)
	}))
	defer srv.Close()
	root := t.TempDir()
	os.MkdirAll(filepath.Join(root, "engine"), 0o755)
	targets, _ := os.ReadFile(filepath.Join(repoRoot, "engine", "targets.json"))
	os.WriteFile(filepath.Join(root, "engine", "targets.json"), targets, 0o644)
	e, err := NewEnv(root)
	if err != nil {
		t.Fatal(err)
	}
	e.ReleasesURL, e.Out = srv.URL, &bytes.Buffer{}
	return root, e.Pin("1-2", srv.Client())
}

func TestPinsEveryPartEachTargetBuildsFrom(t *testing.T) {
	e := env(t)
	var assets []map[string]string
	for _, target := range repo.Names(e.Targets) {
		for _, part := range []string{"lib", "headers", "toolchain", "sysroot", "symbols"} {
			assets = append(assets, map[string]string{"name": "libchromium-1-2-" + target + "-" + part + ".tar.zst", "sha256": target + "-" + part})
		}
	}
	assets = append(assets, map[string]string{"name": "libchromium-1-2-source-third-party-blink.tar.zst", "part": "source", "sha256": "s"})
	raw, _ := json.Marshal(map[string]any{"assets": assets, "source": map[string]any{"chromium": map[string]string{"tarball": "https://x/chromium.tar.xz"}}})
	root, err := pinIn(t, raw)
	if err != nil {
		t.Fatal(err)
	}
	data, _ := os.ReadFile(filepath.Join(root, "engine", "libchromium.json"))
	pin, err := repo.ReadPin(data)
	if err != nil {
		t.Fatal(err)
	}
	s := sha256.Sum256(raw)
	// The manifest itself, which publishing holds the release's source to.
	if pin.Version != "1-2" || pin.Manifest != hex.EncodeToString(s[:]) {
		t.Fatalf("%+v", pin)
	}
	if _, ok := pin.Assets["libchromium-1-2-linux-arm64-sysroot.tar.zst"]; !ok {
		t.Fatal("no linux-arm64 sysroot")
	}
	if _, ok := pin.Assets["libchromium-1-2-macos-arm64-sysroot.tar.zst"]; ok {
		t.Fatal("a macOS sysroot")
	}
	for name := range pin.Assets {
		if strings.HasSuffix(name, "-symbols.tar.zst") {
			t.Fatal(name)
		}
	}
	if len(pin.Assets) != 3*3+2 {
		t.Fatalf("%d parts", len(pin.Assets))
	}
	// Written in the targets' and parts' own order.
	if !strings.Contains(string(data), "\"libchromium-1-2-linux-amd64-lib.tar.zst\": \"linux-amd64-lib\",\n    \"libchromium-1-2-linux-amd64-headers.tar.zst\"") {
		t.Fatal(string(data))
	}
}

func TestAVersionWithoutAPartOrSourceIsRefused(t *testing.T) {
	source := map[string]any{"assets": []map[string]string{{"name": "s", "part": "source", "sha256": "s"}}, "source": map[string]any{"chromium": map[string]string{"tarball": "https://x"}}}
	for _, tc := range []struct {
		manifest map[string]any
		reason   string
	}{
		{source, "has no libchromium-1-2-"},
		{map[string]any{"assets": []any{}}, "publishes no corresponding source"},
		{map[string]any{"assets": source["assets"], "source": map[string]any{}}, "publishes no corresponding source"},
	} {
		raw, _ := json.Marshal(tc.manifest)
		if _, err := pinIn(t, raw); err == nil || !strings.Contains(err.Error(), tc.reason) {
			t.Errorf("err = %v, want %q", err, tc.reason)
		}
	}
}

// --- Flags ---

func TestExpandNeedsTheAppleSDKOnlyWhereItIsNamed(t *testing.T) {
	if got, err := Expand("{sdk}/lib", "/s", ""); got != "/s/lib" || err != nil {
		t.Fatal(got, err)
	}
	if _, err := Expand("-isysroot {apple_sdk}", "/s", ""); err == nil || !strings.Contains(err.Error(), "Apple SDK") {
		t.Fatal(err)
	}
}

func TestClangTarget(t *testing.T) {
	if got, err := ClangTarget(linuxSDK()); got != "aarch64-unknown-linux-gnu" || err != nil {
		t.Fatal(got, err)
	}
}

func ninja(t *testing.T, d Description, target, apple string) string {
	t.Helper()
	e := env(t)
	out := filepath.Join(t.TempDir(), "out")
	os.MkdirAll(out, 0o755)
	if err := e.WriteNinja(filepath.Join(out, "build.ninja"), target, d, "/sdk", apple, "/vpx", filepath.Join(out, "gen"), out,
		[]string{filepath.Join(out, "gen", "engine", "lanescape_browser.mojom.cc")}); err != nil {
		t.Fatal(err)
	}
	data, _ := os.ReadFile(filepath.Join(out, "build.ninja"))
	return string(data)
}

func TestLinuxNinja(t *testing.T) {
	text := ninja(t, linuxSDK(), "linux-arm64", "")
	e := env(t)
	for _, want := range []string{"--sysroot=/sdk/sysroot", "-DLIBCHROMIUM_EXPORTS", "-isystem /sdk/include/third_party/libc++/src/include",
		"-I" + e.Root, "-I/vpx/include", "lanescape_browser.mojom.cc",
		"/vpx/lib/libvpx.a -Wl,--exclude-libs,libvpx.a -lm /sdk/lib/libchromium.so", "'-Wl,-rpath,$$ORIGIN'", "depfile = $out.d"} {
		if !strings.Contains(text, want) {
			t.Errorf("no %q in:\n%s", want, text)
		}
	}
	for _, name := range append(append([]string(nil), Sources...), PlatformSources["linux"]...) {
		if !strings.Contains(text, "engine/"+name) {
			t.Errorf("no engine/%s", name)
		}
	}
	if strings.Contains(text, "tab_view_mac.mm") {
		t.Error("a macOS source")
	}
}

func TestMacNinja(t *testing.T) {
	text := ninja(t, macSDK(), "macos-arm64", "/apple")
	for _, want := range []string{"rule objective_cxx", "-isysroot /apple", "tab_view_mac.mm.o: objective_cxx",
		"-framework IOSurface -framework AppKit -framework Foundation -framework QuartzCore", "-Wl,-rpath,@loader_path"} {
		if !strings.Contains(text, want) {
			t.Errorf("no %q in:\n%s", want, text)
		}
	}
	for _, not := range []string{"platform_linux.cc", "--exclude-libs"} {
		if strings.Contains(text, not) {
			t.Errorf("%q in the macOS build", not)
		}
	}
}

func TestShellQuoteIsShlexs(t *testing.T) {
	for in, want := range map[string]string{"": "''", "plain/path-1.0:x,y@z%=+": "plain/path-1.0:x,y@z%=+", "a b": "'a b'",
		"-Wl,-rpath,$ORIGIN": "'-Wl,-rpath,$ORIGIN'", "it's": `'it'"'"'s'`} {
		if got := ShellQuote([]string{in}); got != want {
			t.Errorf("ShellQuote(%q) = %s, want %s", in, got, want)
		}
	}
}

// The cache's keys are what Python's json.dumps made of the same values,
// so caches made before stay valid. The expected keys are the ones the
// Python tooling computed for this pin and these paths.
func TestCacheKeysAreUnchanged(t *testing.T) {
	root, _ := filepath.Abs(repoRoot)
	e, err := NewEnv(root)
	if err != nil {
		t.Fatal(err)
	}
	pin, err := repo.Pinned(root)
	if err != nil {
		t.Fatal(err)
	}
	if pin.Version == "157.0.8103.39-4" {
		for target, want := range map[string]string{"linux-amd64": "linux-amd64-76ed77fc2103",
			"linux-arm64": "linux-arm64-dcc83031b642", "macos-arm64": "macos-arm64-bf6226ee056b"} {
			if got := e.SDKKey(target, pin); got != want {
				t.Errorf("SDKKey(%s) = %s, want %s", target, got, want)
			}
		}
	}
	if got := e.SDKKey("linux-amd64", repo.Pin{Version: "1"}); got != "linux-amd64-f94139614091" {
		t.Errorf("SDKKey without parts = %s", got)
	}
	for _, tc := range []struct{ target, apple, want string }{
		{"macos-arm64", "/Users/\u00e9/<SDK>&/MacOSX.sdk", "170270e0c1f0"},
		{"macos-arm64", "", "48b039e3a878"},
		{"linux-amd64", "/Users/\u00e9/<SDK>&/MacOSX.sdk", "e5956b5a28c6"},
		{"linux-amd64", "", "57658b00d02d"},
	} {
		if got := e.LibvpxHow(tc.target, tc.apple); got != tc.want {
			t.Errorf("LibvpxHow(%s, %q) = %s, want %s", tc.target, tc.apple, got, tc.want)
		}
	}
}

// Paths in keys are resolved as Python's Path.resolve() did: an alias of a
// directory keys the same as the directory.
func TestPathsResolveThroughLinks(t *testing.T) {
	dir, _ := filepath.EvalSymlinks(t.TempDir())
	real := filepath.Join(dir, "real")
	os.Mkdir(real, 0o755)
	os.Symlink(real, filepath.Join(dir, "alias"))
	os.MkdirAll(filepath.Join(dir, "versions", "1"), 0o755)
	os.MkdirAll(filepath.Join(dir, "deep"), 0o755)
	os.Symlink(filepath.Join(dir, "versions", "1"), filepath.Join(dir, "deep", "current"))
	for in, want := range map[string]string{
		filepath.Join(dir, "alias"):                 real,
		filepath.Join(dir, "alias", "missing", "x"): filepath.Join(real, "missing", "x"),
		filepath.Join(dir, "missing"):               filepath.Join(dir, "missing"),
		// Through the link first, then up: the target's parent.
		filepath.Join(dir, "deep", "current") + "/../SDK": filepath.Join(dir, "versions", "SDK"),
	} {
		if got := Resolve(in); got != want {
			t.Errorf("Resolve(%s) = %s, want %s", in, got, want)
		}
	}
	out := &Env{Root: dir, Getenv: func(k string) string {
		if k == "LANESCAPE_BROWSER_OUT" {
			return filepath.Join(dir, "deep", "current") + "/.."
		}
		return ""
	}}
	if got := out.OutDir("linux-amd64"); got != filepath.Join(dir, "versions", "linux-amd64") {
		t.Errorf("OutDir = %s", got)
	}
	e := &Env{Getenv: func(k string) string {
		if k == "LANESCAPE_BROWSER_CACHE" {
			return filepath.Join(dir, "alias")
		}
		return ""
	}}
	if got := e.Cache(); got != real {
		t.Errorf("Cache() = %s", got)
	}
}

// --- The cache ---

func TestAPartThatIsNotThePinnedOneIsRefused(t *testing.T) {
	e := env(t)
	cache := t.TempDir()
	e.Getenv = func(k string) string {
		if k == "LANESCAPE_BROWSER_CACHE" {
			return cache
		}
		return ""
	}
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { w.Write([]byte("not the part")) }))
	defer srv.Close()
	e.ReleasesURL, e.HTTP, e.Out = srv.URL, srv.Client(), &bytes.Buffer{}
	pin := repo.Pin{Version: "1-1", Assets: map[string]string{}}
	for _, part := range e.PartsOf("linux-arm64") {
		pin.Assets[AssetName("1-1", "linux-arm64", part)] = strings.Repeat("0", 64)
	}
	if _, err := e.FetchSDK("linux-arm64", pin); err == nil || !strings.Contains(err.Error(), "is not the one engine/libchromium.json pins") {
		t.Fatal(err)
	}
	entries, _ := os.ReadDir(filepath.Join(cache, "libchromium", "1-1"))
	for _, entry := range entries {
		if strings.HasPrefix(entry.Name(), "linux-arm64") {
			t.Fatalf("%s was left in the cache", entry.Name())
		}
	}
}

func TestAColdCacheUnpacksEveryPartIntoTheRoot(t *testing.T) {
	if _, err := exec.LookPath("zstd"); err != nil {
		t.Skip("no zstd")
	}
	e := env(t)
	tmp := t.TempDir()
	parts := map[string][]byte{}
	pin := repo.Pin{Version: "1-1", Assets: map[string]string{}}
	for part, member := range map[string]string{"lib": "lib/libchromium.so", "headers": "include/a.h", "toolchain": "toolchain/bin/clang", "sysroot": "sysroot/usr/x"} {
		var buf bytes.Buffer
		tw := tar.NewWriter(&buf)
		files := map[string]string{member: part}
		if part == "lib" {
			files["sdk.json"] = "{}"
		}
		for name, body := range files {
			dir := filepath.Dir(name)
			for dir != "." {
				tw.WriteHeader(&tar.Header{Name: dir + "/", Typeflag: tar.TypeDir, Mode: 0o755})
				dir = filepath.Dir(dir)
			}
			tw.WriteHeader(&tar.Header{Name: name, Typeflag: tar.TypeReg, Mode: 0o644, Size: int64(len(body))})
			tw.Write([]byte(body))
		}
		tw.Close()
		cmd := exec.Command("zstd", "-q", "-c")
		cmd.Stdin = &buf
		out, err := cmd.Output()
		if err != nil {
			t.Fatal(err)
		}
		parts[part] = out
		s := sha256.Sum256(out)
		pin.Assets[AssetName("1-1", "linux-arm64", part)] = hex.EncodeToString(s[:])
	}
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		name := filepath.Base(r.URL.Path)
		part := strings.TrimSuffix(name[strings.LastIndex(name, "-")+1:], ".tar.zst")
		w.Write(parts[part])
	}))
	defer srv.Close()
	e.Getenv = func(k string) string {
		if k == "LANESCAPE_BROWSER_CACHE" {
			return filepath.Join(tmp, "cache")
		}
		return ""
	}
	e.ReleasesURL, e.HTTP, e.Out = srv.URL, srv.Client(), &bytes.Buffer{}
	root, err := e.FetchSDK("linux-arm64", pin)
	if err != nil {
		t.Fatal(err)
	}
	var files []string
	filepath.WalkDir(root, func(p string, d os.DirEntry, err error) error {
		if err == nil && !d.IsDir() {
			rel, _ := filepath.Rel(root, p)
			files = append(files, filepath.ToSlash(rel))
		}
		return nil
	})
	sort.Strings(files)
	if want := []string{"include/a.h", "lib/libchromium.so", "sdk.json", "sysroot/usr/x", "toolchain/bin/clang"}; !slices.Equal(files, want) {
		t.Fatal(files)
	}
	// A second fetch uses what's there.
	srv.Close()
	if again, err := e.FetchSDK("linux-arm64", pin); err != nil || again != root {
		t.Fatal(again, err)
	}
}

func TestEveryTargetNamesItsLibvpxTargetAndHost(t *testing.T) {
	e := env(t)
	for name, target := range e.Targets {
		if _, ok := PlatformSources[target.System]; !ok || target.LibvpxTarget == "" || !strings.HasPrefix(target.HostLibrary, "liblanescape_browser_host.") {
			t.Errorf("%s: %+v", name, target)
		}
	}
	if e.Targets["macos-arm64"].AppleSDK == nil {
		t.Error("macos-arm64 pins no Apple SDK")
	}
}

func TestTestExecutablesFromCargosMessages(t *testing.T) {
	messages := strings.Join([]string{
		`{"reason":"compiler-artifact","profile":{"test":true},"executable":"/t/deps/lanescape_browser_host-1","target":{"name":"lanescape_browser_host"}}`,
		`{"reason":"compiler-artifact","profile":{"test":true},"executable":"/t/deps/ime-2","target":{"name":"ime"}}`,
		`{"reason":"compiler-artifact","profile":{"test":false},"executable":"/t/x","target":{"name":"other"}}`,
		`{"reason":"compiler-artifact","profile":{"test":true},"executable":null,"target":{"name":"lib"}}`,
		`{"reason":"build-finished","success":true}`,
	}, "\n")
	found, err := TestExecutables([]byte(messages))
	if err != nil || len(found) != 2 || found["ime"] != "/t/deps/ime-2" {
		t.Fatal(found, err)
	}
}

// --- Notices ---

func TestBuildDependenciesAreNotLinkedDependencies(t *testing.T) {
	var m cargoMetadata
	json.Unmarshal([]byte(`{"resolve": {"root": "app", "nodes": [
		{"id": "app", "deps": [{"pkg": "runtime", "dep_kinds": [{"kind": null}]}, {"pkg": "compiler", "dep_kinds": [{"kind": "build"}]}]},
		{"id": "runtime", "deps": []}, {"id": "compiler", "deps": []}]},
		"packages": [{"id": "app"}, {"id": "runtime"}, {"id": "compiler"}]}`), &m)
	var ids []string
	for _, p := range RuntimePackages(m) {
		ids = append(ids, p.ID)
	}
	if !slices.Equal(ids, []string{"app", "runtime"}) {
		t.Fatal(ids)
	}
}

func TestLinkedCratesBringTheirLicenses(t *testing.T) {
	root := t.TempDir()
	var m cargoMetadata
	json.Unmarshal([]byte(`{"resolve": {"root": "crate", "nodes": [{"id": "crate", "deps": []}]},
		"packages": [{"id": "crate", "name": "crate", "version": "1.0.0", "source": "registry",
			"manifest_path": "`+filepath.Join(root, "Cargo.toml")+`", "license": "MIT", "repository": "https://example.com/crate"}]}`), &m)
	os.WriteFile(filepath.Join(root, "LICENSE-MIT"), []byte("license text"), 0o644)
	if err := bundle(m, filepath.Join(root, "out")); err != nil {
		t.Fatal(err)
	}
	if got, _ := os.ReadFile(filepath.Join(root, "out", "crate-1.0.0", "LICENSE-MIT")); string(got) != "license text" {
		t.Fatal(string(got))
	}
	inventory, _ := os.ReadFile(filepath.Join(root, "out", "packages.json"))
	var got []map[string]string
	json.Unmarshal(inventory, &got)
	if len(got) != 1 || got[0]["name"] != "crate" || got[0]["version"] != "1.0.0" || got[0]["license"] != "MIT" || got[0]["repository"] != "https://example.com/crate" {
		t.Fatal(string(inventory))
	}
	// A crate without a licence file stops the package.
	os.Remove(filepath.Join(root, "LICENSE-MIT"))
	if err := bundle(m, filepath.Join(root, "out2")); err == nil || !strings.Contains(err.Error(), "missing dependency license: crate") {
		t.Fatal(err)
	}
}

// The repository's own crates, through cargo, if it's here.
func TestTheRepositorysCratesHaveTheirLicenses(t *testing.T) {
	if _, err := exec.LookPath("cargo"); err != nil {
		t.Skip("no cargo")
	}
	root, _ := filepath.Abs(repoRoot)
	dest := t.TempDir()
	if err := BundleNotices(root, dest, ""); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(filepath.Join(dest, "packages.json")); err != nil {
		t.Fatal(err)
	}
}

// If tar stops early, zstd isn't left blocked writing to it, and tar's
// failure is the one reported.
func TestExtractionEndsWhenTarStopsEarly(t *testing.T) {
	if _, err := exec.LookPath("zstd"); err != nil {
		t.Skip("zstd is installed by the machines' setup")
	}
	dir := t.TempDir()
	data := make([]byte, 8<<20)
	rand.Read(data)
	os.WriteFile(filepath.Join(dir, "data"), data, 0o644)
	archive := filepath.Join(dir, "data.zst")
	if out, err := exec.Command("zstd", "-q", "-1", filepath.Join(dir, "data"), "-o", archive).CombinedOutput(); err != nil {
		t.Fatalf("%v %s", err, out)
	}
	done := make(chan error, 1)
	go func() { done <- unzstdTar(archive, t.TempDir(), "false") }()
	select {
	case err := <-done:
		if err == nil || !strings.HasPrefix(err.Error(), "tar:") {
			t.Fatalf("err = %v", err)
		}
	case <-time.After(30 * time.Second):
		t.Fatal("zstd was left blocked")
	}
}

// Resources go under resources/ and nowhere else, at install and at
// packaging alike.
func TestResourcePaths(t *testing.T) {
	for in, want := range map[string]string{"resources/icudtl.dat": "icudtl.dat", "resources/locales/en-US.pak": "locales/en-US.pak"} {
		if got, err := ResourcePath(in); got != want || err != nil {
			t.Errorf("ResourcePath(%s) = %s, %v", in, got, err)
		}
	}
	for _, in := range []string{"icudtl.dat", "resources", "resources/", "/resources/x", "resources/../x", "resources/./x", "resources//x", "other/x", "resources/a/../../x"} {
		if got, err := ResourcePath(in); err == nil {
			t.Errorf("ResourcePath(%s) = %s", in, got)
		}
	}
}

// A tree is copied through links, as shutil.copytree copied the SDK's
// notices: the directory given may itself be a link.
func TestCopyTreeFollowsLinks(t *testing.T) {
	dir := t.TempDir()
	real := filepath.Join(dir, "real")
	os.MkdirAll(filepath.Join(real, "sub"), 0o755)
	os.WriteFile(filepath.Join(real, "sub", "NOTICE"), []byte("n"), 0o644)
	os.Symlink(filepath.Join(real, "sub"), filepath.Join(real, "linked"))
	os.Symlink(real, filepath.Join(dir, "alias"))
	dst := filepath.Join(dir, "copy")
	if err := copyTree(filepath.Join(dir, "alias"), dst); err != nil {
		t.Fatal(err)
	}
	for _, p := range []string{"sub/NOTICE", "linked/NOTICE"} {
		info, err := os.Lstat(filepath.Join(dst, p))
		if err != nil || !info.Mode().IsRegular() {
			t.Errorf("%s: %v %v", p, info, err)
		}
	}
}

// A missing file is already removed; a file that can't be removed is an
// error, so an old build.json or package isn't left in place.
func TestRemoveIfThere(t *testing.T) {
	dir := t.TempDir()
	if err := removeIfThere(filepath.Join(dir, "missing")); err != nil {
		t.Fatal(err)
	}
	stuck := filepath.Join(dir, "stuck")
	os.MkdirAll(filepath.Join(stuck, "inside"), 0o755)
	if err := removeIfThere(stuck); err == nil {
		t.Fatal("a failed removal passed")
	}
	// Even an empty directory isn't a file to remove.
	empty := filepath.Join(dir, "empty")
	os.Mkdir(empty, 0o755)
	if err := removeIfThere(empty); err == nil {
		t.Fatal("removed a directory")
	}
	if _, err := os.Stat(empty); err != nil {
		t.Fatal(err)
	}
}

// A FIFO in a tree is refused, not opened, which would block.
func TestCopyRefusesSpecialFiles(t *testing.T) {
	dir := t.TempDir()
	os.Mkdir(filepath.Join(dir, "src"), 0o755)
	if err := syscall.Mkfifo(filepath.Join(dir, "src", "fifo"), 0o644); err != nil {
		t.Skip(err)
	}
	if err := copyTree(filepath.Join(dir, "src"), filepath.Join(dir, "dst")); err == nil || !strings.Contains(err.Error(), "not a regular file") {
		t.Fatalf("err = %v", err)
	}
	// A FIFO where the copy goes is replaced, not written through.
	os.WriteFile(filepath.Join(dir, "file"), []byte("x"), 0o644)
	syscall.Mkfifo(filepath.Join(dir, "dst-fifo"), 0o644)
	if err := copyFile(filepath.Join(dir, "file"), filepath.Join(dir, "dst-fifo")); err != nil {
		t.Fatal(err)
	}
	if info, _ := os.Lstat(filepath.Join(dir, "dst-fifo")); !info.Mode().IsRegular() {
		t.Fatalf("mode %v", info.Mode())
	}
}
