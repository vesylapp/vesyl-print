"""Wi-Fi setup policy, QR payload, helper CLI, portal parse and spawn."""

from __future__ import annotations

import importlib.util
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

import wifi_portal
import wifi_setup


def _no_command(*args, **kwargs):
    argv = args[0] if args else kwargs.get("args")
    raise AssertionError(f"a test ran a real command: {argv!r}")


def _subprocess_stand_in(**overrides):
    """``subprocess`` as wifi_setup and wifi_portal see it in a test: running
    anything fails the test, unless the test brings its own ``Popen``."""
    fields = {
        "run": _no_command,
        "Popen": _no_command,
        "DEVNULL": subprocess.DEVNULL,
        "SubprocessError": subprocess.SubprocessError,
        "TimeoutExpired": subprocess.TimeoutExpired,
    }
    fields.update(overrides)
    return types.SimpleNamespace(**fields)


class Sandboxed(unittest.TestCase):
    """Base of every test here: nothing reaches the machine's network setup.

    Run as root (sudo on a Pi, a root CI container), the helper's code would
    write the captive-portal snippet into the real NetworkManager config (a
    DNS sinkhole for the whole host), the setup state and portal pid under
    /run, and iptables NAT rules. Here those files live in a temp dir, set
    both in the environment and as the module defaults (so a test that
    clears the environment is covered too), iptables calls are recorded,
    and running any other command fails the test.
    """

    def setUp(self):
        super().setUp()
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.sandbox = Path(tmp.name)
        self.captive_dns = self.sandbox / "vesyl-captive.conf"
        self.state_file = self.sandbox / "wifi-setup.json"
        self.portal_pid = self.sandbox / "wifi-portal.pid"
        self.iptables: list[list[str]] = []

        def iptables(args):
            self.iptables.append(args)
            return 1  # as for a rule that is not there: ends the -D loops

        env = {
            "VESYL_CAPTIVE_DNS": str(self.captive_dns),
            "VESYL_WIFI_STATE": str(self.state_file),
            "VESYL_WIFI_PORTAL_PID": str(self.portal_pid),
        }
        for patcher in (
            mock.patch.dict(os.environ, env),
            mock.patch.multiple(
                wifi_setup,
                CAPTIVE_DNS_FILE=self.captive_dns,
                SETUP_STATE_FILE=self.state_file,
                PORTAL_PID=self.portal_pid,
                _iptables=iptables,
                subprocess=_subprocess_stand_in(),
            ),
            mock.patch.object(wifi_portal, "subprocess", _subprocess_stand_in()),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)

    def nat_rules_added(self) -> list[list[str]]:
        return [a for a in self.iptables if "-A" in a]


# Captured before any test patches them.
REAL_FILES = (
    wifi_setup.CAPTIVE_DNS_FILE,
    wifi_setup.SETUP_STATE_FILE,
    wifi_setup.PORTAL_PID,
)


class IsolationTests(Sandboxed):
    def test_every_test_class_here_is_sandboxed(self):
        for name, obj in vars(sys.modules[__name__]).items():
            if (
                isinstance(obj, type)
                and issubclass(obj, unittest.TestCase)
                and obj.__module__ == __name__
            ):
                self.assertTrue(issubclass(obj, Sandboxed), name)

    def test_the_defaults_point_into_the_sandbox_without_the_environment(self):
        self.assertEqual(
            [str(p) for p in REAL_FILES],
            [
                "/etc/NetworkManager/dnsmasq-shared.d/vesyl-captive.conf",
                "/run/vesyl-print-wifi-setup.json",
                "/run/vesyl-print-wifi-portal.pid",
            ],
        )
        with mock.patch.dict(os.environ):
            for key in ("VESYL_CAPTIVE_DNS", "VESYL_WIFI_STATE", "VESYL_WIFI_PORTAL_PID"):
                del os.environ[key]
            dest = wifi_setup.write_captive_dns(wifi_setup.DEFAULT_AP_IP)
            wifi_setup.save_setup_state(ssid="VESYL-X")
            wifi_setup.stop_portal()
        self.assertEqual(dest, self.captive_dns)
        self.assertIn("address=/#/10.42.0.1", dest.read_text(encoding="ascii"))
        self.assertEqual(json.loads(self.state_file.read_text())["ssid"], "VESYL-X")

    def test_a_test_that_forgets_its_fakes_runs_nothing(self):
        with self.assertRaisesRegex(AssertionError, "ran a real command: .*nmcli"):
            wifi_setup.first_wifi_device()
        with self.assertRaisesRegex(AssertionError, "ran a real command: .*sudo"):
            wifi_setup.WifiSetupController().tick(now_mono=1.0)
        wifi_setup.clear_captive_redirects("wlan0")
        self.assertEqual(len(self.iptables), 3)


class PayloadTests(Sandboxed):
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


class PolicyTests(Sandboxed):
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


class HelperCliTests(Sandboxed):
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
        # The captive-portal DNS snippet, NAT rules and setup state went to
        # the sandbox, not to NetworkManager, iptables and /run.
        self.assertEqual(
            self.captive_dns.read_text(encoding="ascii"),
            wifi_setup.captive_dns_config("10.42.0.1"),
        )
        self.assertEqual(len(self.nat_rules_added()), 3)
        self.assertEqual(json.loads(self.state_file.read_text())["ssid"], "VESYL-X")

    def test_unknown_command(self):
        buf = io.StringIO()
        with mock.patch("sys.stdout", buf):
            rc = wifi_setup.call_helper_cli(["explode"], nm=lambda a, t: (0, "", ""))
        self.assertEqual(rc, 2)


class ControllerTests(Sandboxed):
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
        # Restoring the setup AP rewrote the captive-portal snippet and NAT
        # rules: in the sandbox.
        self.assertIn("address=/#/10.42.0.1", self.captive_dns.read_text(encoding="ascii"))
        self.assertEqual(len(self.nat_rules_added()), 3)

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


class CaptiveDetectTests(Sandboxed):
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


class PortalTests(Sandboxed):
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


class PortalSpawnTests(Sandboxed):
    """The root helper runs the portal of the release it imported
    wifi_setup.py from, never a fixed path under the default install root."""

    # Written to the sandbox pid file; stop_portal may signal it, so it must
    # never be a real process.
    FAKE_PID = 2**31 - 1

    def record_spawns(self, module) -> list[list[str]]:
        spawned: list[list[str]] = []

        def popen(argv, **kwargs):
            spawned.append(argv)
            return types.SimpleNamespace(pid=self.FAKE_PID)

        patcher = mock.patch.object(module, "subprocess", _subprocess_stand_in(Popen=popen))
        patcher.start()
        self.addCleanup(patcher.stop)
        return spawned

    def load_copy(self, path: Path):
        """wifi_setup loaded again from ``path``, as the helper imports it."""
        name = f"wifi_setup_from_{path.parent.name}_{id(self)}"
        spec = importlib.util.spec_from_file_location(name, path)
        module = importlib.util.module_from_spec(spec)
        sys.modules[name] = module  # dataclasses look the module up
        self.addCleanup(sys.modules.pop, name, None)
        spec.loader.exec_module(module)
        return module

    def test_spawns_the_portal_beside_wifi_setup(self):
        spawned = self.record_spawns(wifi_setup)
        # As on a device moved to another INSTALL_ROOT: a stale tree is left
        # under /opt/vesyl-print, and must not be what root runs.
        real_is_file = Path.is_file

        def is_file(path):
            return str(path).startswith("/opt/vesyl-print/") or real_is_file(path)

        with mock.patch.object(Path, "is_file", is_file):
            wifi_setup.spawn_portal("10.42.0.1")
        portal = str(ROOT / "wifi_portal.py")
        self.assertEqual(
            spawned,
            [[sys.executable, portal, "--bind", "10.42.0.1", "--port", "80"]],
        )
        self.assertEqual(self.portal_pid.read_text(encoding="ascii"), str(self.FAKE_PID))

    def test_falls_back_to_the_tree_it_was_imported_through(self):
        # current/wifi_setup.py resolves into a slot that lacks the portal.
        slot = self.sandbox / "releases" / "1.0.0"
        current = self.sandbox / "current"
        slot.mkdir(parents=True)
        current.mkdir()
        shutil.copy(ROOT / "wifi_setup.py", slot / "wifi_setup.py")
        (current / "wifi_setup.py").symlink_to(slot / "wifi_setup.py")
        (current / "wifi_portal.py").write_text("# portal\n", encoding="ascii")
        copy = self.load_copy(current / "wifi_setup.py")
        spawned = self.record_spawns(copy)
        copy.spawn_portal("10.42.0.1", port=8088)
        self.assertEqual(
            spawned,
            [[sys.executable, str(current / "wifi_portal.py"), "--bind", "10.42.0.1", "--port", "8088"]],
        )

    def test_without_a_portal_beside_it_nothing_is_spawned(self):
        lone = self.sandbox / "lone"
        lone.mkdir()
        shutil.copy(ROOT / "wifi_setup.py", lone / "wifi_setup.py")
        copy = self.load_copy(lone / "wifi_setup.py")
        spawned = self.record_spawns(copy)
        with self.assertLogs("vesyl-print.wifi", level="WARNING") as logs:
            copy.spawn_portal("10.42.0.1")
        self.assertEqual(spawned, [])
        self.assertIn("wifi_portal.py missing", logs.output[0])
        self.assertFalse(self.portal_pid.exists())


if __name__ == "__main__":
    unittest.main()
