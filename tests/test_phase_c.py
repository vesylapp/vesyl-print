"""Phase C unit tests — job pull, ack, state (mocked HTTP)."""

from __future__ import annotations

import base64
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

import agent as agent_mod
import auth
import cloud
import jobs
from config import Config
from jobs import JobStore, PrintJob


def _png_b64() -> str:
    png = base64.b64decode(
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
    )
    return base64.b64encode(png).decode("ascii")


PENDING_JOB = {
    "id": "job-uuid-1",
    "printer_id": "printer-1",
    "cups_name": "Label_1",
    "content_type": "png_base64",
    "content": None,  # filled in setUp
    "title": "Ship label",
    "source": "packing",
    "options": {"copies": 1},
    "status": "sent",
    "expires_at": "2026-07-15T20:00:00Z",
}


class TestCloudJobsAPI(unittest.TestCase):
    def test_pending_jobs_parses_list(self):
        client = cloud.CloudClient("https://example.test")
        body = json.dumps({"jobs": [PENDING_JOB]}).encode()

        class Resp:
            status = 200

            def read(self):
                return body

            def __enter__(self):
                return self

            def __exit__(self, *a):
                return False

        with mock.patch("urllib.request.urlopen", return_value=Resp()):
            jobs_list = client.pending_jobs("tok")
        self.assertEqual(len(jobs_list), 1)
        self.assertEqual(jobs_list[0]["id"], "job-uuid-1")
        self.assertEqual(jobs_list[0]["cups_name"], "Label_1")

    def test_pending_empty(self):
        client = cloud.CloudClient("https://example.test")

        class Resp:
            status = 200

            def read(self):
                return b'{"jobs":[]}'

            def __enter__(self):
                return self

            def __exit__(self, *a):
                return False

        with mock.patch("urllib.request.urlopen", return_value=Resp()):
            self.assertEqual(client.pending_jobs("tok"), [])

    def test_ack_and_state_paths(self):
        client = cloud.CloudClient("https://example.test")
        seen: list[tuple[str, str, bytes | None]] = []

        class Resp:
            status = 200

            def read(self):
                return b'{"ok":true}'

            def __enter__(self):
                return self

            def __exit__(self, *a):
                return False

        def fake_urlopen(req, timeout=None):
            seen.append((req.get_method(), req.full_url, req.data))
            return Resp()

        with mock.patch("urllib.request.urlopen", side_effect=fake_urlopen):
            client.ack_job("tok", "job-uuid-1")
            client.report_job_status("tok", "job-uuid-1", "delivered")
            client.report_job_status(
                "tok", "job-uuid-1", "error", message="lp failed"
            )

        self.assertEqual(len(seen), 3)
        self.assertIn("/print/v1/jobs/job-uuid-1/ack", seen[0][1])
        self.assertEqual(seen[0][0], "POST")
        self.assertIn("/print/v1/jobs/job-uuid-1/status", seen[1][1])
        state_body = json.loads(seen[2][2].decode())
        self.assertEqual(state_body["status"], "error")
        self.assertEqual(state_body["message"], "lp failed")


class TestPullAndProcess(unittest.TestCase):
    def setUp(self):
        PENDING_JOB["content"] = _png_b64()

    def _cfg(self, td: str) -> Config:
        cdir = Path(td) / "cfg"
        sdir = Path(td) / "state"
        cdir.mkdir()
        sdir.mkdir()
        return Config(
            api_base_url="https://example.test",
            config_dir=cdir,
            state_dir=sdir,
            pull_jobs_enabled=True,
            pull_interval_seconds=5,
            heartbeat_seconds=30,
        )

    def test_pull_and_process_invokes_pipeline(self):
        with tempfile.TemporaryDirectory() as td:
            cfg = self._cfg(td)
            store = jobs.store_from_config(cfg)
            client = mock.Mock()
            client.pending_jobs.return_value = [dict(PENDING_JOB)]

            with mock.patch("jobs.receive_job", return_value="delivered") as recv:
                result = agent_mod.pull_and_process(
                    cfg, client, "device-token", store=store
                )

            self.assertEqual(result, "ok")
            recv.assert_called_once()
            job_arg = recv.call_args[0][0]
            self.assertEqual(job_arg.id, "job-uuid-1")
            self.assertEqual(job_arg.cups_name, "Label_1")
            client.pending_jobs.assert_called_once_with("device-token")

    def test_pull_ordering_with_injected_lp(self):
        """Full order: pending → write queue → ack → printing → lp → delivered."""
        with tempfile.TemporaryDirectory() as td:
            cfg = self._cfg(td)
            store = jobs.store_from_config(cfg)
            events: list[str] = []
            client = mock.Mock()
            client.pending_jobs.return_value = [dict(PENDING_JOB)]

            def ack_job(token, job_id):
                self.assertTrue(store.has_queue_file(job_id))
                events.append("ack")
                return {"ok": True}

            def report_job_status(token, job_id, status, message=None):
                events.append(f"status:{status}")
                return {"ok": True}

            client.ack_job.side_effect = ack_job
            client.report_job_status.side_effect = report_job_status

            def lp(cups, path, *, title=None, copies=1, raw=False):
                events.append("lp")
                return None  # no CUPS job id → leave as delivered

            # Drive through receive_job with hooks from cloud_job_hooks
            payloads = client.pending_jobs("tok")
            job = PrintJob.from_dict(payloads[0])
            ack, report_state = agent_mod.cloud_job_hooks(client, "tok")
            jobs.receive_job(job, store, lp=lp, ack=ack, report_state=report_state)

            self.assertEqual(
                events, ["ack", "status:printing", "lp", "status:delivered"]
            )
            self.assertTrue(store.is_processed(job.id))
            self.assertFalse(store.has_queue_file(job.id))
            statuses = [c[0][2] for c in client.report_job_status.call_args_list]
            self.assertEqual(statuses, ["printing", "delivered"])

    def test_pull_404_returns_unavailable(self):
        with tempfile.TemporaryDirectory() as td:
            cfg = self._cfg(td)
            client = mock.Mock()
            client.pending_jobs.side_effect = cloud.CloudError(
                "not found", status=404
            )
            result = agent_mod.pull_and_process(cfg, client, "tok")
            self.assertEqual(result, "unavailable")

    def test_pull_401_returns_unauthorized(self):
        with tempfile.TemporaryDirectory() as td:
            cfg = self._cfg(td)
            client = mock.Mock()
            client.pending_jobs.side_effect = cloud.CloudError(
                "nope", status=401, code="unauthorized"
            )
            result = agent_mod.pull_and_process(cfg, client, "tok")
            self.assertEqual(result, "unauthorized")

    def test_already_processed_reports_printed_without_reprint(self):
        with tempfile.TemporaryDirectory() as td:
            cfg = self._cfg(td)
            store = jobs.store_from_config(cfg)
            job = PrintJob.from_dict(
                {**PENDING_JOB, "content": _png_b64()}
            )
            store.mark_processed(job.id)
            client = mock.Mock()
            lp = mock.Mock()
            ack, report_state = agent_mod.cloud_job_hooks(client, "tok")
            jobs.receive_job(job, store, lp=lp, ack=ack, report_state=report_state)
            lp.assert_not_called()
            client.ack_job.assert_not_called()
            client.report_job_status.assert_called_once()
            self.assertEqual(
                client.report_job_status.call_args[0][2], "printed"
            )


class TestCloudHooks(unittest.TestCase):
    def test_hooks_report_printing_and_error(self):
        client = mock.Mock()
        ack, report_state = agent_mod.cloud_job_hooks(client, "tok")
        job = PrintJob(
            id="j1",
            cups_name="P",
            content_type="png_base64",
            content="AA==",
        )
        report_state(job, "printing", None)
        client.report_job_status.assert_called_once_with(
            "tok", "j1", "printing", message=None
        )
        client.report_job_status.reset_mock()
        report_state(job, "error", "boom")
        client.report_job_status.assert_called_once_with(
            "tok", "j1", "error", message="boom"
        )


if __name__ == "__main__":
    unittest.main()
