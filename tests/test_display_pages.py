"""Unit tests for LCD page navigation + display helpers (no framebuffer)."""

from __future__ import annotations

import struct
import tempfile
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path

import display_status as disp
import touch as touch_mod


class PageNavTests(unittest.TestCase):
    def test_normalize_page(self):
        self.assertEqual(disp.normalize_page(None), disp.PAGE_OPS)
        self.assertEqual(disp.normalize_page("NETWORK"), disp.PAGE_NETWORK)
        self.assertEqual(disp.normalize_page("nope"), disp.PAGE_OPS)

    def test_advance_wraps(self):
        self.assertEqual(disp.advance_page(disp.PAGE_OPS), disp.PAGE_NETWORK)
        self.assertEqual(
            disp.advance_page(disp.PAGE_NETWORK), disp.PAGE_SYSTEM
        )
        self.assertEqual(disp.advance_page(disp.PAGE_SYSTEM), disp.PAGE_OPS)

    def test_idle_returns_home(self):
        self.assertEqual(
            disp.page_after_idle(disp.PAGE_NETWORK, 0.0, 10.0, idle_seconds=10),
            disp.PAGE_OPS,
        )
        self.assertEqual(
            disp.page_after_idle(disp.PAGE_NETWORK, 0.0, 9.9, idle_seconds=10),
            disp.PAGE_NETWORK,
        )
        self.assertEqual(
            disp.page_after_idle(disp.PAGE_SYSTEM, None, 100.0, idle_seconds=10),
            disp.PAGE_OPS,
        )
        self.assertEqual(
            disp.page_after_idle(disp.PAGE_OPS, 0.0, 100.0, idle_seconds=10),
            disp.PAGE_OPS,
        )


class IdentityAndLabelsTests(unittest.TestCase):
    def test_identity_line(self):
        self.assertEqual(
            disp.identity_line(
                warehouse_name="North", node_name="pack-03"
            ),
            "North · pack-03",
        )
        self.assertEqual(
            disp.identity_line(organization_name="Acme"),
            "Acme",
        )
        self.assertEqual(disp.identity_line(), "—")

    def test_heartbeat_age(self):
        now = datetime(2026, 7, 17, 12, 0, 0, tzinfo=timezone.utc)
        hb = (now - timedelta(seconds=12)).isoformat()
        self.assertEqual(disp.heartbeat_age_label(hb, now=now), "12s ago")
        hb_m = (now - timedelta(minutes=3)).isoformat()
        self.assertEqual(disp.heartbeat_age_label(hb_m, now=now), "3m ago")
        self.assertEqual(disp.heartbeat_age_label(None), "—")
        self.assertEqual(disp.heartbeat_age_label("not-a-date"), "—")

    def test_printer_status_color(self):
        self.assertEqual(disp.printer_status_color("idle"), disp.OK)
        self.assertEqual(disp.printer_status_color("printing"), disp.WARN)
        self.assertEqual(disp.printer_status_color("stopped"), disp.DOWN)

    def test_printer_status_label_prefers_message(self):
        self.assertEqual(
            disp.printer_status_label("stopped", "Out of paper"),
            "Out of paper",
        )
        self.assertEqual(disp.printer_status_label("idle", None), "idle")

    def test_jobs_strip(self):
        self.assertEqual(disp.jobs_strip_label(0), "JOBS  queue 0 · idle")
        self.assertEqual(disp.jobs_strip_label(3), "JOBS  queue 3")

    def test_count_queue_jobs(self):
        with tempfile.TemporaryDirectory() as td:
            p = Path(td)
            (p / "a.json").write_text("{}", encoding="utf-8")
            (p / "b.json").write_text("{}", encoding="utf-8")
            (p / "note.txt").write_text("x", encoding="utf-8")
            self.assertEqual(disp.count_queue_jobs(p), 2)
        self.assertEqual(disp.count_queue_jobs("/no/such/dir"), 0)


class TouchParseTests(unittest.TestCase):
    def test_parse_btn_touch_events(self):
        fmt = "llHHi"
        size = struct.calcsize(fmt)
        # press then release
        press = struct.pack(fmt, 0, 0, touch_mod.EV_KEY, touch_mod.BTN_TOUCH, 1)
        release = struct.pack(
            fmt, 0, 0, touch_mod.EV_KEY, touch_mod.BTN_TOUCH, 0
        )
        events = touch_mod.parse_events(press + release, fmt, size)
        self.assertEqual(len(events), 2)
        self.assertEqual(events[0], (touch_mod.EV_KEY, touch_mod.BTN_TOUCH, 1))
        self.assertEqual(events[1], (touch_mod.EV_KEY, touch_mod.BTN_TOUCH, 0))

    def test_listener_inject_and_poll(self):
        t = touch_mod.TouchListener(None, debounce_s=0.0)
        self.assertFalse(t.poll_tap())
        t.inject_tap()
        self.assertTrue(t.poll_tap())
        self.assertFalse(t.poll_tap())

    def test_handle_press_release_records_tap(self):
        t = touch_mod.TouchListener(None, debounce_s=0.0)
        t._handle_event(touch_mod.EV_KEY, touch_mod.BTN_TOUCH, 1)
        self.assertFalse(t.poll_tap())
        t._handle_event(touch_mod.EV_KEY, touch_mod.BTN_TOUCH, 0)
        self.assertTrue(t.poll_tap())

    def test_find_touch_override_missing(self):
        self.assertIsNone(
            touch_mod.find_touch_device(override="/no/such/event99")
        )


class PageStateTests(unittest.TestCase):
    def test_note_tap_advances_when_paired(self):
        ps = disp.PageState(disp.PAGE_OPS, idle_seconds=10.0)
        self.assertEqual(ps.note_tap(paired=True, now_mono=1.0), disp.PAGE_NETWORK)
        self.assertEqual(ps.note_tap(paired=True, now_mono=2.0), disp.PAGE_SYSTEM)
        self.assertEqual(ps.note_tap(paired=True, now_mono=3.0), disp.PAGE_OPS)

    def test_note_tap_ignored_when_unpaired(self):
        ps = disp.PageState(disp.PAGE_OPS)
        self.assertEqual(ps.note_tap(paired=False, now_mono=1.0), disp.PAGE_OPS)

    def test_idle_home_via_sync(self):
        ps = disp.PageState(disp.PAGE_NETWORK, idle_seconds=10.0)
        ps.note_tap(paired=True, now_mono=0.0)  # → system? from network advance
        # Reset to network with known timestamp
        ps.set_page(disp.PAGE_NETWORK, now_mono=0.0)
        self.assertEqual(ps.sync(paired=True, now_mono=11.0), disp.PAGE_OPS)

    def test_unpaired_resets(self):
        ps = disp.PageState(disp.PAGE_SYSTEM)
        self.assertEqual(ps.sync(paired=False, now_mono=5.0), disp.PAGE_OPS)


if __name__ == "__main__":
    unittest.main()
