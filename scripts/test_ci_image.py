import json
import shutil
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import ci_build
import ci_image
import ci_seed

REPO = Path(__file__).resolve().parent.parent
TAG = '158.0.8200.10'
COMMIT = 'a'*40
KEY = 'f'*64


class Pin(unittest.TestCase):
    """An image is of a released Chromium, checked as a seed's is, written into main's checkout."""

    def run_pin(self, root, tag=TAG, commit=COMMIT, key=KEY, found=COMMIT):
        args = SimpleNamespace(tag=tag, commit=commit, key=key)
        lister = lambda t: f'{found}\trefs/tags/{t}\n'
        real = ci_seed.check_pin
        with patch.object(ci_build, 'REPO', root), patch.object(ci_build, 'summary'), \
                patch.object(ci_seed, 'check_pin', lambda t, c: real(t, c, lister)):
            ci_image.pin(args)

    def checkout(self, tmp):
        root = Path(tmp)
        (root/'engine').mkdir()
        shutil.copy2(REPO/'engine/chromium-pin.json', root/'engine/chromium-pin.json')
        return root

    def test_writes_a_checked_pin(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = self.checkout(tmp)
            self.run_pin(root)
            self.assertEqual(json.loads((root/'engine/chromium-pin.json').read_text()), {'tag': TAG, 'commit': COMMIT})

    def test_refuses_what_a_seed_would(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = self.checkout(tmp)
            before = (root/'engine/chromium-pin.json').read_bytes()
            for changes in ({'found': 'b'*40}, {'tag': 'main'}, {'commit': 'A'*40}, {'key': 'f'*63}, {'key': 'F'*64},
                            {'key': f'{KEY}\n'}):
                with self.assertRaises(SystemExit, msg=changes):
                    self.run_pin(root, **changes)
            self.assertEqual((root/'engine/chromium-pin.json').read_bytes(), before)

    def test_uses_the_seed_s_check_not_a_copy(self):
        source = (REPO/'scripts/ci_image.py').read_text()
        self.assertIn('ci_seed.check_pin(', source)
        self.assertNotIn('ls-remote', source)


class Finish(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)/'root'
        self.home = self.root/'home/runner'
        files = ['etc/lanescape-browser-ci/expected-job', 'etc/lanescape-browser-ci/job-started.sh', 'var/cache/apt/pkgcache.bin',
                 'var/cache/apt/archives/x.deb', 'var/lib/apt/lists/archive.ubuntu.com_Packages', 'root/.bash_history',
                 'root/.config/gcloud/access_tokens.db', 'home/runner/.bash_history', 'home/runner/.python_history',
                 'home/runner/.config/gcloud/credentials.db', 'home/runner/.config/other', 'home/ubuntu/.bash_history',
                 'home/runner/.gsutil/credstore2', 'root/.gsutil/credstore2',
                 'etc/ssh/ssh_host_ed25519_key', 'etc/ssh/ssh_host_ed25519_key.pub', 'etc/ssh/sshd_config',
                 'home/runner/chromium/src/README', 'home/runner/.cargo/bin/cargo',
                 f'home/runner/.lanescape-browser-stamps/{COMMIT}/source', f'home/runner/.lanescape-browser-stamps/{COMMIT}/system_packages',
                 f'home/runner/.lanescape-browser-stamps/{COMMIT}/rust', f'home/runner/.lanescape-browser-stamps/{"b"*40}/source']
        for name in files:
            (self.root/name).parent.mkdir(parents=True, exist_ok=True)
            (self.root/name).write_text('x')
        (self.root/'var/lib/apt/lists/partial').mkdir()
        (self.root/'etc/machine-id').write_text('0123456789abcdef\n')

    def finish(self):
        with patch.object(ci_build, 'pin', return_value={'tag': TAG, 'commit': COMMIT}), patch.object(ci_build, 'summary'):
            ci_image.finish(SimpleNamespace(home=str(self.home), root=str(self.root)))

    def left(self):
        return sorted(str(path.relative_to(self.root)) for path in self.root.rglob('*') if path.is_file())

    def test_keeps_only_the_source_stamp(self):
        self.finish()
        stamps = self.home/'.lanescape-browser-stamps'
        self.assertEqual(sorted(str(p.relative_to(stamps)) for p in stamps.rglob('*')), [COMMIT, f'{COMMIT}/source'])

    def test_makes_the_disk_safe_to_image(self):
        self.finish()
        self.assertEqual(self.left(), sorted([
            'etc/lanescape-browser-ci/job-started.sh', 'etc/lanescape-browser-ci/image-ready', 'etc/machine-id', 'etc/ssh/sshd_config',
            'home/runner/.config/other', 'home/runner/chromium/src/README', 'home/runner/.cargo/bin/cargo',
            f'home/runner/.lanescape-browser-stamps/{COMMIT}/source']))
        # Empty, so that each machine booted from the image makes its own.
        self.assertEqual((self.root/'etc/machine-id').read_text(), '')
        self.assertTrue((self.root/'var/lib/apt/lists/partial').is_dir())

    def test_refuses_without_the_source_step(self):
        (self.home/f'.lanescape-browser-stamps/{COMMIT}/source').unlink()
        with self.assertRaises(SystemExit):
            self.finish()
        self.assertFalse((self.root/ci_image.READY).exists())

    def test_image_yml_runs_it_last_as_root(self):
        workflow = (REPO/'.github/workflows/image.yml').read_text()
        steps = [line.strip() for line in workflow.splitlines() if line.strip().startswith('run:')]
        self.assertEqual(steps[-2:], ['run: python3 scripts/ci_build.py prepare --target linux-amd64',
                                      'run: sudo python3 scripts/ci_image.py finish --home "$HOME"'])
        self.assertIn('persist-credentials: false', workflow)
        # gcloud's tokens never reach the disk.
        self.assertIn('CLOUDSDK_CONFIG: /dev/shm/', workflow)
        self.assertIn('BOTO_CONFIG: /dev/shm/', workflow)
        self.assertIn('state_dir = /dev/shm/', workflow)
        self.assertLess(workflow.index('state_dir = /dev/shm/'), workflow.index('scripts/ci_build.py prepare'))


if __name__ == '__main__':
    unittest.main()
