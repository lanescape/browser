// Package ci is what CI runs besides building, on the machine of a job's
// own: the checks, the test machines' packages, and the suites, which run
// the package itself, unpacked as a machine that didn't build it gets it,
// never the build's own out/.
package ci

import (
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"syscall"
	"time"

	"github.com/lanescape/browser/tools/internal/archive"
	"github.com/lanescape/browser/tools/internal/build"
	"github.com/lanescape/browser/tools/internal/stop"
	"github.com/lanescape/browser/tools/internal/version"
)

var (
	// Suites are the suites in tests/, run after the unit tests.
	Suites       = []string{"ime", "clipboard", "host", "display", "input", "recording"}
	brewPackages = []string{"zstd", "pigz", "ffmpeg"}
	// testPackages are what Chromium's libraries and the recording tests
	// need on a machine that didn't build them, beyond the runner service's
	// base image, which has zstd and pigz (ci/README.md, Where it runs).
	testPackages = []string{"file", "ffmpeg", "fonts-liberation", "fonts-dejavu-core", "libglib2.0-0t64",
		"libnspr4", "libnss3", "libatk1.0-0t64", "libatk-bridge2.0-0t64", "libatspi2.0-0t64",
		"libdbus-1-3", "libx11-6", "libxcomposite1", "libxdamage1", "libxext6", "libxfixes3",
		"libxrandr2", "libxcb1", "libxkbcommon0", "libgbm1", "libexpat1", "libudev1", "libasound2t64",
		"libcairo2", "libpango-1.0-0", "libcups2t64"}
)

const mac = runtime.GOOS == "darwin"

// CI runs in a repository.
type CI struct {
	Env *build.Env
	Out io.Writer
}

func (c *CI) out() io.Writer {
	if c.Out != nil {
		return c.Out
	}
	return os.Stdout
}

// summary prints a line, and adds it to the job's summary.
func (c *CI) summary(line string) {
	fmt.Fprintln(c.out(), line)
	if path := os.Getenv("GITHUB_STEP_SUMMARY"); path != "" {
		if f, err := os.OpenFile(path, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644); err == nil {
			fmt.Fprintf(f, "- %s\n", line)
			f.Close()
		}
	}
}

// RunFor runs a command in a process group of its own for at most d, then
// kills what's left of the group, unless the tool is stopping, when the
// shutdown ends it. It's the command's exit status, or 124 if it ran out
// of time.
func RunFor(d time.Duration, dir string, env []string, name string, args ...string) int {
	cmd := exec.Command(name, args...)
	cmd.Dir, cmd.Env = dir, env
	cmd.Stdout, cmd.Stderr = os.Stdout, os.Stderr
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	if err := stop.Start(cmd); err != nil {
		if errors.Is(err, stop.ErrStopping) {
			return stop.Status()
		}
		fmt.Fprintln(os.Stderr, err)
		return 127
	}
	done := make(chan error, 1)
	go func() { done <- cmd.Wait() }()
	timer := time.NewTimer(d)
	defer timer.Stop()
	var err error
	timedOut := false
	select {
	case err = <-done:
	case <-timer.C:
		timedOut = true
	}
	// Whether it finished or not, what's left of its group is killed now,
	// unless the tool is stopping: then it's left to the shutdown's grace
	// and kill, which may end it after RunFor has returned.
	stop.EndGroup(cmd.Process.Pid)
	if timedOut {
		<-done
		fmt.Printf("stopped after %d s\n", int(d.Seconds()))
		return 124
	}
	var exit *exec.ExitError
	if errors.As(err, &exit) {
		if status := exit.ExitCode(); status >= 0 {
			return status
		}
		return 128
	}
	return 0
}

// Checks are what needs no build. One machine runs them.
func (c *CI) Checks() error {
	root := c.Env.Root
	for _, step := range [][]string{
		{"go", "vet", "./..."},
		{"go", "test", "-race", "./..."},
		{"cargo", "fmt", "--check"},
		{"cargo", "clippy", "--locked", "--all-targets", "--", "-D", "warnings"},
		{"go", "run", "github.com/rhysd/actionlint/cmd/actionlint@v1.7.12"},
	} {
		if err := c.Env.Run(root, nil, step[0], step[1:]...); err != nil {
			return err
		}
		if step[1] == "vet" {
			// gofmt, after vet, so a file that doesn't parse is vet's to say.
			out, err := stop.Output(exec.Command("gofmt", "-l", root))
			if err != nil {
				return fmt.Errorf("gofmt: %w", err)
			}
			if s := strings.TrimSpace(string(out)); s != "" {
				return fmt.Errorf("these files aren't formatted with gofmt:\n%s", s)
			}
		}
	}
	return nil
}

// InstallTestPackages installs what the suites need on a CI test machine:
// one of the runner service's, beyond its base image, or a Mac.
func (c *CI) InstallTestPackages() error {
	if mac {
		var missing []string
		for _, name := range brewPackages {
			if stop.Run(exec.Command("brew", "list", name)) != nil {
				missing = append(missing, name)
			}
		}
		if len(missing) > 0 {
			return c.Env.Run("", append(os.Environ(), "HOMEBREW_NO_AUTO_UPDATE=1"), "brew", append([]string{"install"}, missing...)...)
		}
		return nil
	}
	env := append(os.Environ(), "DEBIAN_FRONTEND=noninteractive")
	if err := c.Env.Run("", env, "sudo", "-E", "apt-get", "update", "-q"); err != nil {
		return err
	}
	return c.Env.Run("", env, "sudo", append([]string{"-E", "apt-get", "install", "-yq", "--no-install-recommends"}, testPackages...)...)
}

// MissingLibraries are the libraries a binary needs that this machine
// lacks. On macOS everything the package doesn't carry has to be the
// system's own, since nothing else is on a machine it's unpacked on.
func MissingLibraries(binary string) ([]string, error) {
	if !mac {
		out, err := stop.Output(exec.Command("ldd", binary))
		if err != nil {
			return nil, fmt.Errorf("ldd %s: %w", binary, err)
		}
		var missing []string
		for _, line := range strings.Split(string(out), "\n") {
			if strings.Contains(line, "not found") {
				missing = append(missing, strings.TrimSpace(line))
			}
		}
		return missing, nil
	}
	out, err := stop.Output(exec.Command("otool", "-L", binary))
	if err != nil {
		return nil, fmt.Errorf("otool -L %s: %w", binary, err)
	}
	lines := strings.Split(strings.TrimSpace(string(out)), "\n")
	var missing []string
	for _, line := range lines[1:] {
		name, _, _ := strings.Cut(line, " (compatibility")
		name = strings.TrimSpace(name)
		ok := false
		for _, prefix := range []string{"/usr/lib/", "/System/Library/", "@rpath/", "@loader_path/"} {
			ok = ok || strings.HasPrefix(name, prefix)
		}
		if !ok {
			missing = append(missing, name)
		}
	}
	return missing, nil
}

// Unpacked is the package in dist/ and its test executables, unpacked and
// checked.
type Unpacked struct {
	Root, Binary, Tests string
	Env                 []string
}

// Unpack unpacks the target's package and tests; Close removes them.
func (c *CI) Unpack(target string) (*Unpacked, error) {
	dist := filepath.Join(c.Env.Root, "dist")
	home, _ := os.UserHomeDir()
	root, err := os.MkdirTemp(home, "")
	if err != nil {
		return nil, err
	}
	u := &Unpacked{Root: root, Binary: filepath.Join(root, "lanescape-browser", "lanescape-browser"), Tests: filepath.Join(root, "tests")}
	fail := func(err error) (*Unpacked, error) { os.RemoveAll(root); return nil, err }
	if err := archive.Unpack(filepath.Join(dist, "lanescape-browser-"+target+".tar.gz"), root); err != nil {
		return fail(err)
	}
	tests, err := os.Open(filepath.Join(dist, "tests-"+target+".tar"))
	if err != nil {
		return fail(err)
	}
	err = archive.Extract(tests, root)
	tests.Close()
	if err != nil {
		return fail(err)
	}
	if err := c.Env.BinaryCheck(u.Binary, target); err != nil {
		return fail(err)
	}
	missing, err := MissingLibraries(u.Binary)
	if err != nil {
		return fail(err)
	}
	if len(missing) > 0 {
		return fail(fmt.Errorf("the package is missing libraries: %v", missing))
	}
	if !mac {
		if _, err := os.Stat("/proc/sys/kernel/apparmor_restrict_unprivileged_userns"); err == nil {
			// Ubuntu's default stops the sandbox; a package would ship an
			// AppArmor profile instead.
			if err := c.Env.Run("", nil, "sudo", "sysctl", "-q", "-w", "kernel.apparmor_restrict_unprivileged_userns=0"); err != nil {
				return fail(err)
			}
		}
	}
	u.Env = append(os.Environ(), "LANESCAPE_BROWSER_BINARY="+u.Binary)
	return u, nil
}

// Close removes what was unpacked.
func (u *Unpacked) Close() { os.RemoveAll(u.Root) }

// ExpectedVersion is the line --version must print: the package major of
// Cargo.toml, then the build, or development without one.
func ExpectedVersion(build, cargoToml string) (string, error) {
	major, err := version.ReleaseMajor(cargoToml)
	if err != nil {
		return "", err
	}
	if build == "" {
		build = "development"
	}
	return "lanescape-browser " + major[1:] + "." + build, nil
}

// versionTimeout is how long --version may take.
var versionTimeout = 60 * time.Second

// CheckVersion is that the package's --version is exactly its major and
// build, or development when it has none.
func (c *CI) CheckVersion(binary, build string) error {
	cargo, err := version.ReadCargo(c.Env.Root)
	if err != nil {
		return err
	}
	want, err := ExpectedVersion(build, cargo)
	if err != nil {
		return err
	}
	// At most a minute, in a process group of its own that is stopped
	// whole, and its output is waited for only briefly after it exits or
	// is stopped, in case something it started still holds it.
	ctx, cancel := context.WithTimeout(context.Background(), versionTimeout)
	defer cancel()
	cmd := exec.CommandContext(ctx, binary, "--version")
	cmd.Stderr = os.Stderr
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	cmd.Cancel = func() error { return stop.EndGroup(cmd.Process.Pid) }
	cmd.WaitDelay = 5 * time.Second
	out, err := stop.Output(cmd)
	if cmd.Process != nil {
		stop.EndGroup(cmd.Process.Pid)
	}
	fmt.Fprint(c.out(), string(out))
	got := strings.TrimSpace(string(out))
	if err != nil || got != want {
		return fmt.Errorf("--version printed %q (%v), not %q", got, err, want)
	}
	return nil
}

// Suites runs the unit tests and every suite against the package itself,
// reporting each failure.
func (c *CI) Suites(target, build string) error {
	u, err := c.Unpack(target)
	if err != nil {
		return err
	}
	defer u.Close()
	if err := c.CheckVersion(u.Binary, build); err != nil {
		return err
	}
	type run struct {
		name  string
		extra []string
	}
	runs := []run{{"lanescape_browser_host", nil}}
	for _, s := range Suites {
		runs = append(runs, run{s, []string{"--test-threads=6"}})
	}
	var failed []string
	for _, r := range runs {
		if err := stop.Err(); err != nil {
			return err
		}
		fmt.Fprintf(c.out(), "::group::%s\n", r.name)
		status := RunFor(900*time.Second, u.Tests, u.Env, filepath.Join(u.Tests, r.name), r.extra...)
		fmt.Fprintln(c.out(), "::endgroup::")
		result := "passed"
		if status != 0 {
			result = fmt.Sprintf("FAILED (%d)", status)
			failed = append(failed, r.name)
		}
		c.summary(fmt.Sprintf("%s %s: %s", target, r.name, result))
	}
	if len(failed) > 0 {
		return fmt.Errorf("failing suites: %s", strings.Join(failed, ", "))
	}
	return nil
}

// Launch is what a build must do even where the suites don't run on it:
// report its version, then start Chromium, draw a page and shut down
// cleanly. --version alone returns before Chromium starts.
func (c *CI) Launch(target, build string) error {
	u, err := c.Unpack(target)
	if err != nil {
		return err
	}
	defer u.Close()
	if err := c.CheckVersion(u.Binary, build); err != nil {
		return err
	}
	status := RunFor(300*time.Second, u.Tests, u.Env, filepath.Join(u.Tests, "host"), "--exact", "starts_draws_and_stops")
	result := "passed"
	if status != 0 {
		result = fmt.Sprintf("FAILED (%d)", status)
	}
	c.summary(fmt.Sprintf("%s launch check: %s", target, result))
	if status != 0 {
		return errors.New("the launch check failed")
	}
	return nil
}
