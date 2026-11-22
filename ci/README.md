# CI

The workflows in `.github/workflows/` check, build and test every target on the organization's runner service, `lanescape/runners`, through its actions, `lanescape/runners/actions/<name>@v1`, and test macOS on Blacksmith's Macs. Nothing builds Chromium: every build compiles Lanescape browser's own code against the libchromium version `engine/libchromium.json` pins (`scripts/build.py`, README.md).

## What runs where

| Job | Machine | Does |
|---|---|---|
| Reserve, CD only | `lanescape-2vcpu-linux-amd64` | `release.py reserve`: the build number, tagged on the commit |
| Checks | `lanescape-16vcpu-linux-amd64` | `scripts/ci.py checks`: the script tests, rustfmt, clippy and actionlint |
| Build, for each target | `lanescape-16vcpu-linux-amd64` (`target.yml`'s `build-runner`) | `scripts/build.py --release` for the target, then `build.py package --tests`: the package and the test executables, an artifact kept a day for a pull request, and a week for CD, so a publish can be retried with the bytes tested |
| Test, linux-amd64 | `lanescape-16vcpu-linux-amd64` | `scripts/ci.py suites` against the package, unpacked |
| Test, linux-arm64 | `lanescape-16vcpu-linux-arm64` | The same |
| Test, macos-arm64 | Blacksmith `blacksmith-6vcpu-macos-26` | The same |
| Publish, CD only | `lanescape-2vcpu-linux-amd64` | Once every target's suites pass: `publish.py`, then `release.py promote --if-unstable` (specs/0004-releases.md, Publishing) |

`ci.yml` runs on every pull request to `main` or a `support/v*` branch, and `cd.yml` on every push to them; both run the checks, then each target's chain, `target.yml`: build, then test. CD first reserves the push's build number, which each build bakes into the host, and last publishes the packages to R2. A fork's pull request runs the checks alone, on GitHub's own runners; the runner service serves no fork. Every build is a cross-build on Linux amd64, where libchromium's toolchain runs, so the test jobs need no Rust and no compiler: they install the system libraries Chromium loads (`ci.py install-test-packages`) and run the test executables the build made against the package's own executable.

## Caches

Each target's build keeps one cache, `build-<target>`, declared in `.github/runners.json`: `/work/build` on its work disk (not the disk's root, whose `lost+found` the runner's user can't read), holding the libchromium parts it downloaded and checked, its libvpx build, Rust's toolchain and Cargo's downloads, and its build directory. Its key covers what those depend on: the libchromium and libvpx pins, the targets, `Cargo.lock` and the Rust toolchain. As the runner service restores a cache's newest version when the key has moved, a build must never trust what it restored: `build.py` keeps each libchromium version and libvpx build in a directory of its own name, checks a part against its pin before it extracts it, and ninja and Cargo rebuild whatever is out of date. A pull request's caches are its own, and main's are written only by pushes to main, as the runner service's scopes keep them. A `support/vN` branch saves caches of its own only once main's `.github/runners.json` lists it under `"branches"` (specs/0004-releases.md); until then its pushes start from main's and save nothing. After restoring, the build job touches every file of its checkout, so that nothing a cache holds is newer than this commit's own sources.

## macOS

macOS builds on Linux like the other targets, with libchromium's clang and `ld64.lld`, and is tested on a Blacksmith Mac, which runs only what Linux built. The one Apple input is Apple's SDK, which libchromium never ships: `engine/targets.json` pins its archive in the organization's private bucket for CI tools, `gs://lanescape-ci-tools`, with its SHA-256, and `.github/actions/apple-sdk` copies it, checks it and unpacks it in the job's temporary directory, for the macOS build alone. It signs in with GitHub's OIDC token through the bucket's read-only workload identity, so only the macOS build job has `id-token: write`, granted by its workflow call; a Linux build fails at its first step if it could ask for one. The SDK is never packaged, published, logged or cached.

Apple silicon runs only signed code: `ld64.lld` signs ad hoc as it links, and `llvm-strip` signs again what `build.py package` strips. libchromium's macOS build has no V8 context snapshot (its specs/0001-sdk.md, Runtime files), so nothing about a macOS package needs a Mac to finish it.

## What the organization's setup must keep

- **The runner service's selectors** used here, and its caches' scopes: a pull request's caches never reach main.
- **The CI tools bucket's read-only identity,** `lanescape-ci-tools-reader`, reachable through the `lanescape-ci-tools` pool only from the organization's repositories.
- **No write tokens for forks:** a fork's pull request gets no token to sign in with.
- **The `publish` and `promote` environments,** each limited to the branches `main` and `support/v*`, with the secrets `R2_ACCESS_KEY_ID` and `R2_SECRET_ACCESS_KEY` of an R2 API token that may read and write the `lanescape-browser` bucket alone; `promote` needs a maintainer's approval.
- **The `v*` tag ruleset,** which keeps build tags from being moved or deleted, while CD may still create them.
