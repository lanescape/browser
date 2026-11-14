# Sourced by build scripts: cross-env.sh <target>. Points Cargo at a target this machine doesn't run, using
# Chromium's Clang and sysroot, so nothing else has to be installed. Does nothing for a native target. A macOS
# target on Linux links with ld64.lld against the SDK copied into build/mac_files (ci/README.md, macOS).
_target="${1:?target}"
_repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
_sysroot="$(jq -r --arg t "$_target" '.[$t].sysroot // empty' "$_repo/engine/targets.json")"
_os="$(jq -r --arg t "$_target" '.[$t].target_os // "linux"' "$_repo/engine/targets.json")"
_rust="$(jq -r --arg t "$_target" '.[$t].rust_target' "$_repo/engine/targets.json")"
_flags=""
if [ -n "$_sysroot" ]; then
  _flags="--target=${_rust/-unknown/} --sysroot=\"$CHROMIUM_ROOT/src/$_sysroot\""
elif [ "$_os" = mac ] && [ "$(uname -s)" != Darwin ]; then
  export SDKROOT="$CHROMIUM_ROOT/src/build/mac_files/xcode_binaries/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk"
  _flags="--target=${_rust/aarch64/arm64} -isysroot \"$SDKROOT\""
fi
if [ -n "$_flags" ]; then
  rustup target add "$_rust" >&2
  _link="$CHROMIUM_ROOT/link-$_rust"
  cat >"$_link" <<L
#!/bin/sh
exec "$CHROMIUM_ROOT/src/third_party/llvm-build/Release+Asserts/bin/clang" $_flags -fuse-ld=lld "\$@"
L
  chmod +x "$_link"
  _upper="$(echo "$_rust" | tr 'a-z-' 'A-Z_')"
  export "CARGO_TARGET_${_upper}_LINKER=$_link"
  export CARGO_BUILD_TARGET="$_rust"
fi
unset _target _repo _sysroot _os _rust _flags _link _upper
