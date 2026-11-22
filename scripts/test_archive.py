"""Packages stay standard gzip tarballs with the expected layout and modes."""
import gzip
import shutil
import tarfile
import tempfile
import unittest
from pathlib import Path
import archive


@unittest.skipUnless(shutil.which('pigz'), 'pigz is installed by native preparation')
class ArchiveContracts(unittest.TestCase):
    def test_round_trip_keeps_layout_modes_and_gzip_format(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root/'bundle'
            (source/'lib').mkdir(parents=True)
            binary = source/'lanescape-browser'
            binary.write_bytes(b'engine')
            binary.chmod(0o755)
            (source/'lib'/'data.bin').write_bytes(bytes(range(256)) * 4096)
            package = root/'lanescape-browser-test.tar.gz'
            archive.pack(source, package, 'lanescape-browser')
            with gzip.open(package) as plain, tarfile.open(fileobj=plain) as tar:
                names = sorted(tar.getnames())
            self.assertEqual(names, ['lanescape-browser', 'lanescape-browser/lanescape-browser', 'lanescape-browser/lib',
                                     'lanescape-browser/lib/data.bin'])
            out = root/'out'
            out.mkdir()
            archive.unpack(package, out)
            self.assertEqual((out/'lanescape-browser'/'lanescape-browser').stat().st_mode & 0o777, 0o755)
            self.assertEqual((out/'lanescape-browser'/'lib'/'data.bin').read_bytes(), bytes(range(256)) * 4096)


if __name__ == '__main__':
    unittest.main()
