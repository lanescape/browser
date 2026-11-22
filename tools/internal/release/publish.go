package release

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/lanescape/browser/tools/internal/r2"
	"github.com/lanescape/browser/tools/internal/repo"
	"github.com/lanescape/browser/tools/internal/version"
)

// Publisher publishes a build's packages to the release bucket on R2, then
// its complete manifest, last (specs/0004-releases.md).
type Publisher struct {
	Root    string
	Sources Sources
	// R2 is the release bucket; made from the environment when nil, and
	// only once there's something to upload.
	R2 func() (*r2.Client, error)
}

// Publish uploads dist/'s packages of build, made from commit, and then the
// manifest.
func (p Publisher) Publish(build, commit string) error {
	cargo, err := version.ReadCargo(p.Root)
	if err != nil {
		return err
	}
	major, err := version.ReleaseMajor(cargo)
	if err != nil {
		return err
	}
	paths := map[string]string{}
	var missing []string
	for _, target := range Targets {
		path := filepath.Join(p.Root, "dist", "lanescape-browser-"+target+".tar.gz")
		paths[target] = path
		if st, err := os.Stat(path); err != nil || !st.Mode().IsRegular() {
			missing = append(missing, target)
		}
	}
	// Before anything is uploaded, so a build missing a platform leaves
	// nothing behind.
	if len(missing) > 0 {
		sort.Strings(missing)
		return fmt.Errorf("not publishing %s: no package for %s", build, strings.Join(missing, ", "))
	}
	pin, err := repo.Pinned(p.Root)
	if err != nil {
		return err
	}
	// Before anything is uploaded: a build whose engine's corresponding
	// source isn't published doesn't ship.
	if err := p.Sources(pin.Version, pin.Manifest, pin.Assets); err != nil {
		return err
	}
	client, err := p.R2()
	if err != nil {
		return err
	}
	packages := map[string]Package{}
	for _, target := range Targets {
		name := filepath.Base(paths[target])
		id, err := r2.PutImmutable(client, paths[target], major+"/"+build+"/"+name, "application/gzip")
		if err != nil {
			return err
		}
		packages[target] = Package{Name: name, Bytes: id.Bytes, SHA256: id.SHA256}
	}
	lib := LibchromiumOf(pin.Version)
	manifest := Manifest{Major: major, Build: build, Commit: commit, Packages: packages, Libchromium: &lib}
	data, err := json.Marshal(manifest)
	if err != nil {
		return err
	}
	path := filepath.Join(p.Root, "dist", "manifest.json")
	if err := os.WriteFile(path, append(data, '\n'), 0o644); err != nil {
		return err
	}
	_, err = r2.PutImmutable(client, path, major+"/"+build+"/manifest.json", "application/json")
	return err
}
