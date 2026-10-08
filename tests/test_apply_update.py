"""scripts/apply-update: the root OTA helper only ever repoints <install_root>/current.

sudoers lets the service user run the helper as root with any arguments, so it
must not trust any of them as a path. These tests run a copy of the helper with
its fixed install root pointed at a temp dir (standing in for /opt/vesyl-print),
the root check bypassed and systemctl stubbed, then check what it accepts and
that every rejected call leaves the whole tree untouched.
"""

from __future__ import annotations

import os
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
HELPER = ROOT / "scripts" / "apply-update"
HELPER_PATH = "/usr/sbin:/usr/bin:/sbin:/bin"


def _has_coreutils() -> bool:
    dirs = HELPER_PATH.split(":")
    return all(any(Path(d, tool).exists() for d in dirs) for tool in ("ln", "mv"))


def _sub_once(text: str, old: str, new: str) -> str:
    count = text.count(old)
    if count != 1:
        raise AssertionError(f"expected exactly one {old!r} in apply-update, found {count}")
    return text.replace(old, new)


@unittest.skipUnless(
    sys.platform.startswith("linux") and shutil.which("bash") and _has_coreutils(),
    "needs Linux, bash and GNU coreutils",
)
class ApplyUpdateCase(unittest.TestCase):
    def setUp(self):
        td = tempfile.TemporaryDirectory()
        self.addCleanup(td.cleanup)
        self.base = Path(td.name)
        self.root = self.base / "opt" / "vesyl-print"
        (self.root / "releases").mkdir(parents=True)
        self.calls = self.base / "systemctl.calls"
        stubs = self.base / "stubbin"
        stubs.mkdir()
        systemctl = stubs / "systemctl"
        systemctl.write_text(f'#!/bin/sh\necho "$*" >> {shlex.quote(str(self.calls))}\n')
        systemctl.chmod(0o755)

        src = HELPER.read_text(encoding="utf-8")
        src = _sub_once(
            src, "\nINSTALL_ROOT=/opt/vesyl-print\n", f"\nINSTALL_ROOT={shlex.quote(str(self.root))}\n"
        )
        src = _sub_once(src, "[[ $EUID -ne 0 ]]", "false")
        src = _sub_once(
            src, f"\nPATH={HELPER_PATH}\n", f"\nPATH={shlex.quote(str(stubs))}:{HELPER_PATH}\n"
        )
        self.helper = self.base / "apply-update"
        self.helper.write_text(src, encoding="utf-8")
        self.helper.chmod(0o755)

    # -- helpers ------------------------------------------------------------

    def release(self, version: str, entry: str = "agent.py", root: Path | None = None) -> Path:
        d = (root or self.root / "releases") / version
        (d / entry).parent.mkdir(parents=True, exist_ok=True)
        (d / entry).write_text("# entrypoint\n", encoding="utf-8")
        return d

    def run_helper(self, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [str(self.helper), *args], capture_output=True, text=True, timeout=30
        )

    def activate(self, version: str) -> subprocess.CompletedProcess:
        return self.run_helper(
            "activate", str(self.root / "releases" / version), str(self.root / "current")
        )

    def current(self) -> str | None:
        link = self.root / "current"
        return os.readlink(link) if link.is_symlink() else None

    def snapshot(self) -> dict[str, tuple]:
        """Everything under the temp dir except the helper's own files."""
        out: dict[str, tuple] = {}
        for dirpath, dirnames, filenames in os.walk(self.base):
            for name in dirnames + filenames:
                p = Path(dirpath, name)
                rel = str(p.relative_to(self.base))
                if p.is_symlink():
                    out[rel] = ("link", os.readlink(p))
                elif p.is_dir():
                    out[rel] = ("dir",)
                elif p != self.helper:
                    out[rel] = ("file", p.read_bytes())
        return out

    def assert_rejected(self, *args: str, code: int | None = None) -> subprocess.CompletedProcess:
        before = self.snapshot()
        p = self.run_helper(*args)
        self.assertNotEqual(p.returncode, 0, f"accepted {args}: {p.stdout}")
        if code is not None:
            self.assertEqual(p.returncode, code, p.stderr)
        self.assertEqual(self.snapshot(), before, f"rejected call {args} changed the tree")
        return p


class TestAccepted(ApplyUpdateCase):
    def test_activate_points_current_at_relative_slot(self):
        self.release("1.2.3")
        p = self.activate("1.2.3")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(self.current(), "releases/1.2.3")
        self.assertTrue((self.root / "current" / "agent.py").is_file())
        self.assertFalse(os.path.lexists(self.root / "current.new"))

    def test_activate_repoints_existing_current(self):
        self.release("1.2.3")
        self.release("1.2.4")
        self.assertEqual(self.activate("1.2.3").returncode, 0)
        p = self.activate("1.2.4")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(self.current(), "releases/1.2.4")

    def test_activate_accepts_every_entrypoint(self):
        for i, entry in enumerate(("agent.py", "main.py", "vesyl-print", "bin/vesyl-print")):
            with self.subTest(entry=entry):
                version = f"1.0.{i}"
                self.release(version, entry)
                p = self.activate(version)
                self.assertEqual(p.returncode, 0, p.stderr)
                self.assertEqual(self.current(), f"releases/{version}")

    def test_activate_accepts_release_version_shapes(self):
        for version in ("0.4.1-rc.1", "1.0.0.2", "10.20.30-beta"):
            with self.subTest(version=version):
                self.release(version)
                p = self.activate(version)
                self.assertEqual(p.returncode, 0, p.stderr)
                self.assertEqual(self.current(), f"releases/{version}")

    def test_rollback_activates_version(self):
        self.release("1.2.3")
        self.release("1.2.4")
        self.assertEqual(self.activate("1.2.4").returncode, 0)
        p = self.run_helper("rollback", str(self.root), "1.2.3")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(self.current(), "releases/1.2.3")
        p = self.run_helper("rollback", f"{self.root}/", "1.2.4")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(self.current(), "releases/1.2.4")

    def test_stale_current_new_symlink_is_replaced_not_followed(self):
        self.release("1.2.3")
        outside = self.base / "etc"
        outside.mkdir()
        (self.root / "current.new").symlink_to(outside)
        p = self.activate("1.2.3")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(list(outside.iterdir()), [])
        self.assertEqual(self.current(), "releases/1.2.3")
        self.assertFalse(os.path.lexists(self.root / "current.new"))

    def test_restart_restarts_display_then_agent(self):
        p = self.run_helper("restart")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(
            self.calls.read_text().splitlines(),
            [
                "restart --no-block vesyl-print-display.service",
                "restart --no-block vesyl-print-agent.service",
            ],
        )


class TestRejected(ApplyUpdateCase):
    def setUp(self):
        super().setUp()
        self.release("1.2.3")
        self.release("1.2.4")
        self.assertEqual(self.activate("1.2.3").returncode, 0)
        self.attacker = self.release("6.6.6", root=self.base / "attacker")

    def test_current_symlink_must_be_install_root_current(self):
        preload = self.base / "etc" / "ld.so.preload"
        preload.parent.mkdir()
        preload.write_text("original\n")
        release = str(self.root / "releases" / "1.2.4")
        for link in (
            str(preload),
            str(self.base / "current"),
            f"{self.root}/current/",
            f"{self.root}/current.new",
            f"{self.root}/releases/../current",
            f"{self.root}//current",
            "current",
            "",
        ):
            with self.subTest(link=link):
                p = self.assert_rejected("activate", release, link)
                self.assertIn("current_symlink must be", p.stderr)
        self.assertEqual(preload.read_text(), "original\n")
        self.assertEqual(self.current(), "releases/1.2.3")

    def test_release_dir_must_be_a_slot_under_install_root(self):
        current = str(self.root / "current")
        for release in (
            str(self.attacker),
            str(self.base / "opt" / "releases" / "1.2.4"),
            f"{self.root}/releases/../../../attacker/6.6.6",
            f"{self.root}/releases/1.2.4/../../../../attacker/6.6.6",
            f"{self.root}/releases/1.2.4/",
            f"{self.root}/releases/",
            f"{self.root}/releases/.",
            f"{self.root}/releases/..",
            f"{self.root}//releases/1.2.4",
            "releases/1.2.4",
            "",
        ):
            with self.subTest(release=release):
                self.assert_rejected("activate", release, current)

    def test_version_must_look_like_a_release(self):
        for version in ("latest", "1.2", "-1.2.3", "1.2.3 ", "1.2.3\n", " 1.2.3", "v1.2.3", "1.2.3/x"):
            with self.subTest(version=version):
                p = self.assert_rejected(
                    "activate", f"{self.root}/releases/{version}", str(self.root / "current")
                )
                self.assertIn("invalid release version", p.stderr)

    def test_symlinked_slot_rejected(self):
        (self.root / "releases" / "6.6.6").symlink_to(self.attacker)
        p = self.assert_rejected("activate", f"{self.root}/releases/6.6.6", str(self.root / "current"))
        self.assertIn("missing release_dir", p.stderr)
        self.assert_rejected("rollback", str(self.root), "6.6.6")

    def test_symlinked_releases_dir_rejected(self):
        real = self.root / "releases"
        moved = self.base / "elsewhere-releases"
        real.rename(moved)
        real.symlink_to(moved)
        self.assert_rejected("activate", f"{self.root}/releases/1.2.4", str(self.root / "current"))

    def test_missing_slot_rejected(self):
        self.assert_rejected("activate", f"{self.root}/releases/9.9.9", str(self.root / "current"))

    def test_slot_without_entrypoint_rejected(self):
        (self.root / "releases" / "2.0.0").mkdir()
        (self.root / "releases" / "2.0.0" / "README").write_text("x")
        p = self.assert_rejected("activate", f"{self.root}/releases/2.0.0", str(self.root / "current"))
        self.assertIn("no agent entrypoint", p.stderr)

    def test_existing_current_new_directory_is_not_written_into(self):
        (self.root / "current.new").mkdir()
        self.assert_rejected("activate", f"{self.root}/releases/1.2.4", str(self.root / "current"))

    def test_rollback_install_root_is_fixed(self):
        other = self.base / "other"
        self.release("1.2.4", root=other / "releases")
        for root, version in (
            (str(other), "1.2.4"),
            (str(self.base / "attacker" / ".."), "1.2.4"),
            (f"{self.root}/..", "1.2.4"),
            (f"{self.root}//", "1.2.4"),
            (str(self.root), "../../../attacker/6.6.6"),
            (str(self.root), ".."),
            (str(self.root), "."),
            (str(self.root), ""),
        ):
            with self.subTest(root=root, version=version):
                self.assert_rejected("rollback", root, version)
        self.assertEqual(self.current(), "releases/1.2.3")

    def test_wrong_arity_and_unknown_commands(self):
        release = f"{self.root}/releases/1.2.4"
        current = str(self.root / "current")
        for args in (
            (),
            ("bogus",),
            ("activate",),
            ("activate", release),
            ("activate", release, current, "extra"),
            ("rollback", str(self.root)),
            ("rollback", str(self.root), "1.2.4", "extra"),
            ("restart", "now"),
        ):
            with self.subTest(args=args):
                self.assert_rejected(*args, code=2)
        self.assertFalse(self.calls.exists())


@unittest.skipIf(os.geteuid() == 0, "root passes the root check")
class TestRequiresRoot(unittest.TestCase):
    def test_unmodified_helper_refuses_non_root(self):
        p = subprocess.run(
            ["bash", str(HELPER), "rollback", "/opt/vesyl-print", "0.0.0"],
            capture_output=True,
            text=True,
            timeout=30,
        )
        self.assertEqual(p.returncode, 1)
        self.assertIn("must run as root", p.stderr)


if __name__ == "__main__":
    unittest.main()
