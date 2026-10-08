"""agent.py / cli.py hand off to the slot's Rust binary when it ships one."""

from __future__ import annotations

import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

import agent as agent_mod
import cli


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


class TestExec(unittest.TestCase):
    def test_exec_rust_execs_with_argv(self):
        fake = Path("/opt/vesyl-print/current/vesyl-print")
        with mock.patch.object(agent_mod, "rust_binary", return_value=fake), mock.patch(
            "os.execv"
        ) as execv:
            agent_mod.exec_rust(["agent"])
        execv.assert_called_once_with(str(fake), [str(fake), "agent"])

    def test_exec_rust_noop_without_binary(self):
        with mock.patch.object(agent_mod, "rust_binary", return_value=None), mock.patch(
            "os.execv"
        ) as execv:
            agent_mod.exec_rust(["agent"])
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


if __name__ == "__main__":
    unittest.main()
