#!/usr/bin/env python3
"""Publish a build's packages to the release bucket on R2, then its complete manifest, last (specs/0004-releases.md)."""
import json
import os
from pathlib import Path
import storage
from release import TARGETS, libchromium, libchromium_sources
from version import release_major


def publish(r2=None):
    root = Path('dist')
    build, sha, major = os.environ['BUILD_NUMBER'], os.environ['GITHUB_SHA'], release_major()
    paths = {target: root / f'lanescape-browser-{target}.tar.gz' for target in TARGETS}
    # Before anything is uploaded, so a build missing a platform leaves nothing behind.
    missing = sorted(target for target, path in paths.items() if not path.is_file())
    if missing:
        raise SystemExit(f'not publishing {build}: no package for {", ".join(missing)}')
    pin = json.loads(Path('engine/libchromium.json').read_text())
    pinned = pin['version']
    # Before anything is uploaded: a build whose engine's corresponding source isn't published doesn't ship.
    libchromium_sources(pinned, pin.get('manifest'), pin.get('assets'))
    r2 = r2 or storage.R2.from_environment()
    packages = {}
    for target, path in paths.items():
        packages[target] = {'name': path.name,
                            **storage.put_immutable(r2, path, f'{major}/{build}/{path.name}', 'application/gzip')}
    manifest = {'major': major, 'build': build, 'commit': sha, 'packages': packages, 'libchromium': libchromium(pinned)}
    path = root / 'manifest.json'
    path.write_text(json.dumps(manifest, sort_keys=True)+'\n')
    storage.put_immutable(r2, path, f'{major}/{build}/manifest.json', 'application/json')


if __name__ == '__main__':
    publish()
