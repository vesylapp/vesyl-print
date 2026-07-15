"""Phase D unit tests — ActionCable protocol helpers and session dispatch."""

from __future__ import annotations

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
import cable
import jobs
from config import Config
from jobs import JobStore


FIXTURES = ROOT / "tests" / "fixtures" / "actioncable"


def _load_fixture(name: str) -> dict:
    return json.loads((FIXTURES / name).read_text(encoding="utf-8"))


class TestCableHelpers(unittest.TestCase):
    def test_channel_identifier(self):
        self.assertEqual(
            cable.channel_identifier(),
            '{"channel":"PrintNodeChannel"}',
        )

    def test_build_connect_url_appends_token(self):
        url = cable.build_cable_connect_url(
            "wss://wms-api.vesyl.dev/print/cable", "tic.ket-1"
        )
        self.assertTrue(url.startswith("wss://wms-api.vesyl.dev/print/cable?"))
        self.assertIn("token=tic.ket-1", url)

    def test_build_connect_url_replaces_token(self):
        url = cable.build_cable_connect_url(
            "wss://example/print/cable?token=old", "new"
        )
        self.assertIn("token=new", url)
        self.assertNotIn("token=old", url)


class TestActionCableClientProtocol(unittest.TestCase):
    """Drive _on_ws_message without a real network."""

    def test_welcome_subscribe_confirm_message(self):
        events: list[str] = []
        messages: list[dict] = []
        sent: list[dict] = []

        client = cable.ActionCableClient(
            "wss://example/print/cable?token=t",
            on_message=lambda m: messages.append(m),
            on_connected=lambda: events.append("connected"),
            on_subscribed=lambda: events.append("subscribed"),
        )

        def capture_send(obj):
            sent.append(obj)

        client._send = capture_send  # type: ignore[method-assign]

        client._on_ws_message(None, json.dumps(_load_fixture("welcome.json")))
        self.assertIn("connected", events)
        self.assertEqual(sent[0]["command"], "subscribe")
        self.assertEqual(
            sent[0]["identifier"], '{"channel":"PrintNodeChannel"}'
        )

        client._on_ws_message(
            None, json.dumps(_load_fixture("confirm_subscription.json"))
        )
        self.assertTrue(client.subscribed)
        self.assertIn("subscribed", events)

        client._on_ws_message(None, json.dumps(_load_fixture("print_job.json")))
        self.assertEqual(len(messages), 1)
        self.assertEqual(messages[0]["type"], "print_job")
        self.assertEqual(messages[0]["job"]["id"], "job-uuid-1")

    def test_perform_frame_shape(self):
        client = cable.ActionCableClient("wss://example/print/cable?token=t")
        sent: list[dict] = []
        client._send = lambda obj: sent.append(obj)  # type: ignore[method-assign]
        client._subscribed.set()
        client._connected.set()
        # pretend ws exists for perform's check — perform calls _send only
        client._ws = object()
        client.perform("ack_job", job_id="j1")
        self.assertEqual(sent[0]["command"], "message")
        data = json.loads(sent[0]["data"])
        self.assertEqual(data["action"], "ack_job")
        self.assertEqual(data["job_id"], "j1")

    def test_ping_ignored(self):
        client = cable.ActionCableClient("wss://example/print/cable?token=t")
        client._on_ws_message(None, '{"type":"ping","message":123}')
        # no crash


class TestPrintCableSessionDispatch(unittest.TestCase):
    def test_dispatch_print_job_and_revoke(self):
        jobs_seen: list[dict] = []
        revoked = []

        sess = cable.PrintCableSession(
            cable_url="wss://example/print/cable",
            get_ticket=lambda: {"ticket": "t"},
            on_print_job=lambda j: jobs_seen.append(j),
            on_revoke=lambda: revoked.append(True),
        )
        frame = _load_fixture("print_job.json")
        sess._dispatch_message(frame["message"])
        self.assertEqual(jobs_seen[0]["cups_name"], "Label_1")

        sess._dispatch_message(_load_fixture("revoke.json")["message"])
        self.assertTrue(revoked)

    def test_job_canceled(self):
        canceled: list[str] = []
        sess = cable.PrintCableSession(
            cable_url="wss://example/print/cable",
            get_ticket=lambda: {"ticket": "t"},
            on_print_job=lambda j: None,
            on_job_canceled=lambda jid: canceled.append(jid),
        )
        sess._dispatch_message({"type": "job_canceled", "job_id": "abc"})
        self.assertEqual(canceled, ["abc"])


class TestAgentCableHooks(unittest.TestCase):
    def test_hooks_prefer_cable_when_subscribed(self):
        client = mock.Mock()
        sess = mock.Mock()
        sess.perform.return_value = True
        ack, report = agent_mod.cloud_job_hooks(
            client, "tok", cable_session=sess
        )
        job = jobs.PrintJob(
            id="j1",
            cups_name="P",
            content_type="png_base64",
            content="AA==",
        )
        ack(job)
        report(job, "done", None)
        sess.perform.assert_any_call("ack_job", job_id="j1")
        sess.perform.assert_any_call(
            "job_state", job_id="j1", state="done", message=None
        )
        client.ack_job.assert_not_called()
        client.report_job_state.assert_not_called()

    def test_hooks_fallback_rest(self):
        client = mock.Mock()
        sess = mock.Mock()
        sess.perform.return_value = False
        ack, report = agent_mod.cloud_job_hooks(
            client, "tok", cable_session=sess
        )
        job = jobs.PrintJob(
            id="j1",
            cups_name="P",
            content_type="png_base64",
            content="AA==",
        )
        ack(job)
        report(job, "error", "nope")
        client.ack_job.assert_called_once_with("tok", "j1")
        client.report_job_state.assert_called_once_with(
            "tok", "j1", "error", message="nope"
        )

    def test_handle_job_canceled(self):
        with tempfile.TemporaryDirectory() as td:
            store = JobStore(
                queue_dir=Path(td) / "q", processed_dir=Path(td) / "p"
            )
            store.ensure()
            job = jobs.PrintJob(
                id="c1",
                cups_name="P",
                content_type="local_path",
                content="/tmp/x",
            )
            store.write_queue(job)
            agent_mod.handle_job_canceled("c1", store)
            self.assertFalse(store.has_queue_file("c1"))
            self.assertTrue(store.is_processed("c1"))


class TestConfigCable(unittest.TestCase):
    def test_cable_enabled_default(self):
        cfg = Config(api_base_url="https://example.test", cable_url="")
        self.assertTrue(cfg.cable_enabled)
        self.assertTrue(cfg.cable_url.endswith("/print/cable"))


class TestSessionStartNoDeadlock(unittest.TestCase):
    def test_start_does_not_deadlock_without_ws(self):
        """Regression: start() used to call stop() under a non-reentrant Lock."""
        calls = []

        def get_ticket():
            calls.append("ticket")
            return {"ticket": "t", "expires_in": 60, "cable_path": "/print/cable"}

        sess = cable.PrintCableSession(
            cable_url="wss://example/print/cable",
            get_ticket=get_ticket,
            on_print_job=lambda j: None,
        )
        # Even if websocket is missing, start must return (not hang).
        with mock.patch.object(cable, "_HAS_WS", False):
            ok = sess.start()
        self.assertFalse(ok)
        # With WS available but client start mocked, must still return promptly.
        with mock.patch.object(cable, "_HAS_WS", True):
            with mock.patch.object(
                cable.ActionCableClient, "start", lambda self: None
            ):
                ok = sess.start()
        self.assertTrue(ok)
        self.assertEqual(calls, ["ticket"])
        sess.stop()


if __name__ == "__main__":
    unittest.main()
