"""Release identity, immutable retries and forward-only promotion tests."""
import contextlib
import hashlib
import io
import itertools
import json
import os
import subprocess
import tempfile
import threading
import unittest
from pathlib import Path
from unittest.mock import patch
import release
import storage

ORIGINAL_SOURCES = release.libchromium_sources

LIBCHROMIUM_TEXT = Path('engine/libchromium.json').read_text()
LIBCHROMIUM = json.loads(LIBCHROMIUM_TEXT)['version']


def setUpModule():
    # Promotion asks GitHub whether libchromium's release publishes its source; LibchromiumSources tests that.
    guard = patch.object(release, 'libchromium_sources', return_value=None)
    guard.start()
    unittest.addModuleCleanup(guard.stop)


class FakeRepo:
    """Commits with one parent each, origin branches and build tags."""

    def __init__(self):
        self.parents, self.files, self.tags, self.branches = {}, {}, [], {}

    def commit(self, sha, parent=None, major=1, unstable=None):
        cargo = f'[package]\nversion="{major}.0.0"\n'
        if unstable is not None:
            cargo += f'\n[package.metadata.lanescape-browser]\nunstable = {str(unstable).lower()}\n'
        self.parents[sha] = parent
        self.files[sha] = {'Cargo.toml': cargo, 'engine/libchromium.json': LIBCHROMIUM_TEXT}

    def show(self, sha, path):
        return self.files[sha][path]

    def branch_contains(self, branch, sha):
        return branch in self.branches and self.is_ancestor(sha, self.branches[branch])

    def is_ancestor(self, ancestor, descendant):
        if ancestor not in self.parents or descendant not in self.parents:
            raise RuntimeError('unknown commit')
        while descendant is not None:
            if descendant == ancestor:
                return True
            descendant = self.parents[descendant]
        return False

    def build_tags(self):
        return list(self.tags)


class FakeBucket:
    """Objects with ETags; latest supports only compare-and-swap writes, like R2's conditional writes."""

    def __init__(self):
        self.objects, self.version, self.writes, self.lock = {}, 0, [], threading.Lock()
        self.gate = lambda op: contextlib.nullcontext()
        self.before_write = None

    def put(self, key, data, **meta):
        self.version += 1
        self.objects[key] = {'data': data, 'etag': f'"{self.version}"', 'meta': meta}

    def read(self, key):
        found = self.objects.get(key)
        return None if found is None else found['data'].decode()

    def digest(self, key):
        found = self.objects.get(key)
        return found and (len(found['data']), hashlib.sha256(found['data']).hexdigest())

    def read_latest(self, major):
        with self.gate('read'), self.lock:
            found = self.objects.get(f'{major}/latest')
            return (None, None) if found is None else (found['data'].decode(), found['etag'])

    def write_latest(self, major, build, etag):
        if self.before_write:
            self.before_write(major, build)
        with self.gate('write'), self.lock:
            found = self.objects.get(f'{major}/latest')
            if (found['etag'] if found else None) != etag:
                return False
            self.put(f'{major}/latest', (build+'\n').encode(), cache_control='no-store')
            self.writes.append(build)
            return True

    def latest(self, major='v1'):
        found = self.objects.get(f'{major}/latest')
        return found and found['data'].decode()


def publish(bucket, repo, sha, build, major='v1', tag=None, targets=release.TARGETS, manifest=True, **extra):
    """Publish a complete build of sha as the CD workflow does, manifest last."""
    packages = {}
    for target in targets:
        data = f'{target} {build}'.encode()
        packages[target] = {'name': f'lanescape-browser-{target}.tar.gz', 'bytes': len(data),
                            'sha256': hashlib.sha256(data).hexdigest()}
        bucket.put(f'{major}/{build}/lanescape-browser-{target}.tar.gz', data)
    if manifest:
        bucket.put(f'{major}/{build}/manifest.json', json.dumps({
            'major': major, 'build': build, 'commit': sha, 'packages': packages,
            'libchromium': release.libchromium(LIBCHROMIUM), **extra}).encode())
    repo.tags.append((tag or f'{major}.{build}', sha))


def history():
    """main: o - a - b, with builds 20261106.1, 20261107.1 and .2; support/v1: o - s with build 20261107.3."""
    repo, bucket = FakeRepo(), FakeBucket()
    for sha, parent in (('o', None), ('a', 'o'), ('b', 'a'), ('s', 'o')):
        repo.commit(sha, parent)
    repo.branches = {'main': 'b', 'support/v1': 's'}
    publish(bucket, repo, 'o', '20261106.1')
    publish(bucket, repo, 'a', '20261107.1')
    publish(bucket, repo, 'b', '20261107.2')
    publish(bucket, repo, 's', '20261107.3')
    return repo, bucket


class BuildTags(unittest.TestCase):
    def test_build_tags_carry_their_major(self):
        self.assertEqual(release.parse_tag('v1.20261107.1'), ('v1', '20261107.1'))
        self.assertEqual(release.parse_tag('v12.20261107.30'), ('v12', '20261107.30'))
        for name in ('v1', 'v2', 'v0.20261107.1', 'v01.20261107.1', 'v1.20261107.0', 'v1.2026110.1',
                     '20261107.1', '20261107.01', 'latest', 'v1.20261107.1.1', 'x20261107.1', 'v1-20261107.1'):
            with self.subTest(name=name):
                self.assertIsNone(release.parse_tag(name))

    def test_build_lookup_finds_the_tagged_commit(self):
        repo, _ = history()
        self.assertEqual(release.identify('20261106.1', repo), ('o', 'v1'))
        self.assertEqual(release.identify('20261107.2', repo), ('b', 'v1'))
        with self.assertRaisesRegex(release.Refused, 'exactly one'):
            release.identify('20261107.9', repo)
        # A tag without a major names no build.
        repo.tags.append(('20261107.8', 'b'))
        with self.assertRaisesRegex(release.Refused, 'exactly one'):
            release.identify('20261107.8', repo)
        for build in ('2026110.1', '20261102.0', 'v1', 'v1.20261107.1', '20261102.1/../x', ''):
            with self.subTest(build=build), self.assertRaisesRegex(ValueError, 'build number'):
                release.identify(build, repo)

    def test_build_tag_major_must_match_package_major(self):
        repo = FakeRepo()
        repo.commit('abc', major=1)
        repo.tags = [('v2.20261107.1', 'abc')]
        with self.assertRaisesRegex(release.Refused, 'does not match package major v1'):
            release.identify('20261107.1', repo)
        repo.commit('def', major=2)
        repo.tags = [('v2.20261107.2', 'def')]
        self.assertEqual(release.identify('20261107.2', repo), ('def', 'v2'))

    def test_a_contested_number_belongs_to_the_published_commit(self):
        # x was published as 20261107.1; y's reservation created v2.20261107.1 from a stale
        # listing and was cancelled before moving on. Tags alone cannot tell them apart.
        repo, bucket = FakeRepo(), FakeBucket()
        repo.commit('x'); repo.commit('y', 'x', major=2)
        repo.branches = {'main': 'y', 'support/v1': 'x'}
        publish(bucket, repo, 'x', '20261107.1')
        repo.tags.append(('v2.20261107.1', 'y'))
        with self.assertRaisesRegex(release.Refused, 'found 2'):
            release.identify('20261107.1', repo)
        self.assertEqual(release.identify('20261107.1', repo, bucket), ('x', 'v1'))
        # The Promote build workflow's lock lookup has no bucket and still lets promotion run.
        self.assertEqual(release.promotion_group('20261107.1', repo), 'v1,v2')
        self.assertEqual(release.promote('20261107.1', bucket, repo), 'promoted')
        with self.assertRaisesRegex(release.Refused, 'no build tag'):
            release.promotion_group('20261107.9', repo)


class FakeRemote:
    """GitHub's tag refs: creating an existing name fails and reports where it points."""

    def __init__(self, tags=(), support=()):
        self.tags, self.support, self.created, self.on_create = list(tags), list(support), [], None
        self.gate = lambda op: contextlib.nullcontext()

    def matching_refs(self, namespace):
        with self.gate('list'):
            if namespace == 'tags':
                return [{'ref': f'refs/tags/{name}', 'object': {'sha': sha}} for name, sha in self.tags]
            return [{'ref': f'refs/heads/support/{major}', 'object': {'sha': 'f'}} for major in self.support]

    def create_tag(self, name, sha):
        with self.gate('create'):
            self.created.append(name)
            if self.on_create:
                self.on_create(name, sha)
            existing = dict(self.tags)
            if name in existing:
                return existing[name]
            self.tags.append((name, sha))
            return sha

    def patch(self, create=True):
        stack = contextlib.ExitStack()
        stack.enter_context(patch.object(release, 'matching_refs', side_effect=self.matching_refs))
        if create:
            stack.enter_context(patch.object(release, 'create_tag', side_effect=self.create_tag))
        return stack


@patch.object(release, 'today', return_value='20261107')
class Reservation(unittest.TestCase):
    def setUp(self):
        # No test may reach the real GitHub API, or run a command.
        for guard in (patch.object(release, 'run', side_effect=AssertionError('unexpected external command')),
                      patch.object(release.github, 'request', side_effect=AssertionError('unexpected API call')),
                      patch.dict('os.environ', {'GITHUB_REPOSITORY': 'lanescape/browser'})):
            guard.start()
            self.addCleanup(guard.stop)

    def major(self, major):
        return patch.object(release, 'release_major', return_value=major)

    @patch.dict('os.environ', {'GITHUB_REF': 'refs/heads/main'})
    def test_reservation_reuses_the_exact_commit_s_tag(self, _):
        remote = FakeRemote([('v1.20261031.1', 'abc')])
        with self.major('v1'), remote.patch():
            self.assertEqual(release.reserve('abc'), '20261031.1')
        self.assertEqual(remote.created, [])

    @patch.dict('os.environ', {'GITHUB_REF': 'refs/heads/main'})
    def test_reservation_counts_every_major_s_tags_and_creates_a_major_tag(self, _):
        # Tags that aren't build tags, such as one without a major, count for nothing.
        remote = FakeRemote([('v1.20261107.2', 'one'), ('v2.20261107.4', 'other'), ('v1.20261106.9', 'yesterday'),
                             ('20261107.7', 'unrelated'), ('20261107.1', 'abc'), ('v1', 'unrelated')])
        with self.major('v1'), remote.patch():
            self.assertEqual(release.reserve('abc'), '20261107.5')
        self.assertEqual(remote.created, ['v1.20261107.5'])

    @patch.dict('os.environ', {'GITHUB_REF': 'refs/heads/main'})
    def test_new_tag_is_created_through_the_api(self, _):
        remote = FakeRemote()

        def api(method, path, body=None):
            remote.tags.append((body['ref'].removeprefix('refs/tags/'), body['sha']))
            return 201, {}, None
        with self.major('v1'), remote.patch(create=False), patch.object(release.github, 'request', side_effect=api) as call:
            self.assertEqual(release.reserve('abc'), '20261107.1')
        self.assertEqual(call.call_args.args, ('POST', 'repos/lanescape/browser/git/refs',
                                               {'ref': 'refs/tags/v1.20261107.1', 'sha': 'abc'}))
        self.assertEqual(remote.tags, [('v1.20261107.1', 'abc')])

    @patch.dict('os.environ', {'GITHUB_REF': 'refs/heads/main'})
    def test_concurrent_same_commit_gets_same_build(self, _):
        remote = FakeRemote()

        def api(method, path, body=None):
            if method == 'POST':
                remote.tags.append(('v1.20261107.1', 'abc'))  # the other run got there first
                raise release.github.GitHubError(method, path, 422)
            self.assertEqual(path, 'repos/lanescape/browser/git/ref/tags/v1.20261107.1')
            return 200, {'object': {'sha': 'abc'}}, None
        with self.major('v1'), remote.patch(create=False), patch.object(release.github, 'request', side_effect=api):
            self.assertEqual(release.reserve('abc'), '20261107.1')

    @patch.dict('os.environ', {'GITHUB_REF': 'refs/heads/main'})
    def test_an_api_failure_is_not_a_collision(self, _):
        def api(method, path, body=None):
            raise release.github.GitHubError(method, path, 403)
        with self.major('v1'), FakeRemote().patch(create=False), patch.object(release.github, 'request', side_effect=api):
            with self.assertRaisesRegex(release.github.GitHubError, '403'):
                release.reserve('abc')

    @patch.dict('os.environ', {'GITHUB_REF': 'refs/heads/main'})
    def test_same_major_collision_takes_the_next_number(self, _):
        remote = FakeRemote()
        remote.on_create = lambda name, sha: (setattr(remote, 'on_create', None), remote.tags.append((name, 'other')))
        with self.major('v1'), remote.patch():
            self.assertEqual(release.reserve('abc'), '20261107.2')
        self.assertEqual(remote.created, ['v1.20261107.1', 'v1.20261107.2'])

    @patch.dict('os.environ', {'GITHUB_REF': 'refs/heads/support/v1'})
    def test_cross_major_race_keeps_the_number_unique(self, _):
        # main (v2) takes the same number at the same moment under its own tag name.
        remote = FakeRemote()
        remote.on_create = lambda name, sha: (setattr(remote, 'on_create', None), remote.tags.append(('v2.20261107.1', 'main')))
        with self.major('v1'), remote.patch():
            self.assertEqual(release.reserve('abc'), '20261107.2')
        self.assertEqual(remote.created, ['v1.20261107.1', 'v1.20261107.2'])

    @patch.dict('os.environ', {'GITHUB_REF': 'refs/heads/main'})
    def test_a_stale_reservation_returns_the_number_its_commit_already_kept(self, _):
        # While this run collides on .1, another run for the same commit keeps .2.
        remote = FakeRemote()
        remote.on_create = lambda name, sha: (setattr(remote, 'on_create', None),
                                              remote.tags.extend([(name, 'other'), ('v1.20261107.2', 'abc')]))
        with self.major('v1'), remote.patch():
            self.assertEqual(release.reserve('abc'), '20261107.2')
        self.assertEqual(remote.created, ['v1.20261107.1'])

    @patch.dict('os.environ', {'GITHUB_REF': 'refs/heads/main'})
    def test_reruns_give_up_a_contested_number(self, _):
        # x kept .1; y created v2.20261107.1 from a stale listing and was cancelled. Reruns of
        # either never fail and never reuse .1; x's published .1 stays identifiable by its manifest.
        remote = FakeRemote([('v1.20261107.1', 'x'), ('v2.20261107.1', 'y')])
        with remote.patch():
            with self.major('v1'):
                self.assertEqual(release.reserve('x'), '20261107.2')
            with self.major('v2'):
                self.assertEqual(release.reserve('y'), '20261107.3')
            with self.major('v1'):
                self.assertEqual(release.reserve('x'), '20261107.2')
        self.assertEqual(remote.created, ['v1.20261107.2', 'v2.20261107.3'])

    @patch.dict('os.environ', {'GITHUB_REF': 'refs/heads/main'})
    def test_concurrent_reservations_of_two_majors_never_share_a_number(self, _):
        # x (v1) and y (v2) interleave their listings and tag creations in every order.
        for order in itertools.product('xy', repeat=6):
            with self.subTest(order=''.join(order)):
                remote = FakeRemote()
                schedule = Schedule(order)
                remote.gate = schedule.turn
                majors = {'x': 'v1', 'y': 'v2'}
                with remote.patch(), patch.object(release, 'release_major', side_effect=lambda: majors[threading.current_thread().name]):
                    results = schedule.run({'x': lambda: release.reserve('x'), 'y': lambda: release.reserve('y')})
                # The commit that gave a number up may keep its tag for it, but never returns it.
                self.assertNotEqual(results['x'], results['y'])
                for sha, build in results.items():
                    self.assertIn((sha, majors[sha]), release.holders(remote.tags, build))

    @patch.dict('os.environ', {'GITHUB_REF': 'refs/pull/1/merge'})
    def test_pr_cannot_reserve_build(self, _):
        with self.assertRaises(ValueError):
            release.reserve('abc')

    def test_support_branch_reserves_only_its_package_major(self, _):
        with self.major('v1'), FakeRemote([('v1.20261031.1', 'abc')]).patch():
            with patch.dict('os.environ', {'GITHUB_REF': 'refs/heads/support/v1'}):
                self.assertEqual(release.reserve('abc'), '20261031.1')
            for ref in ('refs/heads/support/v2', 'refs/heads/support/v1/x', 'refs/heads/support/v01', 'refs/heads/feature'):
                with self.subTest(ref=ref), patch.dict('os.environ', {'GITHUB_REF': ref}):
                    with self.assertRaises(ValueError):
                        release.reserve('abc')

    @patch.dict('os.environ', {'GITHUB_REF': 'refs/heads/main'})
    def test_main_must_advance_beyond_support_majors(self, _):
        with FakeRemote([('v2.20261031.1', 'abc')], support=['v1']).patch():
            with self.major('v1'):
                with self.assertRaisesRegex(ValueError, 'advance'):
                    release.reserve('abc')
            with self.major('v2'):
                self.assertEqual(release.reserve('abc'), '20261031.1')

class ForwardOnlyPromotion(unittest.TestCase):
    def test_first_promotion_creates_latest(self):
        repo, bucket = history()
        self.assertEqual(release.promote('20261106.1', bucket, repo), 'promoted')
        self.assertEqual(bucket.latest(), '20261106.1\n')
        self.assertEqual(bucket.objects['v1/latest']['meta'], {'cache_control': 'no-store'})

    def test_promotion_moves_to_descendants(self):
        repo, bucket = history()
        for build in ('20261106.1', '20261107.1', '20261107.2'):
            self.assertEqual(release.promote(build, bucket, repo), 'promoted')
        self.assertEqual(bucket.writes, ['20261106.1', '20261107.1', '20261107.2'])
        # A newer build may skip builds in between.
        repo, bucket = history()
        release.promote('20261106.1', bucket, repo)
        self.assertEqual(release.promote('20261107.2', bucket, repo), 'promoted')

    def test_promoting_latest_again_changes_nothing(self):
        repo, bucket = history()
        release.promote('20261107.1', bucket, repo)
        etag = bucket.objects['v1/latest']['etag']
        self.assertEqual(release.promote('20261107.1', bucket, repo), 'current')
        self.assertEqual(bucket.objects['v1/latest']['etag'], etag)

    def test_older_and_sibling_builds_are_refused(self):
        for latest, build, reason in (('20261107.2', '20261107.1', 'not newer'),
                                      ('20261107.2', '20261106.1', 'not newer'),
                                      ('20261107.1', '20261107.3', 'does not descend'),
                                      ('20261107.3', '20261107.2', 'does not descend')):
            with self.subTest(latest=latest, build=build):
                repo, bucket = history()
                release.promote(latest, bucket, repo)
                with self.assertRaisesRegex(release.Refused, reason) as refused:
                    release.promote(build, bucket, repo)
                self.assertEqual(bucket.latest(), latest+'\n')
                # Only an older build is "superseded", which automatic promotion treats as nothing to do; a
                # build off the line of latest is always an error.
                self.assertEqual(isinstance(refused.exception, release.Superseded), reason == 'not newer')

    def test_another_build_of_the_latest_commit_is_refused(self):
        repo, bucket = history()
        release.promote('20261107.2', bucket, repo)
        repo.tags.remove(('v1.20261107.2', 'b'))
        publish(bucket, repo, 'b', '20261107.5')
        with self.assertRaisesRegex(release.Refused, 'not newer'):
            release.promote('20261107.5', bucket, repo)

    def test_incomplete_or_corrupt_builds_are_refused(self):
        cases = {
            'no manifest': (dict(manifest=False), 'no manifest'),
            'missing platform': (dict(targets=release.TARGETS[:2]), 'platform'),
            'other commit': (dict(sha='a'), 'identity'),
        }
        for name, (options, reason) in cases.items():
            with self.subTest(name):
                repo, bucket = history()
                repo.commit('c', 'b'); repo.branches['main'] = 'c'
                publish(bucket, repo, options.pop('sha', 'c'), '20261107.9', **options)
                repo.tags[-1] = ('v1.20261107.9', 'c')
                with self.assertRaisesRegex(release.Refused, reason):
                    release.promote('20261107.9', bucket, repo)
                self.assertEqual(bucket.writes, [])
        repo, bucket = history()
        bucket.objects['v1/20261107.2/lanescape-browser-linux-arm64.tar.gz']['data'] += b'!'
        with self.assertRaisesRegex(release.Refused, 'integrity'):
            release.promote('20261107.2', bucket, repo)
        repo, bucket = history()
        del bucket.objects['v1/20261107.2/lanescape-browser-macos-arm64.tar.gz']
        with self.assertRaisesRegex(release.Refused, 'integrity'):
            release.promote('20261107.2', bucket, repo)
        # The manifest must name the libchromium release the commit pins, whose release has the corresponding
        # source.
        for libchromium in (None, release.libchromium('156.0.1.1-1'),
                            {'version': LIBCHROMIUM, 'release': 'https://example.com/elsewhere'}):
            with self.subTest(libchromium=libchromium):
                repo, bucket = history()
                repo.commit('c', 'b'); repo.branches['main'] = 'c'
                publish(bucket, repo, 'c', '20261107.9', libchromium=libchromium)
                with self.assertRaisesRegex(release.Refused, 'corresponding source'):
                    release.promote('20261107.9', bucket, repo)
                self.assertEqual(bucket.writes, [])

    def test_builds_off_their_majors_branches_or_major_are_refused(self):
        repo, bucket = history()
        repo.commit('feature', 'b')
        publish(bucket, repo, 'feature', '20261107.8')
        with self.assertRaisesRegex(release.Refused, 'not on main or support/v1'):
            release.promote('20261107.8', bucket, repo)
        # A v2 commit on support/v1 is not on any v2 branch.
        repo.commit('t', 's', major=2); repo.branches['support/v1'] = 't'
        publish(bucket, repo, 't', '20261107.9', major='v2')
        with self.assertRaisesRegex(release.Refused, 'not on main or support/v2'):
            release.promote('20261107.9', bucket, repo)
        # Published under v1 but tagged by a v2 package.
        repo, bucket = history()
        repo.commit('u', 'b', major=2); repo.branches['main'] = 'u'
        publish(bucket, repo, 'u', '20261107.9', tag='v1.20261107.9')
        with self.assertRaisesRegex(release.Refused, 'does not match package major v2'):
            release.promote('20261107.9', bucket, repo)
        self.assertEqual(bucket.writes, [])

    def test_latest_without_a_known_build_is_refused(self):
        repo, bucket = history()
        bucket.put('v1/latest', b'20261101.1\n')
        with self.assertRaisesRegex(release.Refused, 'no manifest'):
            release.promote('20261107.2', bucket, repo)
        bucket.put('v1/latest', b'garbage\n')
        with self.assertRaisesRegex(release.Refused, 'build number'):
            release.promote('20261107.2', bucket, repo)

    def test_failed_precondition_rechecks_against_the_new_latest(self):
        # Another promotion lands between this one's read and write: newer than ours → refuse.
        repo, bucket = history()
        release.promote('20261106.1', bucket, repo)
        bucket.before_write = lambda major, build: (setattr(bucket, 'before_write', None),
                                                    release.promote('20261107.2', bucket, repo))
        with self.assertRaisesRegex(release.Refused, 'not newer'):
            release.promote('20261107.1', bucket, repo)
        self.assertEqual(bucket.latest(), '20261107.2\n')
        # Older than ours → retry and land on top of it.
        repo, bucket = history()
        release.promote('20261106.1', bucket, repo)
        bucket.before_write = lambda major, build: (setattr(bucket, 'before_write', None),
                                                    release.promote('20261107.1', bucket, repo))
        self.assertEqual(release.promote('20261107.2', bucket, repo), 'promoted')
        self.assertEqual(bucket.writes, ['20261106.1', '20261107.1', '20261107.2'])

    def test_retries_are_bounded(self):
        repo, bucket = history()
        bucket.before_write = lambda major, build: bucket.put('v1/latest', b'20261106.1\n')
        with self.assertRaisesRegex(RuntimeError, 'kept changing'):
            release.promote('20261107.2', bucket, repo)
        self.assertEqual(bucket.writes, [])

    def test_concurrent_promotions_never_move_latest_backwards(self):
        # A (older) and B (newer) interleave their reads and compare-and-swap writes in every
        # order, including retries after a failed precondition; latest always ends at B.
        older, newer = '20261107.1', '20261107.2'
        for start in (None, '20261106.1'):
            for order in itertools.product('AB', repeat=6):
                with self.subTest(start=start, order=''.join(order)):
                    repo, bucket = history()
                    if start:
                        release.promote(start, bucket, repo)
                        bucket.writes.clear()
                    schedule = Schedule(order)
                    bucket.gate = schedule.turn
                    results = schedule.run({'A': lambda: release.promote(older, bucket, repo),
                                            'B': lambda: release.promote(newer, bucket, repo)})
                    self.assertEqual(bucket.latest(), newer+'\n')
                    self.assertEqual(results['B'], 'promoted')
                    self.assertIn(results['A'], ('promoted', 'not newer'))
                    self.assertEqual(bucket.writes[-1], newer)
                    self.assertEqual(bucket.writes.count(older), results['A'] == 'promoted')


class Schedule:
    """Grant each promotion's latest read or write in a fixed order, then in any order."""

    def __init__(self, order):
        self.order, self.done, self.cv = list(order), set(), threading.Condition()

    @contextlib.contextmanager
    def turn(self, op):
        name = threading.current_thread().name
        with self.cv:
            while True:
                while self.order and self.order[0] in self.done:
                    self.order.pop(0)
                if not self.order or self.order[0] == name:
                    break
                if not self.cv.wait(timeout=5):
                    raise RuntimeError('schedule stalled')
        try:
            yield
        finally:
            with self.cv:
                if self.order:
                    self.order.pop(0)
                self.cv.notify_all()

    def run(self, jobs):
        results = {}

        def work(name, job):
            try:
                results[name] = job()
            except release.Refused as error:
                results[name] = 'not newer' if 'not newer' in str(error) else str(error)
            finally:
                with self.cv:
                    self.done.add(name)
                    self.cv.notify_all()
        threads = [threading.Thread(target=work, name=name, args=(name, job)) for name, job in jobs.items()]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(timeout=10)
        return results


class UnstableAutoPromotion(unittest.TestCase):
    def test_only_unstable_majors_promote_automatically(self):
        for flag, expected in ((True, 'promoted'), (False, 'stable'), (None, 'stable')):
            with self.subTest(unstable=flag):
                repo, bucket = history()
                repo.commit('c', 'b', unstable=flag); repo.branches['main'] = 'c'
                publish(bucket, repo, 'c', '20261107.9')
                self.assertEqual(release.promote_if_unstable('20261107.9', bucket, repo), expected)
                self.assertEqual(bucket.writes, ['20261107.9'] if flag else [])

    def test_unstable_promotion_is_still_forward_only(self):
        repo, bucket = history()
        repo.commit('c', 'b', unstable=True); repo.branches['main'] = 'c'
        publish(bucket, repo, 'c', '20261107.9')
        release.promote('20261107.9', bucket, repo)
        repo.files['a'] = repo.files['c']
        with self.assertRaisesRegex(release.Refused, 'not newer'):
            release.promote_if_unstable('20261107.1', bucket, repo)

    def test_cli(self):
        repo, bucket = history()
        repo.commit('c', 'b', unstable=True); repo.branches['main'] = 'c'
        publish(bucket, repo, 'c', '20261107.9')
        with patch.object(release, 'Repo', return_value=repo), patch.object(release, 'Bucket', return_value=bucket), \
                patch.object(storage.R2, 'from_environment', return_value='r2') as r2:
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                release.main(['promote', '--build', '20261107.9', '--if-unstable'])
                release.main(['promote', '--build', '20261107.2', '--if-unstable'])
                release.main(['major', '--build', '20261107.9'])
            self.assertEqual(output.getvalue().splitlines(), [
                'promoted 20261107.9',
                'not promoting 20261107.2: its major is stable; promote it with the Promote build workflow',
                'v1'])
            # Promotion uses the release bucket, with the environment's keys.
            r2.assert_called_with()
            with self.assertRaisesRegex(SystemExit, 'refused: 20261107.2 is not newer'):
                release.main(['promote', '--build', '20261107.2'])
        self.assertEqual(bucket.writes, ['20261107.9'])


class LibchromiumSources(unittest.TestCase):
    """The pinned libchromium release must publish its corresponding source."""
    VERSION = '157.0.8103.39-4'

    def check(self, release_assets, manifest, status=200):
        import io
        def api(method, path, body=None):
            self.assertEqual(path, f'repos/lanescape/libchromium/releases/tags/v{self.VERSION}')
            if status != 200:
                raise release.github.GitHubError(method, path, status)
            return 200, {'assets': [{'name': n, 'size': s, 'browser_download_url': f'https://dl/{n}'}
                                    for n, s in release_assets.items()]}, None
        response = io.BytesIO(json.dumps(manifest).encode())
        response.__enter__ = lambda: response
        response.__exit__ = lambda *a: False
        with patch.object(release.github, 'request', side_effect=api), \
                patch.object(release.urllib.request, 'urlopen', return_value=contextlib.nullcontext(response)):
            return self.original(self.VERSION)

    def setUp(self):
        # The module's guard replaced the function; test the real one.
        self.original = ORIGINAL_SOURCES

    def test_a_release_with_every_source_archive_passes(self):
        manifest = {'version': self.VERSION, 'assets': [
            {'name': 'a-source-blink.tar.zst', 'part': 'source', 'size': 10},
            {'name': 'a-linux-amd64-lib.tar.zst', 'part': 'lib', 'size': 5}]}
        self.assertIsNone(self.check({'manifest.json': 1, 'a-source-blink.tar.zst': 10, 'a-linux-amd64-lib.tar.zst': 5}, manifest))

    def test_missing_or_short_source_is_refused(self):
        manifest = {'version': self.VERSION, 'assets': [{'name': 'a-source-blink.tar.zst', 'part': 'source', 'size': 10}]}
        for assets, reason in (({'manifest.json': 1}, 'lacks a-source-blink'),
                               ({'manifest.json': 1, 'a-source-blink.tar.zst': 9}, 'lacks a-source-blink'),
                               ({'a-source-blink.tar.zst': 10}, 'no manifest')):
            with self.subTest(assets=assets), self.assertRaisesRegex(release.Refused, reason):
                self.check(assets, manifest)
        with self.assertRaisesRegex(release.Refused, 'lists no source'):
            self.check({'manifest.json': 1}, {'version': self.VERSION, 'assets': []})
        with self.assertRaisesRegex(release.Refused, 'lists no source'):
            self.check({'manifest.json': 1, 'a-source-blink.tar.zst': 10}, {**manifest, 'version': 'other'})
        with self.assertRaisesRegex(release.Refused, 'no release'):
            self.check({}, manifest, status=404)


class R2Layer(unittest.TestCase):
    """Bucket's reads and compare-and-swap writes of latest, against the R2 fake (test_storage.py)."""

    def setUp(self):
        from test_storage import FakeR2
        self.server = FakeR2()
        self.addCleanup(self.server.stop)
        self.bucket = release.Bucket(self.server.client())

    def test_latest_is_read_with_its_etag_and_swapped_only_at_it(self):
        self.assertEqual(self.bucket.read_latest('v1'), (None, None))
        self.assertTrue(self.bucket.write_latest('v1', '20261107.1', None))
        # Making it again fails: it exists.
        self.assertFalse(self.bucket.write_latest('v1', '20261107.9', None))
        text, etag = self.bucket.read_latest('v1')
        self.assertEqual(text, '20261107.1\n')
        self.assertTrue(self.bucket.write_latest('v1', '20261107.2', etag))
        # The ETag read before that write no longer holds.
        self.assertFalse(self.bucket.write_latest('v1', '20261107.3', etag))
        self.assertEqual(self.bucket.read_latest('v1')[0], '20261107.2\n')
        self.assertEqual(self.server.objects['lanescape-browser/v1/latest']['headers']['cache-control'], 'no-store')

    def test_reads_and_digests(self):
        self.assertIsNone(self.bucket.read('v1/none'))
        self.assertIsNone(self.bucket.digest('v1/none'))
        self.server.objects['lanescape-browser/v1/x'] = {'body': b'package', 'etag': '"1"', 'headers': {}}
        self.assertEqual(self.bucket.read('v1/x'), 'package')
        self.assertEqual(self.bucket.digest('v1/x'), (7, hashlib.sha256(b'package').hexdigest()))

    def test_a_refusal_is_not_a_missing_object(self):
        self.server.refuse = 403
        with self.assertRaisesRegex(storage.R2Error, '403'):
            self.bucket.read('v1/x')
        with self.assertRaisesRegex(storage.R2Error, '403'):
            self.bucket.write_latest('v1', '20261107.1', None)

    def test_latest_is_never_written_unconditionally(self):
        for etag in ('', 'unquoted', 'W/"1"', '"1", "2"'):
            with self.subTest(etag=etag), self.assertRaises(ValueError):
                self.bucket.write_latest('v1', '20261107.1', etag)
        self.assertNotIn('lanescape-browser/v1/latest', self.server.objects)
        # latest without an ETag can't be swapped for.
        self.server.objects['lanescape-browser/v1/latest'] = {'body': b'20261107.1\n', 'etag': '', 'headers': {}}
        with self.assertRaisesRegex(RuntimeError, 'without an ETag'):
            self.bucket.read_latest('v1')


class RealGit(unittest.TestCase):
    """The comparison promotion makes, against real git rather than the fake."""

    def git(self, cwd, *args):
        return subprocess.run(['git', '-C', cwd, '-c', 'user.name=t', '-c', 'user.email=t@t', *args],
                              check=True, capture_output=True, text=True).stdout.strip()

    def test_a_commit_pushed_after_the_checkout_is_fetched_before_comparing(self):
        with tempfile.TemporaryDirectory() as tmp:
            origin, clone = f'{tmp}/origin', f'{tmp}/clone'
            self.git(tmp, 'init', '-q', '-b', 'main', origin)
            self.git(origin, 'commit', '-q', '--allow-empty', '-m', 'older')
            older = self.git(origin, 'rev-parse', 'HEAD')
            self.git(tmp, 'clone', '-q', origin, clone)
            # A newer build lands and is promoted while the older one is still publishing.
            self.git(origin, 'commit', '-q', '--allow-empty', '-m', 'newer')
            newer = self.git(origin, 'rev-parse', 'HEAD')
            previous = Path.cwd()
            os.chdir(clone)
            try:
                self.assertTrue(release.Repo().is_ancestor(older, newer))
                self.assertFalse(release.Repo().is_ancestor(newer, older))
            finally:
                os.chdir(previous)


if __name__ == '__main__':
    unittest.main()
