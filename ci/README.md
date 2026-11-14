# CI

The workflows in `.github/workflows/` build and test every target, publish builds and keep build caches, through the scripts in `scripts/`. Linux jobs run on self-hosted runners, one machine for each job, which this repository doesn't create, label or remove. macOS jobs and the small jobs run on Blacksmith's runners. The Linux runners and CI's cloud setup are outside this repository, so the runner labels, cloud identities and dispatched workflows below are what the workflows expect of them, not something set up from here.

## What runs where

| Job | Machine | Identity |
|---|---|---|
| Build, linux-amd64 and linux-arm64 | A self-hosted Linux runner for that job only, booted from a build image | As in "Who gets what", below |
| Test, linux-arm64 | A self-hosted native arm64 runner, plain Ubuntu, with no Rust | The same as its build |
| Seed, linux-amd64 and linux-arm64 | The same as a build | `lanescape-browser-ci-seed` |
| Build image, linux-x64 | A self-hosted Linux runner, plain Ubuntu | `lanescape-browser-ci-image` |
| Build and test, macos-arm64 | Blacksmith `blacksmith-12vcpu-macos-26`: M4, 48 GB, 250 GB disk | `lanescape-browser-ci-main` or `lanescape-browser-ci-pr`, through GitHub's token |
| Reserving a build number | Blacksmith | GitHub's token |

A Linux job asks for its runner by the labels `self-hosted`, `lanescape-browser-ci` and one naming its run, attempt, kind, target and config, such as `run-<run>-<attempt>-build-linux-amd64-dev`, or `seed` or `image` in place of `build`.

After every compile, passed or failed, a build job summarizes why Siso rebuilt what it did, from its `siso_explain` record, and keeps the whole record as the artifact `siso-explain-<target>-<config>` for 7 days. Every build job does the same steps, through `scripts/ci_build.py`: restore the source pack, restore `out/`, compile what's out of date, save `out/` if anything compiled, package, and test. On a machine booted from a build image the source is there already, so the first step only installs packages and Rust, which takes moments when they're already installed. linux-arm64 is cross-compiled, so it hands its package and test binaries to a native machine through the bucket.

## macOS

A macOS job builds and tests on one Blacksmith Mac, through the same `scripts/ci_build.py` steps, with the same caches under its own target, `macos-arm64`, and its own source pack, `macos-arm64`. The Mac is fresh for each job, so it has no build image: `prepare` restores the source pack, or syncs Chromium when there's none, then installs zstd, pigz, jq and ffmpeg from Homebrew, and Rust. Chromium builds with the runner's own Xcode, which the job's summary names, and before building, every file of that Xcode's macOS SDK is given one fixed time. Every compile depends on the SDK's headers, which Chromium even declares as outputs of an action, and Siso compares their times with what the restored build recorded; the same Xcode on another Mac can carry other times, which would make a restored build look out of date throughout. Only CI's own machines are changed this way, since they're thrown away after the job. A different Xcode still rebuilds everything: `configure.sh` records the Xcode and SDK builds a build directory was generated with, in `out/<config>/xcode.txt`, and generates again when they change, which puts the new Xcode's build number into every compile command. Xcode's `gperf`, which Blink's generators name as an input, gets the same fixed time. A package is stripped of local symbols, so the host library's entry point can still be found by name, and signed again ad hoc, without which Apple silicon won't run it.

macOS jobs don't wait for a seed: a PR that moves the pin builds that Chromium from nothing on its Macs, and main does once more after the merge, after which its cache serves the PRs that follow. A PR can't save a source pack, so until a push to main has saved one for a new pin, each of the PR's macOS builds syncs Chromium.

A macOS job signs in to Google Cloud with GitHub's token, through the workload identity pool `lanescape-browser-ci`, whose provider the repository variable `GCP_CI_WIF_PROVIDER` names. Its provider maps a push from `cd.yml` to `main` or a support branch to `lanescape-browser-ci-main`, and a pull request from `ci.yml` to `lanescape-browser-ci-pr`, and nothing else to either. The pool is apart from publishing's, `lanescape-browser-github`, because a grant to a pool's principals covers every provider in it. A fork's pull request gets no token to sign in with, as long as this repo doesn't send write tokens to fork workflows. The jobs refuse forks as the Linux ones do.

## Caches

The bucket `lanescape-browser-ci` holds CI's caches, source packs and the binaries passed between jobs (`scripts/ci_cache.py`).

```
cache/main/<target>-<config>-<hash>/out.tar.zst
cache/pr/<number>/<merge base>-<target>-<config>-<hash>/out.tar.zst
```

The hash covers the pin, the build arguments, the target and the Rust toolchain (`scripts/ci_cache.py`). A PR reads its cache for its current merge base, then its newest cache for the same key from an earlier merge base (after it has caught up with main), then main's, and writes only the first. main reads and writes only its own. Every PR and push builds both a development and a release version, so a PR's first build of either has main's cache to start from. A seed, below, can also create one of main's caches, but never replace one.

## Images

Linux build and seed machines boot from a build image that already holds the source pack, Chromium's build dependencies, Rust and the runner, which take a fresh machine several minutes to download and install. `image.yml`, dispatched on main with a Chromium tag, its commit and the image's key, prepares one; deciding which image a job needs, dispatching `image.yml` and making the image from the machine's disk are the runners' part.

The image job, `scripts/ci_image.py`, refuses unless the tag and commit pass the seed's check, then writes the pin into its checkout of main and runs `ci_build.py prepare`, which restores the pin's source pack, or syncs Chromium and saves the pack when there's none. Then `ci_image.py finish` removes every prepare stamp but the source step's, so that a build booted from the image still installs packages and Rust, quickly, and picks up a change to either. It also removes which job the machine was for, apt's caches, shell histories, SSH host keys, and the machine's id, and last creates `/etc/lanescape-browser-ci/image-ready`, which says the job finished and the disk may be imaged. gcloud and gsutil keep their tokens in memory throughout, so none of the image identity's that still work reach the disk.

An image is only ever made by main's code. A PR that changes the toolchain or the pin gets an image keyed to its files but made by main's `prepare`; its builds then install the PR's toolchain on top, as they would on a fresh machine. The arm64 test machine doesn't build, so it boots plain Ubuntu. `image.yml` has to be on main to be dispatched.

## Seeds

A PR that moves the Chromium pin would otherwise build that Chromium from nothing, and main would build it from nothing again after the merge. A PR's cache can't be given to main, because a PR's unreviewed code writes it. So main's own code builds that Chromium instead, once, in a seed: `seed.yml`, dispatched on main with the tag and commit, for a PR from this repo that moves the pin and changes nothing else the cache key covers, when main has no cache under the PR's key. Its run's title names the commit, since a dispatch's inputs don't show in the runs API.

A seed job, `scripts/ci_seed.py`, refuses unless the tag is a Chromium release, four numbers, whose tag on `chromium.googlesource.com` names the commit. Then it writes that pin into its checkout of main, points `src/lanescape_browser` at empty stand-ins for the targets main's build arguments name, and builds `content_shell`, Chromium's own test shell, keeping going past failures. main's arguments are unchanged, so it makes the very `args.gn` and key a build of that pin does. It saves `out/` only if main has no cache there yet. Its `linux-amd64` dev job also saves the source pack, if there isn't one.

`seed.yml` has to be on main to be dispatched. A failed or missing seed must never hold up a PR: without one, the PR builds that Chromium itself.

## Who gets what

Each run's identity, decided from GitHub's API and never from what a workflow says about itself, and where in the bucket it can write:

| Run | Identity | Can write |
|---|---|---|
| Push to `main` or `support/vN`, from `cd.yml`, whose commit is on that branch | `lanescape-browser-ci-main` | `sources/`, `cache/main/`, `cache/support/`, `binaries/` |
| Pull request from this repo, from `ci.yml` | `lanescape-browser-ci-pr` | `cache/pr/`, `pr/` |
| A macOS job on Blacksmith | As the two rows above, from GitHub's token (macOS, above) | The same |
| Dispatch of `seed.yml` on `main`, whose commit is on main | `lanescape-browser-ci-seed` | New objects only, in `sources/` and `cache/main/` |
| Dispatch of `image.yml` on `main`, whose commit is on main | `lanescape-browser-ci-image` | New objects only, in `sources/` and `images/ready/` |
| Approved fork pull request, from `fork-ci.yml` | `lanescape-browser-ci-fork` | Nothing |
| Anything else | No machine | |

A seed's identity can create objects but not replace, change or delete them, so it can never overwrite a cache or source pack, and can't refresh one's Custom-Time. It reads only `sources/` and `cache/main/`, and never `binaries/` or a PR's prefixes. The image identity is the same, but reads only `sources/`, and creates only there and in `images/ready/`, where the runners record that a machine's disk is safe to image. No job's identity can make or change an image.

All pull requests share one identity, so one PR's job could overwrite another's cache under `cache/pr/`. That's contained, because main never reads those caches, and only people with write access can open a PR from this repo. `fork-ci.yml` doesn't exist yet, so forks get nothing.

## What the cloud setup must keep

- **CI's identities and their bucket prefixes,** as in the table above, and the bucket's expiry rules.
- **CI's machines unreachable** from outside and from each other, and a job's machine used only by the job it was made for, so that a PR's job can't run on a machine meant for main.
- **macOS jobs' sign-in,** the `lanescape-browser-ci` pool and its provider, reaching only the two identities above, and no write tokens sent to fork pull requests.
- **Publishing.** GitHub's sign-in reaches the publisher, `lanescape-browser-publisher`, only from `cd.yml` on a push, or `promote.yml` dispatched, on `main` or a support branch: never from a tag, a PR or another workflow. In the public bucket, `lanescape-browser`, the publisher reads anything and creates new objects, but can replace only a major's `latest`.
- **Build tags,** `vN.YYYYMMDD.N`, which a ruleset stops anyone moving or deleting.
- **Nothing broader.** No project-wide role reaches either bucket's objects beyond what's above.
