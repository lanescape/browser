#!/usr/bin/env python3
"""What CI runs besides building (scripts/build.py), on the machine of a job's own:

    ci.py checks                          what needs no build: script tests, rustfmt, clippy, actionlint
    ci.py install-test-packages           what the suites need on a fresh Linux or macOS machine
    ci.py suites --target T               every suite against the package in dist/, unpacked
    ci.py launch --target T [--build N]   the package's version and one launch, where the suites don't run

The suites run the package itself, unpacked as a machine that didn't build it gets it, never the build's own out/.
"""
import argparse
import os
import platform
import signal
import subprocess
import sys
import tarfile
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import build  # noqa: E402
from archive import unpack  # noqa: E402

REPO = build.REPO
MAC = platform.system() == 'Darwin'
SUITES = ('ime', 'clipboard', 'host', 'display', 'input', 'recording')
BREW_PACKAGES = ('zstd', 'pigz', 'ffmpeg')
# What Chromium's libraries and the recording tests need on a machine that didn't build them.
TEST_PACKAGES = ('zstd', 'file', 'pigz', 'ffmpeg', 'fonts-liberation', 'fonts-dejavu-core', 'libglib2.0-0t64',
                 'libnspr4', 'libnss3', 'libatk1.0-0t64', 'libatk-bridge2.0-0t64', 'libatspi2.0-0t64',
                 'libdbus-1-3', 'libx11-6', 'libxcomposite1', 'libxdamage1', 'libxext6', 'libxfixes3',
                 'libxrandr2', 'libxcb1', 'libxkbcommon0', 'libgbm1', 'libexpat1', 'libudev1', 'libasound2t64',
                 'libcairo2', 'libpango-1.0-0', 'libcups2t64')


def run(*command, **kwargs):
    print('+', ' '.join(str(part) for part in command), flush=True)
    subprocess.run([str(part) for part in command], check=True, **kwargs)


def summary(line):
    print(line, flush=True)
    if 'GITHUB_STEP_SUMMARY' in os.environ:
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a') as out:
            out.write(f'- {line}\n')


def run_for(seconds, command, **kwargs):
    """Runs a command in a session of its own for at most `seconds`, then stops it and everything it started."""
    process = subprocess.Popen([str(part) for part in command], start_new_session=True, **kwargs)
    deadline = time.monotonic() + seconds
    try:
        return process.wait(timeout=max(1, deadline - time.monotonic()))
    except subprocess.TimeoutExpired:
        print(f'stopped after {seconds} s', flush=True)
        return 124
    finally:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError):
            pass
        process.wait()


def missing_libraries(binary):
    """The libraries a binary needs that this machine lacks. On macOS everything the package doesn't carry has to
    be the system's own, since nothing else is on a machine it's unpacked on."""
    if not MAC:
        return [line for line in subprocess.check_output(['ldd', binary], text=True).splitlines()
                if 'not found' in line]
    lines = subprocess.check_output(['otool', '-L', binary], text=True).splitlines()[1:]
    names = [line.split(' (compatibility')[0].strip() for line in lines]
    return [name for name in names if not name.startswith(('/usr/lib/', '/System/Library/', '@rpath/',
                                                           '@loader_path/'))]


class Unpacked:
    """The package in dist/ and its test executables, unpacked and checked."""
    def __init__(self, target):
        self.target = target

    def __enter__(self):
        dist = REPO/'dist'
        self._tmp = tempfile.TemporaryDirectory(dir=Path.home())
        self.root = Path(self._tmp.name)
        unpack(dist/f'lanescape-browser-{self.target}.tar.gz', self.root)
        with tarfile.open(dist/f'tests-{self.target}.tar') as tar:
            tar.extractall(self.root, filter='data')
        self.binary = self.root/'lanescape-browser/lanescape-browser'
        self.tests = self.root/'tests'
        build.binary_check(self.binary, self.target)
        if missing := missing_libraries(self.binary):
            raise SystemExit(f'the package is missing libraries: {missing}')
        if not MAC and Path('/proc/sys/kernel/apparmor_restrict_unprivileged_userns').exists():
            # Ubuntu's default stops the sandbox; a package would ship an AppArmor profile instead.
            run('sudo', 'sysctl', '-q', '-w', 'kernel.apparmor_restrict_unprivileged_userns=0')
        self.env = {**os.environ, 'LANESCAPE_BROWSER_BINARY': str(self.binary)}
        return self

    def __exit__(self, *_):
        self._tmp.cleanup()


def checks(args):
    """What needs no build. One machine runs these."""
    run(sys.executable, '-m', 'unittest', 'discover', '-s', 'scripts', '-p', 'test_*.py', cwd=REPO)
    run('cargo', 'fmt', '--check', cwd=REPO)
    run('cargo', 'clippy', '--locked', '--all-targets', '--', '-D', 'warnings', cwd=REPO)
    run('go', 'run', 'github.com/rhysd/actionlint/cmd/actionlint@v1.7.12', cwd=REPO)


def install_test_packages(args):
    if MAC:
        missing = [name for name in BREW_PACKAGES
                   if subprocess.run(['brew', 'list', name], capture_output=True).returncode]
        if missing:
            run('brew', 'install', *missing, env={**os.environ, 'HOMEBREW_NO_AUTO_UPDATE': '1'})
        return
    env = {**os.environ, 'DEBIAN_FRONTEND': 'noninteractive'}
    run('sudo', '-E', 'apt-get', 'update', '-q', env=env)
    run('sudo', '-E', 'apt-get', 'install', '-yq', '--no-install-recommends', *TEST_PACKAGES, env=env)


def suites(args):
    """The unit tests and every suite against the package itself, reporting each failure."""
    with Unpacked(args.target) as package:
        failed = []
        for name, extra in (('lanescape_browser_host', []), *((suite, ['--test-threads=6']) for suite in SUITES)):
            print(f'::group::{name}', flush=True)
            status = run_for(900, [package.tests/name, *extra], cwd=package.tests, env=package.env)
            print('::endgroup::', flush=True)
            summary(f'{args.target} {name}: {"passed" if status == 0 else f"FAILED ({status})"}')
            if status:
                failed.append(name)
        if failed:
            sys.exit(f'failing suites: {", ".join(failed)}')


def launch(args):
    """What a build must do even where the suites don't run on it: report its version, then start Chromium, draw a
    page and shut down cleanly. --version alone returns before Chromium starts."""
    with Unpacked(args.target) as package:
        result = subprocess.run([package.binary, '--version'], capture_output=True, text=True, timeout=60)
        print(result.stdout, end='', flush=True)
        if result.returncode or not result.stdout.startswith('lanescape-browser '):
            sys.exit(f'--version failed ({result.returncode})')
        reported = result.stdout.split()[1].split('.', 1)[1] if len(result.stdout.split()) > 1 and \
            '.' in result.stdout.split()[1] else None
        if args.build and reported != args.build:
            sys.exit(f'--version names build {reported}, not {args.build}')
        status = run_for(300, [package.tests/'host', '--exact', 'starts_draws_and_stops'], cwd=package.tests,
                         env=package.env)
        summary(f'{args.target} launch check: {"passed" if status == 0 else f"FAILED ({status})"}')
        if status:
            sys.exit('the launch check failed')


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest='command', required=True)
    sub.add_parser('checks')
    sub.add_parser('install-test-packages')
    for name in ('suites', 'launch'):
        command = sub.add_parser(name)
        command.add_argument('--target', required=True, choices=sorted(build.TARGETS))
        if name == 'launch':
            command.add_argument('--build', help='the build number --version must name')
    args = parser.parse_args()
    {'checks': checks, 'install-test-packages': install_test_packages, 'suites': suites,
     'launch': launch}[args.command](args)


if __name__ == '__main__':
    main()
