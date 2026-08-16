"""Tests for LCD stream stats collection (no HTTP server / framebuffer)."""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

import stream_lcd


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

            with mock.patch(
                "printers.inventory_payload",
                return_value=[
                    {
                        "cups_name": "Zebra_ZD",
                        "display_name": "Zebra ZD421",
                        "status": "idle",
                        "status_message": None,
                        "uri": "socket://1.2.3.4:9100",
                        "supports_raw": True,
                    }
                ],
            ):
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
            self.assertEqual(data["jobs"]["queued"], 1)
            self.assertEqual(data["jobs"]["processed"], 2)
            self.assertEqual(data["paths"]["credentials_present"], False)
            self.assertEqual(data["paths"]["api_base_url"], "https://example.test")
            self.assertEqual(len(data["printers"]), 1)
            self.assertEqual(data["printers"][0]["display_name"], "Zebra ZD421")
            self.assertTrue(data["printers"][0]["supports_raw"])
            self.assertEqual(data["printers"][0]["test_formats"], ["pdf", "zpl"])
            self.assertIn("format=pdf", data["printers"][0]["test_print"]["pdf"])
            self.assertIn("format=zpl", data["printers"][0]["test_print"]["zpl"])
            self.assertIn("hostname", data["system"])
            self.assertIn("collected_at", data)

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


class ClaimCodeTests(unittest.TestCase):
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

    def test_claim_rejects_wrong_length(self):
        with self.assertRaises(stream_lcd.ClaimError) as cm:
            stream_lcd.claim_device("ABC")
        self.assertEqual(cm.exception.status, 400)

    def test_claim_success_saves_and_returns_public_fields(self):
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            cfg = SimpleNamespace(
                api_base_url="https://example.test",
                config_path=root / "config.json",
                credentials_path=root / "credentials.json",
                status_path=root / "status.json",
                config_dir=root,
                state_dir=root,
                ensure_dirs=lambda: None,
            )
            pair_resp = {
                "device_token": "secret-token-do-not-return",
                "node_id": "node-1",
                "name": "Pack 1",
                "organization_name": "Acme",
                "warehouse_name": "North",
            }
            fake_creds = SimpleNamespace(
                node_id="node-1",
                name="Pack 1",
                organization_name="Acme",
                warehouse_label=lambda: "North",
            )

            with mock.patch("auth.load_credentials", return_value=None), mock.patch(
                "auth.credentials_from_pair_response", return_value=fake_creds
            ), mock.patch("auth.save_credentials") as save, mock.patch(
                "statusio.write_status"
            ) as write_st, mock.patch(
                "config.write_default_config"
            ), mock.patch(
                "cloud.CloudClient"
            ) as Client, mock.patch(
                "sysinfo.hostname", return_value="pi-test"
            ):
                Client.return_value.claim.return_value = pair_resp
                out = stream_lcd.claim_device(
                    "ab7k-2q9m", name="Pack 1", cfg=cfg
                )

            self.assertTrue(out["ok"])
            self.assertEqual(out["node_id"], "node-1")
            self.assertEqual(out["warehouse_name"], "North")
            self.assertNotIn("device_token", out)
            Client.return_value.claim.assert_called_once()
            call_kw = Client.return_value.claim.call_args
            self.assertEqual(call_kw[0][0], "AB7K2Q9M")
            save.assert_called_once()
            write_st.assert_called_once()


class TestPrintApiTests(unittest.TestCase):
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

        def submit(cups, fmt, *, wait_cups=False):
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

    def test_html_page_formats(self):
        html = stream_lcd.HTML_PAGE.format(w=480, h=320, fps=2, css_max=960)
        self.assertIn("/api/test-print", html)
        self.assertIn("function printerRow", html)


if __name__ == "__main__":
    unittest.main()
