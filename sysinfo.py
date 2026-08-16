"""Collect basic system information for display."""

from __future__ import annotations

import glob
import socket
import subprocess
from datetime import datetime
from pathlib import Path

_SKIP_IFACE_PREFIXES = (
    "lo",
    "docker",
    "br-",
    "veth",
    "virbr",
    "tailscale",
    "wg",
    "tun",
    "tap",
    "cni",
    "flannel",
    "lxc",
    "ap0",
)


def hostname() -> str:
    return socket.gethostname()


def ip_addresses() -> list[str]:
    """Return non-loopback IPv4 addresses currently assigned to the host."""
    try:
        out = subprocess.run(
            ["hostname", "-I"], capture_output=True, text=True, timeout=2
        ).stdout
    except (OSError, subprocess.SubprocessError):
        out = ""

    addrs = []
    for token in out.split():
        # keep IPv4 only (skip IPv6 which contains ':')
        if ":" not in token and token != "127.0.0.1":
            addrs.append(token)
    return addrs


def primary_ip() -> str:
    addrs = ip_addresses()
    return addrs[0] if addrs else "no network"


def tailscale_ip() -> str:
    """Tailscale IPv4 (``tailscale ip -4``), or a short fallback if unavailable."""
    try:
        out = subprocess.run(
            ["tailscale", "ip", "-4"],
            capture_output=True,
            text=True,
            timeout=2,
        )
    except (OSError, subprocess.SubprocessError):
        return "n/a"
    if out.returncode != 0:
        return "n/a"
    for line in (out.stdout or "").splitlines():
        ip = line.strip()
        if ip and ":" not in ip:
            return ip
    return "n/a"


def cpu_temp_c() -> str:
    """CPU temperature in °C, or a short fallback if unavailable.

    Prefers the first readable sysfs thermal zone (millidegrees C). On
    Raspberry Pi, falls back to `vcgencmd measure_temp` when sysfs is empty.
    """
    for path in sorted(glob.glob("/sys/class/thermal/thermal_zone*/temp")):
        try:
            with open(path, encoding="ascii") as f:
                milli = int(f.read().strip())
            return f"{milli / 1000.0:.0f} °C"
        except (OSError, ValueError):
            continue

    # Raspberry Pi firmware path
    try:
        out = subprocess.run(
            ["vcgencmd", "measure_temp"],
            capture_output=True,
            text=True,
            timeout=2,
        ).stdout.strip()
        # e.g. "temp=48.2'C"
        if out.startswith("temp=") and out.endswith("'C"):
            return f"{float(out[5:-2]):.0f} °C"
    except (OSError, subprocess.SubprocessError, ValueError):
        pass

    return "n/a"


def now() -> datetime:
    return datetime.now()


def _skip_iface(name: str) -> bool:
    n = (name or "").strip().lower()
    return any(n == p or n.startswith(p) for p in _SKIP_IFACE_PREFIXES)


def net_ifaces(sys_class_net: str | Path = "/sys/class/net") -> list[str]:
    """Kernel net device names under ``sys_class_net`` (no virtual/mesh)."""
    base = Path(sys_class_net)
    if not base.is_dir():
        return []
    names: list[str] = []
    try:
        for p in sorted(base.iterdir()):
            if not p.is_dir() or _skip_iface(p.name):
                continue
            names.append(p.name)
    except OSError:
        return []
    return names


def _read_sysfs(path: Path) -> str:
    try:
        return path.read_text(encoding="ascii", errors="replace").strip()
    except OSError:
        return ""


def iface_wireless(name: str, *, sys_class_net: str | Path = "/sys/class/net") -> bool:
    return (Path(sys_class_net) / name / "wireless").is_dir()


def iface_carrier_up(name: str, *, sys_class_net: str | Path = "/sys/class/net") -> bool:
    """True when the NIC reports carrier (cable / associated)."""
    base = Path(sys_class_net) / name
    carrier = _read_sysfs(base / "carrier")
    if carrier == "1":
        return True
    # Some drivers only expose operstate.
    return _read_sysfs(base / "operstate").lower() == "up"


def ethernet_ifaces(sys_class_net: str | Path = "/sys/class/net") -> list[str]:
    return [
        n
        for n in net_ifaces(sys_class_net)
        if not iface_wireless(n, sys_class_net=sys_class_net)
    ]


def wifi_ifaces(sys_class_net: str | Path = "/sys/class/net") -> list[str]:
    return [
        n
        for n in net_ifaces(sys_class_net)
        if iface_wireless(n, sys_class_net=sys_class_net)
    ]


def ethernet_up(sys_class_net: str | Path = "/sys/class/net") -> bool:
    """True if any non-Wi-Fi, non-virtual NIC has carrier."""
    return any(
        iface_carrier_up(n, sys_class_net=sys_class_net)
        for n in ethernet_ifaces(sys_class_net)
    )


def wifi_carrier_up(sys_class_net: str | Path = "/sys/class/net") -> bool:
    """True if a Wi-Fi NIC reports carrier (site AP or our own hotspot)."""
    return any(
        iface_carrier_up(n, sys_class_net=sys_class_net)
        for n in wifi_ifaces(sys_class_net)
    )


_SETUP_CON = "vesyl-setup"
_net_link_cache: tuple[float, tuple[str, str, bool]] | None = None


def _run_out(cmd: list[str], timeout: float = 2.0) -> str:
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
    except (OSError, subprocess.SubprocessError):
        return ""
    if r.returncode != 0:
        return ""
    return r.stdout or ""


def wifi_hotspot_active() -> bool:
    """True when our setup AP connection is the active Wi-Fi."""
    for line in _run_out(
        ["nmcli", "-t", "-f", "NAME,TYPE,DEVICE", "connection", "show", "--active"]
    ).splitlines():
        name, _, rest = line.partition(":")
        typ, _, _dev = rest.partition(":")
        if name == _SETUP_CON and typ.lower() in ("802-11-wireless", "wifi"):
            return True
    return False


def wifi_ssid() -> str | None:
    """SSID of the associated site network, or None (ignores the setup hotspot)."""
    if wifi_hotspot_active():
        return None
    ssid = _run_out(["iwgetid", "-r"]).strip()
    if ssid:
        return ssid
    for line in _run_out(
        ["nmcli", "-t", "-f", "ACTIVE,SSID", "device", "wifi"]
    ).splitlines():
        active, _, name = line.partition(":")
        if active.lower() in ("yes", "true", "1") and name.strip():
            return name.strip()
    for line in _run_out(
        ["nmcli", "-t", "-f", "NAME,TYPE,DEVICE", "connection", "show", "--active"]
    ).splitlines():
        name, _, rest = line.partition(":")
        typ, _, _dev = rest.partition(":")
        if typ.lower() in ("802-11-wireless", "wifi") and name != _SETUP_CON:
            return name or None
    return None


def network_link(
    *,
    sys_class_net: str | Path = "/sys/class/net",
    cache_s: float = 1.5,
) -> tuple[str, str, bool]:
    """``(kind, label, up)`` for the LCD: eth / wifi / none.

    Ethernet wins when the cable has carrier. Wi-Fi uses the site SSID.
    The setup hotspot is not treated as an uplink.
    """
    import time

    global _net_link_cache
    now = time.monotonic()
    if _net_link_cache is not None and cache_s > 0:
        ts, val = _net_link_cache
        if now - ts < cache_s:
            return val
    if ethernet_up(sys_class_net):
        val = ("eth", "ethernet", True)
    else:
        ssid = wifi_ssid()
        if ssid:
            val = ("wifi", ssid, True)
        else:
            val = ("none", "no network", False)
    _net_link_cache = (now, val)
    return val


def clear_network_link_cache() -> None:
    global _net_link_cache
    _net_link_cache = None
