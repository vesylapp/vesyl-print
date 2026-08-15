"""Port-9100 Zebra discovery + raw AppSocket CUPS provisioning."""

from __future__ import annotations

import ipaddress
import sys
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

import printers

ZEBRA_HOME_HTML = """<!DOCTYPE HTML PUBLIC "-//W3C//DTD HTML 3.2 Final//EN">
<HTML>
<HEAD><TITLE>D8J241010914 - READY</TITLE></HEAD>
<BODY><CENTER>
<H1>Zebra Technologies<BR>
ZTC ZD421-203dpi ZPL</H1>
<H2>D8J241010914</H2>
Internal Wired PrintServer<H3>Status: <FONT COLOR="GREEN">READY</FONT></H3>
Home: <A HREF="https://www.zebra.com">https://www.zebra.com</A>
</BODY></HTML>
"""

BROTHER_HOME_HTML = """
<html><head><title>Brother HL-L3280CDW</title></head>
<body><h1>Brother</h1><p>Printer Status</p></body></html>
"""


class TestParseZebraHttp(unittest.TestCase):
    def test_real_zd421_page(self):
        model = printers.parse_zebra_http_identity(ZEBRA_HOME_HTML)
        self.assertEqual(model, "Zebra ZD421-203dpi ZPL")

    def test_non_zebra_rejected(self):
        self.assertIsNone(printers.parse_zebra_http_identity(BROTHER_HOME_HTML))
        self.assertIsNone(printers.parse_zebra_http_identity(""))
        self.assertIsNone(printers.parse_zebra_http_identity("<html>ok</html>"))

    def test_zebra_without_ztc(self):
        body = "<html>Zebra Technologies print server zebra.com</html>"
        self.assertEqual(printers.parse_zebra_http_identity(body), "Zebra Printer")


LPINFO_USB_SNIPPET = """\
Device: uri = usb://Zebra%20Technologies/ZTC%20ZD220-203dpi%20ZPL?serial=D4N261201258
        class = direct
        info = Zebra Technologies ZTC ZD220-203dpi ZPL
        make-and-model = Zebra Technologies ZTC ZD220-203dpi ZPL
        device-id = MANUFACTURER:Zebra Technologies ;COMMAND SET:ZPL;MODEL:ZTC ZD220-203dpi ZPL;
        location =
Device: uri = ipp
        class = network
        info = Internet Printing Protocol (ipp)
        make-and-model = Unknown
        device-id =
        location =
Device: uri = dnssd://Brother%20HL-L3280CDW%20series._ipp._tcp.local/
        class = network
        info = Brother HL-L3280CDW series
        make-and-model = Brother HL-L3280CDW series
        device-id =
        location =
"""


class TestUsbDiscovery(unittest.TestCase):
    def test_normalize_usb_model(self):
        self.assertEqual(
            printers._normalize_usb_model(
                "Zebra Technologies ZTC ZD220-203dpi ZPL"
            ),
            "Zebra ZD220-203dpi ZPL",
        )

    def test_discover_usb_from_lpinfo(self):
        with mock.patch("printers._run", return_value=LPINFO_USB_SNIPPET):
            found = printers.discover_usb_printers()
        self.assertEqual(len(found), 1)
        uri, model = found[0]
        self.assertTrue(uri.startswith("usb://"))
        self.assertIn("ZD220", model)
        self.assertTrue(printers._model_looks_thermal_raw(model))

    def test_ensure_printers_adds_usb_raw(self):
        added: list[tuple] = []

        def fake_add_raw(uri, model, *, queue=None, location=None):
            added.append((uri, model, queue))
            return queue or "Zebra_ZD220"

        with mock.patch(
            "printers.configured_network_queues",
            side_effect=[
                [],  # start empty
                [("Zebra_ZD220_203dpi_ZPL", "usb://Zebra/ZD220?serial=1")],
            ],
        ), mock.patch(
            "printers.discover_usb_printers",
            return_value=[
                (
                    "usb://Zebra%20Technologies/ZTC%20ZD220-203dpi%20ZPL?serial=1",
                    "Zebra ZD220-203dpi ZPL",
                )
            ],
        ), mock.patch(
            "printers.discover_network_printers", return_value=[]
        ), mock.patch(
            "printers.discover_zebra_socket_printers", return_value=[]
        ), mock.patch(
            "printers.add_raw_printer", side_effect=fake_add_raw
        ), mock.patch(
            "printers.configured_printers",
            return_value=["Zebra ZD220-203dpi ZPL"],
        ):
            names = printers.ensure_printers()
        self.assertEqual(names, ["Zebra ZD220-203dpi ZPL"])
        self.assertEqual(len(added), 1)
        self.assertTrue(added[0][0].startswith("usb://"))


class TestHostAndIpHelpers(unittest.TestCase):
    def test_host_from_uri(self):
        self.assertEqual(
            printers._host_from_uri("socket://10.0.0.172:9100"), "10.0.0.172"
        )
        self.assertEqual(
            printers._host_from_uri("ipp://printer.local/ipp/print"),
            "printer.local",
        )
        self.assertIsNone(printers._host_from_uri("not-a-uri"))

    def test_ips_from_device_uris(self):
        ips = printers.ips_from_device_uris(
            {
                "ipp://10.0.0.50/ipp/print",
                "socket://10.0.0.172:9100",
                "dnssd://Something._ipp._tcp.local/",
            }
        )
        self.assertIn("10.0.0.50", ips)
        self.assertIn("10.0.0.172", ips)


class TestIdentifyZebraHttp(unittest.TestCase):
    def test_fetch_injection(self):
        model = printers.identify_zebra_http(
            "10.0.0.172",
            fetch=lambda url: ZEBRA_HOME_HTML.encode("utf-8"),
        )
        self.assertEqual(model, "Zebra ZD421-203dpi ZPL")

    def test_fetch_failure(self):
        def boom(_url: str) -> bytes:
            raise OSError("down")

        self.assertIsNone(printers.identify_zebra_http("10.0.0.1", fetch=boom))


class TestDiscoverZebraSocket(unittest.TestCase):
    def test_ignores_known_ipp_ips_and_identifies_zebra(self):
        found = printers.discover_zebra_socket_printers(
            ignore_ips={"10.0.0.50"},
            open_hosts=["10.0.0.50", "10.0.0.172", "10.0.0.200"],
            fetch=lambda url: (
                ZEBRA_HOME_HTML.encode()
                if "10.0.0.172" in url
                else BROTHER_HOME_HTML.encode()
            ),
        )
        self.assertEqual(found, [("10.0.0.172", "Zebra ZD421-203dpi ZPL")])

    def test_no_hosts(self):
        found = printers.discover_zebra_socket_printers(
            open_hosts=[],
            fetch=lambda u: b"",
        )
        self.assertEqual(found, [])


class TestAddRawSocketPrinter(unittest.TestCase):
    def test_lpadmin_raw_socket_command(self):
        with mock.patch("printers.subprocess.run") as run:
            run.return_value = mock.Mock(returncode=0, stdout="", stderr="")
            name = printers.add_raw_socket_printer(
                "10.0.0.172", "Zebra ZD421-203dpi ZPL"
            )
        self.assertEqual(name, "Zebra_ZD421-203dpi_ZPL")
        cmd = run.call_args[0][0]
        self.assertTrue(cmd[0] == "lpadmin" or cmd[0].endswith("/lpadmin"))
        self.assertIn("-p", cmd)
        self.assertEqual(cmd[cmd.index("-p") + 1], "Zebra_ZD421-203dpi_ZPL")
        self.assertEqual(cmd[cmd.index("-v") + 1], "socket://10.0.0.172:9100")
        self.assertEqual(cmd[cmd.index("-m") + 1], "raw")
        self.assertEqual(cmd[cmd.index("-D") + 1], "Zebra ZD421-203dpi ZPL")
        self.assertIn("-E", cmd)

    def test_custom_queue_name(self):
        with mock.patch("printers.subprocess.run") as run:
            run.return_value = mock.Mock(returncode=0, stdout="", stderr="")
            name = printers.add_raw_socket_printer(
                "10.0.0.172", "Zebra ZD421", queue="Zebra_172"
            )
        self.assertEqual(name, "Zebra_172")
        cmd = run.call_args[0][0]
        self.assertEqual(cmd[cmd.index("-p") + 1], "Zebra_172")


class TestEnsurePrintersZebra(unittest.TestCase):
    def test_ensure_adds_zebra_after_ipp(self):
        """IPP first, then 9100 Zebra for hosts not already known."""
        with mock.patch(
            "printers.configured_network_queues",
            side_effect=[
                [],  # initial
                [("Zebra_ZD421-203dpi_ZPL", "socket://10.0.0.172:9100")],  # final
            ],
        ), mock.patch(
            "printers.configured_printers",
            return_value=["Zebra ZD421-203dpi ZPL"],
        ), mock.patch(
            "printers.discover_network_printers",
            return_value=[("ipp://10.0.0.50/ipp/print", "Brother HL")],
        ), mock.patch(
            "printers.add_printer", return_value="Brother_HL"
        ) as add_ipp, mock.patch(
            "printers.discover_zebra_socket_printers",
            return_value=[("10.0.0.172", "Zebra ZD421-203dpi ZPL")],
        ) as disc_z, mock.patch(
            "printers.add_raw_socket_printer",
            return_value="Zebra_ZD421-203dpi_ZPL",
        ) as add_raw:
            names = printers.ensure_printers()

        add_ipp.assert_called_once_with("ipp://10.0.0.50/ipp/print", "Brother HL")
        # Known IPP IP must be passed so we do not re-probe that host.
        ignore = disc_z.call_args.kwargs.get("ignore_ips") or set()
        self.assertIn("10.0.0.50", ignore)
        add_raw.assert_called_once()
        self.assertEqual(add_raw.call_args[0][:2], ("10.0.0.172", "Zebra ZD421-203dpi ZPL"))
        self.assertEqual(names, ["Zebra ZD421-203dpi ZPL"])

    def test_ensure_skips_zebra_ip_already_in_cups(self):
        with mock.patch(
            "printers.configured_network_queues",
            return_value=[("Z", "socket://10.0.0.172:9100")],
        ), mock.patch(
            "printers.configured_printers", return_value=["Z"]
        ), mock.patch(
            "printers.discover_network_printers", return_value=[]
        ), mock.patch(
            "printers.discover_zebra_socket_printers",
            return_value=[("10.0.0.172", "Zebra ZD421")],
        ), mock.patch(
            "printers.add_raw_socket_printer"
        ) as add_raw:
            printers.ensure_printers()
        add_raw.assert_not_called()


class TestLocalScanNetworks(unittest.TestCase):
    def test_parses_ip_output_skips_docker(self):
        sample = (
            "2: enp7s0    inet 10.0.0.164/24 brd 10.0.0.255 scope global enp7s0\\n"
            "6: docker0    inet 172.17.0.1/16 brd 172.17.255.255 scope global docker0\\n"
            "5: tailscale0    inet 100.109.119.86/32 scope global tailscale0\\n"
        )
        with mock.patch("printers._run", return_value=sample.replace("\\n", "\n")):
            nets = printers.local_scan_networks()
        self.assertEqual(len(nets), 1)
        self.assertEqual(nets[0], ipaddress.IPv4Network("10.0.0.0/24"))


if __name__ == "__main__":
    unittest.main()
