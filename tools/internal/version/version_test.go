package version

import (
	"strings"
	"testing"
)

func TestReleaseMajor(t *testing.T) {
	for cargo, want := range map[string]string{
		"[package]\nversion=\"1.0.0\"\n":  "v1",
		"[package]\nversion=\"12.3.4\"\n": "v12",
	} {
		if got, err := ReleaseMajor(cargo); err != nil || got != want {
			t.Errorf("ReleaseMajor(%q) = %q, %v", cargo, got, err)
		}
	}
	for _, cargo := range []string{"[package]\nversion=\"0.1.0\"\n", "[package]\nversion=\"01.0.0\"\n", "[package]\n", "not toml ="} {
		if _, err := ReleaseMajor(cargo); err == nil {
			t.Errorf("ReleaseMajor(%q) accepted", cargo)
		}
	}
}

// The unstable flag marks the package major; anything but a boolean is
// refused, and another table's flag doesn't count.
func TestUnstable(t *testing.T) {
	base := "[package]\nversion=\"2.0.0\"\n"
	for flag, want := range map[string]bool{
		"": false,
		"[package.metadata.lanescape-browser]\nunstable = true\n":  true,
		"[package.metadata.lanescape-browser]\nunstable = false\n": false,
		"[package.metadata.other]\nunstable = true\n":              false,
	} {
		if got, err := Unstable(base + flag); err != nil || got != want {
			t.Errorf("Unstable(%q) = %v, %v", flag, got, err)
		}
	}
	for _, value := range []string{`"true"`, "1"} {
		if _, err := Unstable(base + "[package.metadata.lanescape-browser]\nunstable = " + value + "\n"); err == nil || !strings.Contains(err.Error(), "true or false") {
			t.Errorf("unstable = %s: %v", value, err)
		}
	}
}

// The repository's own Cargo.toml is readable.
func TestTheRepositorysOwn(t *testing.T) {
	cargo, err := ReadCargo("../../..")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := ReleaseMajor(cargo); err != nil {
		t.Fatal(err)
	}
	if _, err := Unstable(cargo); err != nil {
		t.Fatal(err)
	}
}
