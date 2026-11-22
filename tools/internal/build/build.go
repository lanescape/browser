package build

import (
	"archive/tar"
	"bufio"
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"syscall"

	"github.com/lanescape/browser/tools/internal/archive"
	"github.com/lanescape/browser/tools/internal/pyjson"
	"github.com/lanescape/browser/tools/internal/repo"
)

const (
	// Executable is the engine's executable.
	Executable = "lanescape-browser"
	mojom      = "engine/lanescape_browser.mojom"
)

var (
	// Sources are the engine's own sources, and PlatformSources each
	// system's.
	Sources         = []string{"app.cc", "browser.cc", "clipboard.cc", "main.cc", "recorder.cc", "renderer.cc", "tab.cc"}
	PlatformSources = map[string][]string{"linux": {"platform_linux.cc", "tab_view_aura.cc"}, "mac": {"platform_mac.mm", "tab_view_mac.mm"}}
	frameworks      = []string{"AppKit", "Foundation", "QuartzCore"}
	// ownFlags are the engine's own flags: optimised with line tables, as
	// one build serves development and release.
	ownFlags = []string{"-O2", "-g1", "-Wall", "-Wno-unused-parameter"}
	// libvpxOptions build libvpx for VP8 alone: the recorder's encoder
	// (engine/recorder.cc). Static, with every symbol hidden, so none of it
	// is exported beside Chromium's own copy in the library.
	libvpxOptions = []string{"--disable-vp9", "--disable-vp8-decoder", "--enable-realtime-only", "--disable-examples",
		"--disable-tools", "--disable-docs", "--disable-unit-tests", "--disable-shared", "--enable-static",
		"--enable-pic", "--disable-install-bins", "--disable-install-srcs", "--disable-webm-io",
		"--disable-libyuv"}
	// sourcesOfABuild are what a build compiles from, which its package must
	// match: the engine, the host and its protocol, its tests and what it
	// ships beside them.
	sourcesOfABuild = []string{"engine", "src", "proto", "tests", "specs", "build.rs", "Cargo.toml", "Cargo.lock",
		"rust-toolchain.toml", "LICENSE", "README.md", "third_party/README.md"}
)

func sortStrings(s []string) { sort.Strings(s) }

// LibvpxHow keys what a libvpx build depends on beyond the commit, the SDK
// and the target: its options, and the Apple SDK, by its pin and its path.
func (e *Env) LibvpxHow(target, apple string) string {
	t := e.Targets[target]
	var appleSHA, appleDir any
	if t.AppleSDK != nil {
		appleSHA = t.AppleSDK.SHA256
	}
	if apple != "" {
		appleDir = apple
	}
	return shortSHA(pyjson.Dumps([]any{libvpxOptions, t.LibvpxTarget, appleSHA, appleDir}), 12)
}

// BuildLibvpx is libvpx, pinned in engine/libvpx.json, built for the target
// with the SDK's clang: a static library and its headers, under the cache,
// keyed by libvpx's commit, the SDK's identity and the target.
func (e *Env) BuildLibvpx(target, sdk string, d Description, apple string) (string, error) {
	pin, pinJSON, err := repo.LibvpxPin(e.Root)
	if err != nil {
		return "", err
	}
	how := e.LibvpxHow(target, apple)
	identity, err := SDKIdentity(sdk)
	if err != nil {
		return "", err
	}
	prefix := filepath.Join(e.Cache(), "libvpx", pin.Commit+"-"+identity[:12]+"-"+target+"-"+how)
	if _, err := os.Stat(filepath.Join(prefix, ".complete")); err == nil {
		return prefix, nil
	}
	source := filepath.Join(e.Cache(), "src", "libvpx-"+pin.Commit)
	if st, err := os.Stat(source); err != nil || !st.IsDir() {
		if err := os.MkdirAll(filepath.Dir(source), 0o755); err != nil {
			return "", err
		}
		staging, err := os.MkdirTemp(filepath.Dir(source), ".libvpx-")
		if err != nil {
			return "", err
		}
		defer os.RemoveAll(staging)
		if err := e.Run("", nil, "git", "clone", "--quiet", "--depth", "1", "--branch", pin.Tag, pin.Repository, filepath.Join(staging, "src")); err != nil {
			return "", err
		}
		head, err := exec.Command("git", "-C", filepath.Join(staging, "src"), "rev-parse", "HEAD").Output()
		if err != nil {
			return "", err
		}
		if strings.TrimSpace(string(head)) != pin.Commit {
			return "", fmt.Errorf("libvpx %s is %s, not the %s engine/libvpx.json pins", pin.Tag, strings.TrimSpace(string(head)), pin.Commit)
		}
		if err := settle(filepath.Join(staging, "src"), source, filepath.Join(source, "configure")); err != nil {
			return "", err
		}
	}
	clang, err := ClangTarget(d)
	if err != nil {
		return "", err
	}
	flags := []string{"--target=" + clang, "-fvisibility=hidden", "-O2"}
	if d.Sysroot != nil && *d.Sysroot != "" {
		sysroot, err := Expand(*d.Sysroot, sdk, apple)
		if err != nil {
			return "", err
		}
		flags = append(flags, "--sysroot="+sysroot)
	}
	if d.System == "mac" {
		flags = append(flags, "-isysroot", apple)
	}
	joined := strings.Join(flags, " ")
	tools := map[string]string{}
	for name, v := range map[string]string{"cc": d.Toolchain.CC, "cxx": d.Toolchain.CXX, "ar": d.Toolchain.AR, "nm": d.Toolchain.NM, "strip": d.Toolchain.Strip} {
		if tools[name], err = Expand(v, sdk, apple); err != nil {
			return "", err
		}
	}
	env := append(os.Environ(), "CC="+tools["cc"]+" "+joined, "CXX="+tools["cxx"]+" "+joined, "AR="+tools["ar"],
		"NM="+tools["nm"], "STRIP="+tools["strip"], "LD="+tools["cc"]+" "+joined+" -fuse-ld=lld")
	// Installed into a staging directory, which becomes the cache's only
	// once it's whole.
	if err := os.MkdirAll(filepath.Dir(prefix), 0o755); err != nil {
		return "", err
	}
	staging, err := os.MkdirTemp(filepath.Dir(prefix), "."+filepath.Base(prefix)+"-")
	if err != nil {
		return "", err
	}
	defer os.RemoveAll(staging)
	work, err := os.MkdirTemp("", "libvpx-build-")
	if err != nil {
		return "", err
	}
	defer os.RemoveAll(work)
	args := append([]string{"--target=" + e.Targets[target].LibvpxTarget, "--prefix=" + staging}, libvpxOptions...)
	if err := e.Run(work, env, filepath.Join(source, "configure"), args...); err != nil {
		return "", err
	}
	if err := e.Run(work, env, "make", "-j"+strconv.Itoa(runtime.NumCPU())); err != nil {
		return "", err
	}
	if err := e.Run(work, env, "make", "install"); err != nil {
		return "", err
	}
	if st, err := os.Stat(filepath.Join(staging, "lib", "libvpx.a")); err != nil || !st.Mode().IsRegular() {
		return "", errors.New("libvpx installed no lib/libvpx.a")
	}
	if err := os.WriteFile(filepath.Join(staging, ".complete"), pinJSON, 0o644); err != nil {
		return "", err
	}
	if err := settle(staging, prefix, filepath.Join(prefix, ".complete")); err != nil {
		return "", err
	}
	return prefix, nil
}

// GenerateMojom generates the engine's mojom bindings as Chromium's build
// would (libchromium's specs/0001-sdk.md, Mojom), with the SDK's own
// generator, which is Chromium's, in Python.
func (e *Env) GenerateMojom(d Description, sdk, gen string) ([]string, error) {
	m := d.Mojom
	ex := func(v string) (string, error) { return Expand(v, sdk, "") }
	parser, err := ex(m.Parser)
	if err != nil {
		return nil, err
	}
	generator, err := ex(m.Generator)
	if err != nil {
		return nil, err
	}
	modules, err := ex(m.Modules)
	if err != nil {
		return nil, err
	}
	typemap, err := ex(m.TypeMappings)
	if err != nil {
		return nil, err
	}
	imports, err := expandAll(m.Imports, sdk, "")
	if err != nil {
		return nil, err
	}
	templates := filepath.Join(gen, "mojom-templates")
	if err := os.MkdirAll(templates, 0o755); err != nil {
		return nil, err
	}
	python := []string{"-B"}
	args := append(append([]string(nil), python...), parser, "--input-root", e.Root)
	for _, root := range imports {
		args = append(args, "--input-root", root)
	}
	args = append(args, "--module-root", modules, "--output-root", gen, "--mojoms", filepath.Join(e.Root, mojom))
	for _, feature := range m.Features {
		args = append(args, "--enable-feature", feature)
	}
	if err := e.Run("", nil, "python3", args...); err != nil {
		return nil, err
	}
	if err := e.Run("", nil, "python3", append(append([]string(nil), python...), generator, "-o", templates, "--use_bundled_pylibs", "precompile", "-g", "c++")...); err != nil {
		return nil, err
	}
	generate := append(append([]string(nil), python...), generator, "--use_bundled_pylibs", "-o", gen, "generate", "-d", e.Root, "-I", e.Root,
		"--bytecode_path", templates, "-g", "c++")
	for _, extra := range [][]string{{"--generate_non_variant_code"}, {"--generate_non_variant_code", "--generate_message_ids"}, {"--typemap", typemap}} {
		if err := e.Run("", nil, "python3", append(append(append([]string(nil), generate...), extra...), filepath.Join(e.Root, mojom))...); err != nil {
			return nil, err
		}
	}
	return []string{filepath.Join(gen, mojom+".cc"), filepath.Join(gen, mojom+"-shared.cc")}, nil
}

func ninjaEscape(value string) string {
	return strings.NewReplacer("$", "$$", " ", "$ ", ":", "$:").Replace(value)
}

var shellSafe = regexp.MustCompile(`^[\w@%+=:,./-]+$`)

// ShellQuote is Python's shlex.quote of each value, joined by spaces.
func ShellQuote(values []string) string {
	quoted := make([]string, len(values))
	for i, v := range values {
		switch {
		case v == "":
			quoted[i] = "''"
		case shellSafe.MatchString(v):
			quoted[i] = v
		default:
			quoted[i] = "'" + strings.ReplaceAll(v, "'", `'"'"'`) + "'"
		}
	}
	return strings.Join(quoted, " ")
}

// ninjaCommand is arguments for a ninja rule's command: quoted for the
// shell, with ninja's own $ escaped, so that $ORIGIN reaches the linker as
// itself.
func ninjaCommand(values []string) string { return strings.ReplaceAll(ShellQuote(values), "$", "$$") }

// WriteNinja writes build.ninja for the engine: one compile for each
// source, with its dependencies from clang's depfile, and the link, so a
// change to one file recompiles that file and links.
func (e *Env) WriteNinja(path, target string, d Description, sdk, apple, libvpx, gen, out string, generated []string) error {
	system := d.System
	var common []string
	for _, define := range d.Defines {
		common = append(common, "-D"+define)
	}
	common = append(common, "-I"+e.Root, "-I"+gen, "-I"+filepath.Join(libvpx, "include"))
	includes, err := expandAll(d.IncludeDirs, sdk, apple)
	if err != nil {
		return err
	}
	for _, p := range includes {
		common = append(common, "-I"+p)
	}
	systemIncludes, err := expandAll(d.SystemIncludeDirs, sdk, apple)
	if err != nil {
		return err
	}
	for _, p := range systemIncludes {
		common = append(common, "-isystem", p)
	}
	common = append(common, ownFlags...)
	languages := []string{"c++"}
	if system == "mac" {
		languages = append(languages, "objective-c++")
	}
	flags := map[string][]string{}
	for _, l := range languages {
		if flags[l], err = expandAll(d.Compile[l], sdk, apple); err != nil {
			return err
		}
	}
	cxx, err := Expand(d.Toolchain.CXX, sdk, apple)
	if err != nil {
		return err
	}
	type source struct{ path, language string }
	var sources []source
	for _, name := range append(append([]string(nil), Sources...), PlatformSources[system]...) {
		language := "c++"
		if strings.HasSuffix(name, ".mm") {
			language = "objective-c++"
		}
		sources = append(sources, source{filepath.Join(e.Root, "engine", name), language})
	}
	for _, g := range generated {
		sources = append(sources, source{g, "c++"})
	}
	var vpx, rpath, fw []string
	if system == "mac" {
		// libvpx is hidden already; nothing more is needed to keep it out of
		// the executable's exports.
		vpx = []string{filepath.Join(libvpx, "lib", "libvpx.a")}
		rpath = []string{"-Wl,-rpath,@loader_path"}
		for _, name := range append(append([]string(nil), d.Link.Frameworks...), frameworks...) {
			fw = append(fw, "-framework", name)
		}
	} else {
		// Its maths comes from the C library's libm, which macOS's libSystem
		// holds.
		vpx = []string{filepath.Join(libvpx, "lib", "libvpx.a"), "-Wl,--exclude-libs,libvpx.a", "-lm"}
		rpath = []string{"-Wl,-rpath,$ORIGIN"}
	}
	linkFlags, err := expandAll(d.Link.Flags, sdk, apple)
	if err != nil {
		return err
	}
	libraries, err := expandAll(d.Link.Libraries, sdk, apple)
	if err != nil {
		return err
	}
	link := append(append([]string(nil), linkFlags...), vpx...)
	link = append(link, libraries...)
	for _, name := range d.Link.SystemLibs {
		link = append(link, "-l"+name)
	}
	link = append(append(link, fw...), rpath...)
	rule := func(language string) string {
		return strings.ReplaceAll(strings.ReplaceAll(language, "+", "x"), "-", "_")
	}
	lines := []string{"# Written by tools/browser for " + target + "; edit that, not this.", "ninja_required_version = 1.10",
		"cxx = " + ninjaEscape(cxx)}
	for _, l := range languages {
		lines = append(lines, "rule "+rule(l),
			"  command = $cxx -x "+l+" "+ninjaCommand(append(append([]string(nil), flags[l]...), common...))+" -MD -MF $out.d -c $in -o $out",
			"  depfile = $out.d", "  deps = gcc", "  description = "+l+" $in")
	}
	lines = append(lines, "rule link", "  command = $cxx $in "+ninjaCommand(link)+" -o $out", "  description = link $out")
	var objects []string
	for _, s := range sources {
		obj := filepath.Join(out, "obj", filepath.Base(s.path)+".o")
		objects = append(objects, ninjaEscape(obj))
		lines = append(lines, "build "+ninjaEscape(obj)+": "+rule(s.language)+" "+ninjaEscape(s.path))
	}
	linked := []string{ninjaEscape(filepath.Join(libvpx, "lib", "libvpx.a"))}
	for _, l := range libraries {
		linked = append(linked, ninjaEscape(l))
	}
	exe := ninjaEscape(filepath.Join(out, Executable))
	lines = append(lines, "build "+exe+": link "+strings.Join(objects, " ")+" | "+strings.Join(linked, " "), "default "+exe)
	return os.WriteFile(path, []byte(strings.Join(lines, "\n")+"\n"), 0o644)
}

// linkerWrapper is a linker for cargo: the SDK's clang for the target, with
// its sysroot or the Apple SDK.
func (e *Env) linkerWrapper(target string, d Description, sdk, apple, out string) (string, error) {
	clang, err := ClangTarget(d)
	if err != nil {
		return "", err
	}
	cc, err := Expand(d.Toolchain.CC, sdk, apple)
	if err != nil {
		return "", err
	}
	flags := []string{cc, "--target=" + clang, "-fuse-ld=lld"}
	if d.Sysroot != nil && *d.Sysroot != "" {
		sysroot, err := Expand(*d.Sysroot, sdk, apple)
		if err != nil {
			return "", err
		}
		flags = append(flags, "--sysroot="+sysroot)
	}
	if d.System == "mac" {
		flags = append(flags, "-isysroot", apple)
	}
	text := "#!/bin/sh\nexec " + ShellQuote(flags) + " \"$@\"\n"
	// Named for what it runs, so Cargo, which fingerprints the linker's
	// path, relinks when the SDK changes.
	wrapper := filepath.Join(out, "link-"+e.Targets[target].RustTarget+"-"+shortSHA(text, 12))
	if err := os.WriteFile(wrapper, []byte(text), 0o755); err != nil {
		return "", err
	}
	return wrapper, os.Chmod(wrapper, 0o755)
}

// cargoEnv is Cargo's environment for the target: the SDK's clang as its
// linker.
func (e *Env) cargoEnv(target string, d Description, sdk, apple, out string) ([]string, error) {
	rust := e.Targets[target].RustTarget
	if err := e.Run(e.Root, nil, "rustup", "target", "add", rust); err != nil {
		return nil, err
	}
	wrapper, err := e.linkerWrapper(target, d, sdk, apple, out)
	if err != nil {
		return nil, err
	}
	env := append(os.Environ(), "CARGO_TARGET_"+strings.ToUpper(strings.ReplaceAll(rust, "-", "_"))+"_LINKER="+wrapper)
	if apple != "" {
		// Rust finds the Apple SDK by SDKROOT, rather than with xcrun, which
		// Linux lacks.
		env = append(env, "SDKROOT="+apple)
	}
	return env, nil
}

// buildHost builds the Rust host library, for the target, beside the
// executable.
func (e *Env) buildHost(target string, d Description, sdk, apple, out string, release bool) error {
	rust := e.Targets[target].RustTarget
	env, err := e.cargoEnv(target, d, sdk, apple, out)
	if err != nil {
		return err
	}
	args := []string{"build", "--locked"}
	profile := "debug"
	if release {
		args, profile = append(args, "--release"), "release"
	}
	args = append(args, "--target", rust, "--manifest-path", filepath.Join(e.Root, "Cargo.toml"), "--target-dir", filepath.Join(out, "host"))
	if err := e.Run(e.Root, env, "cargo", args...); err != nil {
		return err
	}
	library := e.Targets[target].HostLibrary
	return copyFile(filepath.Join(out, "host", rust, profile, library), filepath.Join(out, library))
}

// ResourcePath is where a runtime resource the SDK lists goes beside the
// executable: its path under resources/, which it must be under. Install
// and package both place resources by it.
func ResourcePath(resource string) (string, error) {
	parts := strings.Split(resource, "/")
	if len(parts) < 2 || parts[0] != "resources" {
		return "", fmt.Errorf("runtime resource %s isn't under resources/", resource)
	}
	for _, p := range parts[1:] {
		if p == "" || p == "." || p == ".." {
			return "", fmt.Errorf("runtime resource %s isn't under resources/", resource)
		}
	}
	return strings.Join(parts[1:], "/"), nil
}

// installRuntime puts the SDK's runtime libraries and resources beside the
// executable, where Chromium looks by default.
func installRuntime(d Description, sdk, out string) error {
	for _, library := range d.Runtime.Libraries {
		if err := copyFile(filepath.Join(sdk, library), filepath.Join(out, filepath.Base(library))); err != nil {
			return err
		}
	}
	for _, resource := range d.Runtime.Resources {
		rel, err := ResourcePath(resource)
		if err != nil {
			return err
		}
		dest := filepath.Join(out, filepath.FromSlash(rel))
		if err := os.MkdirAll(filepath.Dir(dest), 0o755); err != nil {
			return err
		}
		if err := copyFile(filepath.Join(sdk, resource), dest); err != nil {
			return err
		}
	}
	return nil
}

// copyFile copies contents and mode, and keeps the modification time, as
// shutil.copy2 does.
func copyFile(src, dst string) error {
	info, err := os.Stat(src)
	if err != nil {
		return err
	}
	in, err := os.Open(src)
	if err != nil {
		return err
	}
	defer in.Close()
	os.Remove(dst)
	out, err := os.OpenFile(dst, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, info.Mode().Perm())
	if err != nil {
		return err
	}
	if _, err := io.Copy(out, in); err != nil {
		out.Close()
		return err
	}
	if err := out.Close(); err != nil {
		return err
	}
	if err := os.Chmod(dst, info.Mode().Perm()); err != nil {
		return err
	}
	return os.Chtimes(dst, info.ModTime(), info.ModTime())
}

// copyTree copies a directory's files, as shutil.copytree does by default:
// links, src among them, are followed, and what they point to copied.
func copyTree(src, dst string) error {
	info, err := os.Stat(src)
	if err != nil {
		return err
	}
	if !info.IsDir() {
		return copyFile(src, dst)
	}
	if err := os.MkdirAll(dst, 0o755); err != nil {
		return err
	}
	entries, err := os.ReadDir(src)
	if err != nil {
		return err
	}
	for _, e := range entries {
		if err := copyTree(filepath.Join(src, e.Name()), filepath.Join(dst, e.Name())); err != nil {
			return err
		}
	}
	return nil
}

// removeIfThere removes a file, which may already be gone, as
// Path.unlink(missing_ok=True) does: any other failure is one.
func removeIfThere(path string) error {
	if err := os.Remove(path); err != nil && !errors.Is(err, fs.ErrNotExist) {
		return err
	}
	return nil
}

// SDKFor is the SDK a command uses, given or pinned, and its description,
// which must be the target's.
func (e *Env) SDKFor(target, given string) (string, Description, error) {
	sdk := given
	if sdk != "" {
		sdk = Resolve(sdk)
	} else {
		pin, err := repo.Pinned(e.Root)
		if err != nil {
			return "", Description{}, err
		}
		if sdk, err = e.FetchSDK(target, pin); err != nil {
			return "", Description{}, err
		}
	}
	d, err := ReadDescription(sdk)
	if err != nil {
		return "", Description{}, err
	}
	if d.Target != target {
		return "", Description{}, fmt.Errorf("%s is the SDK for %s, not %s", sdk, d.Target, target)
	}
	return sdk, d, nil
}

// Identity is what a build was made from, which package checks against what
// it would ship.
type Identity struct {
	Libchromium string `json:"libchromium"`
	SDK         string `json:"sdk"`
	Target      string `json:"target"`
	Libvpx      string `json:"libvpx"`
	Sources     string `json:"sources"`
}

// sourceIdentity is the contents of every file a build compiles from or a
// package ships, as they are now, edits included.
func (e *Env) sourceIdentity() (string, error) {
	h := sha256.New()
	for _, name := range sourcesOfABuild {
		root := filepath.Join(e.Root, name)
		var files []string
		info, err := os.Stat(root)
		if err != nil {
			return "", err
		}
		if !info.IsDir() {
			files = []string{root}
		} else {
			err := filepath.WalkDir(root, func(path string, d fs.DirEntry, err error) error {
				if err != nil {
					return err
				}
				if st, err := os.Stat(path); err == nil && st.Mode().IsRegular() {
					files = append(files, path)
				}
				return nil
			})
			if err != nil {
				return "", err
			}
			sort.Strings(files)
		}
		for _, path := range files {
			sum, err := SHA256File(path)
			if err != nil {
				return "", err
			}
			rel, _ := filepath.Rel(e.Root, path)
			fmt.Fprintf(h, "%s\n%s\n", filepath.ToSlash(rel), sum)
		}
	}
	return hex.EncodeToString(h.Sum(nil)), nil
}

// identity is a build's identity.
func (e *Env) identity(d Description, sdk string) (Identity, error) {
	sdkID, err := SDKIdentity(sdk)
	if err != nil {
		return Identity{}, err
	}
	pin, _, err := repo.LibvpxPin(e.Root)
	if err != nil {
		return Identity{}, err
	}
	sources, err := e.sourceIdentity()
	if err != nil {
		return Identity{}, err
	}
	return Identity{Libchromium: d.Version, SDK: sdkID, Target: d.Target, Libvpx: pin.Commit, Sources: sources}, nil
}

// locked holds the output directory's lock while f runs, so that a package
// never reads a build another process is changing.
func locked(out string, f func() error) error {
	if err := os.MkdirAll(out, 0o755); err != nil {
		return err
	}
	lock, err := os.OpenFile(filepath.Join(out, ".lock"), os.O_CREATE|os.O_WRONLY, 0o644)
	if err != nil {
		return err
	}
	defer lock.Close()
	if err := syscall.Flock(int(lock.Fd()), syscall.LOCK_EX); err != nil {
		return err
	}
	defer syscall.Flock(int(lock.Fd()), syscall.LOCK_UN)
	return f()
}

// BuildOptions are build's.
type BuildOptions struct {
	Target   string
	Release  bool
	SDK      string
	AppleSDK string
}

// Build builds out/<target>/, ready to run.
func (e *Env) Build(o BuildOptions) error {
	if runtime.GOOS != "linux" || runtime.GOARCH != "amd64" {
		return errors.New("builds run on Linux x86-64, where the SDK's toolchain runs")
	}
	sdk, d, err := e.SDKFor(o.Target, o.SDK)
	if err != nil {
		return err
	}
	if d.Mojom == nil {
		return fmt.Errorf("libchromium %s has no mojom tooling; pin a newer version", d.Version)
	}
	apple := ""
	if d.System == "mac" {
		if apple, err = e.AppleSDK(o.Target, o.AppleSDK); err != nil {
			return err
		}
	}
	out := e.OutDir(o.Target)
	// libvpx's configure and the compile commands take these paths as they
	// are, unquoted.
	for _, p := range []string{sdk, apple, out, e.Cache()} {
		if strings.ContainsFunc(p, func(r rune) bool { return r == ' ' || r == '\t' || r == '\n' || r == '\r' || r == '\f' || r == '\v' }) {
			return fmt.Errorf("%s has whitespace in it, which libvpx's build can't take: use another path", p)
		}
	}
	err = locked(out, func() error {
		// Written again only once every step has succeeded, so a build that
		// stops part-way can't be packaged.
		if err := removeIfThere(filepath.Join(out, "build.json")); err != nil {
			return err
		}
		identity, err := e.identity(d, sdk)
		if err != nil {
			return err
		}
		libvpx, err := e.BuildLibvpx(o.Target, sdk, d, apple)
		if err != nil {
			return err
		}
		gen := filepath.Join(out, "gen")
		generated, err := e.GenerateMojom(d, sdk, gen)
		if err != nil {
			return err
		}
		if err := e.WriteNinja(filepath.Join(out, "build.ninja"), o.Target, d, sdk, apple, libvpx, gen, out, generated); err != nil {
			return err
		}
		if err := e.Run("", nil, "ninja", "-C", out); err != nil {
			return err
		}
		if err := e.buildHost(o.Target, d, sdk, apple, out, o.Release); err != nil {
			return err
		}
		if err := installRuntime(d, sdk, out); err != nil {
			return err
		}
		again, err := e.identity(d, sdk)
		if err != nil {
			return err
		}
		if again != identity {
			return errors.New("the sources or the SDK changed while building: build again")
		}
		data, _ := json.Marshal(identity)
		return os.WriteFile(filepath.Join(out, "build.json"), append(data, '\n'), 0o644)
	})
	if err != nil {
		return err
	}
	fmt.Fprintf(e.out(), "built %s\n", filepath.Join(out, Executable))
	return nil
}

// BinaryCheck is that a binary is for the target's system and processor, as
// `file` reads it.
func (e *Env) BinaryCheck(path, target string) error {
	t := e.Targets[target]
	kind, machine := "ELF", t.ELFMachine
	if t.System == "mac" {
		kind, machine = "Mach-O 64-bit", "arm64"
	}
	out, err := exec.Command("file", "-b", path).Output()
	if err != nil {
		return fmt.Errorf("file %s: %w", path, err)
	}
	description := string(out)
	norm := func(s string) string { return strings.ToLower(strings.ReplaceAll(s, "-", "")) }
	if !strings.HasPrefix(description, kind) || !strings.Contains(norm(description), norm(machine)) {
		return fmt.Errorf("%s is not %s for %s: %s", path, kind, machine, description)
	}
	return nil
}

// TestExecutables are the test executables in cargo's JSON messages, by
// their Cargo targets' names.
func TestExecutables(messages []byte) (map[string]string, error) {
	found := map[string]string{}
	sc := bufio.NewScanner(bytes.NewReader(messages))
	sc.Buffer(make([]byte, 1<<20), 64<<20)
	for sc.Scan() {
		var m struct {
			Reason  string `json:"reason"`
			Profile struct {
				Test bool `json:"test"`
			} `json:"profile"`
			Executable *string `json:"executable"`
			Target     struct {
				Name string `json:"name"`
			} `json:"target"`
		}
		if err := json.Unmarshal(sc.Bytes(), &m); err != nil {
			return nil, err
		}
		if m.Reason == "compiler-artifact" && m.Profile.Test && m.Executable != nil && *m.Executable != "" {
			found[m.Target.Name] = *m.Executable
		}
	}
	return found, sc.Err()
}

// buildTests is the unit tests' and suites' executables, cross-compiled for
// the target, in dist/tests-<target>.tar, each named for its Cargo target
// (`lanescape_browser_host` for the unit tests), for a machine without Rust.
func (e *Env) buildTests(target string, d Description, sdk, apple, out string) error {
	rust := e.Targets[target].RustTarget
	env, err := e.cargoEnv(target, d, sdk, apple, out)
	if err != nil {
		return err
	}
	cmd := exec.Command("cargo", "test", "--locked", "--no-run", "--message-format=json", "--target", rust,
		"--manifest-path", filepath.Join(e.Root, "Cargo.toml"), "--target-dir", filepath.Join(out, "host"))
	cmd.Dir, cmd.Env, cmd.Stderr = e.Root, env, os.Stderr
	messages, err := cmd.Output()
	if err != nil {
		return fmt.Errorf("cargo test --no-run: %w", err)
	}
	found, err := TestExecutables(messages)
	if err != nil {
		return err
	}
	names := make([]string, 0, len(found))
	for name := range found {
		names = append(names, name)
	}
	sort.Strings(names)
	if _, ok := found["lanescape_browser_host"]; !ok || len(found) < 2 {
		return fmt.Errorf("cargo built no test executables for %s: %v", target, names)
	}
	f, err := os.Create(filepath.Join(e.Root, "dist", "tests-"+target+".tar"))
	if err != nil {
		return err
	}
	defer f.Close()
	tw := tar.NewWriter(f)
	for _, name := range names {
		if err := e.BinaryCheck(found[name], target); err != nil {
			return err
		}
		if err := archive.WriteTree(tw, found[name], "tests/"+name); err != nil {
			return err
		}
	}
	if err := tw.Close(); err != nil {
		return err
	}
	fmt.Fprintf(e.out(), "packaged %d test executables in dist/tests-%s.tar\n", len(found), target)
	return f.Close()
}
