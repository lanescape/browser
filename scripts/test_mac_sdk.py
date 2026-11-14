import plistlib
import shutil
import tempfile
import unittest
from pathlib import Path

import mac_sdk


def fake_xcode(root):
    """Xcode's Contents, as far as the SDK goes: its version, an SDK with a header, a link and an executable."""
    contents = Path(root)/'Contents'
    sdks = contents/mac_sdk.SDKS
    core = sdks/'MacOSX.sdk/System/Library/CoreServices'
    core.mkdir(parents=True)
    (contents/'version.plist').write_bytes(plistlib.dumps({'CFBundleShortVersionString': '26.6', 'ProductBuildVersion': '17F113'}))
    (core/'SystemVersion.plist').write_bytes(plistlib.dumps({'ProductVersion': '26.5', 'ProductBuildVersion': '25F70'}))
    (sdks/'MacOSX.sdk/usr/include').mkdir(parents=True)
    (sdks/'MacOSX.sdk/usr/include/stdio.h').write_text('int printf(const char *, ...);\n')
    (sdks/'MacOSX.sdk/usr/bin').mkdir()
    (sdks/'MacOSX.sdk/usr/bin/tool').write_text('#!/bin/sh\n')
    (sdks/'MacOSX.sdk/usr/bin/tool').chmod(0o700)
    (sdks/'MacOSX26.5.sdk').symlink_to('MacOSX.sdk')
    return contents


class Sdk(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)
        self.contents = fake_xcode(self.tmp/'xcode')

    def test_the_same_sdk_packs_to_the_same_bytes(self):
        first = mac_sdk.pack(self.contents, self.tmp/'a.tar.zst')
        (self.contents/mac_sdk.SDKS/'MacOSX.sdk/usr/include/stdio.h').touch()
        second = mac_sdk.pack(self.contents, self.tmp/'b.tar.zst')
        self.assertEqual(first, second)
        self.assertEqual((first['xcode'], first['sdk']), ('26.6 (17F113)', '26.5 (25F70)'))
        self.assertTrue(first['archive'].startswith(f'sources/mac-sdk/{first["sha256"][:16]}/'))

    def test_installs_where_chromium_looks_with_one_time_and_only_once(self):
        entry = mac_sdk.pack(self.contents, self.tmp/'sdk.tar.zst')
        fetched = []

        def fetch(path, dest):
            fetched.append(path)
            shutil.copy(self.tmp/'sdk.tar.zst', dest)
        src = self.tmp/'src'
        self.assertTrue(mac_sdk.install(entry, src, fetch))
        self.assertFalse(mac_sdk.install(entry, src, fetch))
        self.assertEqual(fetched, [entry['archive']])
        # Where build/config/mac/mac_sdk.gni looks.
        sdks = src/'build/mac_files/xcode_binaries/Contents'/mac_sdk.SDKS
        self.assertTrue((src/'build/mac_files/xcode_binaries/Contents/version.plist').is_file())
        header = sdks/'MacOSX.sdk/usr/include/stdio.h'
        self.assertEqual(header.read_text(), 'int printf(const char *, ...);\n')
        self.assertEqual(header.stat().st_mtime, mac_sdk.FIXED_TIME)
        self.assertEqual((sdks/'MacOSX26.5.sdk').readlink(), Path('MacOSX.sdk'))
        self.assertTrue((sdks/'MacOSX.sdk/usr/bin/tool').stat().st_mode & 0o100)

    def test_refuses_an_archive_that_is_not_the_pinned_one(self):
        entry = {**mac_sdk.pack(self.contents, self.tmp/'sdk.tar.zst'), 'sha256': '0'*64}
        with self.assertRaises(RuntimeError):
            mac_sdk.install(entry, self.tmp/'src', lambda path, dest: shutil.copy(self.tmp/'sdk.tar.zst', dest))
        self.assertFalse((self.tmp/'src'/mac_sdk.HERMETIC/'.lanescape-browser-sdk').exists())


if __name__ == '__main__':
    unittest.main()
