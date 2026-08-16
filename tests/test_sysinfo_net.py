"""Ethernet / Wi-Fi sysfs detection (no live NetworkManager)."""

from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

import sysinfo


def _iface(root: Path, name: str, *, wireless: bool, carrier: str, oper: str = "up"):
    p = root / name
    p.mkdir()
    (p / "carrier").write_text(carrier + "\n", encoding="ascii")
    (p / "operstate").write_text(oper + "\n", encoding="ascii")
    if wireless:
        (p / "wireless").mkdir()


class NetDetectTests(unittest.TestCase):
    def test_ethernet_up_ignores_wifi_and_virtual(self):
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            _iface(root, "eth0", wireless=False, carrier="1")
            _iface(root, "wlan0", wireless=True, carrier="1")
            _iface(root, "lo", wireless=False, carrier="1")
            _iface(root, "tailscale0", wireless=False, carrier="1")
            self.assertEqual(sysinfo.ethernet_ifaces(root), ["eth0"])
            self.assertEqual(sysinfo.wifi_ifaces(root), ["wlan0"])
            self.assertTrue(sysinfo.ethernet_up(root))
            self.assertTrue(sysinfo.wifi_carrier_up(root))

    def test_no_eth_wifi_only(self):
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            _iface(root, "wlan0", wireless=True, carrier="0", oper="down")
            self.assertFalse(sysinfo.ethernet_up(root))
            self.assertFalse(sysinfo.wifi_carrier_up(root))

    def test_operstate_fallback(self):
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            p = root / "enp1s0"
            p.mkdir()
            (p / "operstate").write_text("up\n", encoding="ascii")
            self.assertTrue(sysinfo.ethernet_up(root))

    def test_network_link_prefers_ethernet(self):
        sysinfo.clear_network_link_cache()
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            _iface(root, "eth0", wireless=False, carrier="1")
            with mock.patch.object(sysinfo, "wifi_ssid", return_value="Cafe"):
                kind, label, up = sysinfo.network_link(
                    sys_class_net=root, cache_s=0
                )
        self.assertEqual((kind, label, up), ("eth", "ethernet", True))

    def test_network_link_wifi_ssid(self):
        sysinfo.clear_network_link_cache()
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            _iface(root, "wlan0", wireless=True, carrier="1")
            with mock.patch.object(sysinfo, "wifi_ssid", return_value="Acme-WiFi"):
                kind, label, up = sysinfo.network_link(
                    sys_class_net=root, cache_s=0
                )
        self.assertEqual((kind, label, up), ("wifi", "Acme-WiFi", True))

    def test_network_link_none(self):
        sysinfo.clear_network_link_cache()
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            with mock.patch.object(sysinfo, "wifi_ssid", return_value=None):
                kind, label, up = sysinfo.network_link(
                    sys_class_net=root, cache_s=0
                )
        self.assertEqual((kind, label, up), ("none", "no network", False))
