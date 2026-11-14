import tempfile
import unittest
from pathlib import Path

import ci_cache


class Keys(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        (self.root/'engine/args').mkdir(parents=True)
        (self.root/'engine/targets.json').write_text('{"linux-amd64": {"target_cpu": "x64"}}')
        (self.root/'engine/chromium-pin.json').write_text('{"tag": "1", "commit": "abc"}')
        (self.root/'engine/args/dev.gn').write_text('is_debug = false\n')
        (self.root/'rust-toolchain.toml').write_text('channel = "1"\n')
        self.key = ci_cache.cache_key(self.root, 'linux-amd64', 'dev')

    def test_stable(self):
        self.assertEqual(self.key, ci_cache.cache_key(self.root, 'linux-amd64', 'dev'))
        self.assertTrue(self.key.startswith('linux-amd64-dev-'))

    def test_inputs_change_it(self):
        for path, text in (('engine/chromium-pin.json', '{"tag": "1", "commit": "def"}'),
                           ('engine/args/dev.gn', 'is_debug = true\n'),
                           ('rust-toolchain.toml', 'channel = "2"\n'),
                           ('engine/targets.json', '{"linux-amd64": {"target_cpu": "arm64"}}')):
            before = (self.root/path).read_text()
            (self.root/path).write_text(text)
            self.assertNotEqual(self.key, ci_cache.cache_key(self.root, 'linux-amd64', 'dev'), path)
            (self.root/path).write_text(before)


class Paths(unittest.TestCase):
    main = f'{ci_cache.BUCKET}/cache/main/k/out.tar.zst'

    def test_main_reads_and_writes_only_its_own(self):
        self.assertEqual(ci_cache.own_cache('k', 'push', 'main'), self.main)
        self.assertEqual(ci_cache.restore_order('k', 'push', 'main'), [self.main])

    def test_pr_reads_its_own_then_main(self):
        own = f'{ci_cache.BUCKET}/cache/pr/7/base-k/out.tar.zst'
        self.assertEqual(ci_cache.own_cache('k', 'pull_request', 'x', 7, 'base'), own)
        self.assertEqual(ci_cache.restore_order('k', 'pull_request', 'x', 7, 'base'), [own, self.main])

    def test_pr_needs_a_merge_base(self):
        with self.assertRaises(ValueError):
            ci_cache.own_cache('k', 'pull_request', 'x', 7, None)

    def test_support_branch_keeps_its_own(self):
        own = f'{ci_cache.BUCKET}/cache/support/v1/k/out.tar.zst'
        self.assertEqual(ci_cache.restore_order('k', 'push', 'support/v1'), [own, self.main])

    def test_other_events_write_nothing(self):
        self.assertIsNone(ci_cache.own_cache('k', 'pull_request_target', 'x'))
        self.assertIsNone(ci_cache.own_cache('k', 'push', 'feature'))
        self.assertEqual(ci_cache.restore_order('k', 'pull_request_target', 'x'), [self.main])


class EarlierPrCaches(unittest.TestCase):
    def test_the_newest_of_a_pr_s_earlier_caches_is_picked(self):
        listing = (f'4000  2026-11-07T15:00:00Z  {ci_cache.BUCKET}/cache/pr/14/aaa-k/out.tar.zst\n'
                   f'5000  2026-11-07T17:00:00Z  {ci_cache.BUCKET}/cache/pr/14/bbb-k/out.tar.zst\n'
                   'TOTAL: 2 objects, 9000 bytes (8.79KiB)\n')
        self.assertEqual(ci_cache.newest(listing), f'{ci_cache.BUCKET}/cache/pr/14/bbb-k/out.tar.zst')
        self.assertIsNone(ci_cache.newest(''))
        # The current merge base's cache, tried first anyway, is left out, so an earlier one is still offered.
        current = f'{ci_cache.BUCKET}/cache/pr/14/bbb-k/out.tar.zst'
        self.assertEqual(ci_cache.newest(listing, exclude=[current]), f'{ci_cache.BUCKET}/cache/pr/14/aaa-k/out.tar.zst')
        self.assertEqual(ci_cache.earlier_pr_caches('k', 14), f'{ci_cache.BUCKET}/cache/pr/14/*-k/out.tar.zst')


class HandOff(unittest.TestCase):
    def test_dev_and_release_of_one_target_never_share_a_place(self):
        for event, extra in (('pull_request', {'pr_number': 7, 'sha': 'abc'}), ('push', {'major': 'v1', 'build': '1.1'})):
            dev = ci_cache.hand_off(event, 'linux-arm64', 'dev', **extra)
            release = ci_cache.hand_off(event, 'linux-arm64', 'release', **extra)
            self.assertNotEqual(dev, release)
        self.assertEqual(ci_cache.hand_off('pull_request', 'linux-arm64', 'dev', pr_number=7, sha='abc'),
                         f'{ci_cache.BUCKET}/pr/7/abc/linux-arm64-dev')


class SourceOffer(unittest.TestCase):
    def test_the_published_source_is_the_pinned_chromium(self):
        import json
        root = Path(__file__).resolve().parent.parent
        pin = json.loads((root/'engine/chromium-pin.json').read_text())
        offer = json.loads((root/'third_party/sources.json').read_text())
        self.assertEqual((offer['chromium_version'], offer['chromium_commit']), (pin['tag'], pin['commit']))
        self.assertEqual([a['name'] for a in offer['archives']], [f'chromium-{pin["tag"]}.tar.xz'])


if __name__ == '__main__':
    unittest.main()
