"""Where a build's out/ cache lives. Anything that can change what's in out/ is part of the key."""
import hashlib
import json
from pathlib import Path

BUCKET = 'gs://lanescape-browser-ci'
# Bump to retire every cache when the way they're packed changes.
FORMAT = 1


def cache_key(root, target, config):
    root = Path(root)
    targets = json.loads((root/'engine/targets.json').read_text())
    pin = json.loads((root/'engine/chromium-pin.json').read_text())
    digest = hashlib.sha256()
    for part in (str(FORMAT), pin['commit'], json.dumps(targets[target], sort_keys=True),
                 (root/f'engine/args/{config}.gn').read_text(),
                 (root/'rust-toolchain.toml').read_text()):
        digest.update(part.encode()+b'\0')
    return f'{target}-{config}-{digest.hexdigest()[:32]}'


def own_cache(key, event, ref_name, pr_number=None, merge_base=None):
    """The one path this build may write. Only main and support branches write outside cache/pr/."""
    if event == 'pull_request':
        if not pr_number or not merge_base:
            raise ValueError('a PR cache needs the PR number and merge base')
        return f'{BUCKET}/cache/pr/{pr_number}/{merge_base}-{key}/out.tar.zst'
    if event == 'push' and ref_name == 'main':
        return f'{BUCKET}/cache/main/{key}/out.tar.zst'
    if event == 'push' and ref_name.startswith('support/v'):
        return f'{BUCKET}/cache/{ref_name}/{key}/out.tar.zst'
    return None


def earlier_pr_caches(key, pr_number):
    """Where this PR's caches for the key from earlier merge bases are: a PR that catches up with main gets a new
    merge base, and its cache from before that is still a good start, since a build redoes only what changed."""
    return f'{BUCKET}/cache/pr/{pr_number}/*-{key}/out.tar.zst'


def newest(listing, exclude=()):
    """The newest object in `gcloud storage ls -l` output, leaving out any in exclude, or None."""
    entries = []
    for line in listing.splitlines():
        parts = line.split()
        if len(parts) == 3 and parts[2].startswith('gs://') and parts[0].isdigit() and parts[2] not in exclude:
            entries.append((parts[1], parts[2]))
    return max(entries)[1] if entries else None


def restore_order(key, event, ref_name, pr_number=None, merge_base=None):
    """The caches to try, in order: this build's own, then main's. main never reads a PR's."""
    main = f'{BUCKET}/cache/main/{key}/out.tar.zst'
    own = own_cache(key, event, ref_name, pr_number, merge_base)
    return [own, main] if own and own != main else [main]


def source_pack(commit, host, gclient):
    """A pack's path names what it holds: its .gclient, whose systems and processors are those of every target
    sharing the pack, so adding a target to a pack never restores one synced without it."""
    return f'{BUCKET}/sources/{commit}/{host}-{hashlib.sha256(gclient.encode()).hexdigest()[:12]}'


def hand_off(event, target, config, *, pr_number=None, sha=None, major=None, build=None):
    """Where a cross-compiled target's package and tests wait for its native test machine."""
    if event == 'pull_request':
        return f'{BUCKET}/pr/{pr_number}/{sha}/{target}-{config}'
    return f'{BUCKET}/binaries/{major}/{build}/{target}-{config}'
