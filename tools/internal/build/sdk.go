// Package build builds Lanescape browser for a target against the
// libchromium version in engine/libchromium.json, which it never compiles
// (specs/0001-architecture.md, Building), packages a build, and pins a
// libchromium version.
//
// It downloads the version's parts for the target and checks each against
// the pin, builds libvpx with the SDK's toolchain, generates the engine's
// mojom bindings with the SDK's generator, compiles and links the engine
// with ninja, builds the Rust host with cargo, and puts the SDK's runtime
// files beside them. The SDK, libvpx and the Apple SDK are kept in a cache
// (LANESCAPE_BROWSER_CACHE, by default ~/.cache/lanescape-browser), keyed
// by what they're made from, so a second build only compiles what changed.
// The build goes in out/<target>/, or under LANESCAPE_BROWSER_OUT. Every
// build runs on Linux x86-64, where the SDK's toolchain runs, and
// cross-compiles the other targets.
package build

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"

	"github.com/lanescape/browser/tools/internal/repo"
)

// Releases is where libchromium's parts are downloaded from.
const Releases = "https://github.com/lanescape/libchromium/releases/download"

var (
	// parts are what an embedder builds from (libchromium's
	// specs/0001-sdk.md, Parts); Linux also needs the sysroot.
	parts      = []string{"lib", "headers", "toolchain"}
	linuxParts = []string{"sysroot"}
)

// Env is where a build reads and writes.
type Env struct {
	Root    string
	Targets map[string]repo.Target
	// Getenv is os.Getenv unless set.
	Getenv func(string) string
	// HTTP downloads; a client with a generous timeout unless set.
	HTTP *http.Client
	// Out is where commands print; os.Stdout unless set.
	Out io.Writer
	// ReleasesURL is Releases unless set.
	ReleasesURL string
}

func (e *Env) releases() string {
	if e.ReleasesURL != "" {
		return e.ReleasesURL
	}
	return Releases
}

func (e *Env) getenv(name string) string {
	if e.Getenv != nil {
		return e.Getenv(name)
	}
	return os.Getenv(name)
}

func (e *Env) out() io.Writer {
	if e.Out != nil {
		return e.Out
	}
	return os.Stdout
}

func (e *Env) http() *http.Client {
	if e.HTTP != nil {
		return e.HTTP
	}
	return &http.Client{Timeout: 30 * time.Minute}
}

// NewEnv is the repository's environment.
func NewEnv(root string) (*Env, error) {
	targets, err := repo.Targets(root)
	if err != nil {
		return nil, err
	}
	return &Env{Root: root, Targets: targets}, nil
}

// Cache is where downloads and libvpx's builds are kept.
func (e *Env) Cache() string {
	dir := e.getenv("LANESCAPE_BROWSER_CACHE")
	if dir == "" {
		home, _ := os.UserHomeDir()
		dir = filepath.Join(home, ".cache", "lanescape-browser")
	}
	abs, _ := filepath.Abs(dir)
	return abs
}

// OutDir is where a target's build goes: out/<target>/, or under
// LANESCAPE_BROWSER_OUT, as CI's work disk.
func (e *Env) OutDir(target string) string {
	dir := e.getenv("LANESCAPE_BROWSER_OUT")
	if dir == "" {
		dir = filepath.Join(e.Root, "out")
	}
	abs, _ := filepath.Abs(filepath.Join(dir, target))
	return abs
}

// Run prints and runs a command, failing on a nonzero exit.
func (e *Env) Run(dir string, env []string, name string, args ...string) error {
	fmt.Fprintln(e.out(), "+", strings.Join(append([]string{name}, args...), " "))
	cmd := exec.Command(name, args...)
	cmd.Dir, cmd.Env = dir, env
	cmd.Stdout, cmd.Stderr = e.out(), os.Stderr
	if err := cmd.Run(); err != nil {
		return fmt.Errorf("%s: %w", name, err)
	}
	return nil
}

// SHA256File is a file's SHA-256.
func SHA256File(path string) (string, error) {
	f, err := os.Open(path)
	if err != nil {
		return "", err
	}
	defer f.Close()
	h := sha256.New()
	if _, err := io.Copy(h, f); err != nil {
		return "", err
	}
	return hex.EncodeToString(h.Sum(nil)), nil
}

// pyJSON is Python's json.dumps of strings, nulls and lists of them, as the
// cache's keys were first made, so keys stay the same.
func pyJSON(v any) string {
	switch x := v.(type) {
	case nil:
		return "null"
	case string:
		b, _ := json.Marshal(x)
		return string(b)
	case []string:
		items := make([]any, len(x))
		for i, s := range x {
			items[i] = s
		}
		return pyJSON(items)
	case []any:
		parts := make([]string, len(x))
		for i, item := range x {
			parts[i] = pyJSON(item)
		}
		return "[" + strings.Join(parts, ", ") + "]"
	}
	panic(fmt.Sprintf("pyJSON: %T", v))
}

func shortSHA(text string, n int) string {
	s := sha256.Sum256([]byte(text))
	return hex.EncodeToString(s[:])[:n]
}

// extract unpacks a .tar.zst into destination, which appears whole or not
// at all.
func (e *Env) extract(archive, destination string) error {
	staging, err := os.MkdirTemp(filepath.Dir(destination), ".extract-")
	if err != nil {
		return err
	}
	defer os.RemoveAll(staging)
	zstd := exec.Command("zstd", "-dc", archive)
	tar := exec.Command("tar", "-x", "-C", staging)
	pipe, err := zstd.StdoutPipe()
	if err != nil {
		return err
	}
	tar.Stdin, tar.Stderr, zstd.Stderr = pipe, os.Stderr, os.Stderr
	if err := zstd.Start(); err != nil {
		return err
	}
	terr := tar.Run()
	if err := zstd.Wait(); err != nil {
		return fmt.Errorf("zstd could not read %s", archive)
	}
	if terr != nil {
		return fmt.Errorf("tar: %w", terr)
	}
	return os.Rename(staging, destination)
}

// settle moves a finished staging directory into its place in the cache.
// Another build that finished the same entry first wins, and this one uses
// that.
func settle(staging, destination, complete string) error {
	if err := os.Rename(staging, destination); err != nil {
		if _, serr := os.Stat(complete); serr == nil {
			return nil
		}
		return err
	}
	return nil
}

// AssetName is a part's release asset.
func AssetName(version, target, part string) string {
	return "libchromium-" + version + "-" + target + "-" + part + ".tar.zst"
}

// PartsOf are the parts a target builds from.
func (e *Env) PartsOf(target string) []string {
	if e.Targets[target].System == "linux" {
		return append(append([]string(nil), parts...), linuxParts...)
	}
	return append([]string(nil), parts...)
}

// download writes a URL's body to a file.
func (e *Env) download(url, path string) error {
	resp, err := e.http().Get(url)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("%s: %s", url, resp.Status)
	}
	f, err := os.Create(path)
	if err != nil {
		return err
	}
	if _, err := io.Copy(f, resp.Body); err != nil {
		f.Close()
		return err
	}
	return f.Close()
}

// FetchSDK is the pinned SDK for the target, downloaded and checked once:
// each part's SHA-256 must be the pin's.
func (e *Env) FetchSDK(target string, pin repo.Pin) (string, error) {
	var digests []any
	for _, part := range e.PartsOf(target) {
		if sum, ok := pin.Assets[AssetName(pin.Version, target, part)]; ok {
			digests = append(digests, sum)
		} else {
			digests = append(digests, nil)
		}
	}
	root := filepath.Join(e.Cache(), "libchromium", pin.Version, target+"-"+shortSHA(pyJSON(digests), 12))
	if _, err := os.Stat(filepath.Join(root, "sdk.json")); err == nil {
		return root, nil
	}
	if err := os.MkdirAll(filepath.Dir(root), 0o755); err != nil {
		return "", err
	}
	staging, err := os.MkdirTemp(filepath.Dir(root), ".sdk-")
	if err != nil {
		return "", err
	}
	defer os.RemoveAll(staging)
	for _, part := range e.PartsOf(target) {
		name := AssetName(pin.Version, target, part)
		expected := pin.Assets[name]
		if expected == "" {
			return "", fmt.Errorf("engine/libchromium.json pins no %s: run pin %s", name, pin.Version)
		}
		archive := filepath.Join(staging, name)
		fmt.Fprintf(e.out(), "downloading %s\n", name)
		if err := e.download(e.releases()+"/v"+pin.Version+"/"+name, archive); err != nil {
			return "", err
		}
		if sum, err := SHA256File(archive); err != nil || sum != expected {
			return "", fmt.Errorf("%s is not the one engine/libchromium.json pins", name)
		}
		// Each part holds top-level directories of its own, such as lib/,
		// which join the SDK's root.
		unpacked := filepath.Join(staging, ".part-"+part)
		if err := e.extract(archive, unpacked); err != nil {
			return "", err
		}
		os.Remove(archive)
		entries, err := os.ReadDir(unpacked)
		if err != nil {
			return "", err
		}
		for _, entry := range entries {
			if err := os.Rename(filepath.Join(unpacked, entry.Name()), filepath.Join(staging, entry.Name())); err != nil {
				return "", err
			}
		}
		if err := os.Remove(unpacked); err != nil {
			return "", err
		}
	}
	if err := settle(staging, root, filepath.Join(root, "sdk.json")); err != nil {
		return "", err
	}
	return root, nil
}

// AppleSDK is the Apple SDK that a macOS build compiles with: the one given,
// or the pinned archive from the organization's tools bucket, fetched once
// with gcloud's credentials and checked.
func (e *Env) AppleSDK(target, given string) (string, error) {
	if given != "" {
		return filepath.Abs(given)
	}
	pin := e.Targets[target].AppleSDK
	if pin == nil {
		return "", fmt.Errorf("%s pins no Apple SDK", target)
	}
	root := filepath.Join(e.Cache(), "apple-sdk", pin.SHA256)
	sdk := filepath.Join(root, "Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk")
	if st, err := os.Stat(sdk); err == nil && st.IsDir() {
		return sdk, nil
	}
	if err := os.MkdirAll(root, 0o755); err != nil {
		return "", err
	}
	tmp, err := os.MkdirTemp(root, "")
	if err != nil {
		return "", err
	}
	defer os.RemoveAll(tmp)
	archive := filepath.Join(tmp, "sdk.tar.zst")
	if err := e.Run("", nil, "gcloud", "storage", "cp", pin.Object, archive); err != nil {
		return "", err
	}
	if sum, err := SHA256File(archive); err != nil || sum != pin.SHA256 {
		return "", fmt.Errorf("%s is not the archive engine/targets.json pins", pin.Object)
	}
	if err := e.extract(archive, filepath.Join(tmp, "Contents")); err != nil {
		return "", err
	}
	if err := settle(filepath.Join(tmp, "Contents"), filepath.Join(root, "Contents"), sdk); err != nil {
		return "", err
	}
	return sdk, nil
}

// Description is an SDK's sdk.json.
type Description struct {
	Version           string   `json:"version"`
	Target            string   `json:"target"`
	System            string   `json:"system"`
	Sysroot           *string  `json:"sysroot"`
	Defines           []string `json:"defines"`
	IncludeDirs       []string `json:"include_dirs"`
	SystemIncludeDirs []string `json:"system_include_dirs"`
	Toolchain         struct {
		CC    string `json:"cc"`
		CXX   string `json:"cxx"`
		AR    string `json:"ar"`
		NM    string `json:"nm"`
		Strip string `json:"strip"`
	} `json:"toolchain"`
	Compile map[string][]string `json:"compile"`
	Link    struct {
		Flags      []string `json:"flags"`
		Libraries  []string `json:"libraries"`
		SystemLibs []string `json:"system_libs"`
		Frameworks []string `json:"frameworks"`
	} `json:"link"`
	Runtime struct {
		Libraries []string `json:"libraries"`
		Resources []string `json:"resources"`
	} `json:"runtime"`
	Mojom *struct {
		Parser       string   `json:"parser"`
		Generator    string   `json:"generator"`
		Imports      []string `json:"imports"`
		Modules      string   `json:"modules"`
		TypeMappings string   `json:"type_mappings"`
		Features     []string `json:"features"`
	} `json:"mojom"`
}

// ReadDescription reads an SDK's sdk.json.
func ReadDescription(sdk string) (Description, error) {
	data, err := os.ReadFile(filepath.Join(sdk, "sdk.json"))
	if err != nil {
		return Description{}, err
	}
	var d Description
	if err := json.Unmarshal(data, &d); err != nil {
		return Description{}, fmt.Errorf("%s/sdk.json: %w", sdk, err)
	}
	return d, nil
}

// Expand fills an sdk.json value's {sdk} and {apple_sdk}.
func Expand(value, sdk, apple string) (string, error) {
	value = strings.ReplaceAll(value, "{sdk}", sdk)
	if strings.Contains(value, "{apple_sdk}") {
		if apple == "" {
			return "", errors.New("a macOS build needs the Apple SDK")
		}
		value = strings.ReplaceAll(value, "{apple_sdk}", apple)
	}
	return value, nil
}

func expandAll(values []string, sdk, apple string) ([]string, error) {
	out := make([]string, len(values))
	for i, v := range values {
		x, err := Expand(v, sdk, apple)
		if err != nil {
			return nil, err
		}
		out[i] = x
	}
	return out, nil
}

// ClangTarget is the --target the SDK compiles for, from its compile flags.
func ClangTarget(d Description) (string, error) {
	for _, flag := range d.Compile["c++"] {
		if t, ok := strings.CutPrefix(flag, "--target="); ok {
			return t, nil
		}
	}
	return "", errors.New("sdk.json names no --target")
}

// SDKIdentity is which SDK this is, beyond its version: its sdk.json, and
// the contents of every library it links and ships, so that an SDK replaced
// in place is another SDK.
func SDKIdentity(sdk string) (string, error) {
	raw, err := os.ReadFile(filepath.Join(sdk, "sdk.json"))
	if err != nil {
		return "", err
	}
	var d Description
	if err := json.Unmarshal(raw, &d); err != nil {
		return "", err
	}
	h := sha256.New()
	h.Write(raw)
	set := map[string]bool{}
	for _, l := range append(append([]string(nil), d.Link.Libraries...), d.Runtime.Libraries...) {
		set[l] = true
	}
	libraries := make([]string, 0, len(set))
	for l := range set {
		libraries = append(libraries, l)
	}
	sortStrings(libraries)
	for _, library := range libraries {
		path := filepath.Join(sdk, library)
		if strings.Contains(library, "{sdk}") {
			path, _ = Expand(library, sdk, "")
		}
		sum, err := SHA256File(path)
		if err != nil {
			return "", err
		}
		fmt.Fprintf(h, "%s\n%s\n", filepath.Base(path), sum)
	}
	return hex.EncodeToString(h.Sum(nil)), nil
}
