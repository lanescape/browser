// Package version reads the package major used for build publication, and
// whether it's unstable, from Cargo.toml (specs/0004-releases.md).
package version

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strings"

	"github.com/BurntSushi/toml"
)

var major = regexp.MustCompile(`^[1-9][0-9]*$`)

type manifest struct {
	Package struct {
		Version  string         `toml:"version"`
		Metadata map[string]any `toml:"metadata"`
	} `toml:"package"`
}

func parse(cargoToml string) (manifest, error) {
	var m manifest
	if _, err := toml.Decode(cargoToml, &m); err != nil {
		return manifest{}, fmt.Errorf("Cargo.toml: %w", err)
	}
	return m, nil
}

// ReadCargo reads root's Cargo.toml.
func ReadCargo(root string) (string, error) {
	data, err := os.ReadFile(filepath.Join(root, "Cargo.toml"))
	return string(data), err
}

// ReleaseMajor is the package major, as v<N>.
func ReleaseMajor(cargoToml string) (string, error) {
	m, err := parse(cargoToml)
	if err != nil {
		return "", err
	}
	first, _, _ := strings.Cut(m.Package.Version, ".")
	if !major.MatchString(first) {
		return "", errors.New("package version must have a positive major")
	}
	return "v" + first, nil
}

// Unstable is whether `[package.metadata.lanescape-browser] unstable` marks
// the package major unstable.
func Unstable(cargoToml string) (bool, error) {
	m, err := parse(cargoToml)
	if err != nil {
		return false, err
	}
	ours, ok := m.Package.Metadata["lanescape-browser"].(map[string]any)
	if !ok {
		return false, nil
	}
	value, ok := ours["unstable"]
	if !ok {
		return false, nil
	}
	flag, ok := value.(bool)
	if !ok {
		return false, errors.New("package.metadata.lanescape-browser.unstable must be true or false")
	}
	return flag, nil
}
