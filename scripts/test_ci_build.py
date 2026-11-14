import os
import signal
import subprocess
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

import ci_build


class UnusedArchive(unittest.TestCase):
    def outcome(self, returncode, stdout):
        calls = []

        def fake(args, **_):
            calls.append(args)
            return subprocess.CompletedProcess(args, returncode, stdout=stdout, stderr='')
        with patch.object(ci_build.subprocess, 'run', fake):
            ci_build.remove_unused_archive('gs://b/p', 'mine/source.tar.zst')
        return [call for call in calls if 'rm' in call]

    def test_another_build_won(self):
        self.assertEqual(self.outcome(0, '{"archive": "theirs/source.tar.zst"}'),
                         [['gcloud', 'storage', 'rm', 'gs://b/p/mine/source.tar.zst']])

    def test_this_build_won_after_all(self):
        self.assertEqual(self.outcome(0, '{"archive": "mine/source.tar.zst"}'), [])

    def test_no_manifest_to_be_sure_by(self):
        self.assertEqual(self.outcome(1, ''), [])

    def test_an_older_manifest_names_no_archive(self):
        self.assertEqual(self.outcome(0, '{"commit": "abc"}'), [])


class SourcePack(unittest.TestCase):
    """Only main's own code saves a source pack: a push to main, or an image build or seed dispatched on main."""

    def prepare(self, event, ref, pack_exists=False, save_pack=True):
        saved = []
        steps = []
        env = {'GITHUB_EVENT_NAME': event, 'GITHUB_REF': ref, 'GITHUB_REF_NAME': ref.rsplit('/', 1)[-1]}
        # A Linux build machine's steps, wherever the tests run.
        with patch.dict(os.environ, env), patch.object(ci_build, 'once', lambda name, step: steps.append(name)), \
                patch.object(ci_build, 'MAC', False), patch.object(ci_build, 'verify_source'), patch.object(ci_build, 'exists', return_value=pack_exists), \
                patch.object(ci_build, 'save_source_pack', lambda *args: saved.append(args)):
            os.environ.pop('GITHUB_PATH', None)
            ci_build.prepare(SimpleNamespace(target='linux-amd64'), save_pack=save_pack)
        self.assertEqual(steps, ['source', 'system_packages', 'rust'])
        return bool(saved)

    def test_main_s_pushes_and_dispatches_save_a_new_pin_s_pack(self):
        self.assertTrue(self.prepare('push', 'refs/heads/main'))
        self.assertTrue(self.prepare('workflow_dispatch', 'refs/heads/main'))

    def test_nothing_else_does(self):
        for event, ref in (('pull_request', 'refs/pull/7/merge'), ('push', 'refs/heads/support/v1'),
                           ('workflow_dispatch', 'refs/heads/topic'), ('push', 'refs/tags/main'),
                           ('workflow_dispatch', 'refs/tags/main'), ('schedule', 'refs/heads/main')):
            self.assertFalse(self.prepare(event, ref), (event, ref))

    def test_not_when_there_is_one(self):
        self.assertFalse(self.prepare('workflow_dispatch', 'refs/heads/main', pack_exists=True))

    def test_not_when_the_caller_leaves_it_to_another_job(self):
        self.assertFalse(self.prepare('workflow_dispatch', 'refs/heads/main', save_pack=False))

    def lose_race(self, event):
        removed = []
        with tempfile.TemporaryDirectory() as tmp, patch.object(ci_build, 'HOME', Path(tmp)), \
                patch.object(ci_build, 'shell', lambda *_: (Path(tmp)/'source.tar.zst').write_bytes(b'x')), \
                patch.object(ci_build, 'run'), patch.object(ci_build.subprocess, 'check_output', return_value='abc -'), \
                patch.object(ci_build.subprocess, 'run', return_value=subprocess.CompletedProcess([], 1)), \
                patch.object(ci_build, 'remove_unused_archive', lambda *args: removed.append(args)), \
                patch.dict(os.environ, {'GITHUB_EVENT_NAME': event}):
            ci_build.save_source_pack('gs://b/p', 'linux-amd64', 'c'*40, '1.2.3.4')
        return removed

    def test_a_push_that_loses_the_race_removes_its_archive(self):
        self.assertEqual(len(self.lose_race('push')), 1)

    def test_a_dispatch_that_loses_the_race_leaves_it_to_expire(self):
        # Its identity can only create.
        self.assertEqual(self.lose_race('workflow_dispatch'), [])



class Platforms(unittest.TestCase):
    def test_a_source_pack_has_each_of_its_targets_systems_and_processors(self):
        self.assertIn('target_os = ["linux"]\ntarget_cpu = ["arm64", "x64"]\n', ci_build.gclient_config('linux-arm64'))
        self.assertIn('target_os = ["mac"]\ntarget_cpu = ["arm64"]\n', ci_build.gclient_config('macos-arm64'))

    def check(self, target, description):
        with patch.object(ci_build.subprocess, 'check_output', return_value=description):
            ci_build.binary_check('binary', target)

    def test_a_binary_must_be_for_its_target(self):
        self.check('linux-amd64', 'ELF 64-bit LSB pie executable, x86-64, version 1 (SYSV)')
        self.check('linux-arm64', 'ELF 64-bit LSB pie executable, ARM aarch64, version 1 (SYSV)')
        self.check('macos-arm64', 'Mach-O 64-bit executable arm64')
        self.check('macos-arm64', 'Mach-O 64-bit dynamically linked shared library arm64')
        for target, description in (('linux-amd64', 'ELF 64-bit LSB pie executable, ARM aarch64'),
                                    ('linux-arm64', 'Mach-O 64-bit executable arm64'),
                                    ('macos-arm64', 'Mach-O 64-bit executable x86_64'),
                                    ('macos-arm64', 'ELF 64-bit LSB shared object, ARM aarch64')):
            with self.assertRaises(RuntimeError):
                self.check(target, description)

    def test_a_mac_package_needs_nothing_but_the_system(self):
        listing = ('lanescape-browser:\n'
                   '\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1.0.0)\n'
                   '\t/System/Library/Frameworks/Metal.framework/Versions/A/Metal (compatibility version 1.0.0)\n')
        with patch.object(ci_build, 'MAC', True), patch.object(ci_build.subprocess, 'check_output', return_value=listing):
            self.assertEqual(ci_build.missing_libraries('lanescape-browser'), [])
        listing += '\t@rpath/libEGL.dylib (compatibility version 0.0.0, current version 0.0.0)\n'
        with patch.object(ci_build, 'MAC', True), patch.object(ci_build.subprocess, 'check_output', return_value=listing):
            self.assertEqual(ci_build.missing_libraries('lanescape-browser'), ['@rpath/libEGL.dylib'])

    def test_a_command_that_runs_too_long_is_stopped_with_what_it_started(self):
        with tempfile.TemporaryDirectory() as tmp:
            child = Path(tmp)/'child'
            status = ci_build.run_for(1, ['bash', '-c', f'(sleep 3; touch {child}) & wait'])
            self.assertEqual(status, 124)
            ci_build.time.sleep(3)
            self.assertFalse(child.exists())
        self.assertEqual(ci_build.run_for(5, ['bash', '-c', 'exit 3']), 3)

    def test_what_a_command_started_goes_when_it_exits(self):
        for status in (0, 3):
            with tempfile.TemporaryDirectory() as tmp:
                child = Path(tmp)/'child'
                self.assertEqual(ci_build.run_for(30, ['bash', '-c', f'(sleep 2; touch {child}) & exit {status}']), status)
                ci_build.time.sleep(3)
                self.assertFalse(child.exists(), status)

    def test_a_stop_while_cleaning_up_waits_for_the_cleanup(self):
        # How a runner cancels a job, Ctrl-C first, and how a session ends: each must wait for the cleanup.
        self.assertEqual(set(ci_build.STOP_SIGNALS), {signal.SIGINT, signal.SIGTERM, signal.SIGHUP})
        real_killpg = os.killpg
        for number in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            before = self.handlers()

            def kill_then_stop(pid, kill):
                os.kill(os.getpid(), number)
                real_killpg(pid, kill)
            with tempfile.TemporaryDirectory() as tmp:
                child = Path(tmp)/'child'
                with patch.object(ci_build.os, 'killpg', kill_then_stop), self.assertRaises(SystemExit) as stopped:
                    ci_build.run_for(30, ['bash', '-c', f'(sleep 2; touch {child}) & exit 0'])
                self.assertEqual(stopped.exception.code, 128+number)
                self.assertEqual(self.handlers(), before)
                ci_build.time.sleep(3)
                self.assertFalse(child.exists(), number)

    def test_the_deadline_is_kept_to_and_statuses_pass_through(self):
        class Process:
            pid, waits = 1, []

            def __init__(self, *args, **kwargs):
                pass

            def __enter__(self):
                return self

            def __exit__(self, *_):
                pass

            def wait(self, timeout=None):
                if timeout is None:
                    return 0
                self.waits.append(round(timeout, 2))
                clock[0] += timeout
                raise subprocess.TimeoutExpired('command', timeout)
        clock = [100.0]
        with patch.object(ci_build.subprocess, 'Popen', Process), patch.object(ci_build.os, 'killpg'), \
                patch.object(ci_build.time, 'monotonic', lambda: clock[0]):
            self.assertEqual(ci_build.run_for(0.75, ['anything']), 124)
            self.assertEqual(Process.waits, [0.5, 0.25])
            Process.waits.clear()
            self.assertEqual(ci_build.run_for(0, ['anything']), 124)
            self.assertEqual(ci_build.run_for(-1, ['anything']), 124)
            self.assertEqual(Process.waits, [0, 0])
        self.assertEqual(ci_build.run_for(5, ['bash', '-c', 'kill -KILL $$']), -signal.SIGKILL)

    def handlers(self):
        return tuple(signal.getsignal(number) for number in ci_build.STOP_SIGNALS)

    def test_a_stop_while_starting_waits_for_the_cleanup(self):
        real_popen, before = ci_build.subprocess.Popen, self.handlers()

        def stop_then_start(*args, **kwargs):
            os.kill(os.getpid(), signal.SIGHUP)
            os.kill(os.getpid(), signal.SIGTERM)
            return real_popen(*args, **kwargs)
        with tempfile.TemporaryDirectory() as tmp:
            child = Path(tmp)/'child'
            with patch.object(ci_build.subprocess, 'Popen', stop_then_start), self.assertRaises(SystemExit) as stopped:
                ci_build.run_for(30, ['bash', '-c', f'(sleep 2; touch {child}) & wait'])
            self.assertEqual(stopped.exception.code, 128+signal.SIGHUP)
            self.assertEqual(self.handlers(), before)
            ci_build.time.sleep(3)
            self.assertFalse(child.exists())

    def test_a_command_that_cannot_start_leaves_things_as_they_were(self):
        before = self.handlers()
        with self.assertRaises(FileNotFoundError):
            ci_build.run_for(30, ['/nonexistent/command'])
        self.assertEqual(self.handlers(), before)

        def stop_then_fail(*args, **kwargs):
            os.kill(os.getpid(), signal.SIGTERM)
            raise OSError('no')
        with patch.object(ci_build.subprocess, 'Popen', stop_then_fail), self.assertRaises(SystemExit) as stopped:
            ci_build.run_for(30, ['anything'])
        self.assertEqual(stopped.exception.code, 128+signal.SIGTERM)
        self.assertEqual(self.handlers(), before)

    def test_stopping_this_stops_what_it_started(self):
        with tempfile.TemporaryDirectory() as tmp:
            child = Path(tmp)/'child'
            script = (f'import os, signal, sys, threading; sys.path.insert(0, {str(Path(ci_build.__file__).parent)!r}); '
                      'import ci_build; '
                      'threading.Timer(1, lambda: os.kill(os.getpid(), signal.SIGTERM)).start(); '
                      f'ci_build.run_for(60, ["bash", "-c", "(sleep 3; touch {child}) & wait"])')
            result = subprocess.run([ci_build.sys.executable, '-c', script], timeout=30)
            self.assertEqual(result.returncode, 128+15)
            ci_build.time.sleep(3)
            self.assertFalse(child.exists())



class Explain(unittest.TestCase):
    def test_says_why_the_build_ran_and_keeps_the_record(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)/'out'/'dev'
            out.mkdir(parents=True)
            (out/'siso_explain').write_text(
                'output obj/a.o older than most recent input ../../sdk/MacOSX.sdk/usr/include/stdio.h: out:x in:+1s\n'
                'output obj/b.o older than most recent input ../../sdk/MacOSX.sdk/usr/include/stdlib.h: out:x in:+1s\n'
                'deps for lanescape-browser dirty: input out/dev/obj/a.o: dirty\n')
            lines = []
            with patch.object(ci_build, 'out_dir', return_value=out), patch.object(ci_build, 'summary', lines.append), \
                    patch.dict(os.environ, {'RUNNER_TEMP': tmp}):
                ci_build.explain(SimpleNamespace(target='macos-arm64', config='dev'))
            self.assertEqual(lines[0], 'siso explained 3 rebuilds')
            self.assertIn('  2: newer input under sdk/MacOSX.sdk/usr/include', lines)
            self.assertTrue((Path(tmp)/'diagnostics/siso_explain-macos-arm64-dev.txt').exists())

    def compile(self, tmp, build_fails, copy_fails):
        out = Path(tmp)/'out'/'dev'
        out.mkdir(parents=True, exist_ok=True)
        (out/'siso_explain').write_text('from the cache\n')

        def build(script, **kwargs):
            self.assertFalse((out/'siso_explain').exists(), 'the cached record was left in place')
            if build_fails:
                raise subprocess.CalledProcessError(1, script)
        copy = patch.object(ci_build.shutil, 'copy2', side_effect=OSError('disk full')) if copy_fails else patch.object(ci_build, 'REPO', ci_build.REPO)
        with patch.object(ci_build, 'out_dir', return_value=out), patch.object(ci_build, 'shell', side_effect=build), \
                patch.object(ci_build, 'summary'), copy, patch.dict(os.environ, {'RUNNER_TEMP': tmp}):
            ci_build.compile_all(SimpleNamespace(target='macos-arm64', config='dev'))

    def test_the_report_never_changes_the_build_s_outcome(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(subprocess.CalledProcessError):
                self.compile(tmp, build_fails=True, copy_fails=True)
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(subprocess.CalledProcessError):
                self.compile(tmp, build_fails=True, copy_fails=False)



class SteadySdkTimes(unittest.TestCase):
    def times(self, plain_status):
        calls, ran = [], []

        def check_output(command, **kwargs):
            return '/sdk\n' if '--show-sdk-path' in command else '/tools/gperf\n'

        def plain(command, **kwargs):
            calls.append(command)
            return SimpleNamespace(returncode=plain_status)
        with patch.object(ci_build.subprocess, 'check_output', side_effect=check_output), \
                patch.object(ci_build.subprocess, 'run', side_effect=plain), patch.object(ci_build.os.path, 'realpath', side_effect=lambda path: path), \
                patch.object(ci_build, 'run', side_effect=lambda *command: ran.append(list(command))), patch.object(ci_build, 'summary'):
            ci_build.steady_sdk_times()
        return calls, ran

    def test_one_time_in_utc_for_the_sdk_and_gperf(self):
        calls, ran = self.times(0)
        self.assertEqual(calls, [['env', 'TZ=UTC', 'find', '/sdk', '/tools/gperf', '-exec', 'touch', '-h', '-t',
                                  '202001010000.00', '{}', '+']])
        self.assertEqual(ran, [])

    def test_retries_as_root_still_in_utc(self):
        calls, ran = self.times(1)
        self.assertEqual(ran, [['sudo', '-n', *calls[0]]])


if __name__ == '__main__':
    unittest.main()
