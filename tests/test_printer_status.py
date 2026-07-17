"""CUPS printer status parsing + long-lived job wait for paper-out recovery."""

from __future__ import annotations

import sys
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

import jobs
import printers


class TestCupsStatusParse(unittest.TestCase):
    def test_idle(self):
        text = "printer Zebra_1 is idle.  enabled since Mon 01 Jan 2026 10:00:00 AM UTC\n"
        status, reasons = printers._parse_lpstat_printer_block(text, "Zebra_1")
        self.assertEqual(status, "idle")
        self.assertEqual(reasons, [])

    def test_printing(self):
        text = "printer Zebra_1 now printing Zebra_1-42.  enabled since Mon 01 Jan 2026\n"
        status, reasons = printers._parse_lpstat_printer_block(text, "Zebra_1")
        self.assertEqual(status, "printing")

    def test_media_empty_stopped(self):
        text = (
            "printer Zebra_1 now printing Zebra_1-42.  enabled since Mon 01 Jan 2026\n"
            "\tAlert: media-empty-error\n"
            "\tmedia-empty\n"
        )
        status, reasons = printers._parse_lpstat_printer_block(text, "Zebra_1")
        self.assertEqual(status, "stopped")
        self.assertIn("media-empty-error", reasons)
        self.assertIn("media-empty", reasons)
        msg = printers._human_status_message(reasons)
        self.assertEqual(msg, "Out of paper")

    def test_paper_jam(self):
        text = (
            "printer Zebra_1 is idle.  enabled since Mon 01 Jan 2026\n"
            "\tAlerts: media-jam-error, media-jam\n"
        )
        status, reasons = printers._parse_lpstat_printer_block(text, "Zebra_1")
        self.assertEqual(status, "stopped")
        msg = printers._human_status_message(reasons)
        self.assertEqual(msg, "Paper jam")

    def test_disabled(self):
        text = "printer Zebra_1 disabled since Mon 01 Jan 2026 -\n\tPaused\n"
        status, reasons = printers._parse_lpstat_printer_block(text, "Zebra_1")
        self.assertEqual(status, "stopped")

    def test_offline_reason(self):
        text = (
            "printer Zebra_1 is idle.  enabled since Mon 01 Jan 2026\n"
            "\toffline\n"
        )
        status, reasons = printers._parse_lpstat_printer_block(text, "Zebra_1")
        self.assertEqual(status, "offline")

    def test_inventory_payload_includes_status_fields(self):
        with mock.patch(
            "printers.configured_network_queues",
            return_value=[("Zebra_1", "ipp://printer/ipp")],
        ), mock.patch(
            "printers._display_name", return_value="Zebra ZD420"
        ), mock.patch(
            "printers.cups_queue_status",
            return_value={
                "status": "stopped",
                "status_reasons": ["media-empty"],
                "status_message": "Out of paper",
            },
        ):
            items = printers.inventory_payload()
        self.assertEqual(len(items), 1)
        self.assertEqual(items[0]["status"], "stopped")
        self.assertEqual(items[0]["status_message"], "Out of paper")
        self.assertEqual(items[0]["status_reasons"], ["media-empty"])


class TestWaitCupsJob(unittest.TestCase):
    def test_returns_printed_when_job_leaves_active(self):
        calls = {"n": 0}

        def fake_run(cmd, **kwargs):
            calls["n"] += 1
            out = mock.Mock()
            out.stdout = ""
            out.stderr = ""
            # First call: still active; second: not-completed empty
            if cmd[:3] == ["lpstat", "-W", "not-completed"]:
                out.stdout = "Zebra_1-42 root 1024 Mon 01 Jan" if calls["n"] == 1 else ""
            elif cmd[:3] == ["lpstat", "-W", "completed"]:
                out.stdout = "Zebra_1-42 completed"
            return out

        with mock.patch("jobs.subprocess.run", side_effect=fake_run), mock.patch(
            "jobs.time.sleep"
        ):
            result = jobs.wait_cups_job("Zebra_1-42", timeout_s=30, poll_s=0.01)
        self.assertEqual(result, "printed")

    def test_paper_out_recovery_still_reports_printed(self):
        """Job stays in not-completed for several polls (paper out), then completes."""
        n_active = {"v": 0}

        def fake_run(cmd, **kwargs):
            out = mock.Mock()
            out.stdout = ""
            out.stderr = ""
            if cmd[:3] == ["lpstat", "-W", "not-completed"]:
                n_active["v"] += 1
                # Active for first 5 polls, then done
                out.stdout = "Zebra_1-99" if n_active["v"] <= 5 else ""
            return out

        ticks: list[int] = []

        with mock.patch("jobs.subprocess.run", side_effect=fake_run), mock.patch(
            "jobs.time.sleep"
        ):
            result = jobs.wait_cups_job(
                "Zebra_1-99",
                timeout_s=120,
                poll_s=0.01,
                on_tick=lambda: ticks.append(1),
            )
        self.assertEqual(result, "printed")
        self.assertGreaterEqual(len(ticks), 1)

    def test_unknown_only_after_hard_timeout_while_active(self):
        def fake_run(cmd, **kwargs):
            out = mock.Mock()
            out.stdout = "Zebra_1-1 still here"
            out.stderr = ""
            return out

        # Force deadline to pass immediately after first poll by freezing time.
        times = iter([0.0, 0.0, 0.0, 1000.0])

        with mock.patch("jobs.subprocess.run", side_effect=fake_run), mock.patch(
            "jobs.time.sleep"
        ), mock.patch("jobs.time.monotonic", side_effect=lambda: next(times, 1000.0)):
            result = jobs.wait_cups_job("Zebra_1-1", timeout_s=30, poll_s=0.01)
        self.assertEqual(result, "unknown")


if __name__ == "__main__":
    unittest.main()
