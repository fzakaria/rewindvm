#!/usr/bin/env python3
"""Tests for the installer the site serves, site/install.

python3 -m unittest discover -s tools -p 'test_install.py'

Each test runs the installer against a directory of release tarballs made
here, named by a file URL, and installs into a temporary directory. The
tarballs hold a command and an app that print the release they are from.
"""

import io
import os
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path

INSTALLER = Path(__file__).resolve().parent.parent / "site" / "install"

CLI = "rewind-x86_64-linux"
APP = "rewind-app-x86_64-linux"
DESKTOP_ENTRY = "surf.lunchtime.Rewind.desktop"

# The staging directories the installer unpacks into, which none should
# outlive the installer.
STAGING_GLOB = ".install.*"

EXECUTABLE = 0o755
DIRECTORY = 0o755


def add_file(tar, name, text, mode=0o644):
    # A regular file `name` holding `text`.
    data = text.encode()
    info = tarfile.TarInfo(name)
    info.size = len(data)
    info.mode = mode
    tar.addfile(info, io.BytesIO(data))


def add_dir(tar, name):
    info = tarfile.TarInfo(name)
    info.type = tarfile.DIRTYPE
    info.mode = DIRECTORY
    tar.addfile(info)


def launcher(release):
    # A program that prints the release its tarball is from.
    return f"#!/bin/sh\necho {release}\n"


def write_cli(releases, release):
    with tarfile.open(releases / f"{CLI}.tar.gz", "w:gz") as tar:
        add_file(tar, f"{CLI}/bin/rewind", launcher(release), EXECUTABLE)


def write_app(releases, release):
    with tarfile.open(releases / f"{APP}.tar.gz", "w:gz") as tar:
        add_file(tar, f"{APP}/bin/rewind-app", launcher(release), EXECUTABLE)
        entry = f"{APP}/share/applications/{DESKTOP_ENTRY}"
        add_file(tar, entry, "[Desktop Entry]\nExec=rewind-app\n")
        add_dir(tar, f"{APP}/share/icons/hicolor")


class Install(unittest.TestCase):
    # The installer run against releases in a temporary directory. What
    # is installed is read back by running the two linked launchers.
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        self.releases = root / "releases"
        self.install_dir = root / "share" / "rewind"
        self.bin_dir = root / "bin"
        self.releases.mkdir()
        self.env = {
            "PATH": os.environ["PATH"],
            "HOME": str(root),
            "XDG_DATA_HOME": str(root / "share"),
            "REWIND_RELEASE_URL": self.releases.as_uri(),
            "REWIND_INSTALL_DIR": str(self.install_dir),
            "REWIND_BIN_DIR": str(self.bin_dir),
        }

    def tearDown(self):
        self.tmp.cleanup()

    def install(self):
        # The installer's exit status.
        done = subprocess.run(
            ["sh", str(INSTALLER)], env=self.env, capture_output=True, text=True
        )
        return done.returncode

    def installed(self):
        # The releases the linked command and app say they are from.
        def release(name):
            ran = subprocess.run(
                [str(self.bin_dir / name)], capture_output=True, text=True
            )
            return ran.stdout.strip()

        return release("rewind"), release("rewind-app")

    def staging_left(self):
        return list(self.install_dir.glob(STAGING_GLOB))

    def test_an_install_links_the_command_and_the_app(self):
        # A first install of release one: both launchers run and say one,
        # and no staging directory is left.
        write_cli(self.releases, "one")
        write_app(self.releases, "one")
        self.assertEqual(self.install(), 0)
        self.assertEqual(self.installed(), ("one", "one"))
        self.assertEqual(self.staging_left(), [])

    def test_an_upgrade_replaces_both(self):
        # Release one installed, then release two: both launchers say two.
        write_cli(self.releases, "one")
        write_app(self.releases, "one")
        self.assertEqual(self.install(), 0)
        write_cli(self.releases, "two")
        write_app(self.releases, "two")
        self.assertEqual(self.install(), 0)
        self.assertEqual(self.installed(), ("two", "two"))

    def test_an_upgrade_whose_second_download_fails_changes_nothing(self):
        # Release one installed, then an upgrade to release two whose app
        # tarball is missing, as when the connection drops between the
        # two downloads. The installer fails, the command and the app are
        # both still release one, and no staging directory is left.
        write_cli(self.releases, "one")
        write_app(self.releases, "one")
        self.assertEqual(self.install(), 0)
        write_cli(self.releases, "two")
        (self.releases / f"{APP}.tar.gz").unlink()
        self.assertNotEqual(self.install(), 0)
        self.assertEqual(self.installed(), ("one", "one"))
        self.assertEqual(self.staging_left(), [])

    def test_an_upgrade_whose_second_download_is_cut_short_changes_nothing(self):
        # As above, but the app tarball arrives as its first half, as a
        # download cut off part way does.
        write_cli(self.releases, "one")
        write_app(self.releases, "one")
        self.assertEqual(self.install(), 0)
        write_cli(self.releases, "two")
        write_app(self.releases, "two")
        app = self.releases / f"{APP}.tar.gz"
        whole = app.read_bytes()
        app.write_bytes(whole[: len(whole) // 2])
        self.assertNotEqual(self.install(), 0)
        self.assertEqual(self.installed(), ("one", "one"))
        self.assertEqual(self.staging_left(), [])


if __name__ == "__main__":
    unittest.main()
