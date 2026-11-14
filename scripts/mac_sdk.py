#!/usr/bin/env python3
"""The macOS SDK that Linux machines build macOS with, pinned in engine/targets.json as the archive's path in the
bucket and its SHA-256. A maintainer packs it once from an Xcode, on a Mac or from a copy of Xcode's Contents, and
uploads it; every build then installs exactly that. Each file gets one fixed time, so a restored out/ never looks
out of date against the SDK's headers, whichever machine unpacked it.

    mac_sdk.py <Xcode.app/Contents> <out.tar.zst>    packs it, and prints its targets.json entry
"""
import argparse
import hashlib
import json
import os
import plistlib
import shutil
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

# 2020-01-01T00:00:00Z, as CI gave a Mac's SDK before Linux built macOS.
FIXED_TIME = 1577836800
SDKS = 'Developer/Platforms/MacOSX.platform/Developer/SDKs'
# Where Chromium looks for a hermetic Xcode, from build/config/mac/mac_sdk.gni.
HERMETIC = 'build/mac_files/xcode_binaries'


def sha256(path):
    digest = hashlib.sha256()
    with open(path, 'rb') as stream:
        while block := stream.read(1 << 20):
            digest.update(block)
    return digest.hexdigest()


def versions(contents):
    contents = Path(contents)
    xcode = plistlib.loads((contents/'version.plist').read_bytes())
    sdk = plistlib.loads((contents/SDKS/'MacOSX.sdk/System/Library/CoreServices/SystemVersion.plist').read_bytes())
    return (f'{xcode["CFBundleShortVersionString"]} ({xcode["ProductBuildVersion"]})',
            f'{sdk["ProductVersion"]} ({sdk["ProductBuildVersion"]})')


def members(contents):
    """version.plist and the SDKs directory, in a fixed order, symbolic links kept as links."""
    contents = Path(contents)
    yield 'version.plist'
    for directory, names, files in os.walk(contents/SDKS):
        names.sort()
        relative = Path(directory).relative_to(contents)
        yield str(relative)
        for name in sorted(files + [n for n in names if (Path(directory)/n).is_symlink()]):
            yield str(relative/name)
        names[:] = [n for n in names if not (Path(directory)/n).is_symlink()]


def pack(contents, output):
    """Writes the same bytes for the same SDK: sorted, one owner, one time, no extended attributes."""
    contents = Path(contents)
    zstd = subprocess.Popen(['zstd', '-19', '-T0', '-q', '-f', '-o', str(output)], stdin=subprocess.PIPE)
    with tarfile.open(fileobj=zstd.stdin, mode='w|', format=tarfile.PAX_FORMAT) as tar:
        for name in members(contents):
            info = tar.gettarinfo(contents/name, arcname=name)
            info.mtime, info.uid, info.gid, info.uname, info.gname = FIXED_TIME, 0, 0, '', ''
            info.pax_headers = {}
            info.mode = 0o755 if info.isdir() or info.mode & 0o111 else 0o644
            if info.isfile():
                with open(contents/name, 'rb') as stream:
                    tar.addfile(info, stream)
            else:
                tar.addfile(info)
    zstd.stdin.close()
    if zstd.wait():
        raise RuntimeError('zstd failed')
    xcode, sdk = versions(contents)
    return {'archive': f'sources/mac-sdk/{sha256(output)[:16]}/MacOSX{sdk.split()[0]}.sdk.tar.zst',
            'sha256': sha256(output), 'xcode': xcode, 'sdk': sdk}


def install(entry, src, fetch):
    """Puts the pinned SDK where Chromium's build looks for it, unless that SDK is already there. fetch(path, dest)
    copies the archive from the bucket."""
    target = Path(src)/HERMETIC
    stamp = target/'.lanescape-browser-sdk'
    if stamp.is_file() and stamp.read_text() == entry['sha256']:
        return False
    with tempfile.TemporaryDirectory() as tmp:
        archive = Path(tmp)/'sdk.tar.zst'
        fetch(entry['archive'], archive)
        if sha256(archive) != entry['sha256']:
            raise RuntimeError(f'{entry["archive"]} does not match its pin')
        if target.is_symlink() or target.is_file():
            target.unlink()
        shutil.rmtree(target, ignore_errors=True)
        target.mkdir(parents=True)
        zstd = subprocess.Popen(['zstd', '-dc', '-q', str(archive)], stdout=subprocess.PIPE)
        with tarfile.open(fileobj=zstd.stdout, mode='r|') as stream:
            # Every link stays inside the SDK, so the strictest filter fits. The archive is of Xcode.app's Contents.
            stream.extractall(target/'Contents', filter='data')
        if zstd.wait():
            raise RuntimeError('zstd failed')
    stamp.write_text(entry['sha256'])
    return True


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('contents')
    parser.add_argument('output')
    args = parser.parse_args()
    print(json.dumps(pack(args.contents, args.output), indent=2))


if __name__ == '__main__':
    sys.exit(main())
