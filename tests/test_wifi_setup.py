"""Wi-Fi setup policy, QR payload, helper CLI, portal parse."""

from __future__ import annotations

import io
import os
import sys
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

import wifi_portal
import wifi_setup


class PayloadTests(unittest.TestCase):
    def test_ssid_from_hostname(self):
        self.assertEqual(
            wifi_setup.setup_ssid("VESYL-PRINT-D2D071"), "VESYL-D2D071"
        )
        self.assertEqual(wifi_setup.setup_ssid("pi"), "VESYL-PI")
        self.assertLessEqual(len(wifi_setup.setup_ssid("x" * 80)), 32)

    def test_pin_crockford(self):
        pin = wifi_setup.generate_pin()
        self.assertEqual(len(pin), 8)
        self.assertTrue(all(c in wifi_setup._CROCKFORD for c in pin))

    def test_wifi_qr_escape(self):
        self.assertEqual(
            wifi_setup.wifi_qr_payload("Cafe;Net", "p:ass"),
            r"WIFI:T:WPA;S:Cafe\;Net;P:p\:ass;;",
        )
        self.assertEqual(
            wifi_setup.wifi_qr_payload("Open", ""),
            "WIFI:T:nopass;S:Open;;",
        )


class PolicyTests(unittest.TestCase):
    def test_enter_only_when_no_uplink(self):
        self.assertFalse(
            wifi_setup.should_enter_setup(eth_up=True, wifi_site=False)
        )
        self.assertFalse(
            wifi_setup.should_enter_setup(eth_up=False, wifi_site=True)
        )
        self.assertTrue(
            wifi_setup.should_enter_setup(eth_up=False, wifi_site=False)
        )

    def test_force_still_blocked_by_ethernet(self):
        self.assertFalse(
            wifi_setup.should_enter_setup(
                eth_up=True, wifi_site=False, force=True
            )
        )
        self.assertTrue(
            wifi_setup.should_enter_setup(
                eth_up=False, wifi_site=True, force=True
            )
        )


class HelperCliTests(unittest.TestCase):
    def test_status_and_start(self):
        calls: list[list[str]] = []

        def nm(args, timeout):
            calls.append(args)
            if args[:2] == ["radio", "wifi"]:
                return 0, "", ""
            if args[:3] == ["-t", "-f", "DEVICE,TYPE"]:
                return 0, "wlan0:wifi\neth0:ethernet\n", ""
            if args[:3] == ["-t", "-f", "DEVICE,STATE"]:
                return 0, "wlan0:disconnected\n", ""
            if "connection" in args and "show" in args and "--active" in args:
                return 0, "", ""
            if "hotspot" in args:
                return 0, "Hotspot started\n", ""
            if "IP4.ADDRESS" in args:
                return 0, "10.42.0.1/24\n", ""
            if "delete" in args or "down" in args:
                return 0, "", ""
            return 0, "", ""

        buf = io.StringIO()
        with mock.patch.object(wifi_setup, "spawn_portal"):
            with mock.patch("sys.stdout", buf):
                rc = wifi_setup.call_helper_cli(
                    ["start-ap", "--ssid", "VESYL-X", "--password", "ABCD1234"],
                    nm=nm,
                )
        self.assertEqual(rc, 0)
        data = __import__("json").loads(buf.getvalue())
        self.assertTrue(data["ok"])
        self.assertEqual(data["ap_ip"], "10.42.0.1")
        self.assertTrue(any("hotspot" in c for c in calls))

    def test_unknown_command(self):
        buf = io.StringIO()
        with mock.patch("sys.stdout", buf):
            rc = wifi_setup.call_helper_cli(["explode"], nm=lambda a, t: (0, "", ""))
        self.assertEqual(rc, 2)


class ControllerTests(unittest.TestCase):
    def test_starts_when_no_uplink(self):
        calls: list[list[str]] = []

        def run(args):
            calls.append(args)
            if args[0] == "status":
                return {"ok": True, "eth_up": False, "wifi_site": False, "hotspot": False}
            if args[0] == "start-ap":
                return {"ok": True, "ap_ip": "10.42.0.1"}
            if args[0] == "stop-ap":
                return {"ok": True}
            return {"ok": False}

        ctl = wifi_setup.WifiSetupController(run_helper=run, idle_s=100)
        snap = ctl.tick(now_mono=1.0)
        self.assertEqual(snap.phase, "setup")
        self.assertTrue(snap.ssid.startswith("VESYL-"))
        self.assertEqual(len(snap.pin), 8)
        self.assertIn("WIFI:T:WPA", snap.qr_payload)
        self.assertTrue(any(c[0] == "start-ap" for c in calls))

    def test_joining_flag_blocks_new_hotspot(self):
        import tempfile

        starts = {"n": 0}

        def run(args):
            if args[0] == "start-ap":
                starts["n"] += 1
            return {"ok": True, "eth_up": False, "wifi_site": False, "hotspot": False}

        with tempfile.TemporaryDirectory() as td:
            state = Path(td) / "s.json"
            with mock.patch.dict(os.environ, {"VESYL_WIFI_STATE": str(state)}):
                wifi_setup.save_setup_state(joining=True)
                ctl = wifi_setup.WifiSetupController(run_helper=run)
                snap = ctl.tick(now_mono=1.0)
        self.assertEqual(snap.phase, "connecting")
        self.assertEqual(starts["n"], 0)

    def test_skips_when_ethernet_up(self):
        def run(args):
            return {"ok": True, "eth_up": True, "wifi_site": False, "hotspot": False}

        ctl = wifi_setup.WifiSetupController(run_helper=run)
        snap = ctl.tick(now_mono=1.0)
        self.assertEqual(snap.phase, "idle")
        self.assertFalse(snap.show_setup)

    def test_failed_does_not_retry_immediately(self):
        starts = {"n": 0}

        def run(args):
            if args[0] == "status":
                return {"ok": True, "eth_up": False, "wifi_site": False, "hotspot": False}
            if args[0] == "start-ap":
                starts["n"] += 1
                return {"ok": False, "error": "Failed to setup a Wi-Fi hotspot: Connection activation failed"}
            if args[0] == "stop-ap":
                return {"ok": True}
            return {"ok": False}

        ctl = wifi_setup.WifiSetupController(run_helper=run, idle_s=100)
        first = ctl.tick(now_mono=1.0)
        self.assertEqual(first.phase, "failed")
        pin = first.pin
        second = ctl.tick(now_mono=2.0)
        self.assertEqual(second.phase, "failed")
        self.assertEqual(second.pin, pin)
        self.assertEqual(starts["n"], 1)
        third = ctl.tick(now_mono=1.0 + wifi_setup.RETRY_S + 0.1)
        self.assertEqual(starts["n"], 2)
        self.assertEqual(third.pin, pin)

    def test_eth_restore_leaves_failed_screen(self):
        state = {"eth": False}

        def run(args):
            if args[0] == "status":
                return {
                    "ok": True,
                    "eth_up": state["eth"],
                    "wifi_site": False,
                    "hotspot": False,
                }
            if args[0] == "start-ap":
                return {"ok": False, "error": "Connection activation failed"}
            if args[0] == "stop-ap":
                return {"ok": True}
            return {"ok": False}

        ctl = wifi_setup.WifiSetupController(run_helper=run)
        self.assertEqual(ctl.tick(now_mono=1.0).phase, "failed")
        state["eth"] = True
        snap = ctl.tick(now_mono=2.0)
        self.assertEqual(snap.phase, "idle")
        self.assertFalse(snap.show_setup)

    def test_short_hotspot_error(self):
        self.assertEqual(
            wifi_setup.short_hotspot_error(
                "Error: Failed to setup a Wi-Fi hotspot: Connection activation failed."
            ),
            "Connection activation failed.",
        )

    def test_classify_bad_password(self):
        self.assertIn(
            "Wrong password",
            wifi_setup.classify_join_error(
                "Error: Connection activation failed: (7) Secrets were required"
            ),
        )

    def _patch_join_timing(self):
        return mock.patch.multiple(
            wifi_setup,
            STA_SETTLE_S=0,
            SCAN_PAUSE_S=0,
            SCAN_ATTEMPTS=2,
            CONNECT_TRIES=2,
        )

    def _nm_join(self, *, connect_err="", profiles=(), ssids=("Cafe",), calls=None):
        calls = calls if calls is not None else []

        def nm(args, timeout):
            key = " ".join(args)
            calls.append(key)
            if "DEVICE,TYPE" in key:
                return 0, "wlan0:wifi\n", ""
            if "DEVICE,STATE" in key:
                return 0, "wlan0:disconnected\n", ""
            if args[:2] == ["radio", "wifi"]:
                return 0, "", ""
            if "hotspot" in args:
                return 0, "", ""
            if "IP4.ADDRESS" in key:
                return 0, "10.42.0.1/24\n", ""
            if "delete" in args or "down" in args or "disconnect" in args:
                return 0, "", ""
            if args[:3] == ["-t", "-f", "SSID"]:
                return 0, "".join(s + "\n" for s in ssids), ""
            if "rescan" in args:
                return 0, "", ""
            if args[:3] == ["-t", "-f", "NAME,TYPE"]:
                return 0, "".join(f"{n}:802-11-wireless\n" for n in profiles), ""
            if "802-11-wireless.ssid" in key:
                name = args[-1]
                return 0, (name if name in profiles else "") + "\n", ""
            if "connect" in args:
                if connect_err:
                    return 1, "", connect_err
                return 0, "connected\n", ""
            return 0, "", ""

        return nm, calls

    def test_connect_restores_setup_ap_on_bad_password(self):
        import tempfile

        nm, calls = self._nm_join(
            connect_err="Error: Connection activation failed: (7) Secrets were required",
        )
        with tempfile.TemporaryDirectory() as td:
            state = Path(td) / "wifi.json"
            with mock.patch.dict(os.environ, {"VESYL_WIFI_STATE": str(state)}):
                wifi_setup.save_setup_state(ssid="VESYL-X", pin="ABCD1234")
                with self._patch_join_timing():
                    with mock.patch.object(wifi_setup, "spawn_portal"):
                        out = wifi_setup.connect_site("Cafe", "bad", nm=nm)
                saved_err = wifi_setup.load_setup_state().get("last_error", "")
        self.assertFalse(out["ok"])
        self.assertTrue(out["recovered"])
        self.assertIn("Wrong password", out["error"])
        self.assertTrue(any("hotspot" in c for c in calls))
        self.assertIn("Wrong password", saved_err)

    def test_connect_forgets_stale_profile_then_joins(self):
        import tempfile

        nm, calls = self._nm_join(profiles=("Cafe",))
        with tempfile.TemporaryDirectory() as td:
            state = Path(td) / "wifi.json"
            with mock.patch.dict(os.environ, {"VESYL_WIFI_STATE": str(state)}):
                with self._patch_join_timing():
                    with mock.patch.object(wifi_setup, "stop_portal"):
                        out = wifi_setup.connect_site("Cafe", "good-pass", nm=nm)
        self.assertTrue(out["ok"], out)
        deletes = [c for c in calls if "delete" in c and "Cafe" in c]
        self.assertTrue(deletes, calls)
        self.assertTrue(any("connect" in c and "Cafe" in c for c in calls))

    def test_connect_waits_for_scan_then_joins(self):
        import tempfile

        seen = {"n": 0}

        def nm(args, timeout):
            key = " ".join(args)
            if "DEVICE,TYPE" in key:
                return 0, "wlan0:wifi\n", ""
            if "DEVICE,STATE" in key:
                return 0, "wlan0:disconnected\n", ""
            if args[:3] == ["-t", "-f", "SSID"]:
                seen["n"] += 1
                if seen["n"] < 2:
                    return 0, "", ""
                return 0, "AbrahamLinksys\n", ""
            if "connect" in args:
                return 0, "ok\n", ""
            return 0, "", ""

        with tempfile.TemporaryDirectory() as td:
            state = Path(td) / "wifi.json"
            with mock.patch.dict(os.environ, {"VESYL_WIFI_STATE": str(state)}):
                with self._patch_join_timing():
                    with mock.patch.object(wifi_setup, "stop_portal"):
                        out = wifi_setup.connect_site(
                            "AbrahamLinksys", "ok", nm=nm
                        )
        self.assertTrue(out["ok"], out)
        self.assertGreaterEqual(seen["n"], 2)

    def test_connections_for_ssid(self):
        def nm(args, timeout):
            key = " ".join(args)
            if args[:3] == ["-t", "-f", "NAME,TYPE"]:
                return 0, "Cafe:802-11-wireless\nvesyl-setup:802-11-wireless\neth:802-3-ethernet\n", ""
            if "802-11-wireless.ssid" in key:
                name = args[-1]
                return 0, {"Cafe": "Cafe", "vesyl-setup": "VESYL-X"}.get(name, "") + "\n", ""
            return 0, "", ""

        self.assertEqual(wifi_setup.connections_for_ssid(nm, "Cafe"), ["Cafe"])


class CaptiveDetectTests(unittest.TestCase):
    def test_dns_config_sinkholes_and_rfc8910(self):
        cfg = wifi_setup.captive_dns_config("10.42.0.1")
        self.assertIn("address=/#/10.42.0.1", cfg)
        self.assertIn("dhcp-option=114,http://10.42.0.1/", cfg)

    def test_write_captive_dns(self):
        import tempfile

        with tempfile.TemporaryDirectory() as td:
            p = Path(td) / "vesyl-captive.conf"
            wifi_setup.write_captive_dns("10.9.0.1", path=p)
            text = p.read_text(encoding="ascii")
            self.assertIn("10.9.0.1", text)


class PortalTests(unittest.TestCase):
    def test_parse_prefers_typed_ssid(self):
        body = b"ssid=Visible&ssid_other=Hidden+Net&password=s3cret"
        ssid, pw = wifi_portal.parse_connect_body(body)
        self.assertEqual(ssid, "Hidden Net")
        self.assertEqual(pw, "s3cret")

    def test_parse_uses_select_when_other_blank(self):
        ssid, pw = wifi_portal.parse_connect_body(b"ssid=Cafe&ssid_other=&password=")
        self.assertEqual(ssid, "Cafe")
        self.assertEqual(pw, "")

    def test_html_lists_networks(self):
        page = wifi_portal.portal_html(
            [{"ssid": "Acme", "signal": 70, "security": "WPA2"}]
        )
        self.assertIn("Acme", page)
        self.assertIn("Set up Wi-Fi", page)

    def test_success_html_tells_user_to_close(self):
        page = wifi_portal.success_html("AbrahamLinksys")
        self.assertIn("AbrahamLinksys", page)
        self.assertIn("expected", page.lower())
        self.assertIn("wrong", page.lower())
        self.assertIn("Close this page", page)

    def test_post_marks_joining_before_delay(self):
        import tempfile
        import threading
        from http.client import HTTPConnection

        with tempfile.TemporaryDirectory() as td:
            state = Path(td) / "s.json"
            with mock.patch.dict(os.environ, {"VESYL_WIFI_STATE": str(state)}):
                httpd = wifi_portal.serve(
                    "127.0.0.1",
                    0,
                    scan=lambda: [],
                    connect=lambda *a: {"ok": True},
                    connect_delay_s=0.3,
                )
                port = httpd.server_address[1]
                t = threading.Thread(target=httpd.handle_request, daemon=True)
                t.start()
                try:
                    c = HTTPConnection("127.0.0.1", port, timeout=2)
                    c.request(
                        "POST",
                        "/connect",
                        body="ssid=Cafe&password=x",
                        headers={
                            "Content-Type": "application/x-www-form-urlencoded"
                        },
                    )
                    r = c.getresponse()
                    r.read()
                    self.assertEqual(r.status, 200)
                    c.close()
                    self.assertTrue(wifi_setup.load_setup_state().get("joining"))
                finally:
                    t.join(timeout=1)
                    httpd.server_close()

    def test_post_returns_success_before_connect(self):
        import threading
        from http.client import HTTPConnection

        connected: list[str] = []

        def connect(ssid, password):
            connected.append(ssid)
            return {"ok": True}

        httpd = wifi_portal.serve(
            "127.0.0.1",
            0,
            scan=lambda: [],
            connect=connect,
            connect_delay_s=0.2,
        )
        port = httpd.server_address[1]
        t = threading.Thread(target=httpd.handle_request, daemon=True)
        t.start()
        try:
            c = HTTPConnection("127.0.0.1", port, timeout=2)
            c.request(
                "POST",
                "/connect",
                body="ssid=Cafe&password=x",
                headers={"Content-Type": "application/x-www-form-urlencoded"},
            )
            r = c.getresponse()
            body = r.read().decode("utf-8")
            self.assertEqual(r.status, 200)
            self.assertIn("Trying to join", body)
            self.assertEqual(connected, [])
            c.close()
        finally:
            t.join(timeout=1)
            httpd.server_close()
        # connect runs after the response
        threading.Event().wait(0.4)
        self.assertEqual(connected, ["Cafe"])

    def test_generate_204_redirects_not_204(self):
        import threading
        from http.client import HTTPConnection

        httpd = wifi_portal.serve(
            "127.0.0.1", 0, scan=lambda: [], connect=lambda *a: {"ok": True}
        )
        port = httpd.server_address[1]
        t = threading.Thread(target=httpd.handle_request, daemon=True)
        t.start()
        try:
            c = HTTPConnection("127.0.0.1", port, timeout=2)
            c.request("GET", "/generate_204")
            r = c.getresponse()
            r.read()
            self.assertEqual(r.status, 302)
            self.assertEqual(r.getheader("Location"), "/")
            c.close()
        finally:
            httpd.server_close()
            t.join(timeout=1)


if __name__ == "__main__":
    unittest.main()
