# Bundled engine and source

Lanescape browser embeds Chromium through its Content API. Chromium is compiled into the `lanescape-browser` executable, unmodified: this project carries no patches to it. A package also includes the Chromium runtime files the executable needs (`lanescape-browser.pak`, `icudtl.dat`, `v8_context_snapshot.bin`, ANGLE's `libEGL.so` and `libGLESv2.so`, and the Vulkan loader `libvulkan.so.1` on Linux; on macOS the snapshot is `v8_context_snapshot.arm64.bin` and ANGLE is linked into the executable).

Each package carries the notices for what it contains:

- `LICENSE`: Lanescape browser's own MIT license.
- `CHROMIUM-LICENSE`: Chromium's license.
- `CREDITS.html`: the license of every third-party library in Chromium that this build links, generated from the build itself with Chromium's `tools/licenses/licenses.py`.
- `licenses/`: the licenses and notices of the linked Rust crates, and an inventory.

Lanescape browser's MIT license does not replace any of those terms.

## Corresponding source

Each build's `manifest.json` links its public `sources.json` offer. That directory holds the Chromium release source for the pinned version (`third_party/sources.json`, matching `engine/chromium-pin.json`), including third-party sources such as FFmpeg, with its SHA-256 checksum. It also holds this file. Lanescape browser's own source is this repository at the commit the manifest records. Keep the source directory available for at least as long as the binaries it corresponds to.

No Widevine CDM, proprietary codec or externally downloaded engine is included. Builds use `proprietary_codecs = false` and `ffmpeg_branding = "Chromium"` (`engine/args/`). Don't add GPL-enabled codecs or a CDM without separately reviewing their redistribution terms.

## Rebuilding with a modified library

Because Chromium, and the LGPL libraries in it, are linked into the executable, replacing one means rebuilding the executable from source. Everything needed is published:

1. Get the Chromium source for the pinned version: extract the release archive from the source offer, or sync a checkout to the commit in `engine/chromium-pin.json`, as `engine/scripts/bootstrap.sh` does.
2. Get this repository at the build's commit and make your changes to Chromium's tree.
3. Run `engine/scripts/build.sh dev` or `engine/scripts/build.sh release`. That links `engine/` into the tree as `//lanescape_browser`, applies the arguments in `engine/args/`, builds the executable with Chromium's own toolchain, and builds the host library with Cargo from the checked-in `Cargo.lock` and Rust version in `rust-toolchain.toml`.

A release build uses Chromium's PGO profiles, which come with a sync and aren't in the release archive. Without them, set `chrome_pgo_phase = 0`. The result works the same, just a little slower.

The executable doesn't check the hash of anything it loads, and nothing prohibits debugging or reverse engineering it to modify the LGPL components.
