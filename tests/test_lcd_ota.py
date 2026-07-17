"""LCD OTA messaging + agent version helpers (no framebuffer / Pillow)."""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

import display_status as disp
import update as update_mod


class TestOtaDisplayMessage(unittest.TestCase):
    def test_idle_none(self):
        self.assertIsNone(disp.ota_display_message(None))
        st = update_mod.UpdateStatus(status=update_mod.STATUS_IDLE)
        self.assertIsNone(disp.ota_display_message(st))

    def test_progress_labels(self):
        cases = [
            (update_mod.STATUS_DOWNLOADING, "Updating 0.4.0…"),
            (update_mod.STATUS_INSTALLING, "Installing 0.4.0…"),
            (update_mod.STATUS_PENDING_HEALTH, "Verifying 0.4.0…"),
        ]
        for status, expected in cases:
            st = update_mod.UpdateStatus(
                status=status, target_version="0.4.0", current_version="0.3.0"
            )
            out = disp.ota_display_message(st)
            assert out is not None
            label, color = out
            self.assertEqual(label, expected)
            self.assertEqual(color, disp.WARN)

    def test_failed_and_rolled_back(self):
        failed = update_mod.UpdateStatus(
            status=update_mod.STATUS_FAILED,
            last_error="network error",
        )
        out = disp.ota_display_message(failed)
        assert out is not None
        self.assertEqual(out[0], "Update failed")
        self.assertEqual(out[1], disp.DOWN)

        rolled = update_mod.UpdateStatus(status=update_mod.STATUS_ROLLED_BACK)
        out = disp.ota_display_message(rolled)
        assert out is not None
        self.assertEqual(out[0], "Rolled back")
        self.assertEqual(out[1], disp.WARN)

    def test_failed_after_activate_shows_verifying_not_failed(self):
        """Self-restart SIGTERM leaves status=failed; LCD must not say Update failed."""
        st = update_mod.UpdateStatus(
            status=update_mod.STATUS_FAILED,
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
        st = update_mod.UpdateStatus(status=update_mod.STATUS_DOWNLOADING)
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


if __name__ == "__main__":
    unittest.main()
