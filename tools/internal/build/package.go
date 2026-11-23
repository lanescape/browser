package build

import (
	"bytes"
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

	"github.com/lanescape/browser/tools/internal/archive"
	"github.com/lanescape/browser/tools/internal/fetch"
	"github.com/lanescape/browser/tools/internal/repo"
)

// PackageOptions are package's.
type PackageOptions struct {
	Target   string
	SDK      string
	Tests    bool
	AppleSDK string
}

// Package makes dist/lanescape-browser-<target>.tar.gz: what out/<target>/
// runs with, its documentation and protocol, and the notices for all of it
// (third_party/README.md): Lanescape browser's licence, libchromium's
// notices, which say where Chromium's source is, libvpx's licence and
// patent grant, and the Rust crates' notices. With Tests, also the test
// executables.
func (e *Env) Package(o PackageOptions) error {
	target := o.Target
	out := e.OutDir(target)
	return locked(out, func() error {
		dist := filepath.Join(e.Root, "dist")
		// The previous package goes first, so a refused or failed one never
		// leaves an older one in its place.
		made := []string{filepath.Join(dist, "lanescape-browser-"+target+".tar.gz"), filepath.Join(dist, "tests-"+target+".tar")}
		for _, p := range made {
			if err := removeIfThere(p); err != nil {
				return err
			}
		}
		err := e.pack(o, out, dist)
		if err != nil {
			// Nothing half made, or made from sources that changed, is left
			// to be shipped.
			for _, p := range made {
				if rerr := removeIfThere(p); rerr != nil {
					return errors.Join(err, rerr)
				}
			}
		}
		return err
	})
}

func (e *Env) pack(o PackageOptions, out, dist string) error {
	target := o.Target
	sdk, d, err := e.SDKFor(target, o.SDK)
	if err != nil {
		return err
	}
	files := []string{Executable, e.Targets[target].HostLibrary}
	for _, l := range d.Runtime.Libraries {
		files = append(files, filepath.Base(l))
	}
	for _, r := range d.Runtime.Resources {
		rel, err := ResourcePath(r)
		if err != nil {
			return err
		}
		files = append(files, rel)
	}
	var missing []string
	for _, name := range files {
		if st, err := os.Stat(filepath.Join(out, name)); err != nil || !st.Mode().IsRegular() {
			missing = append(missing, name)
		}
	}
	if len(missing) > 0 {
		if len(missing) > 5 {
			missing = missing[:5]
		}
		return fmt.Errorf("%s has no %s: build it first", out, strings.Join(missing, ", "))
	}
	want, err := e.identity(d, sdk)
	if err != nil {
		return err
	}
	var built *Identity
	if data, err := os.ReadFile(filepath.Join(out, "build.json")); err == nil {
		var id Identity
		if json.Unmarshal(data, &id) == nil {
			built = &id
		}
	}
	if built == nil || *built != want {
		return fmt.Errorf("%s was built from %+v, not %+v: build it again", out, built, want)
	}
	libvpx, _, err := repo.LibvpxPin(e.Root)
	if err != nil {
		return err
	}
	libvpxSource := filepath.Join(e.Cache(), "src", "libvpx-"+libvpx.Commit)
	if err := os.MkdirAll(dist, 0o755); err != nil {
		return err
	}
	tmp, err := os.MkdirTemp("", "")
	if err != nil {
		return err
	}
	defer os.RemoveAll(tmp)
	root := filepath.Join(tmp, "lanescape-browser")
	for _, name := range files {
		if err := os.MkdirAll(filepath.Dir(filepath.Join(root, name)), 0o755); err != nil {
			return err
		}
		if err := copyFile(filepath.Join(out, name), filepath.Join(root, name)); err != nil {
			return err
		}
	}
	// Shipped stripped (specs/0004-releases.md): Linux binaries of debug
	// information and symbol tables, but for what they link by; macOS ones of
	// local symbols, which the SDK's llvm-strip signs ad hoc again. The SDK's
	// own libraries ship stripped already.
	strip, err := Expand(d.Toolchain.Strip, sdk, "")
	if err != nil {
		return err
	}
	how := "--strip-all"
	if d.System == "mac" {
		how = "-x"
	}
	for _, name := range files[:2] {
		if err := e.Run("", nil, strip, how, filepath.Join(root, name)); err != nil {
			return err
		}
		if err := e.BinaryCheck(filepath.Join(root, name), target); err != nil {
			return err
		}
	}
	for _, c := range [][2]string{{"LICENSE", "LICENSE"}, {"README.md", "README.md"}, {"third_party/README.md", "ENGINE-SOURCE.md"}} {
		if err := copyFile(filepath.Join(e.Root, c[0]), filepath.Join(root, c[1])); err != nil {
			return err
		}
	}
	for _, c := range [][2]string{{filepath.Join(e.Root, "proto"), "proto"}, {filepath.Join(e.Root, "specs"), "specs"}, {filepath.Join(sdk, "notices"), "notices/libchromium"}} {
		if err := copyTree(c[0], filepath.Join(root, c[1])); err != nil {
			return err
		}
	}
	if err := os.MkdirAll(filepath.Join(root, "notices", "libvpx"), 0o755); err != nil {
		return err
	}
	for _, name := range []string{"LICENSE", "PATENTS", "AUTHORS"} {
		if err := copyFile(filepath.Join(libvpxSource, name), filepath.Join(root, "notices", "libvpx", name)); err != nil {
			return err
		}
	}
	if err := BundleNotices(e.Root, filepath.Join(root, "notices", "crates"), e.Targets[target].RustTarget); err != nil {
		return err
	}
	readme := fmt.Sprintf("Notices for Lanescape browser (%s).\n\n"+
		"LICENSE        Lanescape browser's own licence, beside this directory.\n"+
		"libchromium/   libchromium %s: Chromium's licence, the credits of every component it links,\n"+
		"               and where its corresponding source is. libchromium.so (or .dylib), libffmpeg and the\n"+
		"               other libraries beside the executable are its files.\n"+
		"libvpx/        libvpx %s, linked into the executable for the recorder's VP8 encoder.\n"+
		"crates/        The Rust crates linked into the host library, and packages.json, their inventory.\n",
		target, d.Version, libvpx.Tag)
	if err := os.WriteFile(filepath.Join(root, "notices", "README"), []byte(readme), 0o644); err != nil {
		return err
	}
	pkg := filepath.Join(dist, "lanescape-browser-"+target+".tar.gz")
	if err := archive.Pack(root, pkg, filepath.Base(root)); err != nil {
		return err
	}
	fmt.Fprintf(e.out(), "packaged %s\n", pkg)
	if o.Tests {
		apple := ""
		if d.System == "mac" {
			if apple, err = e.AppleSDK(target, o.AppleSDK); err != nil {
				return err
			}
		}
		if err := e.buildTests(target, d, sdk, apple, out); err != nil {
			return err
		}
	}
	// What it shipped beside the build, such as proto/, and the tests, came
	// from the checkout as it is now.
	again, err := e.identity(d, sdk)
	if err != nil {
		return err
	}
	if again != *built {
		return errors.New("the sources or the SDK changed while packaging: build and package again")
	}
	return nil
}

// Pin pins a libchromium version: every part this repository's targets
// build from, with its SHA-256 from the release's manifest, and the
// manifest's own SHA-256, which publishing holds the release's source to
// (specs/0004-releases.md, Publishing). Review the manifest when moving the
// pin: it lists the corresponding source.
func (e *Env) Pin(version string, client *http.Client) error {
	if client == nil {
		client = e.http()
	} else {
		c := *client
		c.CheckRedirect = fetch.NoReferer
		client = &c
	}
	// Redirected to a signed URL, which no error may name.
	what := fmt.Sprintf("fetching libchromium %s's manifest", version)
	resp, err := client.Get(e.releases() + "/v" + version + "/manifest.json")
	if err != nil {
		return fetch.Error(what, err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("%s: status %d", what, resp.StatusCode)
	}
	raw, err := io.ReadAll(resp.Body)
	if err != nil {
		return fetch.Error(what, err)
	}
	var manifest struct {
		Assets []struct {
			Name   string `json:"name"`
			Part   string `json:"part"`
			SHA256 string `json:"sha256"`
		} `json:"assets"`
		Source struct {
			Chromium struct {
				Tarball string `json:"tarball"`
			} `json:"chromium"`
		} `json:"source"`
	}
	if err := json.Unmarshal(raw, &manifest); err != nil {
		return fmt.Errorf("libchromium %s's manifest: %w", version, err)
	}
	sources := 0
	listed := map[string]string{}
	for _, a := range manifest.Assets {
		listed[a.Name] = a.SHA256
		if a.Part == "source" {
			sources++
		}
	}
	if sources == 0 || manifest.Source.Chromium.Tarball == "" {
		return fmt.Errorf("libchromium %s publishes no corresponding source", version)
	}
	order, err := repo.Order(e.Root)
	if err != nil {
		return err
	}
	// Written in the targets' and parts' own order, as a reader expects.
	var assets bytes.Buffer
	first := true
	for _, target := range order {
		for _, part := range e.PartsOf(target) {
			name := AssetName(version, target, part)
			sum, ok := listed[name]
			if !ok {
				return fmt.Errorf("libchromium %s has no %s", version, name)
			}
			if !first {
				assets.WriteString(",\n")
			}
			first = false
			k, _ := json.Marshal(name)
			v, _ := json.Marshal(sum)
			fmt.Fprintf(&assets, "    %s: %s", k, v)
		}
	}
	s := sha256.Sum256(raw)
	vj, _ := json.Marshal(version)
	text := fmt.Sprintf("{\n  \"version\": %s,\n  \"manifest\": \"%s\",\n  \"assets\": {\n%s\n  }\n}\n", vj, hex.EncodeToString(s[:]), assets.String())
	if err := os.WriteFile(filepath.Join(e.Root, "engine", "libchromium.json"), []byte(text), 0o644); err != nil {
		return err
	}
	fmt.Fprintf(e.out(), "pinned libchromium %s\n", version)
	return nil
}

// cargoMetadata is what notices read of `cargo metadata`.
type cargoMetadata struct {
	Packages []cargoPackage `json:"packages"`
	Resolve  struct {
		Root  string `json:"root"`
		Nodes []struct {
			ID   string `json:"id"`
			Deps []struct {
				Pkg      string `json:"pkg"`
				DepKinds []struct {
					Kind *string `json:"kind"`
				} `json:"dep_kinds"`
			} `json:"deps"`
		} `json:"nodes"`
	} `json:"resolve"`
}

type cargoPackage struct {
	ID           string  `json:"id"`
	Name         string  `json:"name"`
	Version      string  `json:"version"`
	Source       *string `json:"source"`
	ManifestPath string  `json:"manifest_path"`
	License      *string `json:"license"`
	Repository   *string `json:"repository"`
}

// RuntimePackages are the packages linked into the root: reached through
// normal dependencies, not build or dev ones.
func RuntimePackages(m cargoMetadata) []cargoPackage {
	nodes := map[string]int{}
	for i, n := range m.Resolve.Nodes {
		nodes[n.ID] = i
	}
	selected := map[string]bool{}
	pending := []string{m.Resolve.Root}
	for len(pending) > 0 {
		p := pending[len(pending)-1]
		pending = pending[:len(pending)-1]
		if selected[p] {
			continue
		}
		selected[p] = true
		for _, dep := range m.Resolve.Nodes[nodes[p]].Deps {
			for _, k := range dep.DepKinds {
				if k.Kind == nil {
					pending = append(pending, dep.Pkg)
					break
				}
			}
		}
	}
	var out []cargoPackage
	for _, p := range m.Packages {
		if selected[p.ID] {
			out = append(out, p)
		}
	}
	return out
}

// BundleNotices copies the licences of the crates linked for a platform, by
// default the one building, into destination, with packages.json, their
// inventory.
func BundleNotices(root, destination, platform string) error {
	if platform == "" {
		out, err := exec.Command("rustc", "-vV").Output()
		if err != nil {
			return err
		}
		for _, line := range strings.Split(string(out), "\n") {
			if h, ok := strings.CutPrefix(line, "host: "); ok {
				platform = strings.TrimSpace(h)
			}
		}
	}
	cmd := exec.Command("cargo", "metadata", "--locked", "--manifest-path", filepath.Join(root, "Cargo.toml"),
		"--filter-platform", platform, "--format-version=1")
	cmd.Dir, cmd.Stderr = root, os.Stderr
	out, err := cmd.Output()
	if err != nil {
		return fmt.Errorf("cargo metadata: %w", err)
	}
	var m cargoMetadata
	if err := json.Unmarshal(out, &m); err != nil {
		return err
	}
	return bundle(m, destination)
}

func bundle(m cargoMetadata, destination string) error {
	type entry struct {
		License    *string `json:"license"`
		Name       string  `json:"name"`
		Repository *string `json:"repository"`
		Version    string  `json:"version"`
	}
	inventory := []entry{}
	for _, p := range RuntimePackages(m) {
		if p.Source == nil {
			continue
		}
		dir := filepath.Dir(p.ManifestPath)
		entries, err := os.ReadDir(dir)
		if err != nil {
			return err
		}
		var files []string
		for _, e := range entries {
			upper := strings.ToUpper(e.Name())
			st, err := os.Stat(filepath.Join(dir, e.Name()))
			if err == nil && st.Mode().IsRegular() && (strings.HasPrefix(upper, "LICENSE") || strings.HasPrefix(upper, "NOTICE") ||
				strings.HasPrefix(upper, "COPYING") || strings.HasPrefix(upper, "UNLICENSE")) {
				files = append(files, e.Name())
			}
		}
		if len(files) == 0 {
			return fmt.Errorf("missing dependency license: %s", p.Name)
		}
		target := filepath.Join(destination, p.Name+"-"+p.Version)
		if err := os.MkdirAll(target, 0o755); err != nil {
			return err
		}
		for _, f := range files {
			if err := copyFile(filepath.Join(dir, f), filepath.Join(target, f)); err != nil {
				return err
			}
		}
		inventory = append(inventory, entry{License: p.License, Name: p.Name, Repository: p.Repository, Version: p.Version})
	}
	if err := os.MkdirAll(destination, 0o755); err != nil {
		return err
	}
	data, err := json.MarshalIndent(inventory, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(destination, "packages.json"), append(data, '\n'), 0o644)
}
