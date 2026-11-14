"""Bundle the license files for the crates linked into this native executable."""
import json
import shutil
import subprocess
from pathlib import Path


def runtime_packages(metadata):
    nodes = {node['id']: node for node in metadata['resolve']['nodes']}
    selected = set()
    pending = [metadata['resolve']['root']]
    while pending:
        package = pending.pop()
        if package in selected:
            continue
        selected.add(package)
        pending.extend(dep['pkg'] for dep in nodes[package]['deps']
                       if any(kind['kind'] is None for kind in dep['dep_kinds']))
    return [package for package in metadata['packages'] if package['id'] in selected]


def bundle_notices(destination, platform=None):
    """Copies the licenses of the crates linked for a platform, by default the one building."""
    if platform is None:
        platform = next(line.split(': ', 1)[1] for line in subprocess.check_output(
            ['rustc', '-vV'], text=True).splitlines() if line.startswith('host: '))
    metadata = json.loads(subprocess.check_output([
        'cargo', 'metadata', '--locked', '--filter-platform', platform, '--format-version=1'], text=True))
    packages = []
    for package in runtime_packages(metadata):
        if package['source'] is None:
            continue
        root = Path(package['manifest_path']).parent
        files = [path for path in root.iterdir() if path.is_file() and
                 path.name.upper().startswith(('LICENSE', 'NOTICE', 'COPYING', 'UNLICENSE'))]
        if not files:
            raise RuntimeError(f'missing dependency license: {package["name"]}')
        target = destination / f'{package["name"]}-{package["version"]}'
        target.mkdir(parents=True, exist_ok=True)
        for path in files:
            shutil.copy2(path, target/path.name)
        packages.append({key: package[key] for key in ('name', 'version', 'license', 'repository')})
    (destination/'packages.json').write_text(json.dumps(packages, sort_keys=True, indent=2)+'\n')
