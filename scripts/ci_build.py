#!/usr/bin/env python3
"""The steps of a CI build, each safe to rerun on a disk kept from a preempted machine: restore source
and out/, compile, save out/, package, hand off, and test. See ci/README.md."""
import argparse
import hashlib
import json
import os
import platform
import re
import secrets
import signal
import shlex
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

import ci_cache
import mac_sdk
from archive import pack, unpack
from notices import bundle_notices
from storage import describe

REPO = Path(__file__).resolve().parent.parent
HOME = Path.home()
CHROMIUM = HOME/'chromium'
DEPOT_TOOLS = HOME/'depot_tools'
MARKER = '.compiled-this-run'
SUITES = ('ime', 'clipboard', 'host', 'display', 'input', 'recording')
MAC = platform.system() == 'Darwin'
# What a package holds, and which of it is stripped, by the target's system. On macOS, ANGLE is linked into the
# executable. Linux builds macOS, and can't run what makes V8's context snapshot, so a macOS package leaves Linux
# without it, and with what makes it beside it (SNAPSHOT_INPUTS); its test Mac completes the package (complete).
PACKAGE_FILES = {
    'linux': ('lanescape-browser', 'liblanescape_browser_host.so', 'lanescape-browser.pak', 'icudtl.dat', 'v8_context_snapshot.bin',
              'libEGL.so', 'libGLESv2.so', 'libvulkan.so.1'),
    'mac': ('lanescape-browser', 'liblanescape_browser_host.dylib', 'lanescape-browser.pak', 'icudtl.dat'),
}
STRIPPED = {
    'linux': ('lanescape-browser', 'liblanescape_browser_host.so', 'libEGL.so', 'libGLESv2.so', 'libvulkan.so.1'),
    'mac': ('lanescape-browser', 'liblanescape_browser_host.dylib'),
}
# V8's own snapshot, and the generator built for macOS (engine/BUILD.gn), which turns it into the context snapshot.
SNAPSHOT_INPUTS = ('snapshot_blob.bin', 'v8_context_snapshot_generator')
CONTEXT_SNAPSHOT = 'v8_context_snapshot.arm64.bin'
# What a test Mac gets from Homebrew, as a Linux test machine gets its packages from apt.
BREW_PACKAGES = ('zstd', 'pigz', 'ffmpeg')
# What Chromium's executable and the recording tests need on a machine that didn't build them.
TEST_PACKAGES = ('zstd', 'file', 'pigz', 'ffmpeg', 'fonts-liberation', 'fonts-dejavu-core', 'libglib2.0-0t64',
                 'libnspr4', 'libnss3', 'libatk1.0-0t64', 'libatk-bridge2.0-0t64', 'libatspi2.0-0t64',
                 'libdbus-1-3', 'libx11-6', 'libxcomposite1', 'libxdamage1', 'libxext6', 'libxfixes3',
                 'libxrandr2', 'libxcb1', 'libxkbcommon0', 'libgbm1', 'libexpat1', 'libudev1', 'libasound2t64',
                 'libcairo2', 'libpango-1.0-0', 'libcups2t64')

EXTRA_PATH = (str(HOME/'.cargo/bin'), str(DEPOT_TOOLS), '/opt/homebrew/bin' if MAC else '/snap/bin')
os.environ['PATH'] = os.pathsep.join((*EXTRA_PATH, os.environ['PATH']))
# gcloud's composite uploads leave objects that only some clients can read back cheaply.
os.environ['CLOUDSDK_STORAGE_PARALLEL_COMPOSITE_UPLOAD_ENABLED'] = 'False'
os.environ.update(DEPOT_TOOLS_UPDATE='0', DEPOT_TOOLS_METRICS='0')


def run(*args, **kwargs):
    subprocess.run([str(arg) for arg in args], check=True, **kwargs)


def shell(script, **kwargs):
    """Runs a pipeline, failing if any part of it fails."""
    subprocess.run(['bash', '-o', 'pipefail', '-ec', script], check=True, **kwargs)


def sha256(path):
    digest = hashlib.sha256()
    with open(path, 'rb') as stream:
        while block := stream.read(1 << 20):
            digest.update(block)
    return digest.hexdigest()


# How a runner cancels a job, Ctrl-C first, and how a session ends.
STOP_SIGNALS = (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)


def run_for(seconds, command, **kwargs):
    """Runs a command for at most this long, returning its exit status, or 124 as timeout(1) does. Whatever it
    started goes when it does, or when this is stopped, since Chromium's processes would otherwise outlive a test
    binary that's gone, and nothing would hold them to the deadline. A stop is only noted where it lands, so it can
    never cut the cleanup short, and acted on once the cleanup is done."""
    stops = []
    previous = {number: signal.signal(number, lambda signum, _: stops.append(signum))
                for number in STOP_SIGNALS}
    process = None
    try:
        process = subprocess.Popen([str(part) for part in command], start_new_session=True, **kwargs)
        deadline = time.monotonic()+seconds
        while not stops:
            try:
                return process.wait(timeout=max(0, min(0.5, deadline-time.monotonic())))
            except subprocess.TimeoutExpired:
                if time.monotonic() >= deadline:
                    print(f'stopped after {seconds} s', flush=True)
                    return 124
        return None
    finally:
        if process:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait()
        for number, handler in previous.items():
            signal.signal(number, handler)
        if stops:
            raise SystemExit(128+stops[0])


def now():
    return datetime.now(timezone.utc).strftime('%Y-%m-%dT%H:%M:%SZ')


def summary(line):
    print(line, flush=True)
    path = os.environ.get('GITHUB_STEP_SUMMARY')
    if path:
        with open(path, 'a') as out:
            out.write(f'- {line}\n')


def timed(label):
    class Timer:
        def __enter__(self):
            self.start = time.monotonic()

        def __exit__(self, kind, *_):
            if kind is None:
                summary(f'{label}: {time.monotonic()-self.start:.0f} s')
    return Timer()


def stamp(name):
    return HOME/'.lanescape-browser-stamps'/pin()['commit']/name


def once(name, step):
    """Runs a step unless a kept disk shows it already finished."""
    done = stamp(name)
    if done.exists():
        print(f'already done: {name}', flush=True)
        return
    step()
    done.parent.mkdir(parents=True, exist_ok=True)
    done.touch()


def pin():
    return json.loads((REPO/'engine/chromium-pin.json').read_text())


def targets():
    return json.loads((REPO/'engine/targets.json').read_text())


def system(target):
    return targets()[target].get('target_os', 'linux')


def out_dir(config):
    return CHROMIUM/'src/out'/config


def build_env():
    """The environment for this build's own scripts."""
    env = dict(os.environ)
    env['CHROMIUM_ROOT'] = str(CHROMIUM)
    return env


def event_context(args):
    return dict(event=os.environ['GITHUB_EVENT_NAME'], ref_name=os.environ['GITHUB_REF_NAME'],
                pr_number=os.environ.get('PR_NUMBER') or None, merge_base=merge_base())


def merge_base():
    """The commit this PR branched from. base.sha won't do, because it moves whenever main does."""
    base, head = os.environ.get('PR_BASE_SHA'), os.environ.get('PR_HEAD_SHA')
    if not base or not head:
        return None
    request = urllib.request.Request(
        f'https://api.github.com/repos/{os.environ["GITHUB_REPOSITORY"]}/compare/{base}...{head}?per_page=1',
        headers={'Authorization': f'Bearer {os.environ["GITHUB_TOKEN"]}', 'Accept': 'application/vnd.github+json'})
    with urllib.request.urlopen(request, timeout=60) as response:
        return json.load(response)['merge_base_commit']['sha']


def exists(path):
    return describe(path) is not None


def bump(path):
    """Marks an object as used, so expiry counts from its last use and not from its creation."""
    run('gcloud', 'storage', 'objects', 'update', path, f'--custom-time={now()}', stdout=subprocess.DEVNULL)


def restore_pack(prefix, commit):
    manifest = json.loads(subprocess.check_output(['gcloud', 'storage', 'cat', f'{prefix}/manifest.json'], text=True))
    if manifest['commit'] != commit:
        raise RuntimeError(f'the pack at {prefix} is for {manifest["commit"]}')
    scratch = HOME/'source.tar.zst'
    with timed('download source pack'):
        run('gcloud', 'storage', 'cp', f'{prefix}/{manifest.get("archive", "source.tar.zst")}', scratch)
    if sha256(scratch) != manifest['sha256']:
        raise RuntimeError('the source pack does not match its manifest')
    CHROMIUM.mkdir(parents=True, exist_ok=True)
    with timed('unpack source pack'):
        shell(f'zstd -dc -T0 -q {shlex.quote(str(scratch))} | tar -x -C {shlex.quote(str(CHROMIUM))}')
    scratch.unlink()
    return manifest


def fetch_depot_tools(revision=None):
    if not DEPOT_TOOLS.exists():
        run('git', 'clone', '-q', '--depth', '1', 'https://chromium.googlesource.com/chromium/tools/depot_tools.git', DEPOT_TOOLS)
    if revision:
        run('git', '-C', DEPOT_TOOLS, 'fetch', '-q', '--depth', '1', 'origin', revision)
        run('git', '-C', DEPOT_TOOLS, 'checkout', '-q', revision)
    run(DEPOT_TOOLS/'ensure_bootstrap')


def gclient_config(target):
    """One source pack serves every target that names it, so it has each one's system and processor."""
    entries = [entry for entry in targets().values() if entry['source_pack'] == targets()[target]['source_pack']]
    systems = sorted({entry.get('target_os', 'linux') for entry in entries})
    cpus = sorted({entry['target_cpu'] for entry in entries})
    return ('solutions = [{\n  "name": "src",\n  "url": "https://chromium.googlesource.com/chromium/src.git",\n'
            '  "managed": False,\n  "custom_vars": {"checkout_pgo_profiles": True},\n}]\n'
            f'target_os = {json.dumps(systems)}\ntarget_cpu = {json.dumps(cpus)}\n')


def same_gclient(wanted):
    path = CHROMIUM/'.gclient'
    return path.is_file() and path.read_text() == wanted


def sync_source(target, commit, tag):
    """Fetches the pin when no pack exists yet. A sync can hit a rate limit and still leave a tree that builds."""
    fetch_depot_tools()
    src = CHROMIUM/'src'
    CHROMIUM.mkdir(parents=True, exist_ok=True)
    (CHROMIUM/'.gclient').write_text(gclient_config(target))
    if not (src/'.git').exists():
        run('git', 'init', '-q', src)
        run('git', '-C', src, 'remote', 'add', 'origin', 'https://chromium.googlesource.com/chromium/src.git')
    run('git', '-C', src, 'fetch', '--depth=1', 'origin', f'refs/tags/{tag}')
    run('git', '-C', src, 'checkout', '-q', '--detach', 'FETCH_HEAD')
    if subprocess.check_output(['git', '-C', src, 'rev-parse', 'HEAD'], text=True).strip() != commit:
        raise RuntimeError(f'tag {tag} is not the pinned {commit}')
    for attempt in range(4):
        result = subprocess.run(['gclient', 'sync', '--nohooks', '--no-history', '--force', '--delete_unversioned_trees', '-j', '4'], cwd=CHROMIUM)
        if result.returncode == 0:
            break
        if attempt == 3:
            raise RuntimeError('gclient sync kept failing')
        time.sleep(300)
    run('gclient', 'runhooks', cwd=CHROMIUM)


def verify_source(commit, synced):
    """That src is at the pin and, after a sync, that every dependency is too: gclient sync can fail on rate limits
    and still leave a tree that builds. A tree from a source pack was checked like this when its pack was made, by
    main alone, and arrives checksummed, so checking it again only costs time."""
    def revisions(*flags):
        lines = subprocess.check_output(['gclient', 'revinfo', *flags], cwd=CHROMIUM, text=True, stderr=subprocess.DEVNULL).splitlines()
        return sorted(line for line in lines if 'chrome-infra-packages' not in line and not line.startswith('src: '))
    if synced:
        wanted, actual = revisions(), revisions('--actual')
        if not wanted or wanted != actual:
            raise RuntimeError(f'the checkout is off its pins: {sorted(set(wanted) ^ set(actual))[:10]}')
    if subprocess.check_output(['git', '-C', CHROMIUM/'src', 'rev-parse', 'HEAD'], text=True).strip() != commit:
        raise RuntimeError('src is not at the pin')


def saves_source_pack():
    """Only main's own code saves a pack, as a PR's can't be trusted by anyone else: a push to main, or a run
    dispatched on main, which is an image build or a seed."""
    return os.environ.get('GITHUB_REF') == 'refs/heads/main' and os.environ['GITHUB_EVENT_NAME'] in ('push', 'workflow_dispatch')


def save_source_pack(prefix, target, commit, tag):
    """Several builds can get here at once, so each uploads its own archive, and creating the manifest, which names
    one archive, is what decides between them. A dispatched run's identity can only create objects, so the loser's
    archive stays, and expires."""
    scratch = HOME/'source.tar.zst'
    shell(f'tar --format=posix -C {shlex.quote(str(CHROMIUM))} --exclude=src/out -cf - . | zstd -3 -T0 -q -f -o {shlex.quote(str(scratch))}')
    run('zstd', '-t', '-q', scratch)
    archive = f'{secrets.token_hex(16)}/source.tar.zst'
    manifest = {'format': 2, 'kind': 'source', 'tag': tag, 'commit': commit, 'archive': archive, 'host': targets()[target]['source_pack'],
                'targets': [name for name, entry in targets().items() if entry['source_pack'] == targets()[target]['source_pack']],
                'depot_tools': subprocess.check_output(['git', '-C', DEPOT_TOOLS, 'rev-parse', 'HEAD'], text=True).strip(),
                'gclient': gclient_config(target), 'bytes': scratch.stat().st_size,
                'sha256': sha256(scratch), 'created': now()}
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp)/'manifest.json'
        path.write_text(json.dumps(manifest, indent=2)+'\n')
        run('gcloud', 'storage', 'cp', f'--custom-time={now()}', scratch, f'{prefix}/{archive}')
        won = subprocess.run(['gcloud', 'storage', 'cp', '--if-generation-match=0', f'--custom-time={now()}', path, f'{prefix}/manifest.json'])
    if won.returncode != 0:
        if os.environ['GITHUB_EVENT_NAME'] == 'push':
            remove_unused_archive(prefix, archive)
        else:
            print('another build published the source pack first; this one expires unused', flush=True)
    scratch.unlink()


def remove_unused_archive(prefix, archive):
    """A failed create may still have gone through, or there may be no manifest at all. Only a manifest naming a
    different archive proves this one unused, and a stray archive just expires."""
    result = subprocess.run(['gcloud', 'storage', 'cat', f'{prefix}/manifest.json'], capture_output=True, text=True)
    named = json.loads(result.stdout).get('archive') if result.returncode == 0 else None
    if named and named != archive:
        print('another build published the source pack first', flush=True)
        run('gcloud', 'storage', 'rm', f'{prefix}/{archive}')
    else:
        print('the source pack upload is unresolved, so its archive stays', flush=True)


def prepare(args, save_pack=True):
    """Each step is stamped once done, for a kept disk. A build image keeps only the source step's stamp, so that
    the other steps, which are quick when there's nothing to do, still pick up a change to them."""
    entry, details = targets()[args.target], pin()
    commit, tag = details['commit'], details['tag']
    gclient = gclient_config(args.target)
    prefix = ci_cache.source_pack(commit, entry['source_pack'], gclient)
    # A source step done for another .gclient, such as an image's from before a target joined its pack, lacks that
    # target's dependencies, so it's done again.
    if stamp('source').exists() and not same_gclient(gclient):
        summary('this checkout was synced for other targets: restoring the source again')
        stamp('source').unlink()

    def source():
        # The manifest goes last and names its archive, so one existing means the pack is complete.
        if exists(f'{prefix}/manifest.json'):
            manifest = restore_pack(prefix, commit)
            fetch_depot_tools(manifest['depot_tools'])
            if os.environ['GITHUB_EVENT_NAME'] == 'push':
                # Every object it needs, or the rest would expire while the archive stays in use.
                for name in (manifest.get('archive', 'source.tar.zst'), 'manifest.json'):
                    bump(f'{prefix}/{name}')
        else:
            summary('no source pack for this pin: syncing')
            with timed('sync'):
                sync_source(args.target, commit, tag)
            verify_source(commit, synced=True)
    once('source', source)
    verify_source(commit, synced=False)
    if not same_gclient(gclient):
        raise RuntimeError('the checkout was not synced for this target')
    if save_pack and saves_source_pack() and not exists(f'{prefix}/manifest.json'):
        save_source_pack(prefix, args.target, commit, tag)

    def system_packages():
        env = {**os.environ, 'DEBIAN_FRONTEND': 'noninteractive'}
        run('sudo', '-E', CHROMIUM/'src/build/install-build-deps.sh', '--no-prompt', '--no-chromeos-fonts', env=env)
        run('sudo', '-E', 'apt-get', 'install', '-yq', *TEST_PACKAGES, env=env)
    once('system_packages', system_packages)
    if 'mac_sdk' in entry:
        # Not stamped: an image keeps only the source, and this is a no-op when the pinned SDK is in place.
        if mac_sdk.install(entry['mac_sdk'], CHROMIUM/'src', lambda path, dest: run(
                'gcloud', 'storage', 'cp', f'{ci_cache.BUCKET}/{path}', dest)):
            summary(f'macOS SDK {entry["mac_sdk"]["sdk"]}, from Xcode {entry["mac_sdk"]["xcode"]}')

    def rust():
        if not shutil.which('rustup'):
            shell('curl -fsS https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none')
        # rust-toolchain.toml picks the version.
        run('rustup', 'toolchain', 'install', cwd=REPO)
    once('rust', rust)
    github_path = os.environ.get('GITHUB_PATH')
    if github_path:
        with open(github_path, 'a') as out:
            out.write(''.join(f'{path}\n' for path in EXTRA_PATH))


def brew_install():
    """Installs what isn't there already. A runner image can have older versions, which do."""
    installed = set(subprocess.check_output(['brew', 'list', '--formula', '-1'], text=True).split())
    wanted = [name for name in BREW_PACKAGES if name not in installed]
    if wanted:
        run('brew', 'install', '--quiet', *wanted, env={**os.environ, 'HOMEBREW_NO_AUTO_UPDATE': '1',
                                                         'HOMEBREW_NO_INSTALL_CLEANUP': '1'})


def restore_out(args):
    out = out_dir(args.config)
    if (out/'build.ninja').exists():
        print('this disk already has out/; a preempted build carries on from it', flush=True)
        return
    key = ci_cache.cache_key(REPO, args.target, args.config)
    context = event_context(args)
    (out.parent).mkdir(parents=True, exist_ok=True)
    order = ci_cache.restore_order(key, **context)
    if context['event'] == 'pull_request':
        # Before main's: this PR's own cache from before it last caught up with main.
        listing = subprocess.run(['gcloud', 'storage', 'ls', '-l', ci_cache.earlier_pr_caches(key, context['pr_number'])],
                                 capture_output=True, text=True).stdout
        # Not the current one, already first: if it fails to restore, an earlier one is still worth trying.
        earlier = ci_cache.newest(listing, exclude=order)
        if earlier:
            order.insert(1, earlier)
    for path in order:
        if not exists(path):
            continue
        try:
            with timed(f'restore {path.split("/cache/")[1]}'):
                shell(f'gcloud storage cat {shlex.quote(path)} | zstd -dc -T0 -q | tar -x -C {shlex.quote(str(out.parent))}')
        except subprocess.CalledProcessError:
            print(f'could not restore {path}; trying the next', flush=True)
            shutil.rmtree(out, ignore_errors=True)
            continue
        if ci_cache.own_cache(key, **context) == path and context['event'] == 'push':
            bump(path)
        return
    summary('no cache to restore: this is a full build')


def explain(args):
    """Says why the build ran what it ran, from Siso's own record, and keeps that record for the run's artifacts."""
    out = out_dir(args.config)
    record = out/'siso_explain'
    if not record.exists():
        return
    lines = record.read_text(errors='replace').splitlines()
    reasons = {}
    for line in lines:
        match = re.match(r'output \S+ older than most recent input (\S+?):', line)
        if match:
            parts = [part for part in match.group(1).split('/') if part not in ('..', '.')]
            reason = 'newer input under ' + '/'.join(parts[:4])
        else:
            reason = re.sub(r'\S*/\S*', '<path>', line)[:120]
        reasons[reason] = reasons.get(reason, 0) + 1
    summary(f'siso explained {len(lines)} rebuilds')
    for reason, count in sorted(reasons.items(), key=lambda item: -item[1])[:12]:
        summary(f'  {count}: {reason}')
    temp = os.environ.get('RUNNER_TEMP')
    if temp:
        kept = Path(temp)/'diagnostics'
        kept.mkdir(parents=True, exist_ok=True)
        shutil.copy2(record, kept/f'siso_explain-{args.target}-{args.config}.txt')


def compile_all(args):
    """Compiles Chromium, the host and the test binaries, recording whether anything compiled at all."""
    log = out_dir(args.config).parent/f'{args.config}.build.log'
    log.parent.mkdir(parents=True, exist_ok=True)
    env = build_env()
    # The cache brings the last build's record; only this build's may be reported.
    (out_dir(args.config)/'siso_explain').unlink(missing_ok=True)
    try:
        shell(f'{shlex.quote(str(REPO/"engine/scripts/build.sh"))} {args.config} {args.target} 2>&1 | tee {shlex.quote(str(log))}', env=env)
    finally:
        # Only a report: it never fails a build, or hides why one failed.
        try:
            explain(args)
        except OSError as error:
            print(f'could not report why the build ran what it did: {error}', flush=True)
    tests = out_dir(args.config)/'tests.json'
    shell('source engine/scripts/env.sh; '
          f'source engine/scripts/cross-env.sh {args.target}; '
          f'cargo test --locked --no-run --target-dir {shlex.quote(str(out_dir(args.config)/"host"))} '
          f'--message-format=json 2>> {shlex.quote(str(log))} > {shlex.quote(str(tests))}', cwd=REPO, env=env)
    text = log.read_text(errors='replace')
    chromium_idle = re.search(r'Build Succeeded: 0 steps|no work to do', text) is not None
    rust_idle = '   Compiling ' not in text
    # A marker left by a preempted machine stays: what it compiled hasn't been saved yet.
    marker = out_dir(args.config)/MARKER
    if not chromium_idle or not rust_idle:
        marker.touch()
    elif not marker.exists():
        summary('nothing compiled')


def save_out(args):
    out = out_dir(args.config)
    marker = out/MARKER
    if not marker.exists():
        print('nothing compiled, so the cache stands', flush=True)
        return
    key = ci_cache.cache_key(REPO, args.target, args.config)
    path = ci_cache.own_cache(key, **event_context(args))
    if path is None:
        print('this build has no cache of its own to write', flush=True)
        return
    scratch = HOME/'out.tar.zst'
    with timed('pack out'):
        # Siso keeps nanosecond times in its state, and only this format stores them.
        shell(f'tar --format=posix --exclude={MARKER} -C {shlex.quote(str(out.parent))} -cf - {args.config} | zstd -3 -T0 -q -f -o {shlex.quote(str(scratch))}')
    summary(f'out/ cache: {scratch.stat().st_size/1e9:.1f} GB')
    with timed('upload out'):
        run('gcloud', 'storage', 'cp', f'--custom-time={now()}', scratch, path)
    scratch.unlink()
    marker.unlink()


def binary_check(path, target):
    """That a binary is for the target's system and processor, as `file` reads it."""
    entry = targets()[target]
    if entry.get('target_os', 'linux') == 'mac':
        kind, machine = 'Mach-O 64-bit', entry['target_cpu']
    else:
        kind, machine = 'ELF', entry['elf_machine']
    description = subprocess.check_output(['file', '-b', path], text=True)
    if not description.startswith(kind) or machine.replace('-', '').lower() not in description.replace('-', '').lower():
        raise RuntimeError(f'{path} is not {kind} for {machine}: {description}')


def missing_libraries(binary):
    """The libraries a binary needs that this machine lacks. On macOS everything the package doesn't carry has to
    be the system's own, since nothing else is on a machine it's unpacked on."""
    if not MAC:
        return [line for line in subprocess.check_output(['ldd', binary], text=True).splitlines() if 'not found' in line]
    lines = subprocess.check_output(['otool', '-L', binary], text=True).splitlines()[1:]
    names = [line.split(' (compatibility')[0].strip() for line in lines]
    return [name for name in names if not name.startswith(('/usr/lib/', '/System/Library/'))]


def test_binaries(config, target):
    """The unit and suite test binaries Cargo built for this target, by name."""
    found = {}
    for line in (out_dir(config)/'tests.json').read_text().splitlines():
        message = json.loads(line)
        if message.get('reason') == 'compiler-artifact' and message.get('profile', {}).get('test') and message.get('executable'):
            found[message['target']['name']] = Path(message['executable'])
    return found


def package(args):
    out = out_dir(args.config)
    llvm_strip = CHROMIUM/'src/third_party/llvm-build/Release+Asserts/bin/llvm-strip'
    dist = REPO/'dist'
    shutil.rmtree(dist, ignore_errors=True)
    mac = system(args.target) == 'mac'
    with tempfile.TemporaryDirectory(dir=HOME) as tmp:
        staging = Path(tmp)/'package'
        staging.mkdir()
        for name in PACKAGE_FILES[system(args.target)]:
            shutil.copy2(out/name, staging/name)
        for name in STRIPPED[system(args.target)]:
            # A Mac's keep their global symbols: the host library's entry point is found by name. Apple silicon runs
            # only signed code, and llvm-strip writes the linker's ad hoc signature again for what it strips;
            # complete checks it on the Mac.
            run(llvm_strip, *(['-x'] if mac else []), staging/name)
        for name in STRIPPED[system(args.target)][:2]:
            binary_check(staging/name, args.target)
        shutil.copy2(REPO/'LICENSE', staging/'LICENSE')
        # Chromium is compiled into the executable, so its notices ship with it: its own licence, and the
        # credits for every third-party library in what this build links.
        shutil.copy2(CHROMIUM/'src/LICENSE', staging/'CHROMIUM-LICENSE')
        run(sys.executable, CHROMIUM/'src/tools/licenses/licenses.py', 'credits', '--gn-out-dir', out,
            '--gn-target', '//lanescape_browser', staging/'CREDITS.html', cwd=CHROMIUM/'src')
        if 'class="product"' not in (staging/'CREDITS.html').read_text():
            raise RuntimeError("Chromium's credits list no libraries")
        shutil.copy2(REPO/'README.md', staging/'README.md')
        shutil.copytree(REPO/'proto', staging/'proto')
        shutil.copytree(REPO/'specs', staging/'specs')
        shutil.copy2(REPO/'third_party/README.md', staging/'ENGINE-SOURCE.md')
        bundle_notices(staging/'licenses', targets()[args.target]['rust_target'])
        dist.mkdir()
        pack(staging, dist/f'lanescape-browser-{args.target}.tar.gz', 'lanescape-browser')
        if mac:
            snapshot = Path(tmp)/'snapshot'
            snapshot.mkdir()
            for name in SNAPSHOT_INPUTS:
                shutil.copy2(out/name, snapshot/name)
            binary_check(snapshot/'v8_context_snapshot_generator', args.target)
            with tarfile.open(dist/f'snapshot-{args.target}.tar', 'w') as tar:
                tar.add(snapshot, arcname='snapshot')
        tests = Path(tmp)/'tests'
        tests.mkdir()
        binaries = test_binaries(args.config, args.target)
        missing = {'lanescape_browser_host', *SUITES} - set(binaries)
        if missing:
            raise RuntimeError(f'missing test binaries: {sorted(missing)}')
        for name, path in binaries.items():
            binary_check(path, args.target)
            shutil.copy2(path, tests/name)
        with tarfile.open(dist/f'tests-{args.target}.tar', 'w') as tar:
            tar.add(tests, arcname='tests')
    summary(f'package: {(dist/f"lanescape-browser-{args.target}.tar.gz").stat().st_size/1e6:.0f} MB')


def hand_off_path(args, completed=False):
    """Where the build machine leaves a build, or, for a macOS one, where its Mac leaves it completed (complete)."""
    path = ci_cache.hand_off(os.environ['GITHUB_EVENT_NAME'], args.target, args.config,
                             pr_number=os.environ.get('PR_NUMBER'), sha=os.environ.get('GITHUB_SHA'),
                             major=os.environ.get('RELEASE_MAJOR'), build=os.environ.get('LANESCAPE_BROWSER_BUILD'))
    return f'{path}-complete' if completed else path


def fetch(prefix, names):
    """Replaces dist/ with these files from the bucket."""
    dist = REPO/'dist'
    shutil.rmtree(dist, ignore_errors=True)
    dist.mkdir()
    for name in names:
        run('gcloud', 'storage', 'cp', f'{prefix}/{name}', dist/name)


def upload(args, completed=False):
    """Keeps the build where the next job, or a person, can reach it."""
    prefix = hand_off_path(args, completed)
    for path in sorted((REPO/'dist').iterdir()):
        run('gcloud', 'storage', 'cp', f'--custom-time={now()}', path, f'{prefix}/{path.name}')


def download(args):
    """Replaces dist/ with one build, so a machine can test dev and then release in turn. A macOS build is the one its
    Mac completed, so every Mac tests the very package that's published."""
    fetch(hand_off_path(args, completed=system(args.target) == 'mac'),
          (f'lanescape-browser-{args.target}.tar.gz', f'tests-{args.target}.tar'))


def complete(args):
    """On a Mac, once for each macOS build: makes V8's context snapshot with the target's own generator, puts it in
    the package, checks that every binary Linux signed ad hoc still verifies, and hands the completed package on to
    the Macs that test it. It stays in dist/, for publishing."""
    fetch(hand_off_path(args), (f'lanescape-browser-{args.target}.tar.gz', f'tests-{args.target}.tar', f'snapshot-{args.target}.tar'))
    dist = REPO/'dist'
    archive = dist/f'lanescape-browser-{args.target}.tar.gz'
    with tempfile.TemporaryDirectory(dir=HOME) as tmp:
        root = Path(tmp)
        unpack(archive, root)
        with tarfile.open(dist/f'snapshot-{args.target}.tar') as tar:
            tar.extractall(root, filter='data')
        package = root/'lanescape-browser'
        # Beside the package's own files, as the generator finds ICU's data where Lanescape browser does.
        for name in SNAPSHOT_INPUTS:
            (root/'snapshot'/name).rename(package/name)
        for name in (*STRIPPED['mac'], 'v8_context_snapshot_generator'):
            run('codesign', '--verify', '--strict', package/name)
        status = run_for(300, ['./v8_context_snapshot_generator', f'--snapshot_blob={SNAPSHOT_INPUTS[0]}',
                               f'--output_file={CONTEXT_SNAPSHOT}'], cwd=package)
        if status or not (package/CONTEXT_SNAPSHOT).is_file():
            sys.exit(f'the context snapshot was not made ({status})')
        for name in SNAPSHOT_INPUTS:
            (package/name).unlink()
        summary(f'context snapshot: {(package/CONTEXT_SNAPSHOT).stat().st_size/1e6:.1f} MB')
        pack(package, archive, 'lanescape-browser')
    (dist/f'snapshot-{args.target}.tar').unlink()
    upload(args, completed=True)


def install_test_packages(args):
    if MAC:
        brew_install()
        return
    env = {**os.environ, 'DEBIAN_FRONTEND': 'noninteractive'}
    run('sudo', '-E', 'apt-get', 'update', '-q', env=env)
    run('sudo', '-E', 'apt-get', 'install', '-yq', *TEST_PACKAGES, env=env)


class Unpacked:
    """The package in dist/ and its test binaries, unpacked and checked, as a machine that didn't build it
    would get them."""
    def __init__(self, target):
        self.target = target

    def __enter__(self):
        dist = REPO/'dist'
        self._tmp = tempfile.TemporaryDirectory(dir=HOME)
        self.root = Path(self._tmp.name)
        unpack(dist/f'lanescape-browser-{self.target}.tar.gz', self.root)
        with tarfile.open(dist/f'tests-{self.target}.tar') as tar:
            tar.extractall(self.root, filter='data')
        self.binary = self.root/'lanescape-browser/lanescape-browser'
        self.tests = self.root/'tests'
        binary_check(self.binary, self.target)
        missing = missing_libraries(self.binary)
        if missing:
            raise RuntimeError(f'the package is missing libraries: {missing}')
        if not MAC:
            # Ubuntu's default stops the sandbox; a package would ship an AppArmor profile instead.
            run('sudo', 'sysctl', '-q', '-w', 'kernel.apparmor_restrict_unprivileged_userns=0')
        self.env = {**os.environ, 'LANESCAPE_BROWSER_BINARY': str(self.binary)}
        return self

    def __exit__(self, *_):
        self._tmp.cleanup()


def launch(args):
    """What every release build must do even where the suites don't run on it: report its version, then start
    Chromium, draw a page and shut down cleanly. --version alone returns before Chromium starts."""
    with Unpacked(args.target) as package:
        result = subprocess.run([package.binary, '--version'], capture_output=True, text=True, timeout=60)
        print(result.stdout, end='', flush=True)
        if result.returncode or not result.stdout.startswith('lanescape-browser '):
            sys.exit(f'--version failed ({result.returncode})')
        build = os.environ.get('LANESCAPE_BROWSER_BUILD')
        if build and build not in result.stdout:
            sys.exit(f'--version does not name build {build}')
        status = run_for(300, [package.tests/'host', '--exact', 'starts_draws_and_stops'], cwd=package.tests,
                         env=package.env)
        summary(f'launch check: {"passed" if status == 0 else f"FAILED ({status})"}')
        if status:
            sys.exit('the launch check failed')


def suites(args):
    """Runs the unit tests and every suite against the package itself, reporting each failure."""
    with Unpacked(args.target) as package:
        tmp, env = package.root, package.env
        failed = []
        for name, command in (('lanescape_browser_host', []), *((suite, ['--test-threads=6']) for suite in SUITES)):
            print(f'::group::{name}', flush=True)
            status = run_for(900, [tmp/'tests'/name, *command], cwd=tmp/'tests', env=env)
            print('::endgroup::', flush=True)
            summary(f'{name}: {"passed" if status == 0 else f"FAILED ({status})"}')
            if status:
                failed.append(name)
        if failed:
            sys.exit(f'failing suites: {", ".join(failed)}')


def checks(args):
    """What needs no Chromium. One machine runs these."""
    run('python3', '-m', 'unittest', 'discover', '-s', 'scripts', '-p', 'test_*.py', cwd=REPO)
    run('cargo', 'fmt', '--check', cwd=REPO)
    run('cargo', 'clippy', '--locked', '--all-targets', '--', '-D', 'warnings', cwd=REPO)


COMMANDS = {'prepare': prepare, 'restore-out': restore_out, 'compile': compile_all, 'save-out': save_out,
            'package': package, 'upload': upload, 'download': download, 'install-test-packages': install_test_packages,
            'suites': suites, 'launch': launch, 'checks': checks, 'complete': complete}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=COMMANDS)
    parser.add_argument('--target', choices=sorted(targets()))
    parser.add_argument('--config', choices=('dev', 'release'))
    args = parser.parse_args()
    COMMANDS[args.command](args)


if __name__ == '__main__':
    main()
