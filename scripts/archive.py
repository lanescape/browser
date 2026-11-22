"""Package archives: tarfile keeps member names and modes; pigz compresses in parallel."""
import subprocess
import tarfile
from pathlib import Path


def pack(source, archive, arcname):
    """Writes source as a gzip tarball whose single top-level directory is arcname."""
    with Path(archive).open('wb') as out:
        with subprocess.Popen(['pigz', '-6', '-c'], stdin=subprocess.PIPE, stdout=out) as pigz:
            with tarfile.open(fileobj=pigz.stdin, mode='w|') as tar:
                tar.add(source, arcname=arcname)
            pigz.stdin.close()
    if pigz.returncode:
        raise RuntimeError(f'pigz failed with {pigz.returncode}')


def unpack(archive, destination):
    """Extracts a gzip tarball with the data filter, decompressing outside Python."""
    with subprocess.Popen(['pigz', '-dc', str(archive)], stdout=subprocess.PIPE) as pigz:
        with tarfile.open(fileobj=pigz.stdout, mode='r|') as tar:
            tar.extractall(destination, filter='data')
    if pigz.returncode:
        raise RuntimeError(f'pigz failed with {pigz.returncode}')
