import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch
import publish
import release
import storage
from test_release import FakeRepo
from version import release_major, unstable


class VersionContracts(unittest.TestCase):
    def test_suites_expect_the_whole_version_line(self):
        import ci
        cargo = '[package]\nversion="3.0.0"\n'
        self.assertEqual(ci.expected_version('20261122.1', cargo), 'lanescape-browser 3.20261122.1')
        self.assertEqual(ci.expected_version('', cargo), 'lanescape-browser 3.development')

    def test_build_tag_major_must_match_package_major(self):
        repo = FakeRepo()
        repo.commit('abc', major=1)
        repo.tags = [('v2.20261031.2', 'abc')]
        with self.assertRaisesRegex(ValueError, 'does not match'):
            release.identify('20261031.2', repo)

    def test_unstable_flag_marks_the_package_major(self):
        base = '[package]\nversion="2.0.0"\n'
        self.assertFalse(unstable(base))
        self.assertTrue(unstable(base+'[package.metadata.lanescape-browser]\nunstable = true\n'))
        self.assertFalse(unstable(base+'[package.metadata.lanescape-browser]\nunstable = false\n'))
        self.assertFalse(unstable(base+'[package.metadata.other]\nunstable = true\n'))
        for value in ('"true"', '1'):
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, 'true or false'):
                unstable(base+f'[package.metadata.lanescape-browser]\nunstable = {value}\n')
        # The repository's own flag is readable.
        self.assertIsInstance(unstable(), bool)

    def test_publication_uses_package_major_without_stability_configuration(self):
        pinned = json.loads(Path('engine/libchromium.json').read_text())
        for major in ('v1', 'v2', 'v3'):
            for flag in ('', '[package.metadata.lanescape-browser]\nunstable = true\n'):
                with self.subTest(major=major, flag=bool(flag)), tempfile.TemporaryDirectory() as tmp:
                    root = Path(tmp)
                    (root/'dist').mkdir()
                    (root/'engine').mkdir()
                    (root/'engine/libchromium.json').write_text(json.dumps(pinned))
                    for target in publish.TARGETS:
                        (root/f'dist/lanescape-browser-{target}.tar.gz').write_bytes(b'package')
                    # Read real configuration via the same paths as the publisher.
                    (root/'Cargo.toml').write_text(f'[package]\nversion="{major[1:]}.0.0"\n{flag}')
                    previous = Path.cwd()
                    try:
                        os.chdir(root)
                        self.assertEqual(release_major(), major)
                        with patch.dict('os.environ', {'BUILD_NUMBER': '20261031.2', 'GITHUB_SHA': 'abc'}), \
                                patch.object(publish, 'libchromium_sources') as sources, \
                                patch.object(storage, 'put_immutable', return_value={'bytes': 1, 'sha256': 'hash'}) as upload:
                            publish.publish(r2='r2')
                        sources.assert_called_once_with(pinned['version'])
                        # The bucket layout keeps the bare build number, and the manifest goes last.
                        self.assertEqual([call.args[2] for call in upload.call_args_list], [
                            f'{major}/20261031.2/lanescape-browser-{target}.tar.gz'
                            for target in ('linux-amd64', 'linux-arm64', 'macos-arm64')
                        ] + [f'{major}/20261031.2/manifest.json'])
                        manifest = json.loads((root/'dist/manifest.json').read_text())
                        self.assertEqual(manifest['build'], '20261031.2')
                        self.assertEqual(manifest['libchromium'], {
                            'version': pinned['version'],
                            'release': f'https://github.com/lanescape/libchromium/releases/tag/v{pinned["version"]}'})
                        self.assertNotIn('unstable', manifest)
                    finally:
                        os.chdir(previous)

    def test_missing_corresponding_source_publishes_nothing(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root/'dist').mkdir()
            (root/'engine').mkdir()
            (root/'engine/libchromium.json').write_text('{"version": "1-1"}')
            for target in publish.TARGETS:
                (root/f'dist/lanescape-browser-{target}.tar.gz').write_bytes(b'package')
            (root/'Cargo.toml').write_text('[package]\nversion="1.0.0"\n')
            previous = Path.cwd()
            try:
                os.chdir(root)
                with patch.dict('os.environ', {'BUILD_NUMBER': '20261031.2', 'GITHUB_SHA': 'abc'}), \
                        patch.object(publish, 'libchromium_sources', side_effect=release.Refused('no source')), \
                        patch.object(storage, 'put_immutable') as upload:
                    with self.assertRaisesRegex(release.Refused, 'no source'):
                        publish.publish(r2='r2')
                upload.assert_not_called()
            finally:
                os.chdir(previous)

    def test_a_missing_platform_publishes_nothing(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root/'dist').mkdir()
            for target in ('linux-amd64', 'linux-arm64'):
                (root/f'dist/lanescape-browser-{target}.tar.gz').write_bytes(b'package')
            (root/'Cargo.toml').write_text('[package]\nversion="1.0.0"\n')
            previous = Path.cwd()
            try:
                os.chdir(root)
                with patch.dict('os.environ', {'BUILD_NUMBER': '20261031.2', 'GITHUB_SHA': 'abc'}), \
                        patch.object(storage, 'put_immutable') as upload, \
                        patch.object(storage.R2, 'from_environment') as r2:
                    with self.assertRaisesRegex(SystemExit, 'macos-arm64'):
                        publish.publish()
                upload.assert_not_called()
                r2.assert_not_called()
            finally:
                os.chdir(previous)


if __name__ == '__main__':
    unittest.main()
