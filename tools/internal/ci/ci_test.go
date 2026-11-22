package ci

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/lanescape/browser/tools/internal/build"
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
}
