#!/usr/bin/env python3
"""Builds Lanescape browser for a target against the libchromium version in engine/libchromium.json, which it never
compiles (specs/0001-architecture.md, Building):

    build.py [--target T] [--release] [--sdk DIR] [--apple-sdk DIR]    builds out/<target>/, ready to run
    build.py package [--target T] [--sdk DIR] [--tests]                packages out/<target>/ into dist/
    build.py pin <version>                                             pins a libchromium version

It downloads the version's parts for the target and checks each against the pin, builds libvpx with the SDK's
toolchain, generates the engine's mojom bindings with the SDK's generator, compiles and links the engine with ninja,
builds the Rust host with cargo, and puts the SDK's runtime files beside them. The SDK, libvpx and the Apple SDK are
kept in a cache (LANESCAPE_BROWSER_CACHE, by default ~/.cache/lanescape-browser), keyed by what they're made from,
so a second build only compiles what changed. The build goes in out/<target>/, or under LANESCAPE_BROWSER_OUT. Every build runs on Linux x86-64, where the SDK's toolchain runs, and
cross-compiles the other targets.
"""
import argparse
import hashlib
import json
import os
import platform
import shutil
import subprocess
import sys
import tempfile
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
TARGETS = json.loads((REPO/'engine/targets.json').read_text())
RELEASES = 'https://github.com/lanescape/libchromium/releases/download'
# The parts an embedder builds from (libchromium's specs/0001-sdk.md, Parts); Linux also needs the sysroot.
PARTS = ('lib', 'headers', 'toolchain')
LINUX_PARTS = ('sysroot',)
EXECUTABLE = 'lanescape-browser'
MOJOM = 'engine/lanescape_browser.mojom'
SOURCES = ('app.cc', 'browser.cc', 'clipboard.cc', 'main.cc', 'recorder.cc', 'renderer.cc', 'tab.cc')
PLATFORM_SOURCES = {'linux': ('platform_linux.cc', 'tab_view_aura.cc'), 'mac': ('platform_mac.mm', 'tab_view_mac.mm')}
FRAMEWORKS = ('AppKit', 'Foundation', 'QuartzCore')
# The engine's own flags: optimised with line tables, as one build serves development and release.
OWN_FLAGS = ('-O2', '-g1', '-Wall', '-Wno-unused-parameter')
# libvpx, for VP8 alone: the recorder's encoder (engine/recorder.cc). Static, with every symbol hidden, so none
# of it is exported beside Chromium's own copy in the library.
LIBVPX_OPTIONS = ('--disable-vp9', '--disable-vp8-decoder', '--enable-realtime-only', '--disable-examples',
                  '--disable-tools', '--disable-docs', '--disable-unit-tests', '--disable-shared', '--enable-static',
                  '--enable-pic', '--disable-install-bins', '--disable-install-srcs', '--disable-webm-io',
                  '--disable-libyuv')


def cache():
    return Path(os.environ.get('LANESCAPE_BROWSER_CACHE', Path.home()/'.cache/lanescape-browser')).resolve()


def out_dir(target):
    """Where a target's build goes: out/<target>/, or under LANESCAPE_BROWSER_OUT, as CI's work disk."""
    return (Path(os.environ.get('LANESCAPE_BROWSER_OUT', REPO/'out'))/target).resolve()


def run(*command, **kwargs):
    print('+', ' '.join(str(part) for part in command), flush=True)
    subprocess.run([str(part) for part in command], check=True, **kwargs)


def sha256(path):
    digest = hashlib.sha256()
    with open(path, 'rb') as stream:
        for chunk in iter(lambda: stream.read(1 << 20), b''):
            digest.update(chunk)
    return digest.hexdigest()


def extract(archive, destination):
    """Extracts a .tar.zst into destination, which appears whole or not at all."""
    staging = Path(tempfile.mkdtemp(prefix='.extract-', dir=destination.parent))
    try:
        zstd = subprocess.Popen(['zstd', '-dc', str(archive)], stdout=subprocess.PIPE)
        subprocess.run(['tar', '-x', '-C', str(staging)], stdin=zstd.stdout, check=True)
        zstd.stdout.close()
        if zstd.wait():
            raise SystemExit(f'zstd could not read {archive}')
        staging.rename(destination)
    finally:
        shutil.rmtree(staging, ignore_errors=True)


def pinned():
    return json.loads((REPO/'engine/libchromium.json').read_text())


def asset_name(version, target, part):
    return f'libchromium-{version}-{target}-{part}.tar.zst'


def parts_of(target):
    return PARTS + (LINUX_PARTS if TARGETS[target]['system'] == 'linux' else ())


def fetch_sdk(target):
    """The pinned SDK for the target, downloaded and checked once: each part's SHA-256 must be the pin's."""
    pin = pinned()
    digests = hashlib.sha256(json.dumps([pin['assets'].get(asset_name(pin['version'], target, part))
                                         for part in parts_of(target)]).encode()).hexdigest()[:12]
    root = cache()/'libchromium'/pin['version']/f'{target}-{digests}'
    if (root/'sdk.json').is_file():
        return root
    root.parent.mkdir(parents=True, exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix='.sdk-', dir=root.parent))
    try:
        for part in parts_of(target):
            name = asset_name(pin['version'], target, part)
            expected = pin['assets'].get(name)
            if not expected:
                raise SystemExit(f'engine/libchromium.json pins no {name}: run build.py pin {pin["version"]}')
            archive = staging/name
            print(f'downloading {name}', flush=True)
            with urllib.request.urlopen(f'{RELEASES}/v{pin["version"]}/{name}') as response, open(archive, 'wb') as out:
                shutil.copyfileobj(response, out, 1 << 20)
            if sha256(archive) != expected:
                raise SystemExit(f'{name} is not the one engine/libchromium.json pins')
            # Each part holds top-level directories of its own, such as lib/, which join the SDK's root.
            unpacked = staging/f'.part-{part}'
            extract(archive, unpacked)
            archive.unlink()
            for entry in unpacked.iterdir():
                entry.rename(staging/entry.name)
            unpacked.rmdir()
        settle(staging, root, root/'sdk.json')
    finally:
        shutil.rmtree(staging, ignore_errors=True)
    return root


def settle(staging, destination, complete):
    """Moves a finished staging directory into its place in the cache. Another build that finished the same entry
    first wins, and this one uses that."""
    try:
        staging.rename(destination)
    except OSError:
        if not complete.exists():
            raise


def apple_sdk(target, given):
    """The Apple SDK that a macOS build compiles with: the one given, or the pinned archive from the organization's
    tools bucket, fetched once with gcloud's credentials and checked."""
    if given:
        return Path(given).resolve()
    pin = TARGETS[target]['apple_sdk']
    root = cache()/'apple-sdk'/pin['sha256']
    sdk = root/'Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk'
    if sdk.is_dir():
        return sdk
    root.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=root) as tmp:
        archive = Path(tmp)/'sdk.tar.zst'
        run('gcloud', 'storage', 'cp', pin['object'], archive)
        if sha256(archive) != pin['sha256']:
            raise SystemExit(f'{pin["object"]} is not the archive engine/targets.json pins')
        extract(archive, Path(tmp)/'Contents')
        settle(Path(tmp)/'Contents', root/'Contents', sdk)
    return sdk


def expand(value, sdk, apple):
    value = value.replace('{sdk}', str(sdk))
    if '{apple_sdk}' in value:
        if not apple:
            raise SystemExit('a macOS build needs the Apple SDK')
        value = value.replace('{apple_sdk}', str(apple))
    return value


def clang_target(description):
    """The --target the SDK compiles for, from its compile flags."""
    for flag in description['compile']['c++']:
        if flag.startswith('--target='):
            return flag.split('=', 1)[1]
    raise SystemExit('sdk.json names no --target')


def build_libvpx(target, sdk, description, apple):
    """libvpx, pinned in engine/libvpx.json, built for the target with the SDK's clang: a static library and its
    headers, under the cache, keyed by libvpx's commit, the SDK's version and the target."""
    pin = json.loads((REPO/'engine/libvpx.json').read_text())
    # What the build depends on beyond the commit, the SDK and the target: its options, and the Apple SDK.
    how = hashlib.sha256(json.dumps([LIBVPX_OPTIONS, TARGETS[target]['libvpx_target'],
                                     TARGETS[target].get('apple_sdk', {}).get('sha256'),
                                     str(apple) if apple else None]).encode()).hexdigest()[:12]
    prefix = cache()/'libvpx'/f'{pin["commit"]}-{sdk_identity(sdk)[:12]}-{target}-{how}'
    if (prefix/'.complete').is_file():
        return prefix
    source = cache()/'src'/f'libvpx-{pin["commit"]}'
    if not source.is_dir():
        source.parent.mkdir(parents=True, exist_ok=True)
        staging = Path(tempfile.mkdtemp(prefix='.libvpx-', dir=source.parent))
        try:
            run('git', 'clone', '--quiet', '--depth', '1', '--branch', pin['tag'], pin['repository'], staging/'src')
            head = subprocess.run(['git', '-C', str(staging/'src'), 'rev-parse', 'HEAD'], check=True, text=True,
                                  capture_output=True).stdout.strip()
            if head != pin['commit']:
                raise SystemExit(f'libvpx {pin["tag"]} is {head}, not the {pin["commit"]} engine/libvpx.json pins')
            settle(staging/'src', source, source/'configure')
        finally:
            shutil.rmtree(staging, ignore_errors=True)
    toolchain = description['toolchain']
    flags = [f'--target={clang_target(description)}', '-fvisibility=hidden', '-O2']
    if description['sysroot']:
        flags.append(f'--sysroot={expand(description["sysroot"], sdk, apple)}')
    if description['system'] == 'mac':
        flags += ['-isysroot', str(apple)]
    flags = ' '.join(flags)
    env = dict(os.environ, CC=f'{expand(toolchain["cc"], sdk, apple)} {flags}',
               CXX=f'{expand(toolchain["cxx"], sdk, apple)} {flags}', AR=expand(toolchain['ar'], sdk, apple),
               NM=expand(toolchain['nm'], sdk, apple), STRIP=expand(toolchain['strip'], sdk, apple),
               LD=f'{expand(toolchain["cc"], sdk, apple)} {flags} -fuse-ld=lld')
    # Installed into a staging directory, which becomes the cache's only once it's whole.
    prefix.parent.mkdir(parents=True, exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix=f'.{prefix.name}-', dir=prefix.parent))
    try:
        with tempfile.TemporaryDirectory(prefix='libvpx-build-') as work:
            run(source/'configure', f'--target={TARGETS[target]["libvpx_target"]}', f'--prefix={staging}',
                *LIBVPX_OPTIONS, cwd=work, env=env)
            run('make', f'-j{os.cpu_count()}', cwd=work, env=env)
            run('make', 'install', cwd=work, env=env)
        if not (staging/'lib/libvpx.a').is_file():
            raise SystemExit('libvpx installed no lib/libvpx.a')
        (staging/'.complete').write_text(json.dumps(pin)+'\n')
        settle(staging, prefix, prefix/'.complete')
    finally:
        shutil.rmtree(staging, ignore_errors=True)
    return prefix


def generate_mojom(description, sdk, gen):
    """The engine's mojom bindings, generated as Chromium's build would (libchromium's specs/0001-sdk.md, Mojom)."""
    mojom = description['mojom']
    paths = {key: [expand(item, sdk, None) for item in value] if isinstance(value, list) else expand(value, sdk, None)
             for key, value in mojom.items() if key != 'features'}
    templates = gen/'mojom-templates'
    templates.mkdir(parents=True, exist_ok=True)
    python = [sys.executable, '-B']
    run(*python, paths['parser'], '--input-root', REPO, *[part for root in paths['imports'] for part in
                                                           ('--input-root', root)],
        '--module-root', paths['modules'], '--output-root', gen, '--mojoms', REPO/MOJOM,
        *[part for feature in mojom['features'] for part in ('--enable-feature', feature)])
    run(*python, paths['generator'], '-o', templates, '--use_bundled_pylibs', 'precompile', '-g', 'c++')
    generate = [*python, paths['generator'], '--use_bundled_pylibs', '-o', gen, 'generate', '-d', REPO, '-I', REPO,
                '--bytecode_path', templates, '-g', 'c++']
    for extra in (['--generate_non_variant_code'], ['--generate_non_variant_code', '--generate_message_ids'],
                  ['--typemap', paths['type_mappings']]):
        run(*generate, *extra, REPO/MOJOM)
    return [gen/(MOJOM+'.cc'), gen/(MOJOM+'-shared.cc')]


def ninja_escape(value):
    return str(value).replace('$', '$$').replace(' ', '$ ').replace(':', '$:')


def ninja_command(values):
    """Arguments for a ninja rule's command: quoted for the shell, with ninja's own $ escaped, so that $ORIGIN reaches
    the linker as itself."""
    return shell_quote(values).replace('$', '$$')


def shell_quote(values):
    import shlex
    return ' '.join(shlex.quote(str(value)) for value in values)


def write_ninja(path, target, description, sdk, apple, libvpx, gen, out, generated):
    """build.ninja for the engine: one compile for each source, with its dependencies from clang's depfile, and
    the link, so a change to one file recompiles that file and links."""
    system = description['system']
    common = (['-D'+define for define in description['defines']] +
              ['-I'+str(REPO), '-I'+str(gen), '-I'+str(libvpx/'include')] +
              ['-I'+expand(path_, sdk, apple) for path_ in description['include_dirs']] +
              [part for path_ in description['system_include_dirs'] for part in ('-isystem', expand(path_, sdk, apple))] +
              list(OWN_FLAGS))
    languages = {'c++': [expand(flag, sdk, apple) for flag in description['compile']['c++']]}
    if system == 'mac':
        languages['objective-c++'] = [expand(flag, sdk, apple) for flag in description['compile']['objective-c++']]
    cxx = expand(description['toolchain']['cxx'], sdk, apple)
    sources = [(REPO/'engine'/name, 'objective-c++' if name.endswith('.mm') else 'c++')
               for name in SOURCES + PLATFORM_SOURCES[system]]
    sources += [(source, 'c++') for source in generated]
    link = description['link']
    if system == 'mac':
        # libvpx is hidden already; nothing more is needed to keep it out of the executable's exports.
        vpx = [str(libvpx/'lib/libvpx.a')]
        rpath = ['-Wl,-rpath,@loader_path']
        frameworks = [part for name in [*link['frameworks'], *FRAMEWORKS] for part in ('-framework', name)]
    else:
        # Its maths comes from the C library's libm, which macOS's libSystem holds.
        vpx = [str(libvpx/'lib/libvpx.a'), '-Wl,--exclude-libs,libvpx.a', '-lm']
        rpath = ['-Wl,-rpath,$ORIGIN']
        frameworks = []
    link_flags = ([expand(flag, sdk, apple) for flag in link['flags']] + vpx +
                  [expand(library, sdk, apple) for library in link['libraries']] +
                  ['-l'+name for name in link['system_libs']] + frameworks + rpath)
    lines = [f'# Written by scripts/build.py for {target}; edit that, not this.', 'ninja_required_version = 1.10',
             f'cxx = {ninja_escape(cxx)}']
    for language, flags in languages.items():
        rule = language.replace('+', 'x').replace('-', '_')
        lines += [f'rule {rule}',
                  f'  command = $cxx -x {language} {ninja_command(flags + common)} -MD -MF $out.d -c $in -o $out',
                  '  depfile = $out.d', '  deps = gcc', f'  description = {language} $in']
    lines += ['rule link', f'  command = $cxx $in {ninja_command(link_flags)} -o $out', '  description = link $out']
    objects = []
    for source, language in sources:
        obj = out/'obj'/(source.name+'.o')
        objects.append(obj)
        lines.append(f'build {ninja_escape(obj)}: {language.replace("+", "x").replace("-", "_")} '
                     f'{ninja_escape(source)}')
    linked = [libvpx/'lib/libvpx.a'] + [Path(expand(library, sdk, apple)) for library in link['libraries']]
    lines.append(f'build {ninja_escape(out/EXECUTABLE)}: link {" ".join(ninja_escape(o) for o in objects)} | '
                 f'{" ".join(ninja_escape(path) for path in linked)}')
    lines.append(f'default {ninja_escape(out/EXECUTABLE)}')
    path.write_text('\n'.join(lines)+'\n')


def linker_wrapper(target, description, sdk, apple, out):
    """A linker for cargo: the SDK's clang for the target, with its sysroot or the Apple SDK."""
    flags = [f'--target={clang_target(description)}', '-fuse-ld=lld']
    if description['sysroot']:
        flags.append(f'--sysroot={expand(description["sysroot"], sdk, apple)}')
    if description['system'] == 'mac':
        flags += ['-isysroot', str(apple)]
    text = f'#!/bin/sh\nexec {shell_quote([expand(description["toolchain"]["cc"], sdk, apple), *flags])} "$@"\n'
    # Named for what it runs, so Cargo, which fingerprints the linker's path, relinks when the SDK changes.
    digest = hashlib.sha256(text.encode()).hexdigest()[:12]
    wrapper = out/f'link-{TARGETS[target]["rust_target"]}-{digest}'
    wrapper.write_text(text)
    wrapper.chmod(0o755)
    return wrapper


def cargo_env(target, description, sdk, apple, out):
    """Cargo's environment for the target: the SDK's clang as its linker."""
    rust = TARGETS[target]['rust_target']
    run('rustup', 'target', 'add', rust, cwd=REPO)
    env = dict(os.environ)
    env[f'CARGO_TARGET_{rust.upper().replace("-", "_")}_LINKER'] = str(linker_wrapper(target, description, sdk,
                                                                                        apple, out))
    if apple:
        # Rust finds the Apple SDK by SDKROOT, rather than with xcrun, which Linux lacks.
        env['SDKROOT'] = str(apple)
    return env


def build_host(target, description, sdk, apple, out, release):
    """The Rust host library, for the target, beside the executable."""
    rust = TARGETS[target]['rust_target']
    env = cargo_env(target, description, sdk, apple, out)
    profile = ['--release'] if release else []
    run('cargo', 'build', '--locked', *profile, '--target', rust, '--manifest-path', REPO/'Cargo.toml',
        '--target-dir', out/'host', env=env, cwd=REPO)
    library = TARGETS[target]['host_library']
    shutil.copy2(out/'host'/rust/('release' if release else 'debug')/library, out/library)


def install_runtime(description, sdk, out):
    """The SDK's runtime libraries and resources beside the executable, where Chromium looks by default."""
    for library in description['runtime']['libraries']:
        shutil.copy2(sdk/library, out/Path(library).name)
    for resource in description['runtime']['resources']:
        destination = out/Path(resource).relative_to('resources')
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(sdk/resource, destination)


def sdk_for(args):
    """The SDK a command uses, given or pinned, and its description, which must be the target's."""
    sdk = Path(args.sdk).resolve() if args.sdk else fetch_sdk(args.target)
    description = json.loads((sdk/'sdk.json').read_text())
    if description['target'] != args.target:
        raise SystemExit(f'{sdk} is the SDK for {description["target"]}, not {args.target}')
    return sdk, description


def build(args):
    if platform.system() != 'Linux' or platform.machine() not in ('x86_64', 'AMD64'):
        raise SystemExit('builds run on Linux x86-64, where the SDK\'s toolchain runs')
    target = args.target
    sdk, description = sdk_for(args)
    if 'mojom' not in description:
        raise SystemExit(f'libchromium {description["version"]} has no mojom tooling; pin a newer version')
    apple = apple_sdk(target, args.apple_sdk) if description['system'] == 'mac' else None
    out = out_dir(target)
    # libvpx's configure and the compile commands take these paths as they are, unquoted.
    for path in (sdk, apple, out, cache()):
        if path and any(character.isspace() for character in str(path)):
            raise SystemExit(f'{path} has whitespace in it, which libvpx\'s build can\'t take: use another path')
    with locked(out):
        # Written again only once every step has succeeded, so a build that stops part-way can't be packaged.
        (out/'build.json').unlink(missing_ok=True)
        identity = build_identity(description, sdk)
        libvpx = build_libvpx(target, sdk, description, apple)
        generated = generate_mojom(description, sdk, out/'gen')
        write_ninja(out/'build.ninja', target, description, sdk, apple, libvpx, out/'gen', out, generated)
        run('ninja', '-C', out)
        build_host(target, description, sdk, apple, out, args.release)
        install_runtime(description, sdk, out)
        if build_identity(description, sdk) != identity:
            raise SystemExit('the sources or the SDK changed while building: build again')
        (out/'build.json').write_text(json.dumps(identity)+'\n')
    print(f'built {out/EXECUTABLE}')


def sdk_identity(sdk):
    """Which SDK this is, beyond its version: its sdk.json, and the contents of every library it links and ships,
    so that an SDK replaced in place is another SDK."""
    description = json.loads((Path(sdk)/'sdk.json').read_text())
    digest = hashlib.sha256((Path(sdk)/'sdk.json').read_bytes())
    for library in sorted(set(description['link']['libraries']) | set(description['runtime']['libraries'])):
        path = Path(expand(library, sdk, None)) if '{sdk}' in library else Path(sdk)/library
        digest.update(f'{path.name}\n{sha256(path)}\n'.encode())
    return digest.hexdigest()


def locked(out):
    """Holds the output directory's lock, so that a package never reads a build another process is changing."""
    import contextlib
    import fcntl

    @contextlib.contextmanager
    def hold():
        out.mkdir(parents=True, exist_ok=True)
        with open(out/'.lock', 'w') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            yield
    return hold()


# What a build compiles from, which its package must match: the engine, the host and its protocol, its tests and
# what it ships beside them.
SOURCES_OF_A_BUILD = ('engine', 'src', 'proto', 'tests', 'specs', 'build.rs', 'Cargo.toml', 'Cargo.lock',
                      'rust-toolchain.toml', 'LICENSE', 'README.md', 'third_party/README.md')


def source_identity():
    """The contents of every file a build compiles from or a package ships, as they are now, edits included."""
    digest = hashlib.sha256()
    for name in SOURCES_OF_A_BUILD:
        root = REPO/name
        for path in sorted([root] if root.is_file() else (p for p in root.rglob('*') if p.is_file())):
            digest.update(f'{path.relative_to(REPO).as_posix()}\n{sha256(path)}\n'.encode())
    return digest.hexdigest()


def build_identity(description, sdk):
    """What a build was made from, which package checks against what it would ship."""
    return {'libchromium': description['version'], 'sdk': sdk_identity(sdk), 'target': description['target'],
            'libvpx': json.loads((REPO/'engine/libvpx.json').read_text())['commit'],
            'sources': source_identity()}


def binary_check(path, target):
    """That a binary is for the target's system and processor, as `file` reads it."""
    entry = TARGETS[target]
    kind, machine = ('Mach-O 64-bit', 'arm64') if entry['system'] == 'mac' else ('ELF', entry['elf_machine'])
    description = subprocess.check_output(['file', '-b', str(path)], text=True)
    if not description.startswith(kind) or machine.replace('-', '').lower() not in description.replace('-', '').lower():
        raise SystemExit(f'{path} is not {kind} for {machine}: {description}')


def build_tests(target, description, sdk, apple, out):
    """The unit tests' and suites' executables, cross-compiled for the target, in dist/tests-<target>.tar, each
    named for its Cargo target (`lanescape_browser_host` for the unit tests), for a machine without Rust."""
    import tarfile
    rust = TARGETS[target]['rust_target']
    env = cargo_env(target, description, sdk, apple, out)
    result = subprocess.run(['cargo', 'test', '--locked', '--no-run', '--message-format=json', '--target', rust,
                             '--manifest-path', str(REPO/'Cargo.toml'), '--target-dir', str(out/'host')],
                            env=env, check=True, capture_output=True, text=True, cwd=REPO)
    found = {}
    for line in result.stdout.splitlines():
        message = json.loads(line)
        if message.get('reason') == 'compiler-artifact' and message.get('profile', {}).get('test') and \
                message.get('executable'):
            found[message['target']['name']] = Path(message['executable'])
    if 'lanescape_browser_host' not in found or len(found) < 2:
        raise SystemExit(f'cargo built no test executables for {target}: {sorted(found)}')
    with tarfile.open(REPO/'dist'/f'tests-{target}.tar', 'w') as tar:
        for name, executable in sorted(found.items()):
            binary_check(executable, target)
            tar.add(executable, arcname=f'tests/{name}')
    print(f'packaged {len(found)} test executables in dist/tests-{target}.tar')


def package(args):
    """dist/lanescape-browser-<target>.tar.gz: what out/<target>/ runs with, its documentation and protocol, and the
    notices for all of it (third_party/README.md): Lanescape browser's licence, libchromium's notices, which say
    where Chromium's source is, libvpx's licence and patent grant, and the Rust crates' notices. With --tests, also
    the test executables (build_tests())."""
    import archive
    import notices
    target = args.target
    out = out_dir(target)
    with locked(out):
        # The previous package goes first, so a refused or failed one never leaves an older one in its place.
        made = [REPO/'dist'/f'lanescape-browser-{target}.tar.gz', REPO/'dist'/f'tests-{target}.tar']
        for path in made:
            path.unlink(missing_ok=True)
        sdk, description = sdk_for(args)
        files = [EXECUTABLE, TARGETS[target]['host_library']]
        files += [Path(library).name for library in description['runtime']['libraries']]
        files += [Path(resource).relative_to('resources').as_posix() for resource in description['runtime']['resources']]
        if missing := [name for name in files if not (out/name).is_file()]:
            raise SystemExit(f'{out} has no {", ".join(missing[:5])}: build it first')
        built = json.loads((out/'build.json').read_text()) if (out/'build.json').is_file() else None
        if built != build_identity(description, sdk):
            raise SystemExit(f'{out} was built from {built}, not {build_identity(description, sdk)}: build it again')
        try:
            libvpx_pin = json.loads((REPO/'engine/libvpx.json').read_text())
            libvpx_source = cache()/'src'/f'libvpx-{libvpx_pin["commit"]}'
            dist = REPO/'dist'
            dist.mkdir(exist_ok=True)
            with tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)/'lanescape-browser'
                for name in files:
                    (root/name).parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(out/name, root/name)
                # Shipped stripped (specs/0004-releases.md): Linux binaries of debug information and symbol tables, but for
                # what they link by; macOS ones of local symbols, which the SDK's llvm-strip signs ad hoc again. The SDK's
                # own libraries ship stripped already.
                strip = expand(description['toolchain']['strip'], sdk, None)
                for name in files[:2]:
                    run(strip, '-x' if description['system'] == 'mac' else '--strip-all', root/name)
                    binary_check(root/name, target)
                shutil.copy2(REPO/'LICENSE', root/'LICENSE')
                shutil.copy2(REPO/'README.md', root/'README.md')
                shutil.copytree(REPO/'proto', root/'proto')
                shutil.copytree(REPO/'specs', root/'specs')
                shutil.copy2(REPO/'third_party/README.md', root/'ENGINE-SOURCE.md')
                shutil.copytree(sdk/'notices', root/'notices/libchromium')
                (root/'notices/libvpx').mkdir(parents=True)
                for name in ('LICENSE', 'PATENTS', 'AUTHORS'):
                    shutil.copy2(libvpx_source/name, root/'notices/libvpx'/name)
                notices.bundle_notices(root/'notices/crates', TARGETS[target]['rust_target'])
                (root/'notices/README').write_text(
                    f'Notices for Lanescape browser ({target}).\n\n'
                    'LICENSE        Lanescape browser\'s own licence, beside this directory.\n'
                    f'libchromium/   libchromium {description["version"]}: Chromium\'s licence, the credits of every '
                    'component it links,\n               and where its corresponding source is. libchromium.so (or '
                    '.dylib), libffmpeg and the\n               other libraries beside the executable are its files.\n'
                    f'libvpx/        libvpx {libvpx_pin["tag"]}, linked into the executable for the recorder\'s VP8 '
                    'encoder.\ncrates/        The Rust crates linked into the host library, and packages.json, their '
                    'inventory.\n')
                archive.pack(root, dist/f'lanescape-browser-{target}.tar.gz', root.name)
            print(f'packaged {dist/f"lanescape-browser-{target}.tar.gz"}')
            if args.tests:
                apple = apple_sdk(target, getattr(args, 'apple_sdk', None)) if description['system'] == 'mac' else None
                build_tests(target, description, sdk, apple, out)
            # What it shipped beside the build, such as proto/, and the tests, came from the checkout as it is now.
            if build_identity(description, sdk) != built:
                raise SystemExit('the sources or the SDK changed while packaging: build and package again')
        except BaseException:
            # Nothing half made, or made from sources that changed, is left to be shipped.
            for path in made:
                path.unlink(missing_ok=True)
            raise


def pin(args):
    """Pins a libchromium version: every part this repository's targets build from, with its SHA-256 from the
    release's manifest, and the manifest's own SHA-256, which publishing holds the release's source to
    (specs/0004-releases.md, Publishing). Review the manifest when moving the pin: it lists the corresponding source."""
    with urllib.request.urlopen(f'{RELEASES}/v{args.version}/manifest.json') as response:
        raw = response.read()
    manifest = json.loads(raw)
    if not [asset for asset in manifest['assets'] if asset.get('part') == 'source'] or \
            not manifest.get('source', {}).get('chromium', {}).get('tarball'):
        raise SystemExit(f'libchromium {args.version} publishes no corresponding source')
    listed = {asset['name']: asset['sha256'] for asset in manifest['assets']}
    assets = {}
    for target in TARGETS:
        for part in parts_of(target):
            name = asset_name(args.version, target, part)
            if name not in listed:
                raise SystemExit(f'libchromium {args.version} has no {name}')
            assets[name] = listed[name]
    (REPO/'engine/libchromium.json').write_text(json.dumps(
        {'version': args.version, 'manifest': hashlib.sha256(raw).hexdigest(), 'assets': assets}, indent=2)+'\n')
    print(f'pinned libchromium {args.version}')


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest='command')
    pin_parser = sub.add_parser('pin', help='pin a libchromium version')
    pin_parser.add_argument('version')
    package_parser = sub.add_parser('package', help='package out/<target>/ into dist/')
    package_parser.add_argument('--target', default='linux-amd64', choices=sorted(TARGETS))
    package_parser.add_argument('--sdk', help='the extracted SDK it was built with, if not the pinned one')
    package_parser.add_argument('--tests', action='store_true', help='also the test executables, for the target')
    package_parser.add_argument('--apple-sdk', help='macOS: the Apple SDK the tests are built with')
    parser.add_argument('--target', default='linux-amd64', choices=sorted(TARGETS))
    parser.add_argument('--release', action='store_true', help='an optimised host, as releases ship')
    parser.add_argument('--sdk', help='an extracted SDK to use instead of the pinned one')
    parser.add_argument('--apple-sdk', help='macOS: the Apple SDK to build with, instead of the pinned archive')
    args = parser.parse_args()
    if args.command == 'pin':
        pin(args)
    elif args.command == 'package':
        package(args)
    else:
        build(args)


if __name__ == '__main__':
    main()
