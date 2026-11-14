#!/usr/bin/env python3
"""Publish immutable packages and sources; publish the complete manifest last."""
import json
import os
from pathlib import Path
from publish_sources import publish_sources
from storage import put_immutable
from version import release_major

# Every published build has all of these, and promotion refuses one that doesn't.
TARGETS = ('linux-amd64', 'linux-arm64', 'macos-arm64')


def publish():
    root = Path('dist')
    build, bucket, major = os.environ['BUILD_NUMBER'], os.environ['GCS_BUCKET'], release_major()
    paths = {target: root / f'lanescape-browser-{target}.tar.gz' for target in TARGETS}
    # Before anything is uploaded, so a build missing a platform leaves nothing behind.
    missing = sorted(target for target, path in paths.items() if not path.is_file())
    if missing:
        raise SystemExit(f'not publishing {build}: no package for {", ".join(missing)}')
    sources = publish_sources(bucket, major)
    packages = {}
    for target, path in paths.items():
        packages[target] = {'name': path.name, **put_immutable(path, f'gs://{bucket}/{major}/{build}/{path.name}')}
    manifest = {'major': major, 'build': build, 'commit': os.environ['GITHUB_SHA'],
                'packages': packages, 'sources': sources}
    path = root / 'manifest.json'
    path.write_text(json.dumps(manifest, sort_keys=True)+'\n')
    put_immutable(path, f'gs://{bucket}/{major}/{build}/manifest.json')


if __name__ == '__main__':
    publish()
