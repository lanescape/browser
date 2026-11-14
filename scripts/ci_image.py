#!/usr/bin/env python3
"""A build image: a machine whose disk the runners make into the image every build machine boots from, with
the Chromium source at one pin, its build dependencies and Rust already in place. image.yml runs these steps on
main, around ci_build.py prepare, when the runners dispatch it; see ci/README.md."""
import argparse
import os
import re
import shutil
import sys
from pathlib import Path

import ci_build
import ci_seed

KEY = re.compile(r'[0-9a-f]{64}')
# The one prepare step whose stamp an image keeps. The others are quick when there's nothing to do, so a build
# booted from the image still runs them, and picks up a change to its packages or toolchain.
KEPT_STAMP = 'source'
# The runner's startup script stops the machine for imaging only once this exists, and removes it.
READY = 'etc/lanescape-browser-ci/image-ready'


def pin(args):
    """Refuses anything but a Chromium release and its commit, as a seed does, then points this checkout of main
    at it."""
    if not KEY.fullmatch(args.key):
        raise SystemExit(f'refusing to make an image for {args.key!r}: not a key')
    ci_seed.check_pin(args.tag, args.commit)
    ci_seed.write_pin(ci_build.REPO, args.tag, args.commit)
    ci_build.summary(f'image of Chromium {args.tag} for {args.key[:20]}')


def prune_stamps(home, commit):
    """Removes every prepare stamp but the source step's for this pin, which has to be there."""
    stamps = Path(home)/'.lanescape-browser-stamps'
    if not (stamps/commit/KEPT_STAMP).is_file():
        raise SystemExit(f'prepare did not finish the source step for {commit}, so there is nothing to image')
    for path in stamps.iterdir():
        if path.name != commit:
            remove(path)
    for path in (stamps/commit).iterdir():
        if path.name != KEPT_STAMP:
            remove(path)


def remove(path):
    if path.is_dir() and not path.is_symlink():
        shutil.rmtree(path)
    else:
        path.unlink(missing_ok=True)


def homes(root):
    return [root/'root', *sorted(path for path in (root/'home').glob('*') if path.is_dir())]


def make_safe(root):
    """Removes what a machine booted from the image mustn't see or share: which job this one was for, apt's
    caches, shell histories, SSH host keys, the machine's id, and any cached access tokens, which would still be
    the image identity's for a while. The runner's own files, and its checkout, go once it has exited
    (the runner's startup script)."""
    root = Path(root)
    (root/'etc/lanescape-browser-ci/expected-job').unlink(missing_ok=True)
    for cache in ('var/cache/apt', 'var/lib/apt/lists'):
        for path in (root/cache).rglob('*') if (root/cache).is_dir() else ():
            if path.is_file() or path.is_symlink():
                path.unlink()
    for home in homes(root):
        for path in home.glob('.*_history'):
            remove(path)
        # The image job keeps gcloud's and gsutil's in memory; these would be from anything that didn't.
        remove(home/'.config/gcloud')
        remove(home/'.gsutil')
    for path in (root/'etc/ssh').glob('ssh_host_*'):
        path.unlink()
    machine_id = root/'etc/machine-id'
    if machine_id.exists():
        # Empty, not missing, so that the next boot makes a new one.
        machine_id.write_text('')


def finish(args):
    """The image job's last step, run as root: then the runner's startup script clears what the runner holds, and
    stops the machine for the runners to image."""
    root = Path(args.root)
    prune_stamps(args.home, ci_build.pin()['commit'])
    make_safe(root)
    os.sync()
    ready = root/READY
    ready.parent.mkdir(parents=True, exist_ok=True)
    ready.touch()
    ci_build.summary('the disk is ready to image')


COMMANDS = {'pin': pin, 'finish': finish}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=COMMANDS)
    parser.add_argument('--tag')
    parser.add_argument('--commit')
    parser.add_argument('--key')
    parser.add_argument('--home', help="the runner's home, which holds the stamps")
    parser.add_argument('--root', default='/', help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.command == 'pin' and not (args.tag and args.commit and args.key):
        parser.error('pin needs --tag, --commit and --key')
    if args.command == 'finish' and not args.home:
        parser.error('finish needs --home')
    if args.command == 'finish' and os.geteuid() != 0:
        sys.exit('finish has to run as root')
    COMMANDS[args.command](args)


if __name__ == '__main__':
    main()
