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


class TestIppStatusParse(unittest.TestCase):
    """lpstat only shows idle/printing — media-empty comes from IPP attrs."""

    def test_ipp_processing_with_media_empty(self):
        text = """
        printer-state (enum) = processing
        printer-state-reasons (1setOf keyword) = media-empty,media-needed
        printer-state-message (textWithoutLanguage) =
        queued-job-count (integer) = 1
        """
        parsed = printers._parse_ipp_printer_attrs(text)
        self.assertEqual(parsed["status"], "stopped")
        self.assertIn("media-empty", parsed["status_reasons"])
        self.assertEqual(parsed["status_message"], "Out of paper")

    def test_ipp_processing_media_empty_error_single(self):
        text = """
        printer-state (enum) = processing
        printer-state-reasons (keyword) = media-empty-error
        printer-state-message (textWithoutLanguage) = The printer is out of paper.
        queued-job-count (integer) = 1
        """
        parsed = printers._parse_ipp_printer_attrs(text)
        self.assertEqual(parsed["status"], "stopped")
        self.assertEqual(parsed["status_message"], "The printer is out of paper.")

    def test_ipp_idle_none(self):
        text = """
        printer-state (enum) = idle
        printer-state-reasons (keyword) = none
        printer-state-message (textWithoutLanguage) =
        queued-job-count (integer) = 0
        """
        parsed = printers._parse_ipp_printer_attrs(text)
        self.assertEqual(parsed["status"], "idle")
        self.assertEqual(parsed["status_reasons"], [])
        self.assertIsNone(parsed["status_message"])

    def test_ipp_numeric_state_and_jam(self):
        text = """
        printer-state (enum) = 5
        printer-state-reasons (keyword) = media-jam-error
        """
        parsed = printers._parse_ipp_printer_attrs(text)
        self.assertEqual(parsed["status"], "stopped")
        self.assertEqual(parsed["status_message"], "Paper jam")

    def test_job_reasons_media_empty(self):
        text = """
        job-id (integer) = 42
        job-state (enum) = processing
        job-printer-state-reasons (1setOf keyword) = media-empty-error
        job-state-reasons (keyword) = job-printing
        """
        info = printers._parse_ipp_jobs_attrs(text)
        self.assertIn("media-empty-error", info["status_reasons"])
        # job-printing is benign and filtered
        self.assertNotIn("job-printing", info["status_reasons"])

    def test_cups_queue_status_prefers_ipp(self):
        ipp_dump = """
        printer-state (enum) = processing
        printer-state-reasons (keyword) = media-empty
        printer-state-message (textWithoutLanguage) =
        queued-job-count (integer) = 1
        """
        with mock.patch("printers._ipptool", side_effect=[ipp_dump, ""]), mock.patch(
            "printers._run", return_value="printer Z is now printing Z-1.\n"
        ):
            st = printers.cups_queue_status("Z")
        self.assertEqual(st["status"], "stopped")
        self.assertEqual(st["status_message"], "Out of paper")
        self.assertIn("media-empty", st["status_reasons"])

    def test_device_uri_merge_when_local_reasons_empty(self):
        local = """
        printer-state (enum) = processing
        printer-state-reasons (keyword) = none
        queued-job-count (integer) = 1
        """
        device = """
        printer-state (enum) = stopped
        printer-state-reasons (keyword) = media-empty-error
        printer-state-message (textWithoutLanguage) = Out of paper
        """

        def fake_ipptool(uri, test_body, timeout=5.0):
            if "localhost" in uri and "Get-Jobs" in test_body:
                return ""
            if "localhost" in uri:
                return local
            return device

        with mock.patch("printers._ipptool", side_effect=fake_ipptool):
            st = printers.cups_queue_status(
                "Brother_X", device_uri="ipps://brother.local/ipp/print"
            )
        self.assertEqual(st["status"], "stopped")
        self.assertIn("media-empty-error", st["status_reasons"])
        self.assertEqual(st["status_message"], "Out of paper")


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


class TestWaitTickLiveness(unittest.TestCase):
    """While blocked on CUPS (paper-out), REST heartbeats must keep flowing."""

    def test_rest_heartbeat_even_when_cable_subscribed(self):
        import agent as agent_mod

        client = mock.Mock()
        sess = mock.Mock()
        sess.subscribed = True
        sess.perform.return_value = True

        tick = agent_mod._inventory_wait_tick(
            sess,
            client=client,
            device_token="tok",
            heartbeat_seconds=30,
        )
        with mock.patch(
            "agent.printers.inventory_payload",
            return_value=[{"cups_name": "P1", "state": "stopped"}],
        ), mock.patch("agent.time.monotonic", return_value=100.0):
            tick()
            tick()  # second call within interval — no extra REST

        # REST heartbeat always sent (cloud last_seen), even if cable is up.
        self.assertEqual(client.heartbeat.call_count, 1)
        client.heartbeat.assert_called_with(
            "tok",
            agent_version=mock.ANY,
            hostname=mock.ANY,
            printers=[{"cups_name": "P1", "state": "stopped"}],
            platform=mock.ANY,
        )
        # Cable still gets a refresh too.
        self.assertTrue(sess.perform.called)

    def test_rest_heartbeat_when_inventory_fails(self):
        import agent as agent_mod

        client = mock.Mock()
        tick = agent_mod._inventory_wait_tick(
            None, client=client, device_token="tok", heartbeat_seconds=30
        )
        with mock.patch(
            "agent.printers.inventory_payload", side_effect=RuntimeError("cups down")
        ), mock.patch("agent.time.monotonic", return_value=50.0):
            tick()
        client.heartbeat.assert_called_once()
        kwargs = client.heartbeat.call_args.kwargs
        self.assertIsNone(kwargs.get("printers"))


if __name__ == "__main__":
    unittest.main()
