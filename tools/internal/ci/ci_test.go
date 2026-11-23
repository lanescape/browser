package ci

import (
	"context"
	"errors"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/lanescape/browser/tools/internal/build"
	"github.com/lanescape/browser/tools/internal/stop"
)

func TestSuitesExpectTheWholeVersionLine(t *testing.T) {
	cargo := "[package]\nversion=\"3.0.0\"\n"
	for build, want := range map[string]string{"20261122.1": "lanescape-browser 3.20261122.1", "": "lanescape-browser 3.development"} {
		if got, err := ExpectedVersion(build, cargo); got != want || err != nil {
			t.Errorf("ExpectedVersion(%q) = %q, %v", build, got, err)
		}
	}
}

// A package of another major or build is refused.
func TestSuitesRefuseAPackageOfAnotherMajorOrBuild(t *testing.T) {
	root, _ := filepath.Abs("../../..")
	e, err := build.NewEnv(root)
	if err != nil {
		t.Fatal(err)
	}
	c := &CI{Env: e, Out: &strings.Builder{}}
	cargo, _ := os.ReadFile(filepath.Join(root, "Cargo.toml"))
	want, _ := ExpectedVersion("", string(cargo))
	major := strings.TrimSuffix(strings.TrimPrefix(want, "lanescape-browser "), ".development")
	for _, tc := range []struct {
		printed, build string
		ok             bool
	}{
		{"lanescape-browser " + major + ".20261122.1", "20261122.1", true},
		{"lanescape-browser 999.20261122.1", "20261122.1", false},
		{"lanescape-browser " + major + ".20261122.2", "20261122.1", false},
		{"lanescape-browser " + major + ".development", "", true},
		{"lanescape-browser " + major + ".20261122.1", "", false},
	} {
		binary := filepath.Join(t.TempDir(), "lanescape-browser")
		os.WriteFile(binary, []byte("#!/bin/sh\necho \""+tc.printed+"\"\n"), 0o755)
		err := c.CheckVersion(binary, tc.build)
		if (err == nil) != tc.ok {
			t.Errorf("%q for build %q: %v", tc.printed, tc.build, err)
		}
	}
}

// RunFor stops a command that runs too long, and what it started.
func TestRunForStopsWhatRunsTooLong(t *testing.T) {
	marker := filepath.Join(t.TempDir(), "child")
	began := time.Now()
	status := RunFor(time.Second, "", nil, "/bin/sh", "-c", "(sleep 5; touch "+marker+") & sleep 30")
	if status != 124 || time.Since(began) > 10*time.Second {
		t.Fatalf("status %d after %s", status, time.Since(began))
	}
	time.Sleep(5 * time.Second)
	if _, err := os.Stat(marker); err == nil {
		t.Fatal("the command's child kept running")
	}
	if status := RunFor(10*time.Second, "", nil, "/bin/sh", "-c", "exit 3"); status != 3 {
		t.Fatalf("status %d", status)
	}
	// A command's own 124 is its status, not a timeout.
	began = time.Now()
	if status := RunFor(10*time.Second, "", nil, "/bin/sh", "-c", "exit 124"); status != 124 || time.Since(began) > 5*time.Second {
		t.Fatalf("status %d after %s", status, time.Since(began))
	}
	// What a command left running when it finished is stopped too.
	marker = filepath.Join(t.TempDir(), "left")
	if status := RunFor(10*time.Second, "", nil, "/bin/sh", "-c", "(sleep 2; touch "+marker+") & exit 0"); status != 0 {
		t.Fatalf("status %d", status)
	}
	time.Sleep(3 * time.Second)
	if _, err := os.Stat(marker); err == nil {
		t.Fatal("what the command left running kept running")
	}
}

// --version's deadline holds even when what the binary started keeps its
// output open.
func TestCheckVersionStopsAHungBinary(t *testing.T) {
	defer func(was time.Duration) { versionTimeout = was }(versionTimeout)
	versionTimeout = time.Second
	root, _ := filepath.Abs("../../..")
	e, err := build.NewEnv(root)
	if err != nil {
		t.Fatal(err)
	}
	c := &CI{Env: e, Out: &strings.Builder{}}
	for _, script := range []string{
		"#!/bin/sh\nsleep 30 &\nsleep 30\n",
		"#!/bin/sh\n(sleep 30) &\necho \"lanescape-browser x\"\n",
	} {
		binary := filepath.Join(t.TempDir(), "lanescape-browser")
		os.WriteFile(binary, []byte(script), 0o755)
		began := time.Now()
		if err := c.CheckVersion(binary, ""); err == nil {
			t.Fatal("passed")
		}
		if time.Since(began) > 10*time.Second {
			t.Fatalf("took %s", time.Since(began))
		}
	}
}

// TestMain runs the test binary as the tool for
// TestTheVersionTimeoutWaitsForAStoppingTool when CI_TEST_STOPPING_VERSION
// is set.
func TestMain(m *testing.M) {
	if os.Getenv("CI_TEST_STOPPING_VERSION") != "" {
		stoppingVersion()
		return
	}
	os.Exit(m.Run())
}

// stoppingVersion checks a --version that outlasts its deadline while the
// tool, which the test signals as it runs, is stopping.
func stoppingVersion() {
	stop.Grace = 4 * time.Second
	stop.Init()
	versionTimeout = time.Second
	e, err := build.NewEnv(os.Getenv("ROOT"))
	if err != nil {
		panic(err)
	}
	c := &CI{Env: e, Out: io.Discard}
	c.CheckVersion(os.Getenv("BINARY"), "")
	stop.Exit(1)
}

// TestTheVersionTimeoutWaitsForAStoppingTool: once the tool is stopping,
// --version's own deadline leaves the binary to the shutdown's grace,
// whose kill ends it, rather than killing it at once.
func TestTheVersionTimeoutWaitsForAStoppingTool(t *testing.T) {
	exe, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	root, _ := filepath.Abs("../../..")
	dir := t.TempDir()
	pidfile := filepath.Join(dir, "pid")
	binary := filepath.Join(dir, "lanescape-browser")
	os.WriteFile(binary, []byte("#!/bin/sh\ntrap \"\" INT TERM HUP\necho $$ >\""+pidfile+"\"\nexec sleep 30\n"), 0o755)
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	cmd := exec.CommandContext(ctx, exe)
	cmd.Env = append(os.Environ(), "CI_TEST_STOPPING_VERSION=1", "ROOT="+root, "BINARY="+binary)
	began := time.Now()
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	// One waiter, for the test and for its cleanup if it fails first.
	waited := make(chan error, 1)
	go func() { waited <- cmd.Wait() }()
	t.Cleanup(func() {
		cmd.Process.Kill()
		<-waited
	})
	var pid int
	for pid == 0 {
		if time.Since(began) > 10*time.Second {
			t.Fatal("the binary never started")
		}
		data, _ := os.ReadFile(pidfile)
		pid, _ = strconv.Atoi(strings.TrimSpace(string(data)))
		time.Sleep(20 * time.Millisecond)
	}
	// Only if it's still the process it is now, not a later one given its
	// PID.
	started := startOf(pid)
	t.Cleanup(func() {
		if started != "" && startOf(pid) == started {
			syscall.Kill(pid, syscall.SIGKILL)
		}
	})
	// --version's deadline started before the binary did; the grace starts
	// now.
	ready := time.Now()
	if err := cmd.Process.Signal(syscall.SIGTERM); err != nil {
		t.Fatal(err)
	}
	// Past --version's deadline, inside the grace.
	time.Sleep(time.Until(ready.Add(2 * time.Second)))
	if !running(pid) {
		t.Fatal("--version's deadline killed the binary during the grace")
	}
	err = <-waited
	waited <- err
	var exit *exec.ExitError
	if !errors.As(err, &exit) || exit.ExitCode() != 128+int(syscall.SIGTERM) {
		t.Fatalf("%v, want exit status %d", err, 128+int(syscall.SIGTERM))
	}
	deadline := time.Now().Add(2 * time.Second)
	for running(pid) {
		if time.Now().After(deadline) {
			t.Fatal("the binary outlived the tool")
		}
		time.Sleep(20 * time.Millisecond)
	}
}

// running is whether pid is a live process, not ended or merely unreaped.
func running(pid int) bool {
	if syscall.Kill(pid, 0) != nil {
		return false
	}
	out, err := exec.Command("ps", "-o", "stat=", "-p", strconv.Itoa(pid)).Output()
	return err == nil && !strings.HasPrefix(strings.TrimSpace(string(out)), "Z")
}

// startOf is when pid started, as ps says, or "" if it can't say.
func startOf(pid int) string {
	out, err := exec.Command("ps", "-o", "lstart=", "-p", strconv.Itoa(pid)).Output()
	if err != nil {
		return ""
	}
	return strings.TrimSpace(string(out))
}
