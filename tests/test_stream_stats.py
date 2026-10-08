"""Tests for the LCD stream page: stats, claim and test print (no framebuffer).

Printers come from the agent's printers.json; the claim and the test print
run a fake ``vesyl-print`` binary (a shell script on a temp path).
"""

from __future__ import annotations

import json
import os
import sys
import tempfile
import threading
import unittest
from datetime import datetime, timedelta, timezone
from http.client import HTTPConnection
from http.server import ThreadingHTTPServer
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
for p in (ROOT, Path(__file__).resolve().parent):
    if str(p) not in sys.path:
        sys.path.insert(0, str(p))

import display_status
import stream_lcd
from config import AGENT_VERSION
from fakecli import make_fake_cli, recorded_args, recorded_env

ZEBRA = {
    "cups_name": "Zebra_ZD",
    "display_name": "Zebra ZD421",
    "status": "idle",
    "status_reasons": [],
    "status_message": None,
    "uri": "socket://1.2.3.4:9100",
    "supports_raw": True,
}
HP = {
    "cups_name": "HP_OfficeJet",
    "display_name": "HP OfficeJet Pro",
    "status": "stopped",
    "status_reasons": ["media-empty-error"],
    "status_message": "Out of paper",
    "uri": "ipp://1.2.3.5/ipp/print",
    "supports_raw": False,
}


def write_printers(path: Path, printers, *, age_s: float = 5.0) -> None:
    ts = datetime.now(timezone.utc) - timedelta(seconds=age_s)
    path.write_text(
        json.dumps(
            {"updated_at": ts.replace(microsecond=0).isoformat(), "printers": printers}
        ),
        encoding="utf-8",
    )


class CollectStatsTests(unittest.TestCase):
    def test_pairing_and_jobs_from_files(self):
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            status = root / "status.json"
            status.write_text(
                json.dumps(
                    {
                        "pairing": "paired",
                        "cloud": "online",
                        "node_id": "n1",
                        "name": "pack-01",
                        "organization_name": "Acme",
                        "warehouse_name": "North",
                        "last_heartbeat_at": "2026-07-17T12:00:00+00:00",
                        "agent_version": "0.3.8",
                    }
                ),
                encoding="utf-8",
            )
            q = root / "queue"
            q.mkdir()
            (q / "job1.json").write_text("{}", encoding="utf-8")
            p = root / "processed"
            p.mkdir()
            (p / "old.json").write_text("{}", encoding="utf-8")
            (p / "older.json").write_text("{}", encoding="utf-8")
            write_printers(root / "printers.json", [ZEBRA])

            data = stream_lcd.collect_stats(
                status_path=status,
                queue_dir=q,
                processed_dir=p,
                credentials_path=root / "credentials.json",
                config_dir=root / "etc",
                state_dir=root,
                api_base_url="https://example.test",
                include_printers=True,
            )

            self.assertEqual(data["pairing"]["pairing"], "paired")
            self.assertFalse(data["pairing"]["needs_claim"])
            self.assertEqual(data["pairing"]["cloud"], "online")
            self.assertEqual(data["pairing"]["name"], "pack-01")
            self.assertEqual(data["pairing"]["organization_name"], "Acme")
            self.assertEqual(data["pairing"]["agent_version"], "v0.3.8")
            self.assertEqual(data["jobs"]["queued"], 1)
            self.assertEqual(data["jobs"]["processed"], 2)
            self.assertEqual(data["paths"]["credentials_present"], False)
            self.assertEqual(data["paths"]["api_base_url"], "https://example.test")
            self.assertEqual(
                data["printers"],
                [
                    {
                        "cups_name": "Zebra_ZD",
                        "display_name": "Zebra ZD421",
                        "status": "idle",
                        "status_message": None,
                        "uri": "socket://1.2.3.4:9100",
                        "supports_raw": True,
                        "test_formats": ["pdf", "zpl"],
                        "test_print": {
                            "pdf": "/api/test-print?cups_name=Zebra_ZD&format=pdf",
                            "zpl": "/api/test-print?cups_name=Zebra_ZD&format=zpl",
                        },
                    }
                ],
            )
            self.assertIn("hostname", data["system"])
            self.assertIn("collected_at", data)

    def test_printers_path_overrides_state_dir(self):
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            write_printers(root / "printers.json", [ZEBRA])
            other = root / "elsewhere.json"
            write_printers(other, [HP])
            data = stream_lcd.collect_stats(state_dir=root, printers_path=other)
        self.assertEqual([p["cups_name"] for p in data["printers"]], ["HP_OfficeJet"])
        self.assertEqual(data["printers"][0]["status_message"], "Out of paper")
        self.assertEqual(data["printers"][0]["test_formats"], ["pdf"])

    def test_stale_printers_read_unknown(self):
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            write_printers(root / "printers.json", [ZEBRA, HP], age_s=600)
            data = stream_lcd.collect_stats(state_dir=root)
        self.assertEqual(
            [(p["cups_name"], p["status"], p["status_message"]) for p in data["printers"]],
            [("Zebra_ZD", "unknown", None), ("HP_OfficeJet", "unknown", None)],
        )
        self.assertEqual(data["printers"][0]["test_formats"], ["pdf", "zpl"])

    def test_missing_or_corrupt_printers_json_lists_none(self):
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            self.assertEqual(stream_lcd.collect_stats(state_dir=root)["printers"], [])
            (root / "printers.json").write_text("{\"printers\": ", encoding="utf-8")
            self.assertEqual(stream_lcd.collect_stats(state_dir=root)["printers"], [])
            write_printers(root / "printers.json", [ZEBRA])
            data = stream_lcd.collect_stats(state_dir=root, include_printers=False)
            self.assertEqual(data["printers"], [])

    def test_update_block_from_update_status_json(self):
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            ust = root / "update_status.json"
            missing = stream_lcd.collect_stats(update_status_path=ust)["update"]
            self.assertEqual(missing["status"], "idle")
            self.assertEqual(missing["current_version"], f"v{AGENT_VERSION}")
            ust.write_text(
                json.dumps(
                    {
                        "status": "pending_health",
                        "current_version": "0.5.1",
                        "target_version": "0.5.1",
                        "channel": "stable",
                        "previous_version": "0.5.0",
                        "last_checked_at": "2026-10-08T12:00:00+00:00",
                    }
                ),
                encoding="utf-8",
            )
            upd = stream_lcd.collect_stats(update_status_path=ust)["update"]
        self.assertEqual(
            upd,
            {
                "status": "pending_health",
                "current_version": "0.5.1",
                "target_version": "0.5.1",
                "channel": "stable",
                "last_error": None,
                "last_checked_at": "2026-10-08T12:00:00+00:00",
                "previous_version": "0.5.0",
            },
        )

    def test_unpaired_defaults(self):
        with tempfile.TemporaryDirectory() as td:
            data = stream_lcd.collect_stats(
                status_path=Path(td) / "missing.json",
                include_printers=False,
            )
        self.assertEqual(data["pairing"]["pairing"], "unpaired")
        self.assertTrue(data["pairing"]["needs_claim"])
        self.assertEqual(data["jobs"]["queued"], 0)
        self.assertEqual(data["printers"], [])

    def test_stats_provider_cache(self):
        calls = {"n": 0}

        def coll():
            calls["n"] += 1
            return {
                "n": calls["n"],
                "pairing": {},
                "system": {},
                "jobs": {},
                "printers": [],
                "update": {},
                "paths": {},
            }

        sp = stream_lcd.StatsProvider(coll, cache_s=60.0)
        a = sp.get()
        b = sp.get()
        self.assertEqual(a["n"], 1)
        self.assertEqual(b["n"], 1)
        self.assertEqual(calls["n"], 1)
        sp.invalidate()
        c = sp.get()
        self.assertEqual(c["n"], 2)

    def test_html_contains_claim_form_and_layout(self):
        # Page column is top-aligned (stretch), not vertically centered.
        self.assertIn("align-items: stretch", stream_lcd.HTML_PAGE)
        self.assertIn(".page {{", stream_lcd.HTML_PAGE)
        self.assertIn("/api/stats", stream_lcd.HTML_PAGE)
        self.assertIn("/api/claim", stream_lcd.HTML_PAGE)
        self.assertIn("/api/test-print", stream_lcd.HTML_PAGE)
        self.assertIn("test-print", stream_lcd.HTML_PAGE)
        self.assertIn("printerRow", stream_lcd.HTML_PAGE)
        self.assertIn("id=\"claim-panel\"", stream_lcd.HTML_PAGE)
        self.assertIn("code-box", stream_lcd.HTML_PAGE)
        self.assertIn("code-dash", stream_lcd.HTML_PAGE)
        self.assertIn("normalizePasted", stream_lcd.HTML_PAGE)
        self.assertIn("/assets/logo.svg", stream_lcd.HTML_PAGE)


class FakeCliCase(unittest.TestCase):
    """Temp dir holding a fake ``vesyl-print`` that display_status will run."""

    def setUp(self):
        td = tempfile.TemporaryDirectory()
        self.addCleanup(td.cleanup)
        self.tmp = Path(td.name)
        self.bin_dir = self.tmp / "slot"
        self.bin_dir.mkdir()
        self.exe = self.bin_dir / "vesyl-print"
        patcher = mock.patch.object(display_status, "VESYL_PRINT_BIN", self.exe)
        patcher.start()
        self.addCleanup(patcher.stop)

    def fake(self, stdout, **kw) -> Path:
        return make_fake_cli(self.bin_dir, stdout, **kw)

    def ran_with(self):
        return recorded_args(self.bin_dir)


class ClaimCodeTests(FakeCliCase):
    def setUp(self):
        super().setUp()
        self.cfg = SimpleNamespace(
            api_base_url="https://example.test",
            config_dir=self.tmp / "etc",
            state_dir=self.tmp / "state",
            credentials_path=self.tmp / "etc" / "credentials.json",
            status_path=self.tmp / "state" / "status.json",
        )
        self.cfg.config_dir.mkdir()
        self.cfg.state_dir.mkdir()

    def claim(self, code="ab7k-2q9m", **kw):
        return stream_lcd.claim_device(code, cfg=self.cfg, **kw)

    def assert_claim_error(self, status: int, message: str, **kw):
        with self.assertRaises(stream_lcd.ClaimError) as cm:
            self.claim(**kw)
        self.assertEqual((cm.exception.status, cm.exception.message), (status, message))

    def test_normalize_strips_dashes_and_spaces(self):
        self.assertEqual(
            stream_lcd.normalize_claim_code("ab7k-2q9m"),
            "AB7K2Q9M",
        )
        self.assertEqual(
            stream_lcd.normalize_claim_code(" ab 7k - 2q 9m "),
            "AB7K2Q9M",
        )
        self.assertEqual(stream_lcd.normalize_claim_code(None), "")

    def test_claim_rejects_wrong_length_without_running_cli(self):
        self.fake({"ok": True})
        with self.assertRaises(stream_lcd.ClaimError) as cm:
            stream_lcd.claim_device("ABC")
        self.assertEqual(cm.exception.status, 400)
        self.assertIn("got 3", cm.exception.message)
        self.assert_claim_error(
            400,
            "Claim code must be 8 characters (got 9 after removing dashes/spaces)",
            code="AB7K-2Q9M-X",
        )
        self.assertIsNone(self.ran_with())

    def test_claim_success_runs_cli_and_returns_public_fields(self):
        self.fake(
            {
                "ok": True,
                "node_id": "node-1",
                "name": "Pack 1",
                "organization_name": "Acme",
                "warehouse_name": "North",
                "device_token": "secret-token-do-not-return",
            }
        )
        out = self.claim(name="  Pack 1 ")
        self.assertEqual(
            out,
            {
                "ok": True,
                "node_id": "node-1",
                "name": "Pack 1",
                "organization_name": "Acme",
                "warehouse_name": "North",
            },
        )
        self.assertEqual(self.ran_with(), ["claim", "AB7K2Q9M", "--name", "Pack 1", "--json"])
        # The CLI writes into the directories the 409 check looked at.
        self.assertEqual(
            recorded_env(self.bin_dir), [str(self.cfg.config_dir), str(self.cfg.state_dir)]
        )

    def test_claim_name_optional_and_dash_safe(self):
        self.fake({"ok": True, "node_id": "n", "name": None,
                   "organization_name": None, "warehouse_name": "—"})
        out = self.claim()
        self.assertEqual(out["warehouse_name"], "—")
        self.assertIsNone(out["name"])
        self.assertEqual(self.ran_with(), ["claim", "AB7K2Q9M", "--json"])
        self.claim(name="   ")
        self.assertEqual(self.ran_with(), ["claim", "AB7K2Q9M", "--json"])
        self.claim(name="-dock 3")
        self.assertEqual(self.ran_with(), ["claim", "AB7K2Q9M", "--name=-dock 3", "--json"])

    def test_cli_error_keeps_message_and_status(self):
        self.fake(
            {"ok": False, "error": "Unknown claim code", "status": 422, "code": "invalid_code"},
            rc=1,
        )
        self.assert_claim_error(422, "Unknown claim code")

    def test_transport_error_and_bad_status_become_502(self):
        self.fake({"ok": False, "error": "network error: refused", "status": 0, "code": None}, rc=1)
        self.assert_claim_error(502, "network error: refused")
        self.fake({"ok": False, "error": "", "status": None}, rc=1)
        self.assert_claim_error(502, "Claim failed")
        # A failed claim never answers with a success status.
        self.fake({"ok": False, "error": "invalid JSON response (HTTP 200)", "status": 200}, rc=1)
        self.assert_claim_error(502, "invalid JSON response (HTTP 200)")

    def test_cli_unavailable_or_without_json_is_502(self):
        self.assert_claim_error(502, "vesyl-print not found")
        self.fake("", stderr="error: unrecognized subcommand 'claim'\n\nUsage: vesyl-print\n", rc=2)
        self.assert_claim_error(502, "error: unrecognized subcommand 'claim'")
        self.fake("Paired successfully.\n", rc=0)
        self.assert_claim_error(502, "vesyl-print exited with status 0")

    def test_claim_timeout_is_502(self):
        self.fake({"ok": True}, sleep=10)
        with mock.patch.object(stream_lcd, "_CLAIM_TIMEOUT_S", 0.5):
            self.assert_claim_error(502, "timed out after 0.5s")

    def test_already_claimed_is_409_without_running_cli(self):
        self.fake({"ok": True, "node_id": "n"})
        self.cfg.credentials_path.write_text("{}", encoding="utf-8")
        self.cfg.status_path.write_text(json.dumps({"pairing": "paired"}), encoding="utf-8")
        self.assert_claim_error(409, "This node is already claimed")
        self.assertIsNone(self.ran_with())

    def test_credentials_without_paired_status_can_reclaim(self):
        self.fake({"ok": True, "node_id": "n2"})
        self.cfg.credentials_path.write_text("{}", encoding="utf-8")
        self.cfg.status_path.write_text(json.dumps({"pairing": "revoked"}), encoding="utf-8")
        self.assertEqual(self.claim()["node_id"], "n2")
        # paired status but no credentials file: not claimed either
        self.cfg.credentials_path.unlink()
        self.cfg.status_path.write_text(json.dumps({"pairing": "paired"}), encoding="utf-8")
        self.assertEqual(self.claim()["node_id"], "n2")


class TestPrintApiTests(FakeCliCase):
    _INV = [
        {
            "cups_name": "Zebra_ZD220-203dpi_ZPL",
            "display_name": "Zebra ZD220-203dpi ZPL",
            "supports_raw": True,
        },
        {
            "cups_name": "HP_OfficeJet_Pro_9010",
            "display_name": "HP OfficeJet Pro 9010 series",
            "supports_raw": False,
        },
    ]

    def test_href_encodes_queue(self):
        href = stream_lcd.test_print_href("Zebra ZD/raw", "pdf")
        self.assertTrue(href.startswith("/api/test-print?"))
        self.assertIn("format=pdf", href)
        self.assertIn("Zebra", href)

    def test_run_pdf_and_zpl_on_raw(self):
        seen: list[tuple[str, str]] = []

        def submit(cups, fmt):
            seen.append((cups, fmt))
            return "delivered"

        out = stream_lcd.run_test_print(
            "Zebra_ZD220-203dpi_ZPL",
            "zpl",
            inventory=self._INV,
            submit=submit,
        )
        self.assertTrue(out["ok"])
        self.assertEqual(out["format"], "zpl")
        self.assertEqual(seen, [("Zebra_ZD220-203dpi_ZPL", "zpl")])

        stream_lcd.run_test_print(
            "Zebra ZD220-203dpi ZPL",
            "pdf",
            inventory=self._INV,
            submit=submit,
        )
        self.assertEqual(seen[-1], ("Zebra_ZD220-203dpi_ZPL", "pdf"))

    def test_zpl_rejected_on_non_raw(self):
        with self.assertRaises(stream_lcd.TestPrintError) as cm:
            stream_lcd.run_test_print(
                "HP_OfficeJet_Pro_9010",
                "zpl",
                inventory=self._INV,
                submit=lambda *a, **k: "nope",
            )
        self.assertEqual(cm.exception.status, 400)

    def test_unknown_printer(self):
        with self.assertRaises(stream_lcd.TestPrintError) as cm:
            stream_lcd.run_test_print(
                "NoSuch", "pdf", inventory=self._INV, submit=lambda *a, **k: "x"
            )
        self.assertEqual(cm.exception.status, 404)

    def test_resolves_from_printers_json_and_runs_cli(self):
        printers = self.tmp / "printers.json"
        write_printers(printers, self._INV)
        self.fake(
            {"ok": True, "state": "delivered", "job_id": "3f1c", "queue": "x", "format": "zpl"}
        )
        out = stream_lcd.run_test_print(
            "Zebra ZD220-203dpi ZPL", "ZPL", printers_path=printers
        )
        self.assertEqual(
            out,
            {
                "ok": True,
                "cups_name": "Zebra_ZD220-203dpi_ZPL",
                "display_name": "Zebra ZD220-203dpi ZPL",
                "format": "zpl",
                "result": "delivered",
            },
        )
        self.assertEqual(
            self.ran_with(),
            ["test-print", "--queue", "Zebra_ZD220-203dpi_ZPL", "--format", "zpl", "--json"],
        )

    def test_stale_snapshot_still_names_the_queues(self):
        printers = self.tmp / "printers.json"
        write_printers(printers, self._INV, age_s=3600)
        self.fake({"ok": True, "state": "delivered"})
        out = stream_lcd.run_test_print("HP_OfficeJet_Pro_9010", "pdf", printers_path=printers)
        self.assertEqual(out["result"], "delivered")
        with self.assertRaises(stream_lcd.TestPrintError) as cm:
            stream_lcd.run_test_print("HP_OfficeJet_Pro_9010", "zpl", printers_path=printers)
        self.assertEqual(cm.exception.status, 400)

    def test_inventory_unavailable_is_503(self):
        printers = self.tmp / "printers.json"
        for content in (None, "{\"printers\": ", "[]"):
            if content is not None:
                printers.write_text(content, encoding="utf-8")
            with self.assertRaises(stream_lcd.TestPrintError) as cm:
                stream_lcd.run_test_print("Zebra_ZD220-203dpi_ZPL", "pdf", printers_path=printers)
            self.assertEqual(cm.exception.status, 503)
            self.assertTrue(
                cm.exception.message.startswith("printer inventory unavailable: "),
                cm.exception.message,
            )
        self.assertIsNone(self.ran_with())

    def test_default_printers_path_comes_from_config(self):
        state = self.tmp / "state"
        state.mkdir()
        write_printers(state / "printers.json", self._INV)
        self.fake({"ok": True, "state": "delivered"})
        env = {"VESYL_PRINT_STATE_DIR": str(state), "VESYL_PRINT_CONFIG_DIR": str(self.tmp)}
        with mock.patch.dict(os.environ, env):
            out = stream_lcd.run_test_print("HP OfficeJet Pro 9010 series", "pdf")
        self.assertEqual(out["cups_name"], "HP_OfficeJet_Pro_9010")

    def test_cli_failure_is_500_with_its_message(self):
        printers = self.tmp / "printers.json"
        write_printers(printers, self._INV)
        self.fake(
            {"ok": False, "error": "lp failed: printer not found", "code": "lp_failed"}, rc=1
        )
        with self.assertRaises(stream_lcd.TestPrintError) as cm:
            stream_lcd.run_test_print("HP_OfficeJet_Pro_9010", "pdf", printers_path=printers)
        self.assertEqual((cm.exception.status, cm.exception.message),
                         (500, "lp failed: printer not found"))

    def test_html_page_formats(self):
        html = stream_lcd.HTML_PAGE.format(w=480, h=320, fps=2, css_max=960)
        self.assertIn("/api/test-print", html)
        self.assertIn("function printerRow", html)


class HttpEndpointTests(FakeCliCase):
    """POST /api/test-print and /api/claim through the real handler."""

    def setUp(self):
        super().setUp()
        self.state = self.tmp / "state"
        self.state.mkdir()
        self.etc = self.tmp / "etc"
        self.etc.mkdir()
        env = mock.patch.dict(
            os.environ,
            {"VESYL_PRINT_STATE_DIR": str(self.state), "VESYL_PRINT_CONFIG_DIR": str(self.etc)},
        )
        env.start()
        self.addCleanup(env.stop)
        self.stats = stream_lcd.StatsProvider(lambda: {"printers": []}, cache_s=60.0)
        handler = stream_lcd.make_handler(stream_lcd.FrameSource(), 2.0, self.stats)
        handler.log_message = lambda *a, **k: None
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)

    def post(self, path: str, body: dict) -> tuple[int, dict]:
        conn = HTTPConnection("127.0.0.1", self.server.server_address[1], timeout=10)
        try:
            raw = json.dumps(body).encode("utf-8")
            conn.request("POST", path, body=raw, headers={"Content-Type": "application/json"})
            resp = conn.getresponse()
            return resp.status, json.loads(resp.read().decode("utf-8"))
        finally:
            conn.close()

    def test_test_print_endpoint(self):
        write_printers(self.state / "printers.json", [ZEBRA])
        self.fake({"ok": True, "state": "delivered"})
        status, body = self.post("/api/test-print", {"cups_name": "Zebra_ZD", "format": "zpl"})
        self.assertEqual((status, body["result"], body["display_name"]),
                         (200, "delivered", "Zebra ZD421"))
        self.fake({"ok": False, "error": "lp failed: boom", "code": "lp_failed"}, rc=1)
        status, body = self.post("/api/test-print", {"cups_name": "Zebra_ZD", "format": "pdf"})
        self.assertEqual((status, body), (500, {"ok": False, "error": "lp failed: boom"}))
        (self.state / "printers.json").unlink()
        status, body = self.post("/api/test-print", {"cups_name": "Zebra_ZD", "format": "pdf"})
        self.assertEqual(status, 503)

    def test_claim_endpoint(self):
        self.fake({"ok": True, "node_id": "node-9", "name": "Dock", "organization_name": "Acme",
                   "warehouse_name": "North"})
        self.stats.get()
        status, body = self.post("/api/claim", {"code": ["AB7K", "2Q9M"], "name": "Dock"})
        self.assertEqual(status, 200)
        self.assertEqual(body["node_id"], "node-9")
        self.assertEqual(self.ran_with(), ["claim", "AB7K2Q9M", "--name", "Dock", "--json"])
        self.assertEqual(recorded_env(self.bin_dir), [str(self.etc), str(self.state)])
        self.assertIsNone(self.stats._cached)  # invalidated after a claim
        self.fake({"ok": False, "error": "Unknown claim code", "status": 422}, rc=1)
        status, body = self.post("/api/claim", {"code": "ZZZZ-ZZZZ"})
        self.assertEqual((status, body), (422, {"ok": False, "error": "Unknown claim code"}))
        status, body = self.post("/api/claim", {"code": "ZZZ"})
        self.assertEqual(status, 400)


if __name__ == "__main__":
    unittest.main()
