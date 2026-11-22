"""Building against libchromium: the pin, the SDK's flags in build.ninja, and the cache's checks."""
import hashlib
import io
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import build

LINUX_SDK = {
    'version': '1-1', 'target': 'linux-arm64', 'system': 'linux', 'sysroot': '{sdk}/sysroot',
    'toolchain': {'cc': '{sdk}/toolchain/bin/clang', 'cxx': '{sdk}/toolchain/bin/clang++',
                  'ar': '{sdk}/toolchain/bin/llvm-ar', 'nm': '{sdk}/toolchain/bin/llvm-nm',
                  'strip': '{sdk}/toolchain/bin/llvm-strip'},
    'defines': ['LIBCHROMIUM_EXPORTS'], 'include_dirs': ['{sdk}/include', '{sdk}/gen'],
    'system_include_dirs': ['{sdk}/include/third_party/libc++/src/include'],
    'compile': {'c++': ['--target=aarch64-unknown-linux-gnu', '--sysroot={sdk}/sysroot', '-std=c++23']},
    'link': {'flags': ['-fuse-ld=lld', '-Wl,-z,defs'], 'libraries': ['{sdk}/lib/libchromium.so', '{sdk}/lib/libc++.so'],
             'system_libs': ['dl'], 'frameworks': []},
}
MAC_SDK = dict(LINUX_SDK, target='macos-arm64', system='mac', sysroot=None,
               compile={'c++': ['--target=arm64-apple-macos', '-isysroot', '{apple_sdk}'],
                        'objective-c++': ['--target=arm64-apple-macos', '-isysroot', '{apple_sdk}', '-fobjc-arc']},
               link={'flags': ['-fuse-ld=lld'], 'libraries': ['{sdk}/lib/libchromium.dylib'], 'system_libs': [],
                     'frameworks': ['IOSurface']})


class Pin(unittest.TestCase):
    def test_pins_every_part_each_target_builds_from(self):
        assets = [{'name': f'libchromium-1-2-{target}-{part}.tar.zst', 'sha256': f'{target}-{part}'}
                  for target in build.TARGETS for part in ('lib', 'headers', 'toolchain', 'sysroot', 'symbols')]
        assets.append({'name': 'libchromium-1-2-source-third-party-blink.tar.zst', 'part': 'source', 'sha256': 's'})
        raw = json.dumps({'assets': assets, 'source': {'chromium': {'tarball': 'https://x/chromium.tar.xz'}}}).encode()
        with tempfile.TemporaryDirectory() as tmp, \
                patch.object(build, 'REPO', Path(tmp)), \
                patch.object(build.urllib.request, 'urlopen', return_value=io.BytesIO(raw)):
            (Path(tmp)/'engine').mkdir()
            build.pin(type('Args', (), {'version': '1-2'}))
            pinned = json.loads((Path(tmp)/'engine/libchromium.json').read_text())
        self.assertEqual(pinned['version'], '1-2')
        # The manifest itself, which publishing holds the release's source to.
        self.assertEqual(pinned['manifest'], hashlib.sha256(raw).hexdigest())
        self.assertIn('libchromium-1-2-linux-arm64-sysroot.tar.zst', pinned['assets'])
        self.assertNotIn('libchromium-1-2-macos-arm64-sysroot.tar.zst', pinned['assets'])
        self.assertFalse([name for name in pinned['assets'] if name.endswith('-symbols.tar.zst')])
        self.assertEqual(len(pinned['assets']), 3*3+2)

    def test_a_version_without_a_part_or_source_is_refused(self):
        source = {'assets': [{'name': 's', 'part': 'source', 'sha256': 's'}], 'source': {'chromium': {'tarball': 'https://x'}}}
        for manifest, reason in ((source, 'has no libchromium-1-2-'), ({'assets': []}, 'publishes no corresponding source'),
                                 ({**source, 'source': {}}, 'publishes no corresponding source')):
            with self.subTest(reason=reason), tempfile.TemporaryDirectory() as tmp, patch.object(build, 'REPO', Path(tmp)), \
                    patch.object(build.urllib.request, 'urlopen', return_value=io.BytesIO(json.dumps(manifest).encode())):
                with self.assertRaisesRegex(SystemExit, reason):
                    build.pin(type('Args', (), {'version': '1-2'}))


class Flags(unittest.TestCase):
    def test_expand_needs_the_apple_sdk_only_where_it_is_named(self):
        self.assertEqual(build.expand('{sdk}/lib', Path('/s'), None), '/s/lib')
        with self.assertRaisesRegex(SystemExit, 'Apple SDK'):
            build.expand('-isysroot {apple_sdk}', Path('/s'), None)

    def test_clang_target(self):
        self.assertEqual(build.clang_target(LINUX_SDK), 'aarch64-unknown-linux-gnu')

    def ninja(self, description, target, apple=None):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)/'out'
            out.mkdir()
            build.write_ninja(out/'build.ninja', target, description, Path('/sdk'), apple, Path('/vpx'), out/'gen', out,
                              [out/'gen/engine/lanescape_browser.mojom.cc'])
            return (out/'build.ninja').read_text().replace(tmp, '<tmp>')

    def test_linux_ninja(self):
        text = self.ninja(LINUX_SDK, 'linux-arm64')
        self.assertIn('--sysroot=/sdk/sysroot', text)
        self.assertIn('-DLIBCHROMIUM_EXPORTS', text)
        self.assertIn('-isystem /sdk/include/third_party/libc++/src/include', text)
        self.assertIn(f'-I{build.REPO}', text)
        self.assertIn('-I/vpx/include', text)
        for name in build.SOURCES + build.PLATFORM_SOURCES['linux']:
            self.assertIn(f'engine/{name}', text)
        self.assertNotIn('tab_view_mac.mm', text)
        self.assertIn('lanescape_browser.mojom.cc', text)
        self.assertIn("/vpx/lib/libvpx.a -Wl,--exclude-libs,libvpx.a -lm /sdk/lib/libchromium.so", text)
        self.assertIn("'-Wl,-rpath,$$ORIGIN'", text)
        self.assertIn('depfile = $out.d', text)

    def test_mac_ninja(self):
        text = self.ninja(MAC_SDK, 'macos-arm64', apple=Path('/apple'))
        self.assertIn('rule objective_cxx', text)
        self.assertIn('-isysroot /apple', text)
        self.assertIn('tab_view_mac.mm.o: objective_cxx', text)
        self.assertNotIn('platform_linux.cc', text)
        self.assertIn('-framework IOSurface -framework AppKit -framework Foundation -framework QuartzCore', text)
        self.assertNotIn('--exclude-libs', text)
        self.assertIn('-Wl,-rpath,@loader_path', text)


class Cache(unittest.TestCase):
    def test_a_part_that_is_not_the_pinned_one_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            pin = {'version': '1-1', 'assets': {f'libchromium-1-1-linux-arm64-{part}.tar.zst': '0'*64
                                                for part in build.parts_of('linux-arm64')}}

            class Response(io.BytesIO):
                def __enter__(self):
                    return self

                def __exit__(self, *_):
                    return False

            with patch.dict(build.os.environ, {'LANESCAPE_BROWSER_CACHE': tmp}), \
                    patch.object(build, 'pinned', return_value=pin), \
                    patch.object(build.urllib.request, 'urlopen', return_value=Response(b'not the part')):
                with self.assertRaisesRegex(SystemExit, 'is not the one engine/libchromium.json pins'):
                    build.fetch_sdk('linux-arm64')
            self.assertFalse((Path(tmp)/'libchromium/1-1/linux-arm64').exists())

    def test_a_cold_cache_unpacks_every_part_into_the_root(self):
        import hashlib
        import subprocess
        import tarfile
        with tempfile.TemporaryDirectory() as tmp:
            parts, assets = {}, {}
            for part, member in (('lib', 'lib/libchromium.so'), ('headers', 'include/a.h'),
                                 ('toolchain', 'toolchain/bin/clang'), ('sysroot', 'sysroot/usr/x')):
                source = Path(tmp)/'src'/part
                (source/member).parent.mkdir(parents=True)
                (source/member).write_text(part)
                if part == 'lib':
                    (source/'sdk.json').write_text('{}')
                tar = Path(tmp)/f'{part}.tar'
                with tarfile.open(tar, 'w') as archive:
                    for path in sorted(source.rglob('*')):
                        archive.add(path, arcname=path.relative_to(source).as_posix(), recursive=False)
                parts[part] = subprocess.run(['zstd', '-q', '-c', str(tar)], check=True, capture_output=True).stdout
                assets[f'libchromium-1-1-linux-arm64-{part}.tar.zst'] = hashlib.sha256(parts[part]).hexdigest()

            class Response(io.BytesIO):
                def __enter__(self):
                    return self

                def __exit__(self, *_):
                    return False

            def urlopen(url):
                return Response(parts[url.rsplit('-', 1)[1].split('.')[0]])

            with patch.dict(build.os.environ, {'LANESCAPE_BROWSER_CACHE': tmp+'/cache'}), \
                    patch.object(build, 'pinned', return_value={'version': '1-1', 'assets': assets}), \
                    patch.object(build.urllib.request, 'urlopen', side_effect=urlopen):
                root = build.fetch_sdk('linux-arm64')
            self.assertEqual(sorted(p.relative_to(root).as_posix() for p in root.rglob('*') if p.is_file()),
                             ['include/a.h', 'lib/libchromium.so', 'sdk.json', 'sysroot/usr/x', 'toolchain/bin/clang'])

    def test_every_target_names_its_libvpx_target_and_host(self):
        for target, entry in build.TARGETS.items():
            self.assertIn(entry['system'], build.PLATFORM_SOURCES)
            self.assertTrue(entry['libvpx_target'])
            self.assertTrue(entry['host_library'].startswith('liblanescape_browser_host.'))
        self.assertIn('apple_sdk', build.TARGETS['macos-arm64'])


if __name__ == '__main__':
    unittest.main()
