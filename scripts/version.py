"""Read the major used for build publication, and whether it is unstable, from Cargo.toml."""
import re
import tomllib
from pathlib import Path


def _manifest(cargo_toml):
    if cargo_toml is None:
        cargo_toml = Path('Cargo.toml').read_text()
    return tomllib.loads(cargo_toml)


def release_major(cargo_toml=None):
    version = _manifest(cargo_toml)['package']['version']
    major = version.split('.')[0]
    if not re.fullmatch(r'[1-9][0-9]*', major):
        raise ValueError('package version must have a positive major')
    return f'v{major}'


def unstable(cargo_toml=None):
    """True while `[package.metadata.lanescape-browser] unstable` marks the package major unstable."""
    value = _manifest(cargo_toml)['package'].get('metadata', {}).get('lanescape-browser', {}).get('unstable', False)
    if not isinstance(value, bool):
        raise ValueError('package.metadata.lanescape-browser.unstable must be true or false')
    return value
