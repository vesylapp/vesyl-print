"""agent.py / cli.py hand off to the slot's Rust binary when it ships one.

The binary only gets the process after ``vesyl-print --version`` runs and
reports the slot's VERSION; anything else (wrong architecture, missing loader,
old glibc, stale build, execv failure) leaves the Python agent/CLI running.
"""

from __future__ import annotations

import errno
import io
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from contextlib import redirect_stderr
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

import agent as agent_mod
import cli

# A stand-in for a healthy slot binary built for VERSION 1.2.3.
GOOD_BINARY = """#!/bin/sh
if [ "$1" = "--version" ]; then echo "vesyl-print 1.2.3"; exit 0; fi
echo "RUST:$*"
exit 7
"""
# Not ELF and no shebang: execve fails with ENOEXEC (e.g. a corrupt download).
GARBAGE_BINARY = b"\x00\x01\x02 definitely not an executable\n"


def _write_exe(path: Path, content: str | bytes) -> Path:
    path.write_bytes(content.encode() if isinstance(content, str) else content)
    path.chmod(0o755)
    return path


class SlotTestCase(unittest.TestCase):
    """A temp release slot holding VERSION 1.2.3, with the Python override unset."""

    def setUp(self):
        td = tempfile.TemporaryDirectory()
        self.addCleanup(td.cleanup)
        self.slot = Path(td.name)
        (self.slot / "VERSION").write_text("1.2.3\n", encoding="utf-8")
        env = mock.patch.dict(os.environ)
        env.start()
        self.addCleanup(env.stop)
        os.environ.pop(agent_mod.ENV_FORCE_PYTHON, None)

    def binary(self, content: str | bytes = GOOD_BINARY) -> Path:
        return _write_exe(self.slot / agent_mod.RUST_BINARY, content)

    def probe(self, **kw) -> str | None:
        b = self.slot / agent_mod.RUST_BINARY
        return agent_mod.probe_rust_binary(b, agent_mod.slot_version(self.slot), **kw)


class TestRustBinary(unittest.TestCase):
    def test_missing_binary(self):
        with tempfile.TemporaryDirectory() as td:
            self.assertIsNone(agent_mod.rust_binary(Path(td)))

    def test_executable_binary_found(self):
        with tempfile.TemporaryDirectory() as td:
            b = Path(td) / "vesyl-print"
            b.write_bytes(b"\x7fELF")
            b.chmod(0o755)
            with mock.patch.dict(os.environ, {}, clear=False):
                os.environ.pop(agent_mod.ENV_FORCE_PYTHON, None)
                self.assertEqual(agent_mod.rust_binary(Path(td)), b)

    def test_non_executable_ignored(self):
        with tempfile.TemporaryDirectory() as td:
            b = Path(td) / "vesyl-print"
            b.write_bytes(b"\x7fELF")
            b.chmod(0o644)
            self.assertIsNone(agent_mod.rust_binary(Path(td)))

    def test_env_forces_python(self):
        with tempfile.TemporaryDirectory() as td:
            b = Path(td) / "vesyl-print"
            b.write_bytes(b"\x7fELF")
            b.chmod(0o755)
            with mock.patch.dict(os.environ, {agent_mod.ENV_FORCE_PYTHON: "1"}):
                self.assertIsNone(agent_mod.rust_binary(Path(td)))


class TestSlotVersion(SlotTestCase):
    def test_reads_stripped_version(self):
        self.assertEqual(agent_mod.slot_version(self.slot), "1.2.3")

    def test_v_prefix_dropped(self):
        (self.slot / "VERSION").write_text("v1.2.3\n", encoding="utf-8")
        self.assertEqual(agent_mod.slot_version(self.slot), "1.2.3")

    def test_missing_or_empty(self):
        (self.slot / "VERSION").write_text("  \n", encoding="utf-8")
        self.assertIsNone(agent_mod.slot_version(self.slot))
        (self.slot / "VERSION").unlink()
        self.assertIsNone(agent_mod.slot_version(self.slot))


class TestProbe(SlotTestCase):
    def test_matching_version_passes(self):
        self.binary()
        self.assertIsNone(self.probe())

    def test_v_prefixed_slot_version_passes(self):
        self.binary()
        (self.slot / "VERSION").write_text("v1.2.3\n", encoding="utf-8")
        self.assertIsNone(self.probe())

    def test_nonzero_exit_fails(self):
        # What a too-old glibc looks like: the loader prints and exits 1
        # *after* execve succeeded, so only a probe can catch it.
        self.binary(
            "#!/bin/sh\n"
            "echo \"vesyl-print: /lib/aarch64-linux-gnu/libc.so.6: version "
            "\\`GLIBC_2.30' not found\" >&2\n"
            "exit 1\n"
        )
        problem = self.probe()
        self.assertIn("exited with status 1", problem)
        self.assertIn("GLIBC_2.30", problem)

    def test_killed_by_signal_fails(self):
        self.binary("#!/bin/sh\nkill -SEGV $$\n")
        self.assertIn("killed by signal 11", self.probe())

    def test_wrong_version_fails(self):
        # A stale binary left in the slot (or packaged by a broken build).
        self.binary('#!/bin/sh\necho "vesyl-print 0.4.0"\n')
        problem = self.probe()
        self.assertIn("'vesyl-print 0.4.0'", problem)
        self.assertIn("expected 'vesyl-print 1.2.3'", problem)

    def test_other_program_fails(self):
        self.binary('#!/bin/sh\necho "something-else 1.2.3"\n')
        self.assertIn("expected 'vesyl-print 1.2.3'", self.probe())

    def test_extra_output_fails(self):
        self.binary('#!/bin/sh\necho "vesyl-print 1.2.3"\necho "warning: x"\n')
        self.assertIsNotNone(self.probe())

    def test_exec_format_error_fails(self):
        self.binary(GARBAGE_BINARY)
        problem = self.probe()
        self.assertIn("cannot be executed", problem)
        self.assertIn(os.strerror(errno.ENOEXEC), problem)

    def test_missing_loader_fails(self):
        # An aarch64 binary on an armhf userland: the ELF interpreter
        # (/lib/ld-linux-aarch64.so.1) does not exist, so execve gives ENOENT.
        self.binary("#!/nonexistent/lib/ld-linux-aarch64.so.1\n")
        problem = self.probe()
        self.assertIn("cannot be executed", problem)
        self.assertIn(os.strerror(errno.ENOENT), problem)

    def test_hang_times_out(self):
        self.binary("#!/bin/sh\nexec sleep 30\n")
        start = time.monotonic()
        problem = self.probe(timeout=0.3)
        self.assertLess(time.monotonic() - start, 10)
        self.assertIn("did not finish within 0.3s", problem)

    def test_no_slot_version_fails_without_running(self):
        self.binary()
        (self.slot / "VERSION").unlink()
        with mock.patch("subprocess.run") as run:
            self.assertIn("no VERSION", self.probe())
        run.assert_not_called()

    def test_default_timeout_is_short(self):
        self.assertLessEqual(agent_mod.RUST_PROBE_TIMEOUT_S, 10)


class TestExec(SlotTestCase):
    def exec_rust(self, argv, **patches):
        err = io.StringIO()
        with mock.patch("os.execv", **patches) as execv, redirect_stderr(err):
            agent_mod.exec_rust(argv, base_dir=self.slot)
        return execv, err.getvalue()

    def test_exec_rust_execs_with_argv(self):
        fake = Path("/opt/vesyl-print/current/vesyl-print")
        with mock.patch.object(agent_mod, "rust_binary", return_value=fake), mock.patch.object(
            agent_mod, "probe_rust_binary", return_value=None
        ), mock.patch("os.execv") as execv:
            agent_mod.exec_rust(["agent"])
        execv.assert_called_once_with(str(fake), [str(fake), "agent"])

    def test_exec_rust_noop_without_binary(self):
        with mock.patch.object(agent_mod, "rust_binary", return_value=None), mock.patch(
            "os.execv"
        ) as execv:
            agent_mod.exec_rust(["agent"])
        execv.assert_not_called()

    def test_probed_binary_is_execed(self):
        b = self.binary()
        execv, err = self.exec_rust(["queues", "--json"])
        execv.assert_called_once_with(str(b), [str(b), "queues", "--json"])
        self.assertEqual(err, "")

    def test_unusable_binary_falls_back_with_warning(self):
        b = self.binary(GARBAGE_BINARY)
        execv, err = self.exec_rust(["agent"])
        execv.assert_not_called()
        self.assertIn(str(b), err)
        self.assertIn("cannot be executed", err)
        self.assertIn("Python implementation", err)
        self.assertIn(agent_mod.ENV_FORCE_PYTHON, err)

    def test_version_mismatch_falls_back(self):
        (self.slot / "VERSION").write_text("2.0.0\n", encoding="utf-8")
        self.binary()
        execv, err = self.exec_rust(["agent"])
        execv.assert_not_called()
        self.assertIn("expected 'vesyl-print 2.0.0'", err)

    def test_execv_oserror_falls_back(self):
        self.binary()
        execv, err = self.exec_rust(
            ["agent"], side_effect=OSError(errno.ENOEXEC, os.strerror(errno.ENOEXEC))
        )
        execv.assert_called_once()
        self.assertIn("exec failed", err)
        self.assertIn(os.strerror(errno.ENOEXEC), err)

    def test_force_python_skips_probe(self):
        self.binary()
        os.environ[agent_mod.ENV_FORCE_PYTHON] = "1"
        with mock.patch.object(agent_mod, "probe_rust_binary") as probe:
            execv, err = self.exec_rust(["agent"])
        probe.assert_not_called()
        execv.assert_not_called()
        self.assertEqual(err, "")


class TestEntrypointFallback(SlotTestCase):
    """agent.main() / cli.main() keep working in Python when the binary cannot run."""

    def setUp(self):
        super().setUp()
        p = mock.patch.object(agent_mod, "_slot_dir", return_value=self.slot)
        p.start()
        self.addCleanup(p.stop)

    def test_agent_main_runs_python_agent(self):
        self.binary(GARBAGE_BINARY)
        cfg = object()
        with mock.patch.object(agent_mod, "run_agent") as run_agent, mock.patch.object(
            agent_mod, "load_config", return_value=cfg
        ), mock.patch("logging.basicConfig"), mock.patch("os.execv") as execv, redirect_stderr(
            io.StringIO()
        ) as err:
            agent_mod.main()
        execv.assert_not_called()
        run_agent.assert_called_once_with(cfg)
        self.assertIn("WARNING", err.getvalue())

    def test_agent_main_execs_usable_binary(self):
        b = self.binary()
        with mock.patch("os.execv", side_effect=SystemExit(0)) as execv, mock.patch.object(
            agent_mod, "run_agent"
        ) as run_agent:
            with self.assertRaises(SystemExit):
                agent_mod.main()
        execv.assert_called_once_with(str(b), [str(b), "agent"])
        run_agent.assert_not_called()

    def test_cli_main_runs_python_command(self):
        self.binary(GARBAGE_BINARY)
        with mock.patch.object(sys, "argv", ["cli.py", "queues"]), mock.patch(
            "printers.inventory_payload", return_value=[]
        ), mock.patch("os.execv") as execv, redirect_stderr(io.StringIO()):
            self.assertEqual(cli.main(), 0)
        execv.assert_not_called()

    def test_cli_main_hands_off_full_argv(self):
        with mock.patch.object(agent_mod, "exec_rust", side_effect=SystemExit(0)) as ex, mock.patch.object(
            sys, "argv", ["cli.py", "queues", "--json"]
        ):
            with self.assertRaises(SystemExit):
                cli.main()
        ex.assert_called_once_with(["queues", "--json"])

    def test_cli_main_with_explicit_argv_stays_python(self):
        """Tests call cli.main([...]) — never hand off."""
        with mock.patch.object(agent_mod, "exec_rust") as ex, mock.patch(
            "printers.inventory_payload", return_value=[]
        ):
            cli.main(["queues"])
        ex.assert_not_called()


@unittest.skipUnless(Path("/bin/sh").exists(), "needs /bin/sh")
class TestSlotCopyEndToEnd(unittest.TestCase):
    """Run a copied release slot's cli.py the way /usr/local/bin/vesyl-print does."""

    @classmethod
    def setUpClass(cls):
        cls._td = tempfile.TemporaryDirectory()
        cls.slot = Path(cls._td.name) / "releases" / "1.2.3"
        cls.slot.mkdir(parents=True)
        for src in ROOT.glob("*.py"):
            shutil.copy2(src, cls.slot / src.name)
        (cls.slot / "VERSION").write_text("1.2.3\n", encoding="utf-8")

    @classmethod
    def tearDownClass(cls):
        cls._td.cleanup()

    def run_cli(self, binary: str | bytes, *args: str) -> subprocess.CompletedProcess:
        _write_exe(self.slot / agent_mod.RUST_BINARY, binary)
        env = {k: v for k, v in os.environ.items() if k not in ("PYTHONPATH", agent_mod.ENV_FORCE_PYTHON)}
        env["PYTHONDONTWRITEBYTECODE"] = "1"
        return subprocess.run(
            [sys.executable, str(self.slot / "cli.py"), *args],
            env=env,
            capture_output=True,
            text=True,
            timeout=60,
        )

    def test_garbage_binary_cli_still_works(self):
        # Before the fix: OSError [Errno 8] Exec format error, exit 1.
        p = self.run_cli(GARBAGE_BINARY, "--help")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertIn("usage:", p.stdout)
        self.assertIn("WARNING: not running", p.stderr)

    def test_stale_binary_cli_still_works(self):
        p = self.run_cli('#!/bin/sh\necho "vesyl-print 0.4.0"\nexit 0\n', "update", "--help")
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertIn("rollback", p.stdout)
        self.assertIn("expected 'vesyl-print 1.2.3'", p.stderr)

    def test_usable_binary_gets_the_command(self):
        p = self.run_cli(GOOD_BINARY, "queues", "--json")
        self.assertEqual(p.returncode, 7, p.stderr)
        self.assertEqual(p.stdout.strip(), "RUST:queues --json")
        self.assertEqual(p.stderr, "")


if __name__ == "__main__":
    unittest.main()
