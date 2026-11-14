# Sourced by build scripts. Chromium's caches stay in their own directory, apart from the rest of
# the user's ~/.cache on a shared workstation.
export CHROMIUM_ROOT="$HOME/chromium"
export DEPOT_TOOLS="$HOME/depot_tools"
export PATH="$DEPOT_TOOLS:$PATH"
export XDG_CACHE_HOME="$HOME/.lanescape-browser-cache"
export DEPOT_TOOLS_UPDATE=0
export DEPOT_TOOLS_METRICS=0
mkdir -p "$XDG_CACHE_HOME"
# A Chromium build holds many files open at once, and some hosts default to 1024. macOS reports
# "unlimited" as the hard limit, which it refuses as a soft one.
ulimit -n "$(ulimit -Hn)" 2>/dev/null || ulimit -n 65536 2>/dev/null || true

# The target a script builds when none is named: this machine's own system. Linux also builds macOS,
# given the SDK (ci/README.md, macOS); macOS builds only macOS.
default_target() {
  if [ "$(uname -s)" = Darwin ]; then
    echo macos-arm64
  else
    echo linux-amd64
  fi
}

if [ "$(uname -s)" = Darwin ]; then
  # Chromium builds with the installed Xcode's SDK, even when the command-line tools are selected.
  if [ -z "${DEVELOPER_DIR:-}" ] && [ -d /Applications/Xcode.app/Contents/Developer ]; then
    export DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer
  fi
fi
