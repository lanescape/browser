# Bundled engine and source

Lanescape browser embeds Chromium through its Content API. Chromium is libchromium (`lanescape/libchromium`): upstream Chromium at one release, built with that repository's settings and patch as one shared library, `libchromium.so` (`libchromium.dylib` on macOS), at the version `engine/libchromium.json` pins. This project carries no patches to Chromium of its own and never compiles it. A package holds the executable, the Rust host library, and libchromium's runtime files beside them: the library, Chromium's libc++, FFmpeg (`libffmpeg`), ANGLE, SwiftShader and the Vulkan loader, its resource packs, ICU's data and V8's snapshot, as the version's `sdk.json` lists them. libvpx, at the release `engine/libvpx.json` pins, is linked into the executable for the recorder's VP8 encoder.

Each package carries the notices for what it contains:

- `LICENSE`: Lanescape browser's own MIT license.
- `notices/libchromium/`: libchromium's notices, exactly as its version ships them: Chromium's licence, the credits of every third-party component the library links or ships, which components' corresponding source the version publishes, and where Chromium's own source and libchromium's patch are.
- `notices/libvpx/`: libvpx's BSD licence, patent grant and authors.
- `notices/crates/`: the licenses and notices of the Rust crates linked into the host library, and `packages.json`, their inventory.
- `notices/README`: which is which.

Lanescape browser's MIT license does not replace any of those terms.

## Corresponding source

Chromium's corresponding source is published with the libchromium version a package was built with, on that version's GitHub release: the source of every component whose licence asks for it, such as FFmpeg and Blink's LGPL code, as built, and pointers to Chromium's official source tarball for the release and to libchromium's patch, which `notices/libchromium/README` repeats. libchromium keeps a version's release for as long as any build that uses it is downloadable. Lanescape browser's own source is this repository at the commit a build's manifest records, and libvpx's is its release.

No Widevine CDM, proprietary codec or externally downloaded engine is included: libchromium builds with proprietary codecs off. Don't add GPL-enabled codecs or a CDM without separately reviewing their redistribution terms.

## Replacing a library

Chromium's LGPL components are in shared libraries beside the executable, not in it. FFmpeg is `libffmpeg`: build it from the libchromium version's published FFmpeg source and put it in the shipped one's place. Blink's LGPL code is inside `libchromium`: rebuild the library from Chromium's source at the tag with libchromium's patch and settings, as its repository describes, and replace `libchromium` beside the executable. Either keeps the executable as it is, as long as the replacement keeps the same exports; to rebuild Lanescape browser itself, run `scripts/build.py` at the build's commit, with `--sdk` pointing at an SDK of your own.

The executable doesn't check the hash of anything it loads, and nothing prohibits debugging or reverse engineering it to modify the LGPL components.
