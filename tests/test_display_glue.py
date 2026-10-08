"""Display ↔ agent glue: printers.json, the vesyl-print CLI, config paths.

The display reads the agent's state_dir/printers.json for its printer rows
and runs this slot's ``vesyl-print`` binary for the test print. Tests use a
fake binary (a shell script on a temp path) and temp state dirs.
"""

from __future__ import annotations

import json
import os
import sys
import tempfile
import threading
import time
import types
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
for p in (ROOT, Path(__file__).resolve().parent):
    if str(p) not in sys.path:
        sys.path.insert(0, str(p))

import config
import display_status as disp
import statusio
from fakecli import make_fake_cli, recorded_args

NOW = datetime(2026, 10, 8, 14, 0, 0, tzinfo=timezone.utc)
ZEBRA = {
    "cups_name": "Zebra_ZD421",
    "uri": "socket://192.168.1.50:9100",
    "display_name": "Zebra ZD421",
    "status": "idle",
    "status_reasons": [],
    "status_message": None,
    "supports_raw": True,
}
HP = {
    "cups_name": "HP_OfficeJet",
    "uri": "ipp://192.168.1.60/ipp/print",
    "display_name": "HP OfficeJet Pro",
    "status": "stopped",
    "status_reasons": ["media-empty-error"],
    "status_message": "Out of paper",
    "supports_raw": False,
}


def _import_main():
    """main.py, with a stand-in framebuffer module when numpy is missing."""
    try:
        import framebuffer  # noqa: F401
    except ImportError:
        stub = types.ModuleType("framebuffer")
        stub.Framebuffer = object
        sys.modules["framebuffer"] = stub
    import main

    return main


class PrintersJsonTests(unittest.TestCase):
    def setUp(self):
        td = tempfile.TemporaryDirectory()
        self.addCleanup(td.cleanup)
        self.path = Path(td.name) / "printers.json"

    def write(self, printers, updated_at="2026-10-08T13:59:50+00:00", **extra):
        body = {"printers": printers, **extra}
        if updated_at is not None:
            body["updated_at"] = updated_at
        self.path.write_text(json.dumps(body), encoding="utf-8")

    def test_valid_snapshot(self):
        self.write([ZEBRA, HP])
        snap = statusio.read_printers(self.path, now=NOW)
        self.assertFalse(snap.stale)
        self.assertEqual(snap.updated_at, "2026-10-08T13:59:50+00:00")
        self.assertEqual(snap.printers, [ZEBRA, HP])

    def test_missing_or_unreadable(self):
        self.assertIsNone(statusio.read_printers(self.path, now=NOW))
        with self.assertRaises(OSError):
            statusio.load_printers(self.path, now=NOW)
        for raw in ("{\"printers\": ", "[]", "{}", "{\"printers\": {}}", "null"):
            self.path.write_text(raw, encoding="utf-8")
            self.assertIsNone(statusio.read_printers(self.path, now=NOW), raw)
            with self.assertRaises(ValueError):
                statusio.load_printers(self.path, now=NOW)
        self.path.write_bytes(b"\xff\xfe{")
        self.assertIsNone(statusio.read_printers(self.path, now=NOW))

    def test_stale_snapshot_reads_unknown(self):
        self.write([ZEBRA, HP], updated_at="2026-10-08T13:57:59+00:00")  # 121 s old
        snap = statusio.read_printers(self.path, now=NOW)
        self.assertTrue(snap.stale)
        self.assertEqual(
            snap.printers,
            [
                {**ZEBRA, "status": "unknown", "status_message": None, "status_reasons": []},
                {**HP, "status": "unknown", "status_message": None, "status_reasons": []},
            ],
        )

    def test_stale_threshold_is_120_seconds(self):
        self.write([ZEBRA], updated_at="2026-10-08T13:58:00+00:00")  # exactly 120 s
        self.assertFalse(statusio.read_printers(self.path, now=NOW).stale)
        self.assertEqual(statusio.PRINTERS_STALE_AFTER_S, 120.0)
        snap = statusio.read_printers(self.path, now=NOW, stale_after_s=60)
        self.assertTrue(snap.stale)

    def test_timestamp_forms(self):
        for ts, stale in (
            ("2026-10-08T13:59:00Z", False),
            ("2026-10-08T13:59:00", False),  # naive: UTC
            ("2026-10-08T15:59:00+02:00", False),
            ("2026-10-08T14:30:00+00:00", False),  # ahead of the clock: fresh
            ("2026-10-08T12:00:00Z", True),
        ):
            self.write([ZEBRA], updated_at=ts)
            self.assertEqual(statusio.read_printers(self.path, now=NOW).stale, stale, ts)

    def test_missing_timestamp_uses_file_mtime(self):
        for updated_at in (None, "", "yesterday", 1234):
            self.write([HP], updated_at=updated_at)
            snap = statusio.read_printers(self.path)
            self.assertFalse(snap.stale, updated_at)
            self.assertEqual(snap.printers[0]["status"], "stopped")
            old = time.time() - 600
            os.utime(self.path, (old, old))
            snap = statusio.read_printers(self.path)
            self.assertTrue(snap.stale, updated_at)
            self.assertEqual(snap.printers[0]["status"], "unknown")

    def test_non_object_items_are_skipped(self):
        self.write([ZEBRA, "junk", None, 3, HP])
        snap = statusio.read_printers(self.path, now=NOW)
        self.assertEqual([p["cups_name"] for p in snap.printers], ["Zebra_ZD421", "HP_OfficeJet"])


class PrinterRowsTests(unittest.TestCase):
    def test_rows_match_the_lcd_derivation(self):
        rows = disp.printer_rows(
            [
                ZEBRA,
                HP,
                {"cups_name": "Brother_QL", "display_name": None, "status": "printing"},
                {"cups_name": "", "display_name": "", "status": None, "supports_raw": 0},
                {},
            ]
        )
        self.assertEqual(
            rows,
            [
                {"name": "Zebra ZD421", "cups_name": "Zebra_ZD421", "status": "idle",
                 "message": None, "supports_raw": True},
                {"name": "HP OfficeJet Pro", "cups_name": "HP_OfficeJet", "status": "stopped",
                 "message": "Out of paper", "supports_raw": False},
                {"name": "Brother_QL", "cups_name": "Brother_QL", "status": "printing",
                 "message": None, "supports_raw": False},
                {"name": "—", "cups_name": "", "status": None, "message": None,
                 "supports_raw": False},
                {"name": "—", "cups_name": "", "status": None, "message": None,
                 "supports_raw": False},
            ],
        )


class CliTests(unittest.TestCase):
    def setUp(self):
        td = tempfile.TemporaryDirectory()
        self.addCleanup(td.cleanup)
        self.dir = Path(td.name)
        self.exe = self.dir / "vesyl-print"

    def fake(self, stdout, **kw):
        return make_fake_cli(self.dir, stdout, **kw)

    def test_test_print_success(self):
        self.fake({"ok": True, "state": "delivered", "job_id": "j1", "queue": "Zebra_ZD421",
                   "format": "zpl"})
        state = disp.submit_test_print(" Zebra_ZD421 ", "ZPL", binary=self.exe)
        self.assertEqual(state, "delivered")
        self.assertEqual(
            recorded_args(self.dir),
            ["test-print", "--queue", "Zebra_ZD421", "--format", "zpl", "--json"],
        )

    def test_default_binary_is_the_slot_binary(self):
        self.fake({"ok": True, "state": "delivered"})
        with mock.patch.object(disp, "VESYL_PRINT_BIN", self.exe):
            self.assertEqual(disp.submit_test_print("Q", "pdf"), "delivered")
        self.assertEqual(recorded_args(self.dir)[:3], ["test-print", "--queue", "Q"])

    def test_test_print_failure_carries_message_and_code(self):
        self.fake({"ok": False, "error": "lp failed: no such queue", "code": "lp_failed"}, rc=1)
        with self.assertRaises(disp.CliError) as cm:
            disp.submit_test_print("Gone", "pdf", binary=self.exe)
        self.assertEqual((cm.exception.message, cm.exception.code),
                         ("lp failed: no such queue", "lp_failed"))
        self.fake({"ok": False}, rc=1)
        with self.assertRaises(disp.CliError) as cm:
            disp.submit_test_print("Gone", "pdf", binary=self.exe)
        self.assertEqual(cm.exception.message, "test print failed")

    def test_queue_starting_with_dash_is_passed_as_one_argument(self):
        self.fake({"ok": True, "state": "delivered"})
        disp.submit_test_print("-odd", "pdf", binary=self.exe)
        self.assertEqual(
            recorded_args(self.dir),
            ["test-print", "--queue=-odd", "--format", "pdf", "--json"],
        )

    def test_unrunnable_binary(self):
        out = disp.run_vesyl_print(["test-print", "--json"], timeout=5, binary=self.exe)
        self.assertEqual(out, {"ok": False, "error": "vesyl-print not found"})
        self.exe.write_text("not a program\n", encoding="utf-8")
        self.exe.chmod(0o644)
        out = disp.run_vesyl_print(["test-print", "--json"], timeout=5, binary=self.exe)
        self.assertFalse(out["ok"])
        self.assertTrue(out["error"].startswith("cannot run vesyl-print: "), out)
        self.fake({"ok": True})
        out = disp.run_vesyl_print(["claim", "A\x00B", "--json"], timeout=5, binary=self.exe)
        self.assertFalse(out["ok"])
        self.assertTrue(out["error"].startswith("cannot run vesyl-print: "), out)
        self.assertIsNone(recorded_args(self.dir))

    def test_output_without_json(self):
        self.fake("", stderr="error: unrecognized subcommand 'test-print'\n\nUsage: x\n", rc=2)
        out = disp.run_vesyl_print(["test-print", "--json"], timeout=5, binary=self.exe)
        self.assertEqual(out, {"ok": False, "error": "error: unrecognized subcommand 'test-print'"})
        self.fake("Printed.\n", stderr="lp: some warning\nthe last line\n", rc=1)
        out = disp.run_vesyl_print(["test-print", "--json"], timeout=5, binary=self.exe)
        self.assertEqual(out, {"ok": False, "error": "the last line"})
        self.fake("", rc=3)
        out = disp.run_vesyl_print(["test-print", "--json"], timeout=5, binary=self.exe)
        self.assertEqual(out, {"ok": False, "error": "vesyl-print exited with status 3"})

    def test_ok_json_with_failing_exit_status_is_a_failure(self):
        self.fake({"ok": True, "state": "delivered"}, rc=1)
        out = disp.run_vesyl_print(["test-print", "--json"], timeout=5, binary=self.exe)
        self.assertEqual(out, {"ok": False, "error": "vesyl-print exited with status 1"})

    def test_json_after_noise_and_pretty_json(self):
        self.fake('log line\n{"ok": true, "state": "delivered"}\n')
        out = disp.run_vesyl_print(["test-print", "--json"], timeout=5, binary=self.exe)
        self.assertEqual(out, {"ok": True, "state": "delivered"})
        self.fake(json.dumps({"ok": False, "error": "x", "code": "c"}, indent=2), rc=1)
        out = disp.run_vesyl_print(["test-print", "--json"], timeout=5, binary=self.exe)
        self.assertEqual(out, {"ok": False, "error": "x", "code": "c"})

    def test_timeout(self):
        self.fake({"ok": True}, sleep=10)
        t0 = time.monotonic()
        with self.assertRaises(disp.CliError) as cm:
            disp.submit_test_print("Q", "pdf", binary=self.exe, timeout=0.5)
        self.assertEqual(cm.exception.message, "timed out after 0.5s")
        self.assertLess(time.monotonic() - t0, 5)
        self.assertEqual(disp.TEST_PRINT_TIMEOUT_S, 90.0)


class MainGlueTests(unittest.TestCase):
    """main.py's printer thread and LCD test print (no fonts / framebuffer)."""

    @classmethod
    def setUpClass(cls):
        cls.main = _import_main()

    def setUp(self):
        td = tempfile.TemporaryDirectory()
        self.addCleanup(td.cleanup)
        self.dir = Path(td.name)
        self.path = self.dir / "printers.json"
        self.screen = SimpleNamespace(printer_rows=[], printer_names=[])

    def write(self, printers, age_s=5):
        ts = (datetime.now(timezone.utc) - timedelta(seconds=age_s)).replace(microsecond=0)
        self.path.write_text(
            json.dumps({"updated_at": ts.isoformat(), "printers": printers}), encoding="utf-8"
        )

    def refresh_once(self):
        stop = mock.Mock()
        stop.is_set.side_effect = [False, True]
        self.main._refresh_printers(self.screen, stop, self.path)
        stop.wait.assert_called_once_with(self.main._PRINTER_REFRESH_S)

    def ops_rows(self):
        return self.main.InfoScreen._ops_printer_rows(self.screen)

    def test_rows_from_printers_json(self):
        self.write([ZEBRA, HP])
        self.refresh_once()
        self.assertEqual(self.screen.printer_rows, disp.printer_rows([ZEBRA, HP]))
        self.assertEqual(self.screen.printer_names, ["Zebra ZD421", "HP OfficeJet Pro"])

    def test_missing_file_shows_no_printers_then_keeps_rows(self):
        self.refresh_once()
        self.assertEqual(self.ops_rows(), [])
        self.write([ZEBRA])
        self.refresh_once()
        rows = self.screen.printer_rows
        self.path.unlink()
        self.refresh_once()
        self.assertIs(self.screen.printer_rows, rows)
        self.path.write_text("{\"printers\": [", encoding="utf-8")
        self.refresh_once()
        self.assertIs(self.screen.printer_rows, rows)

    def test_stale_file_shows_unknown_statuses(self):
        self.write([ZEBRA, HP], age_s=600)
        self.refresh_once()
        self.assertEqual(
            [(r["name"], r["status"], r["message"]) for r in self.screen.printer_rows],
            [("Zebra ZD421", "unknown", None), ("HP OfficeJet Pro", "unknown", None)],
        )
        self.assertTrue(self.screen.printer_rows[0]["supports_raw"])
        self.write([ZEBRA, HP])
        self.refresh_once()
        self.assertEqual(self.screen.printer_rows[1]["message"], "Out of paper")

    def test_empty_inventory_falls_back_to_placeholder_rows(self):
        self.write([ZEBRA, HP])
        self.refresh_once()
        self.write([])
        self.refresh_once()
        self.assertEqual(self.screen.printer_rows, [])
        self.assertEqual(
            self.ops_rows(),
            [
                {"name": n, "cups_name": n, "status": None, "message": None,
                 "supports_raw": False}
                for n in ("Zebra ZD421", "HP OfficeJet Pro")
            ],
        )

    def _start_test_print(self, fmt, selected):
        ui = disp.TestPrintState()
        ui.open_panel(time.monotonic())
        ui.selected = selected
        screen = SimpleNamespace(test_ui=ui, _print_lock=threading.Lock())
        self.main.InfoScreen._start_test_print(screen, fmt)
        for t in threading.enumerate():
            if t.name == "vesyl-test-print":
                t.join(10)
        self.assertFalse(ui.busy)
        self.assertFalse(screen._print_lock.locked())
        return ui.message

    def test_lcd_test_print_runs_cli(self):
        exe = make_fake_cli(self.dir, {"ok": True, "state": "delivered", "job_id": "j"})
        with mock.patch.object(disp, "VESYL_PRINT_BIN", exe):
            msg = self._start_test_print("zpl", {"cups_name": "Zebra_ZD421 "})
        self.assertEqual(msg, "ZPL delivered")
        self.assertEqual(
            recorded_args(self.dir),
            ["test-print", "--queue", "Zebra_ZD421", "--format", "zpl", "--json"],
        )

    def test_lcd_test_print_failures(self):
        exe = make_fake_cli(
            self.dir,
            {"ok": False, "code": "lp_failed",
             "error": "lp failed: lp: Error - The printer or class does not exist."},
            rc=1,
        )
        with mock.patch.object(disp, "VESYL_PRINT_BIN", exe):
            msg = self._start_test_print("pdf", {"cups_name": "Gone"})
        self.assertEqual(msg, "Failed: lp failed: lp: Error - The printer or cl")
        self.assertEqual(len(msg), 48)
        with mock.patch.object(disp, "VESYL_PRINT_BIN", self.dir / "missing"):
            msg = self._start_test_print("pdf", {"cups_name": "Gone"})
        self.assertEqual(msg, "Failed: vesyl-print not found")

    def test_lcd_test_print_needs_a_queue(self):
        self.assertEqual(self._start_test_print("pdf", {"cups_name": " "}), "No CUPS queue")


class ConfigTests(unittest.TestCase):
    def test_paths(self):
        cfg = config.Config(config_dir=Path("/c"), state_dir=Path("/s"))
        self.assertEqual(cfg.printers_path, Path("/s/printers.json"))
        self.assertEqual(cfg.status_path, Path("/s/status.json"))
        self.assertEqual(cfg.update_status_path, Path("/s/update_status.json"))
        self.assertEqual(cfg.queue_dir, Path("/s/queue"))
        self.assertEqual(cfg.processed_dir, Path("/s/processed"))
        self.assertEqual(cfg.credentials_path, Path("/c/credentials.json"))
        self.assertEqual(cfg.config_path, Path("/c/config.json"))

    def test_load_config_api_url_and_dirs(self):
        with tempfile.TemporaryDirectory() as td:
            cdir, sdir = Path(td) / "etc", Path(td) / "state"
            cdir.mkdir()
            env = {"VESYL_PRINT_CONFIG_DIR": str(cdir), "VESYL_PRINT_STATE_DIR": str(sdir)}
            with mock.patch.dict(os.environ, env):
                os.environ.pop("VESYL_PRINT_API_URL", None)
                cfg = config.load_config()
                self.assertEqual((cfg.config_dir, cfg.state_dir), (cdir, sdir))
                self.assertEqual(cfg.api_base_url, config.DEFAULT_API_BASE_URL)
                (cdir / "config.json").write_text(
                    json.dumps({"api_base_url": "https://api.example/", "heartbeat_seconds": "x"}),
                    encoding="utf-8",
                )
                self.assertEqual(config.load_config().api_base_url, "https://api.example")
                (cdir / "config.json").write_text("{oops", encoding="utf-8")
                self.assertEqual(config.load_config().api_base_url, config.DEFAULT_API_BASE_URL)
                os.environ["VESYL_PRINT_API_URL"] = "http://localhost:3000/"
                self.assertEqual(config.load_config().api_base_url, "http://localhost:3000")
            # nothing is created just by loading
            self.assertFalse(sdir.exists())


if __name__ == "__main__":
    unittest.main()
