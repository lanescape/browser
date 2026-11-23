# Lanescape browser

Lanescape browser runs browser tabs on a server and streams their display using RFB 3.8. One host manages multiple named profiles, each with its own persistent cookies and site storage. Private tabs use independent temporary contexts. The control channel uses Protocol Buffers.

## Interfaces

Launch `lanescape-browser --data-dir /absolute/private-directory`. The directory must have mode 0700. Standard input/output carry protobuf messages prefixed by a four-byte little-endian byte length (maximum 65536). Diagnostics use stderr. The schema is in `proto/lanescape/browser/control.proto`. Supervisors send Version first and use the binary only if its major matches; the Version exchange is identical in every major. An optional `native-context-menu` capability lists what a person could do at that point in the page, for the client to present. `file-dialog`, `downloads` and `user-preferences` are specified but not implemented yet. Optional `native-ime` carries a viewer's input-method composition into the page, in whichever frame holds focus, and reports the caret and each character's position in the tab. Optional `clipboard` carries explicit copy, cut and paste transfers between the controlling viewer and the page, `text/plain` for now, running the page's own copy, cut and paste. It never touches the host's clipboard: each browsing context and private tab has its own transfers, side by side in one host. A page's own clipboard write is dropped, never reported, and its clipboard reads are refused. Ctrl/Command+V and Shift+Insert over RFB are dropped with or without the capability. Copy and cut keys over RFB reach the page but never the viewer's clipboard; send those shortcuts as ClipboardOperation. See `specs/0002-control.md` for cancellation, limits and artifact ownership.

Initialize optionally negotiates bounded capability tokens. `loading-state` enables engine-observed loading and back/forward-availability events; check the tokens returned by Initialize/Status. An empty Initialize requests none. Capabilities remain enabled for the process after subsequent initialization. See the [control contract](specs/0002-control.md) for bounds and lifecycle details.

Each created tab returns a private Unix socket serving RFB. Multiple viewers may connect; the controlling process grants input ownership to one viewer. This endpoint is intended for forwarding through an authenticated transport such as SSH, not public exposure.

The optional `devtools` capability opens one Chrome DevTools Protocol session per tab on its own private socket, for automation clients. A session reaches what the tab's pages can reach and never data Chromium hides from page script, such as HttpOnly cookie values, or the server's files. Dialogs, file pickers, downloads, viewport size and display stay on the control channel and RFB.

The optional `display-fence` capability names the first framebuffer frame that includes what a tab's page had committed, and the `FrameSequence` RFB pseudo-encoding numbers updates so a viewer can recognize it. A client that takes screenshots from RFB uses them to order a screenshot after the action it follows.

The optional `recording` capability streams a tab as VP8 between StartRecording and StopRecording, over a private socket of its own, as IVF that ffmpeg and others read directly. One recording to a tab, and many tabs at once.

The default rendering mode prefers hardware acceleration and falls back to the CPU. `--rendering=cpu` forces CPU rendering. Status reports what Chromium is actually compositing with. WebGL needs a GPU, since a software fallback would compile a page's shaders in the GPU process; without one, pages see no WebGL. Linux uses Ozone headless with Vulkan/ANGLE for hardware rendering; neither rendering mode launches a virtual display. macOS uses Metal through ANGLE, and its tabs are Chromium's Cocoa views with no window, on a display placed beyond the machine's own, so nothing of a tab appears on its screen and Lanescape browser has no Dock icon.

A distribution package is the `lanescape-browser` executable and the Rust host library (`liblanescape_browser_host.so`, or `.dylib` on macOS), with libchromium's shared library and runtime files beside them: Chromium's libc++, FFmpeg, ANGLE, SwiftShader, the Vulkan loader, its resource packs, ICU's data and V8's snapshot, as the libchromium version lists them, and the license notices described in [third_party/README.md](third_party/README.md). A Linux package has no setuid helper: renderers need unprivileged user namespaces, and Chromium won't start without a usable sandbox. Some distributions restrict those namespaces with AppArmor, as Ubuntu has since 23.10, and need a profile allowing them. A macOS package is one directory rather than an app bundle: renderers run under Chromium's Seatbelt policy, and a process that is itself already sandboxed can't sandbox them, so Lanescape browser won't start in one. There is no runtime engine download. CI builds and tests Linux amd64, Linux arm64 and macOS arm64; Windows comes later.

## Development

Lanescape browser compiles its own C++ and Rust against libchromium, Chromium prebuilt as one shared library with its headers, flags and toolchain, at the version `engine/libchromium.json` pins; it never compiles Chromium. Every target builds on Linux x86-64, where libchromium's toolchain runs:

```
go run ./tools/browser build --target linux-amd64   # or linux-arm64 or macos-arm64; --release: an optimised host
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --lib
LANESCAPE_BROWSER_BINARY=$PWD/out/linux-amd64/lanescape-browser cargo test --locked --tests
```

`tools/browser build` downloads the pinned libchromium parts for the target and checks each against the pin, builds libvpx (`engine/libvpx.json`) for the recorder's VP8 encoder, generates the engine's mojom bindings with libchromium's generator, compiles and links the engine with ninja, builds the Rust host with cargo, and puts libchromium's runtime files beside them, in `out/<target>/`, ready to run. What it downloads and builds is cached in `~/.cache/lanescape-browser` (`LANESCAPE_BROWSER_CACHE`). It needs Go, at the version `go.mod` names, ninja, zstd, git, make, Rust, Python 3 for libchromium's mojom generator, which is Chromium's own, and, for x86-64's libvpx, nasm. CI and CD run on Debian 13 with Linux 6.12, Ubuntu 26.04 for forks' checks, and macOS 26.2, and the tooling is written for nothing older ([ci/README.md](ci/README.md), Where it runs). A `macos-arm64` build needs Apple's SDK: the archive `engine/targets.json` pins, fetched from the organization's tools bucket with `gcloud`, or `--apple-sdk <MacOSX.sdk>`; its suites run on a Mac. `go run ./tools/browser pin <version>` moves to another libchromium version. A change to `engine/` recompiles what it touches and links; a change to the host compiles only Rust. Without `--release` the host is unoptimised, so measure rendering with `--release`.

`tools/browser` is the repository's tooling, in Go: `build`, `package` (the release package, and with `--tests` the test executables), `pin`, `ci` (what CI runs besides building), `release` and `publish` (specs/0004-releases.md). Its own tests are `go test ./...`, which `go run ./tools/browser ci checks` runs with rustfmt, clippy and actionlint.

`tests/` runs the actual executable against protobuf, RFB and DevTools clients and local HTTP fixtures, by goal: `ime` (exact composition, in every frame), `clipboard` (per browsing context, with no host clipboard), `display`, `input`, `recording` and `host` (one process, its contexts, its sandbox). `LANESCAPE_BROWSER_MEASURE=cpu,auto` makes `tests/measure.rs` print frame rates, latency and CPU use by process. `LANESCAPE_BROWSER_TEST_GPU=1` requires hardware acceleration; otherwise only the fallback is checked. The recording tests use ffmpeg when it's installed.

Set `LANESCAPE_BROWSER_DIAGNOSTICS=1` to trace startup, control requests and focus reports on stderr.

## Releases

PRs to `main` and `support/vN` run CI, one machine for each job ([ci/README.md](ci/README.md)). Every PR and every push builds each target once, optimised, against the pinned libchromium, and runs every suite on its package, on a machine like the target's.

Each push to `main` or `support/vN` reserves a build number, `YYYYMMDD.N` in UTC, tagged on its commit as `v1.20261107.1` with its major in front, and once every target's tests pass, publishes the packages and a manifest, which names the libchromium version whose release has Chromium's source. Download them from `https://downloads.lanescape.dev/lanescape/browser/vN/BUILD/`, such as `.../v1/20261107.1/lanescape-browser-linux-amd64.tar.gz` and `.../v1/20261107.1/manifest.json`; `https://downloads.lanescape.dev/lanescape/browser/v1/latest` holds the current build of v1 (specs/0004-releases.md).

`vN/latest` names the build Lanescape gets. Moving it to a newer build is a promotion, and promotion only ever goes forward: the build must be complete, and its commit must come after the current one's. A major marked unstable in `Cargo.toml` is promoted automatically when it publishes. A stable major is promoted by a maintainer through the **Promote build** workflow. To go back to earlier behaviour, merge a revert, which builds and promotes as usual.

The package major in `Cargo.toml` is the only version source: it is the protocol major reported by Version, the `--version` output and the build's storage prefix. A breaking change to a stable major N first branches `support/vN` from main's last vN build; later vN fixes target that branch. The breaking PR to main bumps `Cargo.toml` to `N+1.0.0` and marks it unstable. See [releases](specs/0004-releases.md).

Lanescape browser's own code uses the [MIT License](LICENSE). Chromium and bundled components retain their own terms. See [engine licensing and source](third_party/README.md).
