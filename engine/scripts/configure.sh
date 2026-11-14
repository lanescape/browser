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
# Likewise the system: a macOS target built on Linux uses Chromium's clang and ld64.lld, with an SDK copied from a
# Mac in build/mac_files/xcode_binaries (ci/README.md, macOS).
os="$(jq -r --arg t "$target" '.[$t].target_os // "linux"' "$repo/engine/targets.json")"
case "$(uname -s)" in
  Darwin) native_os=mac ;;
  *) native_os=linux ;;
esac
if [ "$native_os" != "$os" ]; then
  echo "target_os = \"$os\"" >>"$wanted"
  if [ "$os" = mac ]; then
    # Chromium names the SDK version it expects; use the copied SDK's own instead.
    sdk_plist="$CHROMIUM_ROOT/src/build/mac_files/xcode_binaries/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk/System/Library/CoreServices/SystemVersion.plist"
    read -r sdk_version sdk_build < <(python3 -c 'import plistlib,sys; p=plistlib.load(open(sys.argv[1],"rb")); print(p["ProductVersion"], p["ProductBuildVersion"])' "$sdk_plist")
    for kind in official official_universal; do
      printf 'mac_sdk_%s_version = "%s"\nmac_sdk_%s_build_version = "%s"\n' "$kind" "$sdk_version" "$kind" "$sdk_build" >>"$wanted"
    done
    # Chromium fetches dsymutil only for a Mac, and nothing keeps a release build's dSYMs.
    printf 'use_system_xcode = false\nuse_v8_context_snapshot = true\nenable_dsyms = false\n' >>"$wanted"
  fi
fi
ln -sfn "$repo/engine" "$CHROMIUM_ROOT/src/lanescape_browser"
mkdir -p "$dir"
# Chromium writes Xcode's build number into its compile commands when it generates, so a restored build directory
# from another Xcode generates again, whatever its files' times say.
xcode="$dir/xcode.txt"
xcode_now=""
if [ "$(uname -s)" = Darwin ]; then
  xcode_now="$(xcodebuild -version | tr '\n' ' ') $(xcrun --sdk macosx --show-sdk-build-version)"
elif [ "$os" = mac ]; then
  xcode_now="$(cat "$CHROMIUM_ROOT/src/build/mac_files/xcode_binaries/Contents/version.plist" \
    "$CHROMIUM_ROOT/src/build/mac_files/xcode_binaries/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk/System/Library/CoreServices/SystemVersion.plist" | sha256sum | cut -c1-16)"
fi
if ! cmp -s "$wanted" "$dir/args.gn"; then
  cp "$wanted" "$dir/args.gn"
elif [ -e "$dir/build.ninja" ] && [ "$xcode_now" = "$(cat "$xcode" 2>/dev/null)" ]; then
  exit 0
fi
cd "$CHROMIUM_ROOT/src"
gn gen "out/$out"
printf '%s' "$xcode_now" >"$xcode"
