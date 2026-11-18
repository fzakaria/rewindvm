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

# The directory under the install directory that holds one directory per
# release, and the link beside it that names the release in use.
RELEASES = "releases"
CURRENT = "current"

EXECUTABLE = 0o755
DIRECTORY = 0o755
READ_ONLY = 0o555


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
        # The temporary directory goes after every other cleanup, so one
        # that gives a directory its permissions back finds it there.
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
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
        # What the installer left that is not the release in use: a
        # release that was never switched to, or one that was replaced.
        in_use = (self.install_dir / CURRENT).resolve()
        releases = (self.install_dir / RELEASES).iterdir()
        return [release for release in releases if release.resolve() != in_use]

    def make_read_only(self, directory):
        # `directory` takes no new entries until the test is over.
        directory.chmod(READ_ONLY)
        self.addCleanup(directory.chmod, DIRECTORY)

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
        self.assertEqual(self.staging_left(), [])

    def test_an_upgrade_that_cannot_switch_changes_nothing(self):
        # Release one installed, then an upgrade to release two with the
        # install directory read-only, so both tarballs download and
        # unpack but the link that names the release in use cannot be
        # replaced. The installer fails, the command and the app are both
        # still release one, and release two's directory is gone.
        write_cli(self.releases, "one")
        write_app(self.releases, "one")
        self.assertEqual(self.install(), 0)
        write_cli(self.releases, "two")
        write_app(self.releases, "two")
        self.make_read_only(self.install_dir)
        self.assertNotEqual(self.install(), 0)
        self.assertEqual(self.installed(), ("one", "one"))
        self.assertEqual(self.staging_left(), [])

    def test_an_upgrade_needs_no_new_launcher_links(self):
        # Release one installed, then an upgrade to release two with the
        # directory of the launcher links read-only. The links name the
        # release in use and not a release, so the upgrade writes none:
        # the installer succeeds and both launchers say two.
        write_cli(self.releases, "one")
        write_app(self.releases, "one")
        self.assertEqual(self.install(), 0)
        write_cli(self.releases, "two")
        write_app(self.releases, "two")
        self.make_read_only(self.bin_dir)
        self.assertEqual(self.install(), 0)
        self.assertEqual(self.installed(), ("two", "two"))

    def test_an_install_from_before_releases_is_replaced(self):
        # An install made before the installer kept releases: the command
        # and the app each in a directory of the install directory, with
        # the launcher links naming those. Installing release one over it
        # leaves both launchers saying one and neither old directory.
        old = tempfile.TemporaryDirectory()
        self.addCleanup(old.cleanup)
        write_cli(Path(old.name), "old")
        write_app(Path(old.name), "old")
        self.install_dir.mkdir(parents=True)
        self.bin_dir.mkdir()
        for name, program in ((CLI, "rewind"), (APP, "rewind-app")):
            with tarfile.open(Path(old.name) / f"{name}.tar.gz") as tar:
                tar.extractall(self.install_dir, filter="data")
            target = self.install_dir / name / "bin" / program
            (self.bin_dir / program).symlink_to(target)
        self.assertEqual(self.installed(), ("old", "old"))

        write_cli(self.releases, "one")
        write_app(self.releases, "one")
        self.assertEqual(self.install(), 0)
        self.assertEqual(self.installed(), ("one", "one"))
        self.assertFalse((self.install_dir / CLI).exists())
        self.assertFalse((self.install_dir / APP).exists())

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
