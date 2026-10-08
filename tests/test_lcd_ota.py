"""LCD OTA messaging, update_status.json reader and version helpers (no Pillow)."""

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

import display_status as disp
from config import AGENT_VERSION


class TestOtaDisplayMessage(unittest.TestCase):
    def test_idle_none(self):
        self.assertIsNone(disp.ota_display_message(None))
        st = disp.UpdateStatus(status=disp.STATUS_IDLE)
        self.assertIsNone(disp.ota_display_message(st))

    def test_progress_labels(self):
        cases = [
            (disp.STATUS_DOWNLOADING, "Updating 0.4.0…"),
            (disp.STATUS_INSTALLING, "Installing 0.4.0…"),
            (disp.STATUS_PENDING_HEALTH, "Verifying 0.4.0…"),
        ]
        for status, expected in cases:
            st = disp.UpdateStatus(
                status=status, target_version="0.4.0", current_version="0.3.0"
            )
            out = disp.ota_display_message(st)
            assert out is not None
            label, color = out
            self.assertEqual(label, expected)
            self.assertEqual(color, disp.WARN)

    def test_failed_and_rolled_back(self):
        failed = disp.UpdateStatus(
            status=disp.STATUS_FAILED,
            last_error="network error",
        )
        out = disp.ota_display_message(failed)
        assert out is not None
        self.assertEqual(out[0], "Update failed")
        self.assertEqual(out[1], disp.DOWN)

        rolled = disp.UpdateStatus(status=disp.STATUS_ROLLED_BACK)
        out = disp.ota_display_message(rolled)
        assert out is not None
        self.assertEqual(out[0], "Rolled back")
        self.assertEqual(out[1], disp.WARN)

    def test_failed_after_activate_shows_verifying_not_failed(self):
        """Self-restart SIGTERM leaves status=failed; LCD must not say Update failed."""
        st = disp.UpdateStatus(
            status=disp.STATUS_FAILED,
            current_version="0.3.1",
            target_version="0.3.1",
            previous_version="0.3.0",
            last_error=(
                "Command '['sudo', '-n', '/usr/local/lib/vesyl-print/apply-update', "
                "'restart']' died with <Signals.SIGTERM: 15>."
            ),
        )
        out = disp.ota_display_message(st)
        assert out is not None
        self.assertEqual(out[0], "Verifying 0.3.1…")
        self.assertEqual(out[1], disp.WARN)

    def test_no_target_version(self):
        st = disp.UpdateStatus(status=disp.STATUS_DOWNLOADING)
        out = disp.ota_display_message(st)
        assert out is not None
        self.assertEqual(out[0], "Updating…")


class TestFormatAgentVersion(unittest.TestCase):
    def test_adds_v_prefix(self):
        self.assertEqual(disp.format_agent_version("0.3.0"), "v0.3.0")
        self.assertEqual(disp.format_agent_version("v0.4.0"), "v0.4.0")

    def test_empty_falls_back(self):
        v = disp.format_agent_version(None)
        self.assertTrue(v.startswith("v") or v == "")


class TestStatusValues(unittest.TestCase):
    def test_match_the_agent_strings(self):
        self.assertEqual(
            (
                disp.STATUS_IDLE,
                disp.STATUS_DOWNLOADING,
                disp.STATUS_INSTALLING,
                disp.STATUS_PENDING_HEALTH,
                disp.STATUS_FAILED,
                disp.STATUS_ROLLED_BACK,
            ),
            ("idle", "downloading", "installing", "pending_health", "failed", "rolled_back"),
        )


class TestReadUpdateStatus(unittest.TestCase):
    def setUp(self):
        td = tempfile.TemporaryDirectory()
        self.addCleanup(td.cleanup)
        self.path = Path(td.name) / "update_status.json"

    def _write(self, data) -> None:
        text = data if isinstance(data, str) else json.dumps(data)
        self.path.write_text(text, encoding="utf-8")

    def test_missing_or_unreadable_is_none(self):
        self.assertIsNone(disp.read_update_status(self.path))
        self._write("{not json")
        self.assertIsNone(disp.read_update_status(self.path))
        self._write([1, 2])
        self.assertIsNone(disp.read_update_status(self.path))
        self.path.write_bytes(b'{"status": "\xff"}')
        self.assertIsNone(disp.read_update_status(self.path))

    def test_defaults(self):
        self._write({})
        st = disp.read_update_status(self.path)
        self.assertEqual(st.status, "idle")
        self.assertEqual(st.current_version, disp.package_version())
        self.assertIsNone(st.target_version)
        self.assertIsNone(st.last_error)

        self._write({"status": "", "current_version": None, "health_attempts": "x"})
        st = disp.read_update_status(str(self.path))
        self.assertEqual(st.status, "idle")
        self.assertEqual(st.current_version, disp.package_version())

    def test_fields_passed_through(self):
        self._write(
            {
                "status": "failed",
                "current_version": "0.5.0",
                "target_version": "0.5.1",
                "last_error": "bad signature",
                "last_checked_at": "2026-10-08T12:00:00+00:00",
                "channel": "beta",
                "previous_version": "0.4.9",
                "health_deadline_at": None,
                "health_attempts": 2,
            }
        )
        st = disp.read_update_status(self.path)
        self.assertEqual(
            st,
            disp.UpdateStatus(
                status="failed",
                current_version="0.5.0",
                target_version="0.5.1",
                last_error="bad signature",
                last_checked_at="2026-10-08T12:00:00+00:00",
                channel="beta",
                previous_version="0.4.9",
            ),
        )
        label, color = disp.ota_display_message(st)
        self.assertEqual((label, color), ("Update failed", disp.DOWN))


class TestVersionHelpers(unittest.TestCase):
    def test_version_cmp(self):
        self.assertEqual(disp.version_cmp("0.3.1", "0.3.1"), 0)
        self.assertEqual(disp.version_cmp("0.3.10", "0.3.9"), 1)
        self.assertEqual(disp.version_cmp("0.3.9", "0.4.0"), -1)
        self.assertEqual(disp.version_cmp("0.4", "0.4.0"), 0)
        self.assertEqual(disp.version_cmp("1.0.0-rc1", "1.0.0"), 0)
        self.assertEqual(disp.version_cmp("1.0.0+build", "1.0.0"), 0)
        self.assertEqual(disp.version_cmp("x.1", "0.1"), 0)

    def test_package_version_reads_slot_version_file(self):
        expected = (ROOT / "VERSION").read_text(encoding="utf-8").strip()
        self.assertEqual(disp.SLOT_DIR, ROOT)
        self.assertEqual(disp.package_version(), expected)

    def test_package_version_falls_back_to_agent_version(self):
        with tempfile.TemporaryDirectory() as td:
            with mock.patch.object(disp, "SLOT_DIR", Path(td)):
                self.assertEqual(disp.package_version(), AGENT_VERSION)
                (Path(td) / "VERSION").write_text("  \n", encoding="utf-8")
                self.assertEqual(disp.package_version(), AGENT_VERSION)
                (Path(td) / "VERSION").write_text("9.9.9\n", encoding="utf-8")
                self.assertEqual(disp.package_version(), "9.9.9")

    def test_binary_sits_next_to_main(self):
        self.assertEqual(disp.VESYL_PRINT_BIN, ROOT / "vesyl-print")


if __name__ == "__main__":
    unittest.main()
