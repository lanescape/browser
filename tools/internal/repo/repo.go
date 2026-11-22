// Package repo finds the repository the tools work in, and reads what it
// pins: the targets (engine/targets.json) and the libchromium version
// (engine/libchromium.json).
package repo

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
)

// Root is the repository's root: LANESCAPE_BROWSER_REPO if set, else the
// nearest directory up from the working one that holds engine/targets.json.
func Root() (string, error) {
	if dir := os.Getenv("LANESCAPE_BROWSER_REPO"); dir != "" {
		abs, err := filepath.Abs(dir)
		if err != nil {
			return "", err
		}
		return resolved(abs), nil
	}
	dir, err := os.Getwd()
	if err != nil {
		return "", err
	}
	for {
		if _, err := os.Stat(filepath.Join(dir, "engine", "targets.json")); err == nil {
			return resolved(dir), nil
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			return "", errors.New("not in the lanescape/browser repository: no engine/targets.json above here")
		}
		dir = parent
	}
}

// resolved is dir with its links resolved, as Python's Path.resolve()
// gave the repository's root.
func resolved(dir string) string {
	if real, err := filepath.EvalSymlinks(dir); err == nil {
		return real
	}
	return dir
}

// MustRoot is Root, or exits.
func MustRoot() string {
	root, err := Root()
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	return root
}

// AppleSDK is a macOS target's pinned Apple SDK archive.
type AppleSDK struct {
	Object string `json:"object"`
	SHA256 string `json:"sha256"`
	Xcode  string `json:"xcode"`
	SDK    string `json:"sdk"`
}

// Target is one of engine/targets.json's targets.
type Target struct {
	System       string    `json:"system"`
	RustTarget   string    `json:"rust_target"`
	ELFMachine   string    `json:"elf_machine"`
	HostLibrary  string    `json:"host_library"`
	LibvpxTarget string    `json:"libvpx_target"`
	AppleSDK     *AppleSDK `json:"apple_sdk"`
}

// Targets reads engine/targets.json.
func Targets(root string) (map[string]Target, error) {
	data, err := os.ReadFile(filepath.Join(root, "engine", "targets.json"))
	if err != nil {
		return nil, err
	}
	targets := map[string]Target{}
	if err := json.Unmarshal(data, &targets); err != nil {
		return nil, fmt.Errorf("engine/targets.json: %w", err)
	}
	return targets, nil
}

// Names are the targets' names, sorted.
func Names(targets map[string]Target) []string {
	names := make([]string, 0, len(targets))
	for name := range targets {
		names = append(names, name)
	}
	sort.Strings(names)
	return names
}

// Pin is engine/libchromium.json: the version, its release manifest's
// SHA-256, and each part's SHA-256.
type Pin struct {
	Version  string            `json:"version"`
	Manifest string            `json:"manifest,omitempty"`
	Assets   map[string]string `json:"assets"`
}

// ReadPin reads a pin from its JSON.
func ReadPin(data []byte) (Pin, error) {
	var pin Pin
	if err := json.Unmarshal(data, &pin); err != nil {
		return Pin{}, fmt.Errorf("engine/libchromium.json: %w", err)
	}
	return pin, nil
}

// Pinned reads engine/libchromium.json.
func Pinned(root string) (Pin, error) {
	data, err := os.ReadFile(filepath.Join(root, "engine", "libchromium.json"))
	if err != nil {
		return Pin{}, err
	}
	return ReadPin(data)
}

// Libvpx is engine/libvpx.json.
type Libvpx struct {
	Repository string `json:"repository"`
	Tag        string `json:"tag"`
	Commit     string `json:"commit"`
}

// LibvpxPin reads engine/libvpx.json.
func LibvpxPin(root string) (Libvpx, []byte, error) {
	data, err := os.ReadFile(filepath.Join(root, "engine", "libvpx.json"))
	if err != nil {
		return Libvpx{}, nil, err
	}
	var pin Libvpx
	if err := json.Unmarshal(data, &pin); err != nil {
		return Libvpx{}, nil, fmt.Errorf("engine/libvpx.json: %w", err)
	}
	return pin, data, nil
}

// Order is engine/targets.json's targets in the file's own order.
func Order(root string) ([]string, error) {
	data, err := os.ReadFile(filepath.Join(root, "engine", "targets.json"))
	if err != nil {
		return nil, err
	}
	dec := json.NewDecoder(strings.NewReader(string(data)))
	if _, err := dec.Token(); err != nil {
		return nil, err
	}
	var names []string
	for dec.More() {
		key, err := dec.Token()
		if err != nil {
			return nil, err
		}
		names = append(names, key.(string))
		var skip json.RawMessage
		if err := dec.Decode(&skip); err != nil {
			return nil, err
		}
	}
	return names, nil
}
