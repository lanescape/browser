"""Small immutable-object helpers shared by publication scripts."""
import hashlib
import json
import subprocess
from pathlib import Path


def identity(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(chunk)
    return {'bytes': Path(path).stat().st_size, 'sha256': digest.hexdigest()}


def missing(stderr):
    return any(s in stderr.lower() for s in ('not found', 'matched no objects', 'no urls matched', '404'))


def describe(destination):
    result = subprocess.run(
        ['gcloud', 'storage', 'objects', 'describe', destination, '--format=json'],
        capture_output=True, text=True)
    if result.returncode == 0:
        return json.loads(result.stdout)
    if missing(result.stderr):
        return None
    raise RuntimeError(result.stderr)


def put_immutable(path, destination):
    expected = identity(path)
    existing = describe(destination)
    if existing is None:
        result = subprocess.run([
            'gcloud', 'storage', 'cp', str(path), destination,
            '--if-generation-match=0', '--cache-control=public,max-age=31536000,immutable',
            f'--custom-metadata=sha256={expected["sha256"]}'])
        if result.returncode == 0:
            return expected
        existing = describe(destination)
    if (existing is None or int(existing['size']) != expected['bytes'] or
            existing.get('custom_fields', {}).get('sha256') != expected['sha256']):
        raise RuntimeError(f'immutable object conflict: {destination}')
    return expected
