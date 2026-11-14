#!/usr/bin/env python3
"""A seed: Chromium alone, built by main's code at the pin a pull request moves to, and saved as main's out/ cache
for that pin, so that neither the PR nor main after it builds that Chromium from nothing. seed.yml runs these
steps when the runners dispatch it; see ci/README.md. A seed only ever creates objects, so it can't
replace a cache, or change one, and each step is safe to rerun on a disk kept from a preempted machine."""
import argparse
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
from pathlib import Path

import ci_build
import ci_cache

CHROMIUM_SOURCE = 'https://chromium.googlesource.com/chromium/src'
TAG = re.compile(r'[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+')
COMMIT = re.compile(r'[0-9a-f]{40}')
# Where the stand-in for Lanescape browser's own code goes. It's laid out like the repo, so engine/scripts/configure.sh
# runs from it unchanged and writes the very args.gn a build of the same pin does.
STUB = ci_build.HOME/'lanescape-browser-seed'
# Chromium's own test shell: nearly all of what the embedder compiles, with none of its code. Linux builds macOS, where
# the shell is an app bundle needing Apple's tools, and its test support would build Blink again for Linux to make a
# context snapshot (engine/BUILD.gn). There it's the Content API's public targets, which are what Lanescape browser's
# executable depends on but two small resource packs, and V8's snapshot.
SHELL_TARGETS = {'linux': ('content_shell',),
                 'mac': ('content/public/app:app', 'content/public/browser:browser', 'content/public/gpu:gpu',
                         'content/public/renderer:renderer', 'content/public/utility:utility',
                         'components/keyed_service/content:content', 'snapshot_blob.bin')}
NAME = re.compile(r'[A-Za-z0-9_]+')


def ls_remote(tag):
    return subprocess.check_output(['git', 'ls-remote', CHROMIUM_SOURCE, f'refs/tags/{tag}', f'refs/tags/{tag}^{{}}'],
                                   text=True, timeout=120)


def tag_commit(tag, lister=ls_remote):
    """The commit a tag on Chromium's own repository names, or None. An annotated tag names its commit through
    its peeled entry."""
    refs = {}
    for line in lister(tag).splitlines():
        sha, _, ref = line.partition('\t')
        refs[ref.strip()] = sha.strip()
    return refs.get(f'refs/tags/{tag}^{{}}') or refs.get(f'refs/tags/{tag}')


def check_pin(tag, commit, lister=ls_remote):
    """Refuses anything but a Chromium release and the commit its tag names. A PR can ask for a seed of whatever
    its pin says, and only a released tag keeps that to Chromium's own reviewed code: a bare commit could be an
    unmerged change on Chromium's code review."""
    if not TAG.fullmatch(tag):
        raise SystemExit(f'refusing to seed {tag!r}: not a Chromium release version')
    if not COMMIT.fullmatch(commit):
        raise SystemExit(f'refusing to seed {commit!r}: not a full commit')
    found = tag_commit(tag, lister)
    if found != commit:
        raise SystemExit(f'refusing to seed: Chromium {tag} is {found or "not a tag"}, not {commit}')


def write_pin(root, tag, commit):
    """Points this checkout of main at the seed's Chromium, and changes nothing else."""
    (Path(root)/'engine/chromium-pin.json').write_text(json.dumps({'tag': tag, 'commit': commit}, indent=2)+'\n')


def main_cache(key):
    return f'{ci_cache.BUCKET}/cache/main/{key}/out.tar.zst'


def output(name, value):
    path = os.environ.get('GITHUB_OUTPUT')
    if path:
        with open(path, 'a') as out:
            out.write(f'{name}={value}\n')


def pin(args, lister=ls_remote, exists=ci_build.exists):
    check_pin(args.tag, args.commit, lister)
    write_pin(ci_build.REPO, args.tag, args.commit)
    key = ci_cache.cache_key(ci_build.REPO, args.target, args.config)
    cached = exists(main_cache(key))
    ci_build.summary(f'seed of Chromium {args.tag} for {key}: {"main has it already" if cached else "building"}')
    output('cached', 'true' if cached else 'false')


def referenced_targets(texts):
    """Every //lanescape_browser target that build arguments name, as (directory under //lanescape_browser, name)."""
    found = set()
    for text in texts:
        for line in text.splitlines():
            code = line.split('#', 1)[0]
            if '//lanescape_browser' in code and re.search(r'\bimport\s*\(', code):
                raise RuntimeError('the build arguments import from //lanescape_browser, which a seed has no stand-in for')
            for rest in re.findall(r'"//lanescape_browser((?:[/:][^"]*)?)"', code):
                directory, _, name = rest.lstrip('/').partition(':')
                parts = [part for part in directory.split('/') if part]
                name = name or (parts[-1] if parts else 'lanescape_browser')
                if not all(NAME.fullmatch(part) for part in [*parts, name]):
                    raise RuntimeError(f'cannot make a stand-in for //lanescape_browser{rest}')
                found.add(('/'.join(parts), name))
    return found


def make_stub(repo, stub):
    """A stand-in for the repo whose engine has empty targets under every name main's arguments use, with main's
    own arguments, targets and build scripts beside them."""
    repo, stub = Path(repo), Path(stub)
    engine = stub/'engine'
    args = sorted((repo/'engine/args').glob('*.gn'))
    targets = referenced_targets(path.read_text() for path in args)
    if not targets:
        raise RuntimeError('the build arguments name no //lanescape_browser target, so a seed would build what main does not')
    shutil.rmtree(engine, ignore_errors=True)
    (engine/'args').mkdir(parents=True)
    for path in args:
        (engine/'args'/path.name).write_bytes(path.read_bytes())
    (engine/'targets.json').write_bytes((repo/'engine/targets.json').read_bytes())
    (engine/'scripts').mkdir()
    for path in sorted((repo/'engine/scripts').glob('*.sh')):
        (engine/'scripts'/path.name).write_bytes(path.read_bytes())
        (engine/'scripts'/path.name).chmod(0o755)
    by_directory = {}
    for directory, name in targets:
        by_directory.setdefault(directory, []).append(name)
    for directory, names in by_directory.items():
        build = engine/directory/'BUILD.gn' if directory else engine/'BUILD.gn'
        build.parent.mkdir(parents=True, exist_ok=True)
        groups = ''.join(f'group("{name}") {{\n}}\n\n' for name in sorted(names))
        build.write_text('# A seed builds Chromium alone: these stand in for what main\'s arguments name.\n\n'+groups)
    return engine


def saves_source_pack(target, config):
    """One job of a seed saves the source pack its targets share, so the others don't each upload one and leave
    the losers behind, since a seed can't delete."""
    targets = ci_build.targets()
    pack = targets[target]['source_pack']
    return config == 'dev' and target == min(name for name, entry in targets.items() if entry['source_pack'] == pack)


def prepare(args):
    # This is main's code at a validated release, so ci_build saves the pack if there's none yet, which there
    # normally is: the build image this machine booted from was made from it, or made it.
    ci_build.prepare(args, save_pack=saves_source_pack(args.target, args.config))


def compile_chromium(args):
    stub = make_stub(ci_build.REPO, STUB)
    env = ci_build.build_env()
    scripts = shlex.quote(str(stub/'scripts'))
    out = shlex.quote(str(ci_build.out_dir(args.config)))
    shell_targets = SHELL_TARGETS[ci_build.system(args.target)]
    with ci_build.timed(f'compile {" ".join(shell_targets)}'):
        # Keeps going past a failure, so everything else still compiles and is saved.
        result = subprocess.run(['bash', '-o', 'pipefail', '-ec',
                                 f'{scripts}/configure.sh {shlex.quote(args.config)} {shlex.quote(args.target)}\n'
                                 f'source {scripts}/env.sh\n'
                                 f'{scripts}/peak-disk.sh "$CHROMIUM_ROOT" autoninja -k 0 -C {out} {" ".join(shell_targets)}'], env=env)
    if result.returncode:
        sys.exit(f'{" ".join(shell_targets)} did not build ({result.returncode}); what did is still saved')


def save(args, runner=subprocess.run, exists=ci_build.exists):
    """Creates main's cache for this key, and never replaces one: the upload only succeeds if there's nothing there."""
    out = ci_build.out_dir(args.config)
    if not (out/'build.ninja').exists():
        raise SystemExit('nothing was generated, so there is nothing to save')
    path = main_cache(ci_cache.cache_key(ci_build.REPO, args.target, args.config))
    if exists(path):
        print('main has this cache already, and a seed never replaces one', flush=True)
        return
    scratch = ci_build.HOME/'out.tar.zst'
    with ci_build.timed('pack out'):
        ci_build.shell(f'tar --format=posix --exclude={ci_build.MARKER} -C {shlex.quote(str(out.parent))} -cf - {shlex.quote(args.config)}'
                       f' | zstd -3 -T0 -q -f -o {shlex.quote(str(scratch))}')
    ci_build.summary(f'out/ cache: {scratch.stat().st_size/1e9:.1f} GB')
    with ci_build.timed('upload out'):
        # Custom-Time is set as the object is created, which is all a seed may do, so the bucket's expiry applies.
        result = runner(['gcloud', 'storage', 'cp', '--if-generation-match=0', f'--custom-time={ci_build.now()}',
                         str(scratch), path])
    if result.returncode != 0:
        if not exists(path):
            raise SystemExit('could not save the cache')
        print('another build saved this cache first', flush=True)
    scratch.unlink()


COMMANDS = {'pin': pin, 'prepare': prepare, 'compile': compile_chromium, 'save': save}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=COMMANDS)
    parser.add_argument('--target', choices=sorted(ci_build.targets()), required=True)
    parser.add_argument('--config', choices=('dev', 'release'), required=True)
    parser.add_argument('--tag')
    parser.add_argument('--commit')
    args = parser.parse_args()
    if args.command == 'pin' and not (args.tag and args.commit):
        parser.error('pin needs --tag and --commit')
    COMMANDS[args.command](args)


if __name__ == '__main__':
    main()
