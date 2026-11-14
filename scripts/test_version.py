import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch
import publish
import release
from test_release import FakeRepo
from version import release_major, unstable


class VersionContracts(unittest.TestCase):
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
        for major in ('v1', 'v2', 'v3'):
            for flag in ('', '[package.metadata.lanescape-browser]\nunstable = true\n'):
                with self.subTest(major=major, flag=bool(flag)), tempfile.TemporaryDirectory() as tmp:
                    root = Path(tmp)
                    (root/'dist').mkdir()
                    for target in publish.TARGETS:
                        (root/f'dist/lanescape-browser-{target}.tar.gz').write_bytes(b'package')
                    # Read real configuration via the same paths as the publisher.
                    (root/'Cargo.toml').write_text(f'[package]\nversion="{major[1:]}.0.0"\n{flag}')
                    previous = Path.cwd()
                    try:
                        os.chdir(root)
                        self.assertEqual(release_major(), major)
                        with patch.dict('os.environ', {'BUILD_NUMBER': '20261031.2', 'GCS_BUCKET': 'bucket', 'GITHUB_SHA': 'abc'}), patch.object(publish, 'publish_sources', return_value='source-offer') as sources, patch.object(publish, 'put_immutable', return_value={'bytes': 1, 'sha256': 'hash'}) as upload:
                            publish.publish()
                        sources.assert_called_once_with('bucket', major)
                        # The bucket layout keeps the bare build number.
                        self.assertEqual([call.args[1] for call in upload.call_args_list], [
                            f'gs://bucket/{major}/20261031.2/lanescape-browser-{target}.tar.gz'
                            for target in ('linux-amd64', 'linux-arm64', 'macos-arm64')
                        ] + [f'gs://bucket/{major}/20261031.2/manifest.json'])
                        manifest = json.loads((root/'dist/manifest.json').read_text())
                        self.assertEqual(manifest['build'], '20261031.2')
                        self.assertNotIn('unstable', manifest)
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
                with patch.dict('os.environ', {'BUILD_NUMBER': '20261031.2', 'GCS_BUCKET': 'bucket', 'GITHUB_SHA': 'abc'}), patch.object(publish, 'publish_sources') as sources, patch.object(publish, 'put_immutable') as upload:
                    with self.assertRaisesRegex(SystemExit, 'macos-arm64'):
                        publish.publish()
                sources.assert_not_called()
                upload.assert_not_called()
            finally:
                os.chdir(previous)

    def test_source_cli_uses_package_major(self):
        import contextlib
        import io
        import runpy
        import storage
        import version

        pins = json.loads(Path('third_party/sources.json').read_text())
        metadata = [{'size': source['bytes'], 'custom_fields': {'sha256': source['sha256']}}
                    for source in pins['archives']]
        output = io.StringIO()
        with (
            patch.dict('os.environ', {'GCS_BUCKET': 'bucket'}),
            patch.object(version, 'release_major', return_value='v2'),
            patch.object(storage, 'describe', side_effect=metadata),
            patch.object(storage, 'put_immutable') as upload,
            contextlib.redirect_stdout(output),
        ):
            runpy.run_module('publish_sources', run_name='__main__')
        self.assertTrue(output.getvalue().startswith('https://storage.googleapis.com/bucket/v2/sources/'))
        self.assertEqual(upload.call_count, 2)
        self.assertTrue(all(call.args[1].startswith('gs://bucket/v2/sources/') for call in upload.call_args_list))


if __name__ == '__main__':
    unittest.main()
