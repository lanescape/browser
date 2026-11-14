#!/usr/bin/env bash
# Fetches the pinned Chromium and builds its objects, so later embedder builds reuse them. Each
# phase leaves a stamp, so a rerun resumes after the last phase that finished.
set -euo pipefail
repo="$(cd "$(dirname "$0")/../.." && pwd)"
source "$repo/engine/scripts/env.sh"
tag="$(jq -r .tag "$repo/engine/chromium-pin.json")"
commit="$(jq -r .commit "$repo/engine/chromium-pin.json")"
# Stamps belong to one Chromium revision, so changing the pin runs every phase again.
stamps="$HOME/.lanescape-browser-stamps/$commit"
mkdir -p "$stamps"

phase() {
  local name="$1"
  shift
  if [[ -e "$stamps/$name" ]]; then
    echo "=== skip $name"
    return
  fi
  echo "=== start $name $(date -u +%FT%TZ)"
  "$@"
  touch "$stamps/$name"
  echo "=== done $name $(date -u +%FT%TZ)"
}

depot_tools() {
  [[ -d "$DEPOT_TOOLS" ]] ||
    git clone --depth 1 https://chromium.googlesource.com/chromium/tools/depot_tools.git "$DEPOT_TOOLS"
}

# DEPOT_TOOLS_UPDATE=0 also skips this, and gn's wrapper needs the Python it sets up.
depot_tools_bootstrap() {
  "$DEPOT_TOOLS/ensure_bootstrap"
}

source_tree() {
  mkdir -p "$CHROMIUM_ROOT"
  cd "$CHROMIUM_ROOT"
  # PGO profiles are needed by the later release build, and only come with a sync.
  cat >.gclient <<'EOF'
solutions = [{
  "name": "src",
  "url": "https://chromium.googlesource.com/chromium/src.git",
  "managed": False,
  "custom_vars": {"checkout_pgo_profiles": True},
}]
EOF
  [[ -d src/.git ]] || git init -q src
  cd src
  git remote get-url origin >/dev/null 2>&1 ||
    git remote add origin https://chromium.googlesource.com/chromium/src.git
  git fetch --depth=1 origin "refs/tags/$tag"
  git checkout -q --detach FETCH_HEAD
  local head
  head="$(git rev-parse HEAD)"
  if [[ "$head" != "$commit" ]]; then
    echo "tag $tag is $head, not the pinned $commit" >&2
    exit 1
  fi
}

dependencies() {
  cd "$CHROMIUM_ROOT"
  gclient sync --nohooks --no-history --force --delete_unversioned_trees
}

system_packages() {
  # Chromium's list is of Linux packages; a Mac needs only Xcode.
  if [ "$(uname -s)" = Darwin ]; then
    return
  fi
  local script="$CHROMIUM_ROOT/src/build/install-build-deps.sh"
  if sudo -n true 2>/dev/null; then
    sudo DEBIAN_FRONTEND=noninteractive "$script" --no-prompt --no-chromeos-fonts
  else
    # Never wait on a password prompt in a detached job. Most of the list is unused here: libraries
    # come from Chromium's sysroot, and the rest is cross tools and desktop test utilities. A tool
    # the build really needs fails its action by name, so report the list and carry on.
    "$script" --quick-check --unsupported ||
      echo "continuing without those packages"
  fi
}

hooks() {
  cd "$CHROMIUM_ROOT"
  gclient runhooks
}

objects() {
  cd "$CHROMIUM_ROOT/src"
  "$repo/engine/scripts/peak-disk.sh" "$CHROMIUM_ROOT" autoninja -C out/dev lanescape_browser
}

phase depot_tools depot_tools
phase depot_tools_bootstrap depot_tools_bootstrap
phase source_tree source_tree
phase dependencies dependencies
phase system_packages system_packages
phase hooks hooks
# Not stamped: this depends on this checkout and its build arguments, and is a no-op if unchanged. Chromium's
# build only sees files inside its tree, so it also makes the embedder appear there as //lanescape_browser.
"$repo/engine/scripts/configure.sh" dev
phase objects objects
echo "=== bootstrap complete $(date -u +%FT%TZ)"
