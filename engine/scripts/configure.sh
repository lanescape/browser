#!/usr/bin/env bash
# Writes a build directory's args.gn and generates it: configure.sh <dev|release> [target]. A restored
# directory is left untouched when its arguments still match, because a newer args.gn would make every
# later build regenerate.
set -euo pipefail
repo="$(cd "$(dirname "$0")/../.." && pwd)"
source "$repo/engine/scripts/env.sh"
out="${1:-dev}"
target="${2:-$(default_target)}"
dir="$CHROMIUM_ROOT/src/out/$out"
wanted="$(mktemp)"
trap 'rm -f "$wanted"' EXIT
cat "$repo/engine/args/$out.gn" >"$wanted"
# The host's own architecture is the default, so a native build's arguments stay as they are in the repo.
# Linux calls 64-bit Arm aarch64 and macOS calls it arm64.
cpu="$(jq -r --arg t "$target" '.[$t].target_cpu' "$repo/engine/targets.json")"
case "$(uname -m)" in
  aarch64 | arm64) native=arm64 ;;
  *) native=x64 ;;
esac
if [ "$native" != "$cpu" ]; then
  echo "target_cpu = \"$cpu\"" >>"$wanted"
fi
ln -sfn "$repo/engine" "$CHROMIUM_ROOT/src/lanescape_browser"
mkdir -p "$dir"
# Chromium writes Xcode's build number into its compile commands when it generates, so a restored build directory
# from another Xcode generates again, whatever its files' times say.
xcode="$dir/xcode.txt"
xcode_now=""
if [ "$(uname -s)" = Darwin ]; then
  xcode_now="$(xcodebuild -version | tr '\n' ' ') $(xcrun --sdk macosx --show-sdk-build-version)"
fi
if ! cmp -s "$wanted" "$dir/args.gn"; then
  cp "$wanted" "$dir/args.gn"
elif [ -e "$dir/build.ninja" ] && [ "$xcode_now" = "$(cat "$xcode" 2>/dev/null)" ]; then
  exit 0
fi
cd "$CHROMIUM_ROOT/src"
gn gen "out/$out"
printf '%s' "$xcode_now" >"$xcode"
