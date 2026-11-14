#!/usr/bin/env python3
"""Keep exact engine source snapshots public alongside the binaries."""
import hashlib
import json
import os
import shutil
import tempfile
import urllib.request
from pathlib import Path
from storage import describe, identity, put_immutable
from version import release_major


def publish_sources(bucket, major):
    pins = json.loads(Path('third_party/sources.json').read_text())
    public = f'https://storage.googleapis.com/{bucket}/{major}/sources'
    with tempfile.TemporaryDirectory() as tmp:
        for source in pins['archives']:
            relative = f'{source["sha256"]}/{source["name"]}'
            destination = f'gs://{bucket}/{major}/sources/{relative}'
            existing = describe(destination)
            if existing is not None:
                if (int(existing['size']) != source['bytes'] or
                        existing.get('custom_fields', {}).get('sha256') != source['sha256']):
                    raise RuntimeError(f'source object conflict: {destination}')
            else:
                path = Path(tmp) / source['name']
                with urllib.request.urlopen(source['url'], timeout=120) as stream, path.open('wb') as out:
                    shutil.copyfileobj(stream, out)
                if identity(path) != {'sha256': source['sha256'], 'bytes': source['bytes']}:
                    raise RuntimeError(f'source pin mismatch: {source["name"]}')
                put_immutable(path, destination)
                path.unlink()
            source['mirror'] = f'{public}/{relative}'
        manifest = json.dumps(pins, sort_keys=True, indent=2)+'\n'
        instructions = Path('third_party/README.md')
        offer = hashlib.sha256(manifest.encode()+instructions.read_bytes()).hexdigest()
        prefix = f'gs://{bucket}/{major}/sources/{offer}'
        path = Path(tmp)/'sources.json'
        path.write_text(manifest)
        put_immutable(instructions, f'{prefix}/README.md')
        put_immutable(path, f'{prefix}/sources.json')
    return f'{public}/{offer}/sources.json'


if __name__ == '__main__':
    print(publish_sources(os.environ['GCS_BUCKET'], release_major()))
