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


class TouchMapAndLongPressTests(unittest.TestCase):
    def test_default_mhs35_swap_invert_x(self):
        tf = touch_mod.TouchTransform.mhs35_rotate90()
        self.assertTrue(tf.swap_xy)
        self.assertTrue(tf.invert_x)
        self.assertFalse(tf.invert_y)
        # Logged tap on › : raw (1501, 688) → right nav, not the top bar.
        x, y = touch_mod.map_touch_to_screen(1501, 688, 480, 320, tf)
        self.assertGreater(x, 380)
        self.assertGreater(y, 80)
        self.assertLess(y, 160)
        # Logged tap on Back: raw (3028, 3147) → bottom-left.
        x, y = touch_mod.map_touch_to_screen(3028, 3147, 480, 320, tf)
        self.assertLess(x, 180)
        self.assertGreater(y, 200)

    def test_map_center_swap_invert_y(self):
        tf = touch_mod.TouchTransform(
            swap_xy=True, invert_x=False, invert_y=True,
            xmin=0, xmax=4095, ymin=0, ymax=4095,
        )
        x, y = touch_mod.map_touch_to_screen(2048, 2048, 480, 320, tf)
        self.assertAlmostEqual(x, 240, delta=2)
        self.assertAlmostEqual(y, 160, delta=2)

    def test_map_corners(self):
        tf = touch_mod.TouchTransform(
            swap_xy=True, invert_x=False, invert_y=True,
            xmin=0, xmax=100, ymin=0, ymax=100,
        )
        # raw (0,0) → after swap (0,0) → invert Y → (0, h-1)
        self.assertEqual(touch_mod.map_touch_to_screen(0, 0, 480, 320, tf), (0, 319))
        # raw (100,100) → swap (1,1) → invert Y → (479, 0)
        self.assertEqual(
            touch_mod.map_touch_to_screen(100, 100, 480, 320, tf), (479, 0)
        )

    def test_long_press_emits_before_release(self):
        t = touch_mod.TouchListener(None, debounce_s=0.0, long_press_s=3.0)
        t._handle_event(touch_mod.EV_ABS, touch_mod.ABS_X, 100)
        t._handle_event(touch_mod.EV_ABS, touch_mod.ABS_Y, 200)
        t._handle_event(touch_mod.EV_KEY, touch_mod.BTN_TOUCH, 1)
        t._press_mono = t._press_mono - 3.1
        t._maybe_long_press()
        ev = t.poll_event()
        self.assertIsNotNone(ev)
        self.assertEqual(ev.kind, "long_press")
        self.assertFalse(t.poll_tap())
        # release after long-press must not also count as a tap
        t._handle_event(touch_mod.EV_KEY, touch_mod.BTN_TOUCH, 0)
        self.assertIsNone(t.poll_event())
        self.assertFalse(t.poll_tap())

    def test_short_press_is_tap_with_coords(self):
        t = touch_mod.TouchListener(
            None,
            debounce_s=0.0,
            long_press_s=3.0,
            screen_size=(480, 320),
            transform=touch_mod.TouchTransform(
                swap_xy=False, invert_x=False, invert_y=False,
                xmin=0, xmax=100, ymin=0, ymax=100,
            ),
        )
        t._handle_event(touch_mod.EV_ABS, touch_mod.ABS_X, 50)
        t._handle_event(touch_mod.EV_ABS, touch_mod.ABS_Y, 25)
        t._handle_event(touch_mod.EV_KEY, touch_mod.BTN_TOUCH, 1)
        t._handle_event(touch_mod.EV_KEY, touch_mod.BTN_TOUCH, 0)
        ev = t.poll_event()
        self.assertIsNotNone(ev)
        self.assertEqual(ev.kind, "tap")
        self.assertEqual(ev.x, 240)
        self.assertEqual(ev.y, 80)
        self.assertTrue(t.poll_tap() is False)  # already consumed

    def test_classify_swipe(self):
        self.assertEqual(
            touch_mod.classify_swipe(10, 50, 200, 55, min_px=80), "right"
        )
        self.assertEqual(
            touch_mod.classify_swipe(200, 50, 40, 60, min_px=80), "left"
        )
        self.assertIsNone(touch_mod.classify_swipe(10, 50, 20, 55))
        self.assertIsNone(touch_mod.classify_swipe(10, 10, 20, 200))

    def test_horizontal_drag_emits_swipe_not_tap(self):
        t = touch_mod.TouchListener(
            None,
            debounce_s=0.0,
            long_press_s=3.0,
            screen_size=(480, 320),
            swipe_px=40,
            transform=touch_mod.TouchTransform(
                swap_xy=False, invert_x=False, invert_y=False,
                xmin=0, xmax=480, ymin=0, ymax=320,
            ),
        )
        t._handle_event(touch_mod.EV_ABS, touch_mod.ABS_X, 400)
        t._handle_event(touch_mod.EV_ABS, touch_mod.ABS_Y, 160)
        t._handle_event(touch_mod.EV_KEY, touch_mod.BTN_TOUCH, 1)
        t._handle_event(touch_mod.EV_ABS, touch_mod.ABS_X, 80)
        t._handle_event(touch_mod.EV_KEY, touch_mod.BTN_TOUCH, 0)
        ev = t.poll_event()
        self.assertIsNotNone(ev)
        self.assertEqual(ev.kind, "swipe_left")
        self.assertEqual(ev.direction, "left")
        self.assertFalse(t.poll_tap())


class TestPrintUiTests(unittest.TestCase):
    def test_formats_raw_vs_cups(self):
        self.assertEqual(disp.test_print_formats(True), ("pdf", "zpl"))
        self.assertEqual(disp.test_print_formats(False), ("pdf",))
        self.assertEqual(disp.test_print_default_format(True), "zpl")
        self.assertEqual(disp.test_print_default_format(False), "pdf")

    def test_layout_is_test_and_back(self):
        raw_p = {"name": "Zebra", "cups_name": "Z1", "supports_raw": True}
        cups_p = {"name": "Office", "cups_name": "O1", "supports_raw": False}
        raw_rects = disp.layout_test_print(
            w=480, h=320, body_top=80, printer=raw_p, nav_y=80
        )
        self.assertEqual([r.id for r in raw_rects], ["prev", "next", "back", "test"])
        prev = next(r for r in raw_rects if r.id == "prev")
        nxt = next(r for r in raw_rects if r.id == "next")
        back = next(r for r in raw_rects if r.id == "back")
        test = next(r for r in raw_rects if r.id == "test")
        self.assertEqual(prev.y, nxt.y)
        self.assertLess(prev.x + prev.w, nxt.x)
        self.assertEqual(back.y, test.y)
        self.assertLess(back.x + back.w, test.x)
        self.assertEqual(test.payload["format"], "zpl")
        self.assertEqual(test.payload["label"], "Test ZPL")
        cups_rects = disp.layout_test_print(
            w=480, h=320, body_top=80, printer=cups_p
        )
        cups_test = next(r for r in cups_rects if r.id == "test")
        self.assertEqual(cups_test.payload["format"], "pdf")
        self.assertEqual(cups_test.payload["label"], "Test PDF")

    def test_apply_hit_flow(self):
        st = disp.TestPrintState(idle_seconds=30)
        st.open_panel(1.0)
        printers = [
            {"name": "Zebra", "cups_name": "Z1", "supports_raw": True},
            {"name": "Office", "cups_name": "O1", "supports_raw": False},
        ]
        st.sync_printers(printers)
        rects = disp.layout_test_print(
            w=480, h=320, body_top=80, printer=st.selected
        )
        test = next(r for r in rects if r.id == "test")
        self.assertEqual(disp.apply_test_hit(st, test, 2.0), "print:zpl")

        self.assertEqual(disp.apply_test_hit(st, next(
            r for r in rects if r.id == "next"
        ), 3.0), "next")
        st.cycle(1, len(printers))
        st.sync_printers(printers)
        self.assertEqual(st.selected["cups_name"], "O1")
        office_rects = disp.layout_test_print(
            w=480, h=320, body_top=80, printer=st.selected
        )
        office_test = next(r for r in office_rects if r.id == "test")
        self.assertEqual(disp.apply_test_hit(st, office_test, 4.0), "print:pdf")

        back = next(r for r in office_rects if r.id == "back")
        self.assertEqual(disp.apply_test_hit(st, back, 5.0), "close")

    def test_missed_tap_stays_on_overlay(self):
        st = disp.TestPrintState(idle_seconds=30)
        st.open_panel(1.0)
        self.assertIsNone(disp.apply_test_hit(st, None, 2.0))
        self.assertTrue(st.open)

    def test_nav_buttons_cycle(self):
        st = disp.TestPrintState()
        st.open_panel(0.0)
        printers = [
            {"name": "A", "cups_name": "A", "supports_raw": False},
            {"name": "B", "cups_name": "B", "supports_raw": True},
        ]
        st.sync_printers(printers)
        rects = disp.layout_test_print(
            w=480, h=320, body_top=80, printer=st.selected, nav_y=80
        )
        prev = next(r for r in rects if r.id == "prev")
        nxt = next(r for r in rects if r.id == "next")
        self.assertEqual(disp.apply_test_hit(st, prev, 1.0), "prev")
        st.cycle(-1, 2)
        st.sync_printers(printers)
        self.assertEqual(st.selected["cups_name"], "B")
        self.assertEqual(disp.apply_test_hit(st, nxt, 2.0), "next")
        st.cycle(1, 2)
        st.sync_printers(printers)
        self.assertEqual(st.selected["cups_name"], "A")

    def test_hit_test_and_idle_close(self):
        r = disp.HitRect("a", 10, 10, 40, 20)
        self.assertEqual(disp.hit_test([r], 15, 15).id, "a")
        self.assertIsNone(disp.hit_test([r], 0, 0))
        self.assertEqual(disp.hit_test([r], 8, 8, pad=4).id, "a")

    def test_coarse_test_zones(self):
        self.assertEqual(disp.coarse_test_action(20, 280, 480, 320), "close")
        self.assertEqual(disp.coarse_test_action(400, 280, 480, 320), "test")
        self.assertEqual(disp.coarse_test_action(10, 120, 480, 320), "prev")
        self.assertEqual(disp.coarse_test_action(470, 120, 480, 320), "next")
        self.assertIsNone(disp.coarse_test_action(240, 120, 480, 320))
        st = disp.TestPrintState(idle_seconds=10)
        st.open_panel(0.0)
        self.assertTrue(st.sync_idle(9.0))
        self.assertFalse(st.sync_idle(11.0))
        self.assertFalse(st.open)

    def test_busy_blocks_idle_and_hits(self):
        st = disp.TestPrintState(idle_seconds=1)
        st.open_panel(0.0)
        st.busy = True
        self.assertTrue(st.sync_idle(10.0))
        self.assertIsNone(disp.apply_test_hit(st, None, 11.0))


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
