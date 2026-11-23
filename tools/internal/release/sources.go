package release

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"strings"
	"time"

	"github.com/lanescape/browser/tools/internal/fetch"
	"github.com/lanescape/browser/tools/internal/github"
	"github.com/lanescape/browser/tools/internal/stop"
)

// maxManifest is the most of a release's manifest read.
var maxManifest = 16 << 20

// ChromiumTarballs is where Chromium publishes its official source
// tarballs.
const ChromiumTarballs = "https://commondatastorage.googleapis.com/chromium-browser-official/"

// SourceChecker checks libchromium's releases.
type SourceChecker struct {
	// TarballBase replaces ChromiumTarballs, for tests.
	TarballBase string
	API         *github.Client
	// HTTP fetches the release's manifest and checks Chromium's tarball.
	HTTP *http.Client
}

type releaseAsset struct {
	Name     string `json:"name"`
	Size     int64  `json:"size"`
	State    string `json:"state"`
	Digest   string `json:"digest"`
	Download string `json:"browser_download_url"`
}

type sdkManifest struct {
	Version string `json:"version"`
	Assets  []struct {
		Name   string `json:"name"`
		Part   string `json:"part"`
		Size   int64  `json:"size"`
		SHA256 string `json:"sha256"`
	} `json:"assets"`
	Source struct {
		Chromium struct {
			Tarball string `json:"tarball"`
		} `json:"chromium"`
	} `json:"source"`
}

// Check refuses unless libchromium's release of version publishes its
// corresponding source: its manifest is the one engine/libchromium.json
// pins, reviewed when the pin moved; the SDK parts pinned are the ones it
// lists; the release has every source archive it lists, uploaded, at the
// size and SHA-256 it gives; and Chromium's official source tarball it
// points to is there. Publishing checks before it uploads, and promotion
// again.
func (s SourceChecker) Check(version, manifestSHA256 string, pinnedAssets map[string]string) error {
	client := &http.Client{Timeout: 60 * time.Second}
	if s.HTTP != nil {
		c := *s.HTTP
		client = &c
	}
	client.CheckRedirect = fetch.NoReferer
	var found struct {
		Assets []releaseAsset `json:"assets"`
	}
	if _, err := s.API.Request(http.MethodGet, "repos/lanescape/libchromium/releases/tags/v"+version, nil, &found); err != nil {
		var e *github.Error
		if errors.As(err, &e) {
			return refused("corresponding source missing: libchromium has no release v%s (%d)", version, e.Status)
		}
		return err
	}
	assets := map[string]releaseAsset{}
	for _, a := range found.Assets {
		assets[a.Name] = a
	}
	m, ok := assets["manifest.json"]
	if !ok {
		return refused("corresponding source missing: libchromium v%s has no manifest", version)
	}
	// The download is redirected to a signed URL, which no error may name.
	what := fmt.Sprintf("fetching libchromium v%s's manifest", version)
	resp, err := stop.Get(client, m.Download)
	if err != nil {
		return fetch.Error(what, err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("%s: status %d", what, resp.StatusCode)
	}
	// All of it is hashed: a limit that stopped reading quietly could pass
	// the pinned manifest followed by anything.
	raw, err := io.ReadAll(io.LimitReader(resp.Body, int64(maxManifest)+1))
	if err != nil {
		return fetch.Error(what, err)
	}
	if len(raw) > maxManifest {
		return refused("corresponding source missing: libchromium v%s's manifest is not the pinned one", version)
	}
	sum := sha256.Sum256(raw)
	if manifestSHA256 == "" || hex.EncodeToString(sum[:]) != manifestSHA256 {
		return refused("corresponding source missing: libchromium v%s's manifest is not the pinned one", version)
	}
	var manifest sdkManifest
	if err := json.Unmarshal(raw, &manifest); err != nil {
		return refused("corresponding source missing: libchromium v%s's manifest is not readable", version)
	}
	// The SDK's parts the build checked are the ones this manifest lists, so
	// build and source are one release.
	listed := map[string]string{}
	for _, a := range manifest.Assets {
		listed[a.Name] = a.SHA256
	}
	if len(pinnedAssets) == 0 {
		return refused("corresponding source mismatch: libchromium v%s's manifest lists other SDK parts than the pin", version)
	}
	for name, sum := range pinnedAssets {
		if got, ok := listed[name]; !ok || got != sum {
			return refused("corresponding source mismatch: libchromium v%s's manifest lists other SDK parts than the pin", version)
		}
	}
	var sources int
	for _, a := range manifest.Assets {
		if a.Part == "source" {
			sources++
		}
	}
	if manifest.Version != version || sources == 0 {
		return refused("corresponding source missing: libchromium v%s's manifest lists no source", version)
	}
	// A release's assets can be replaced; each must be uploaded, at the size
	// and SHA-256 its manifest gives.
	for _, a := range manifest.Assets {
		if a.Part != "source" {
			continue
		}
		got, ok := assets[a.Name]
		if !ok || got.State != "uploaded" || got.Size != a.Size || got.Digest != "sha256:"+a.SHA256 {
			return refused("corresponding source missing: libchromium v%s lacks %s", version, a.Name)
		}
	}
	// Chromium's official tarball of the upstream version libchromium
	// builds, where Chromium publishes it, and there itself: not a page
	// that answers, nor a redirect elsewhere.
	base := s.TarballBase
	if base == "" {
		base = ChromiumTarballs
	}
	upstream, _, _ := strings.Cut(version, "-")
	tarball := manifest.Source.Chromium.Tarball
	if tarball != base+"chromium-"+upstream+".tar.xz" {
		return refused("corresponding source missing: libchromium v%s names no Chromium tarball", version)
	}
	req, err := http.NewRequestWithContext(stop.Context(), http.MethodHead, tarball, nil)
	if err != nil {
		return refused("corresponding source missing: libchromium v%s names no Chromium tarball", version)
	}
	direct := *client
	direct.CheckRedirect = func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }
	head, err := direct.Do(req)
	if err == nil {
		head.Body.Close()
	}
	if err := stop.Err(); err != nil {
		return err
	}
	if err != nil || head.StatusCode != http.StatusOK {
		return refused("corresponding source missing: Chromium's tarball for libchromium v%s is not there", version)
	}
	return nil
}
