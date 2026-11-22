#!/usr/bin/env python3
"""Allocate immutable build tags on main/support branches and promote complete builds forward only."""
import argparse, datetime, json, os, re, subprocess, sys
import github
import storage
from version import release_major, unstable

BUILD = r'[0-9]{8}\.[1-9][0-9]*'
# A build tag carries its major in front of the build number: v1.20261107.1.
TAG = re.compile(rf'(v[1-9][0-9]*)\.({BUILD})')
TARGETS = ('linux-amd64', 'linux-arm64', 'macos-arm64')
ATTEMPTS = 5
LIBCHROMIUM_RELEASES = 'https://github.com/lanescape/libchromium/releases/tag'


class Refused(ValueError):
    """A promotion that would not move a major strictly forward to a complete build."""


class Superseded(Refused):
    """latest already names this build's commit or a later one: expected when an older build finishes last."""


def run(*args, **kw):
    return subprocess.check_output(args, text=True, **kw).strip()


def parse_tag(name):
    """(major, build) for a build tag, else None."""
    match = TAG.fullmatch(name)
    return (match[1], match[2]) if match else None


def build_key(build):
    day, number = build.split('.')
    return int(day), int(number)


def holders(tags, build):
    """The (commit, tag major) pairs with a build tag for build."""
    return sorted({(sha, parsed[0]) for name, sha in tags if (parsed := parse_tag(name)) and parsed[1] == build})


def own_build(tags, sha):
    """sha's highest build number, if no other commit has a tag for that number."""
    mine = [parsed[1] for name, holder in tags if holder == sha and (parsed := parse_tag(name))]
    if not mine:
        return None
    build = max(mine, key=build_key)
    return build if all(holder == sha for holder, _ in holders(tags, build)) else None


def matching_refs(namespace):
    return github.paginate(f'repos/{github.repository()}/git/matching-refs/{namespace}/')


def remote_tags():
    return [(ref['ref'].removeprefix('refs/tags/'), ref['object']['sha']) for ref in matching_refs('tags')]


def create_tag(name, sha):
    """Create a tag and return the commit it points at, which differs from sha if it already existed."""
    repo = github.repository()
    try:
        github.request('POST', f'repos/{repo}/git/refs', {'ref': f'refs/tags/{name}', 'sha': sha})
        return sha
    except github.GitHubError as error:
        # GitHub refuses a ref that exists with 422.
        if error.status != 422:
            raise
    try:
        _, ref, _ = github.request('GET', f'repos/{repo}/git/ref/tags/{name}')
    except github.GitHubError:
        raise RuntimeError('tag reservation failed without a collision') from None
    return ref['object']['sha']


def today():
    return datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%d')


def reserve(sha):
    """Tag sha v<major>.YYYYMMDD.N and return YYYYMMDD.N, which is unique across all majors."""
    branch = re.fullmatch(r'refs/heads/(main|support/v[1-9][0-9]*)', os.environ.get('GITHUB_REF', ''))
    if not branch:
        raise ValueError('build numbers only exist on main and support/vN')
    major = release_major()
    if branch[1] == 'main':
        # A major moves to a support branch only after main advances its package major.
        if any(ref['ref'] == f'refs/heads/support/{major}' for ref in matching_refs('heads/support')):
            raise ValueError(f'main must advance its package major beyond support/{major}')
    elif branch[1] != f'support/{major}':
        raise ValueError('support branch does not match package major')
    day = today()
    number = 0
    for _ in range(100):
        # Another major can take the same number at once under its own tag name, and protected
        # tags are never deleted. A number is kept only if, listed after this commit's tag for it
        # exists, no other commit has a tag for it. Two commits cannot both keep one: whichever
        # lists later sees the other's tag. A contested number is given up for the next, even by a
        # rerun of a reservation that kept it before.
        tags = remote_tags()
        build = own_build(tags, sha)
        if build:
            return build
        numbers = [build_key(p[1])[1] for p in (parse_tag(n) for n, _ in tags) if p and p[1].startswith(day+'.')]
        number = max(number, *numbers, 0) + 1
        create_tag(f'{major}.{day}.{number}', sha)
    raise RuntimeError('too many tag collisions')


class Repo:
    """The local clone; promotion needs full history and origin's branches."""

    def show(self, sha, path):
        return run('git', 'show', f'{sha}:{path}')

    def branch_contains(self, branch, sha):
        ref = f'refs/remotes/origin/{branch}'
        if subprocess.run(['git', 'rev-parse', '--verify', '--quiet', ref], stdout=subprocess.DEVNULL).returncode:
            return False
        return self.is_ancestor(sha, ref)

    def fetch_if_missing(self, rev):
        """latest can name a commit pushed after this checkout was made, as when a newer build is promoted while
        an older one publishes; fetch it so the comparison can still be made."""
        if not re.fullmatch(r'[0-9a-f]{40}', rev):
            return
        if subprocess.run(['git', 'cat-file', '-e', f'{rev}^{{commit}}'], stderr=subprocess.DEVNULL).returncode:
            subprocess.run(['git', 'fetch', '--quiet', 'origin', rev], check=True)

    def is_ancestor(self, ancestor, descendant):
        for rev in (ancestor, descendant):
            self.fetch_if_missing(rev)
        code = subprocess.run(['git', 'merge-base', '--is-ancestor', ancestor, descendant]).returncode
        if code not in (0, 1):
            raise RuntimeError(f'cannot compare {ancestor} with {descendant}')
        return code == 0

    def build_tags(self):
        lines = run('git', 'for-each-ref', '--format=%(refname:strip=2) %(objectname) %(*objectname)', 'refs/tags').splitlines()
        return [(fields[0], fields[-1]) for fields in (line.split() for line in lines)]


class Bucket:
    """The release bucket on R2 (storage.R2)."""

    def __init__(self, r2):
        self.r2 = r2

    def read(self, key):
        found = self.r2.get(key)
        return None if found is None else found[0].decode()

    def digest(self, key):
        return self.r2.digest(key)

    def read_latest(self, major):
        """The build in <major>/latest and the ETag it was read at, from one request; (None, None) if absent."""
        found = self.r2.get(f'{major}/latest')
        return (None, None) if found is None else (found[0].decode(), found[1])

    def write_latest(self, major, build, etag):
        """Replace <major>/latest only if it is still at etag, or make it only if there's none when etag is None;
        False on a failed precondition."""
        try:
            self.r2.put(f'{major}/latest', (build+'\n').encode(), 'text/plain; charset=utf-8', storage.NO_STORE,
                        if_none_match=etag is None, if_match=etag)
            return True
        except storage.PreconditionFailed:
            return False


def candidates(build, repo):
    """The (commit, package major) pairs tagged with a build number."""
    if not re.fullmatch(BUILD, build):
        raise ValueError('invalid build number')
    found = []
    for sha, tag_major in holders(repo.build_tags(), build):
        major = release_major(repo.show(sha, 'Cargo.toml'))
        if tag_major != major:
            raise Refused(f'build tag {tag_major}.{build} does not match package major {major}')
        if (sha, major) not in found:
            found.append((sha, major))
    return found


def promotion_group(build, repo):
    """The major(s) a build number is tagged under, naming the Promote build workflow's lock.

    Normally one major. A contested number can name two; the lock only saves retries, so the
    workflow goes on and promotion settles the commit from the bucket."""
    majors = sorted({major for _, major in candidates(build, repo)})
    if not majors:
        raise Refused(f'no build tag for {build}')
    return ','.join(majors)


def identify(build, repo, bucket=None):
    """The commit and package major of a build number.

    A number contested in a reservation race is tagged on more than one commit; only the commit
    that kept it was published under it, so with a bucket the published manifest settles it."""
    found = candidates(build, repo)
    if len(found) > 1 and bucket is not None:
        found = [(sha, major) for sha, major in found
                 if json.loads(bucket.read(f'{major}/{build}/manifest.json') or '{}').get('commit') == sha]
    if len(found) != 1:
        raise Refused(f'expected exactly one commit tagged with build number {build}, found {len(found)}')
    return found[0]


def manifest(build, major, bucket):
    text = bucket.read(f'{major}/{build}/manifest.json')
    if text is None:
        raise Refused(f'{major}/{build} has no manifest; its publication is not complete')
    return json.loads(text)


def verify(build, sha, major, bucket, repo):
    """Refuse a build that is not complete, intact and published from its major's branch."""
    if not any(repo.branch_contains(branch, sha) for branch in ('main', f'support/{major}')):
        raise Refused(f'{build} is not on main or support/{major}')
    found = manifest(build, major, bucket)
    if found.get('commit') != sha or found.get('build') != build or found.get('major') != major:
        raise Refused('manifest identity mismatch')
    if set(found.get('packages', {})) != set(TARGETS):
        raise Refused('incomplete platform set')
    for target, package in found['packages'].items():
        if package['name'] != f'lanescape-browser-{target}.tar.gz':
            raise Refused('package target mismatch')
        if bucket.digest(f'{major}/{build}/{package["name"]}') != (package['bytes'], package['sha256']):
            raise Refused('package integrity mismatch')
    # The corresponding source is published with the libchromium version the build was made with.
    pinned = json.loads(repo.show(sha, 'engine/libchromium.json'))['version']
    if found.get('libchromium') != libchromium(pinned):
        raise Refused('corresponding source mismatch: the manifest must name the pinned libchromium release')


def libchromium(version):
    """What a manifest records of the libchromium version a build was made with, whose GitHub release publishes
    Chromium's corresponding source (third_party/README.md)."""
    return {'version': version, 'release': f'{LIBCHROMIUM_RELEASES}/v{version}'}


def promote(build, bucket, repo):
    """Point <major>/latest at build if that moves it strictly forward; every promotion goes here.

    Returns 'promoted', or 'current' if latest already names build. The write is a compare-and-swap
    on the ETag read, so concurrent promotions cannot move latest backwards in any order."""
    sha, major = identify(build, repo, bucket)
    # Published objects are immutable, so this holds for every attempt below.
    verify(build, sha, major, bucket, repo)
    for _ in range(ATTEMPTS):
        text, etag = bucket.read_latest(major)
        if text is not None:
            current = text.strip()
            if current == build:
                return 'current'
            if not re.fullmatch(BUILD, current):
                raise Refused(f'{major}/latest does not hold a build number')
            previous = manifest(current, major, bucket).get('commit')
            if not previous:
                raise Refused(f'{major}/latest names {current}, whose manifest has no commit')
            if previous == sha or repo.is_ancestor(sha, previous):
                raise Superseded(f'{build} is not newer than {major}/latest {current}')
            if not repo.is_ancestor(previous, sha):
                raise Refused(f'{build} does not descend from {major}/latest {current}')
        if bucket.write_latest(major, build, etag):
            return 'promoted'
        # Another promotion replaced latest after it was read: check again against the new value.
    raise RuntimeError(f'{major}/latest kept changing; promotion of {build} gave up')


def promote_if_unstable(build, bucket, repo):
    """Promote build only if its major is flagged unstable at its own commit."""
    sha, major = identify(build, repo, bucket)
    if not unstable(repo.show(sha, 'Cargo.toml')):
        return 'stable'
    return promote(build, bucket, repo)


def main(argv=None):
    p = argparse.ArgumentParser()
    p.add_argument('action', choices=['reserve', 'promote', 'major'])
    p.add_argument('--sha'); p.add_argument('--build')
    p.add_argument('--if-unstable', action='store_true', help='do nothing unless the build\'s major is unstable')
    a = p.parse_args(argv)
    if a.action == 'reserve':
        if not a.sha: p.error('reserve requires --sha')
        print(reserve(a.sha))
    elif a.action == 'major':
        if not a.build: p.error('major requires --build')
        print(promotion_group(a.build, Repo()))
    else:
        if not a.build: p.error('promote requires --build')
        try:
            # The bucket of .github/release.json, with the keys of the publish or promote environment.
            result = (promote_if_unstable if a.if_unstable else promote)(a.build, Bucket(storage.R2.from_environment()), Repo())
        except Superseded as error:
            # A newer build of this major got there first. Automatic promotion expects that; a person asking
            # for this build should hear it was refused.
            if a.if_unstable:
                print(f'not promoting {a.build}: {error}')
                return
            sys.exit(f'promotion of {a.build} refused: {error}')
        except Refused as error:
            sys.exit(f'promotion of {a.build} refused: {error}')
        print({'promoted': f'promoted {a.build}',
               'current': f'{a.build} is already latest',
               'stable': f'not promoting {a.build}: its major is stable; promote it with the Promote build workflow'}[result])


if __name__ == '__main__':
    main()
