import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import ci_build
import ci_cache
import ci_seed

REPO = Path(__file__).resolve().parent.parent
TAG = '158.0.8200.10'
COMMIT = 'a'*40
OBJECT = 'f'*40


def listing(*lines):
    return lambda tag: ''.join(f'{sha}\t{ref}\n' for sha, ref in lines)


class Pin(unittest.TestCase):
    """A seed builds only a released Chromium, at the commit its tag names on Chromium's own repository."""

    def test_a_lightweight_tag(self):
        ci_seed.check_pin(TAG, COMMIT, listing((COMMIT, f'refs/tags/{TAG}')))

    def test_an_annotated_tag_by_its_commit(self):
        ci_seed.check_pin(TAG, COMMIT, listing((OBJECT, f'refs/tags/{TAG}'), (COMMIT, f'refs/tags/{TAG}^{{}}')))

    def test_an_annotated_tag_by_its_tag_object_is_refused(self):
        with self.assertRaises(SystemExit):
            ci_seed.check_pin(TAG, OBJECT, listing((OBJECT, f'refs/tags/{TAG}'), (COMMIT, f'refs/tags/{TAG}^{{}}')))

    def test_a_tag_naming_another_commit_is_refused(self):
        with self.assertRaises(SystemExit) as caught:
            ci_seed.check_pin(TAG, COMMIT, listing(('b'*40, f'refs/tags/{TAG}')))
        self.assertIn('b'*40, str(caught.exception))

    def test_no_such_tag_is_refused(self):
        for lines in ((), (('b'*40, f'refs/tags/{TAG}0'),), ((COMMIT, f'refs/heads/{TAG}'),)):
            with self.assertRaises(SystemExit, msg=lines):
                ci_seed.check_pin(TAG, COMMIT, listing(*lines))

    def test_anything_but_a_release_and_a_full_commit_is_refused_unasked(self):
        asked = []

        def lister(tag):
            asked.append(tag)
            return f'{COMMIT}\trefs/tags/{tag}\n'
        for tag, commit in (('main', COMMIT), ('158.0.8200', COMMIT), ('158.0.8200.10.1', COMMIT), ('v158.0.8200.10', COMMIT),
                            ('158.0.8200.10 x', COMMIT), ('refs/changes/12/345/6', COMMIT), (TAG, 'a'*39), (TAG, 'A'*40),
                            (TAG, 'refs/changes/12/345/6'), (TAG, f'{COMMIT}\n')):
            with self.assertRaises(SystemExit, msg=(tag, commit)):
                ci_seed.check_pin(tag, commit, lister)
        self.assertEqual(asked, [])

    def test_asks_chromium_itself_for_the_tag_and_its_peeled_commit(self):
        with patch.object(ci_seed.subprocess, 'check_output', return_value=f'{COMMIT}\trefs/tags/{TAG}\n') as call:
            self.assertEqual(ci_seed.tag_commit(TAG), COMMIT)
        self.assertEqual(call.call_args.args[0], ['git', 'ls-remote', 'https://chromium.googlesource.com/chromium/src',
                                                  f'refs/tags/{TAG}', f'refs/tags/{TAG}^{{}}'])

    def test_writes_only_the_pin(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            shutil.copytree(REPO/'engine/args', root/'engine/args')
            for path in ('engine/targets.json', 'engine/chromium-pin.json', 'rust-toolchain.toml'):
                shutil.copy2(REPO/path, root/path)
            before = ci_cache.cache_key(root, 'linux-amd64', 'dev')
            ci_seed.write_pin(root, TAG, COMMIT)
            self.assertEqual(json.loads((root/'engine/chromium-pin.json').read_text()), {'tag': TAG, 'commit': COMMIT})
            self.assertNotEqual(ci_cache.cache_key(root, 'linux-amd64', 'dev'), before)
            for path in ('engine/targets.json', 'engine/args/dev.gn', 'engine/args/release.gn', 'rust-toolchain.toml'):
                self.assertEqual((root/path).read_bytes(), (REPO/path).read_bytes(), path)

    def test_says_whether_main_has_the_cache(self):
        with tempfile.TemporaryDirectory() as tmp:
            seen = []
            outputs = Path(tmp)/'outputs'
            args = SimpleNamespace(tag=TAG, commit=COMMIT, target='linux-amd64', config='dev')
            with patch.object(ci_seed, 'write_pin'), patch.dict(os.environ, {'GITHUB_OUTPUT': str(outputs)}), \
                    patch.object(ci_build, 'summary'):
                ci_seed.pin(args, listing((COMMIT, f'refs/tags/{TAG}')), exists=lambda path: seen.append(path) or True)
            self.assertEqual(outputs.read_text(), 'cached=true\n')
            self.assertTrue(seen[0].startswith(f'{ci_cache.BUCKET}/cache/main/linux-amd64-dev-'))


class Stub(unittest.TestCase):
    """main's arguments unchanged, with empty targets standing in for Lanescape browser's code."""

    def test_finds_what_main_arguments_name(self):
        texts = [(REPO/'engine/args'/name).read_text() for name in ('dev.gn', 'release.gn')]
        self.assertEqual(ci_seed.referenced_targets(texts), {('', 'lanescape_browser')})

    def test_every_form_of_label(self):
        text = ('root_extra_deps = [ "//lanescape_browser", "//lanescape_browser:tests", "//lanescape_browser/tools", "//lanescape_browser/a/b:c" ]\n'
                '# root_extra_deps = [ "//lanescape_browser:commented" ]\nother = "//chrome:chrome"\n')
        self.assertEqual(ci_seed.referenced_targets([text]),
                         {('', 'lanescape_browser'), ('', 'tests'), ('tools', 'tools'), ('a/b', 'c')})

    def test_what_it_cant_stand_in_for(self):
        for text in ('import("//lanescape_browser/args.gni")\n', 'x = [ "//lanescape_browser:a(//build/toolchain:x)" ]\n',
                     'x = [ "//lanescape_browser/../chrome" ]\n'):
            with self.assertRaises(RuntimeError, msg=text):
                ci_seed.referenced_targets([text])

    def make(self, tmp):
        stub = ci_seed.make_stub(REPO, Path(tmp)/'stub')
        return stub

    def test_the_stub_has_empty_groups_and_main_files(self):
        with tempfile.TemporaryDirectory() as tmp:
            stub = self.make(tmp)
            build = (stub/'BUILD.gn').read_text()
            self.assertIn('group("lanescape_browser") {\n}', build)
            self.assertNotIn('executable', build)
            self.assertNotIn('sources', build)
            self.assertEqual(sorted(p.name for p in stub.iterdir()), ['BUILD.gn', 'args', 'scripts', 'targets.json'])
            for name in ('dev.gn', 'release.gn'):
                self.assertEqual((stub/'args'/name).read_bytes(), (REPO/'engine/args'/name).read_bytes())
            self.assertTrue(os.access(stub/'scripts/configure.sh', os.X_OK))
            # Again, as a rerun on a kept disk would.
            self.assertEqual((self.make(tmp)/'BUILD.gn').read_text(), build)

    def configure(self, home, script, config, target):
        """Runs configure.sh with a stand-in for gn that records what it was asked."""
        tools = home/'depot_tools'
        tools.mkdir(exist_ok=True)
        (tools/'gn').write_text('#!/bin/sh\necho "$@" >"$HOME/gn-called"\ntouch "$2/build.ninja"\n')
        (tools/'gn').chmod(0o755)
        (home/'chromium/src').mkdir(parents=True, exist_ok=True)
        env = {'HOME': str(home), 'PATH': os.environ['PATH']}
        subprocess.run(['bash', str(script), config, target], env=env, check=True, capture_output=True)
        out = home/'chromium/src/out'/config
        args = (out/'args.gn').read_bytes()
        link = os.readlink(home/'chromium/src/lanescape_browser')
        shutil.rmtree(out)
        return args, link

    def test_configures_exactly_as_main_would_with_its_link_on_the_stub(self):
        if not shutil.which('jq'):
            self.skipTest('configure.sh needs jq')
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            stub = self.make(tmp)
            for config in ('dev', 'release'):
                for target in ('linux-amd64', 'linux-arm64'):
                    theirs, real = self.configure(home, REPO/'engine/scripts/configure.sh', config, target)
                    ours, linked = self.configure(home, stub/'scripts/configure.sh', config, target)
                    self.assertEqual(ours, theirs, (config, target))
                    self.assertEqual((Path(real), Path(linked)), (REPO/'engine', stub))

    def test_one_job_saves_each_source_pack(self):
        savers = [(t, c) for t in ci_build.targets() for c in ('dev', 'release') if ci_seed.saves_source_pack(t, c)]
        # Every target builds on Linux, from one pack.
        self.assertEqual(savers, [('linux-amd64', 'dev')])


class Saving(unittest.TestCase):
    """A seed creates main's cache only where there's none, and can never replace or touch one."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        home = Path(self.tmp.name)
        for name, value in (('HOME', home), ('CHROMIUM', home/'chromium')):
            patcher = patch.object(ci_build, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)
        patcher = patch.object(ci_build, 'summary')
        patcher.start()
        self.addCleanup(patcher.stop)
        self.out = ci_build.out_dir('dev')
        self.out.mkdir(parents=True)
        self.args = SimpleNamespace(target='linux-amd64', config='dev')
        self.path = ci_seed.main_cache(ci_cache.cache_key(REPO, 'linux-amd64', 'dev'))

    def save(self, returncode=0, exists=(False,)):
        calls, answers = [], list(exists)

        def runner(args, **_):
            calls.append(args)
            return subprocess.CompletedProcess(args, returncode)
        ci_seed.save(self.args, runner=runner, exists=lambda path: answers.pop(0) if len(answers) > 1 else answers[0])
        return calls

    def test_nothing_generated_nothing_saved(self):
        with self.assertRaises(SystemExit):
            self.save()

    def test_creates_only_if_there_is_nothing_there(self):
        (self.out/'build.ninja').write_text('')
        calls = self.save()
        self.assertEqual(len(calls), 1)
        upload = calls[0]
        self.assertEqual(upload[:4], ['gcloud', 'storage', 'cp', '--if-generation-match=0'])
        self.assertTrue(upload[4].startswith('--custom-time='))
        self.assertEqual(upload[-1], self.path)
        self.assertTrue(self.path.startswith(f'{ci_cache.BUCKET}/cache/main/linux-amd64-dev-'))
        self.assertFalse((ci_build.HOME/'out.tar.zst').exists())

    def test_leaves_an_existing_cache_alone(self):
        (self.out/'build.ninja').write_text('')
        self.assertEqual(self.save(exists=(True,)), [])

    def test_losing_the_race_is_fine(self):
        (self.out/'build.ninja').write_text('')
        self.assertEqual(len(self.save(returncode=1, exists=(False, True))), 1)

    def test_a_failed_upload_fails(self):
        (self.out/'build.ninja').write_text('')
        with self.assertRaises(SystemExit):
            self.save(returncode=1, exists=(False,))

    def test_never_updates_or_deletes(self):
        source = (REPO/'scripts/ci_seed.py').read_text()
        for word in ('bump(', 'objects update', "'rm'", 'storage rm', 'save_out('):
            self.assertNotIn(word, source)

    def test_only_one_job_saves_the_source_pack(self):
        asked = []
        with patch.object(ci_build, 'prepare', lambda args, save_pack: asked.append((args.target, args.config, save_pack))):
            for target in ('linux-amd64', 'linux-arm64'):
                for config in ('dev', 'release'):
                    ci_seed.prepare(SimpleNamespace(target=target, config=config))
        self.assertEqual([(t, c) for t, c, save in asked if save], [('linux-amd64', 'dev')])
        self.assertEqual(len(asked), 4)


if __name__ == '__main__':
    unittest.main()
