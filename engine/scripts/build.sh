#!/usr/bin/env bash
# Builds the Chromium executable and the Rust host library it loads, placing the host beside the
# executable: build.sh <dev|release> [target]. A host-only change compiles only Rust.
set -euo pipefail
repo="$(cd "$(dirname "$0")/../.." && pwd)"
source "$repo/engine/scripts/env.sh"
out="${1:-dev}"
target="${2:-$(default_target)}"
dir="$CHROMIUM_ROOT/src/out/$out"
"$repo/engine/scripts/configure.sh" "$out" "$target"
# LANESCAPE_BROWSER_JOBS limits a build's parallelism on a machine short of memory.
"$repo/engine/scripts/peak-disk.sh" "$CHROMIUM_ROOT" autoninja ${LANESCAPE_BROWSER_JOBS:+-j "$LANESCAPE_BROWSER_JOBS"} -C "$dir" lanescape_browser
source "$repo/engine/scripts/cross-env.sh" "$target"
# A release build gets an optimized host; rendering costs depend on it heavily.
profile=debug
cargo_flags=()
if [ "$out" = release ]; then
  profile=release
  cargo_flags=(--release)
fi
cargo build --locked "${cargo_flags[@]}" --manifest-path "$repo/Cargo.toml" --target-dir "$dir/host"
library="$(jq -r --arg t "$target" '.[$t].host_library // "liblanescape_browser_host.so"' "$repo/engine/targets.json")"
install -m 755 "$dir/host/${CARGO_BUILD_TARGET:+$CARGO_BUILD_TARGET/}$profile/$library" "$dir/$library"
echo "built $dir/lanescape-browser"
