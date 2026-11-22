package pyjson

import (
	"strings"
	"testing"
)

// u spells Python's \u escapes as U+, so that nothing on the way to this
// file decodes them.
func u(s string) string { return strings.ReplaceAll(s, "U+", "\\u") }

// The expected strings are what Python 3's json.dumps(v, sort_keys=True)
// printed for the same values.
func TestDumpsIsPythons(t *testing.T) {
	for _, tc := range []struct {
		v    any
		want string
	}{
		{[]any{"a", nil, []string{"b", "c"}}, `["a", null, ["b", "c"]]`},
		{[]any{"é/<&>", nil, []string{"\x7f\x01\n\t\"\\", "😀", "\xff"}},
			u(`["U+00e9/<&>", null, ["U+007fU+0001\n\t\"\\", "U+d83dU+de00", "U+dcff"]]`)},
		{map[string]any{"major": "v1", "build": "20261122.1", "commit": "abc",
			"packages":    map[string]any{"linux-amd64": map[string]any{"name": "x", "bytes": int64(5), "sha256": "aa"}},
			"libchromium": map[string]any{"version": "1", "release": "https://x/v1"}},
			`{"build": "20261122.1", "commit": "abc", "libchromium": {"release": "https://x/v1", "version": "1"}, "major": "v1", "packages": {"linux-amd64": {"bytes": 5, "name": "x", "sha256": "aa"}}}`},
		{[]any{true, false, 0, []any{}, map[string]any{}}, `[true, false, 0, [], {}]`},
	} {
		if got := Dumps(tc.v); got != tc.want {
			t.Errorf("Dumps(%#v)\n got %s\nwant %s", tc.v, got, tc.want)
		}
	}
}
