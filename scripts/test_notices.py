import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch
from notices import bundle_notices, runtime_packages


class NoticesContract(unittest.TestCase):
    def test_build_dependencies_are_not_linked_dependencies(self):
        metadata = {'resolve': {'root': 'app', 'nodes': [
            {'id': 'app', 'deps': [
                {'pkg': 'runtime', 'dep_kinds': [{'kind': None}]},
                {'pkg': 'compiler', 'dep_kinds': [{'kind': 'build'}]}]},
            {'id': 'runtime', 'deps': []}, {'id': 'compiler', 'deps': []}]},
            'packages': [{'id': 'app'}, {'id': 'runtime'}, {'id': 'compiler'}]}
        self.assertEqual(runtime_packages(metadata), [{'id': 'app'}, {'id': 'runtime'}])

    def test_linked_crates_bring_their_licenses(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            package = dict(id='crate', name='crate', version='1.0.0', source='registry',
                           manifest_path=str(root/'Cargo.toml'), license='MIT',
                           repository='https://example.com/crate')
            metadata = {'resolve': {'root': 'crate', 'nodes': [{'id': 'crate', 'deps': []}]},
                        'packages': [package]}
            (root/'LICENSE-MIT').write_text('license text')
            with patch('notices.subprocess.check_output', side_effect=[
                    'host: aarch64-apple-darwin', json.dumps(metadata)]):
                bundle_notices(root/'out')
            self.assertEqual((root/'out/crate-1.0.0/LICENSE-MIT').read_text(), 'license text')
            self.assertEqual(json.loads((root/'out/packages.json').read_text()), [
                {'name': 'crate', 'version': '1.0.0', 'license': 'MIT', 'repository': 'https://example.com/crate'}])
            # A crate without a license file stops the package.
            (root/'LICENSE-MIT').unlink()
            with patch('notices.subprocess.check_output', side_effect=[
                    'host: aarch64-apple-darwin', json.dumps(metadata)]):
                with self.assertRaisesRegex(RuntimeError, 'missing dependency license: crate'):
                    bundle_notices(root/'out2')


if __name__ == '__main__':
    unittest.main()
