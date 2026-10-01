"""vesyl-print queues: list configured CUPS queues without touching CUPS."""

from __future__ import annotations

import contextlib
import io
import json
import sys
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

import cli


def _sample() -> list[dict]:
    return [
        {
            "cups_name": "Zebra_ZD220",
            "display_name": "Zebra ZD220-203dpi ZPL",
            "uri": "socket://192.168.1.50:9100",
            "status": "idle",
            "status_reasons": [],
            "status_message": None,
            "supports_raw": True,
        },
        {
            "cups_name": "Brother_HL",
            "display_name": "Brother HL-L3280CDW",
            "uri": "ipp://brother.local/ipp/print",
            "status": "stopped",
            "status_reasons": ["media-empty"],
            "status_message": "Out of paper",
            "supports_raw": False,
        },
    ]


class TestFormatQueues(unittest.TestCase):
    def test_lists_name_status_and_formats(self):
        text = cli.format_queues(_sample())
        self.assertIn("Zebra_ZD220", text)
        self.assertIn("display:  Zebra ZD220-203dpi ZPL", text)
        self.assertIn("status:   idle", text)
        self.assertIn("raw:      yes", text)
        self.assertIn("formats:  pdf, zpl", text)
        self.assertIn("socket://192.168.1.50:9100", text)
        self.assertIn("Brother_HL", text)
        self.assertIn("status:   stopped (Out of paper)", text)
        self.assertIn("raw:      no", text)
        self.assertIn("formats:  pdf", text)
        self.assertNotIn("zpl", text.split("Brother_HL", 1)[1])

    def test_empty(self):
        self.assertEqual(cli.format_queues([]), "No CUPS queues configured.")
        self.assertEqual(cli.format_queues(None), "No CUPS queues configured.")


class TestQueuesCommand(unittest.TestCase):
    def test_json(self):
        buf = io.StringIO()
        with mock.patch("printers.inventory_payload", return_value=_sample()):
            with contextlib.redirect_stdout(buf):
                rc = cli.main(["queues", "--json"])
        self.assertEqual(rc, 0)
        data = json.loads(buf.getvalue())
        self.assertEqual([row["cups_name"] for row in data], ["Zebra_ZD220", "Brother_HL"])
        self.assertEqual(data[0]["test_formats"], ["pdf", "zpl"])
        self.assertEqual(data[1]["test_formats"], ["pdf"])
        self.assertTrue(data[0]["supports_raw"])
        self.assertFalse(data[1]["supports_raw"])

    def test_human(self):
        buf = io.StringIO()
        with mock.patch("printers.inventory_payload", return_value=_sample()):
            with contextlib.redirect_stdout(buf):
                rc = cli.main(["queues"])
        self.assertEqual(rc, 0)
        self.assertEqual(buf.getvalue().rstrip("\n"), cli.format_queues(_sample()))

    def test_inventory_error(self):
        with mock.patch(
            "printers.inventory_payload", side_effect=RuntimeError("lpstat missing")
        ):
            with self.assertRaises(SystemExit) as cm:
                cli.main(["queues"])
        self.assertEqual(cm.exception.code, 1)


if __name__ == "__main__":
    unittest.main()
