"""Wi-Fi provisioning when Ethernet is unplugged.

Policy lives here so the LCD, portal, and ``wifi-setup`` helper share one
implementation. Privileged NetworkManager calls go through ``run_nmcli``
(injected in tests).
"""

from __future__ import annotations

import json
import logging
import os
import re
import secrets
import signal
import subprocess
import sys
import threading
import time
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any, Callable

import sysinfo

log = logging.getLogger("vesyl-print.wifi")

HOTSPOT_CON = "vesyl-setup"
DEFAULT_HELPER = "/usr/local/lib/vesyl-print/wifi-setup"
SETUP_IDLE_S = 18 * 60
RETRY_S = 20.0
PORTAL_PID = Path("/run/vesyl-print-wifi-portal.pid")
CAPTIVE_DNS_FILE = Path("/etc/NetworkManager/dnsmasq-shared.d/vesyl-captive.conf")
SETUP_STATE_FILE = Path("/run/vesyl-print-wifi-setup.json")
DEFAULT_AP_IP = "10.42.0.1"
IPTABLES_COMMENT = "vesyl-captive"
# After AP teardown the Broadcom radio needs a beat before STA scan works.
STA_SETTLE_S = 1.5
SCAN_PAUSE_S = 1.0
SCAN_ATTEMPTS = 12
CONNECT_TRIES = 3
_CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"
_WIFI_ESC = str.maketrans(
    {"\\": r"\\", ";": r"\;", ",": r"\,", ":": r"\:", '"': r"\""}
)


def setup_ssid(host: str | None = None) -> str:
    """``VESYL-`` + last 6 alphanumerics of the hostname (max 32)."""
    raw = re.sub(r"[^A-Za-z0-9]", "", (host or sysinfo.hostname()).upper())
    if raw.startswith("VESYLPRINT"):
        raw = raw[len("VESYLPRINT") :]
    elif raw.startswith("VESYL"):
        raw = raw[len("VESYL") :]
    suffix = (raw[-6:] or "SETUP").lstrip("0") or raw[-6:] or "SETUP"
    ssid = f"VESYL-{suffix}"
    return ssid[:32]


def generate_pin(*, n: int = 8) -> str:
    return "".join(secrets.choice(_CROCKFORD) for _ in range(max(4, n)))


def wifi_qr_payload(ssid: str, password: str, *, auth: str = "WPA") -> str:
    """WPA QR string phones can scan to join the setup AP."""
    ssid_e = (ssid or "").translate(_WIFI_ESC)
    if not password:
        return f"WIFI:T:nopass;S:{ssid_e};;"
    kind = "WPA" if (auth or "WPA").upper().startswith("WPA") else "WEP"
    pw_e = password.translate(_WIFI_ESC)
    return f"WIFI:T:{kind};S:{ssid_e};P:{pw_e};;"


def should_enter_setup(
    *,
    eth_up: bool,
    wifi_site: bool,
    force: bool = False,
) -> bool:
    """Start the setup AP only with no uplink, unless the operator forced it."""
    if force:
        return not eth_up
    return (not eth_up) and (not wifi_site)


def qr_image(payload: str, box: int = 190):
    """Pillow RGB image of ``payload``, or None if segno is missing."""
    try:
        import segno
        from PIL import Image
    except ImportError:
        return None
    q = segno.make(payload, error="m")
    size = max(80, int(box))
    # scale so the bitmap fits in ``size`` with a quiet zone.
    n = q.symbol_size(scale=1, border=2)[0]
    scale = max(1, size // max(1, n))
    bio = __import__("io").BytesIO()
    q.save(bio, kind="png", scale=scale, border=2, dark="#101218", light="#ffffff")
    bio.seek(0)
    img = Image.open(bio).convert("RGB")
    if img.size[0] != size:
        img = img.resize((size, size), Image.Resampling.NEAREST)
    return img


NmRun = Callable[[list[str], float], tuple[int, str, str]]


def _default_nmcli(args: list[str], timeout: float) -> tuple[int, str, str]:
    cmd = ["nmcli", *args]
    try:
        r = subprocess.run(
            cmd, capture_output=True, text=True, timeout=timeout
        )
    except (OSError, subprocess.SubprocessError) as e:
        return 1, "", str(e)
    return r.returncode, r.stdout or "", r.stderr or ""


def first_wifi_device(nm: NmRun = _default_nmcli) -> str | None:
    code, out, _err = nm(["-t", "-f", "DEVICE,TYPE", "device", "status"], 5)
    if code != 0:
        return None
    for line in out.splitlines():
        dev, _, typ = line.partition(":")
        if typ.strip() == "wifi" and dev.strip():
            return dev.strip()
    return None


def parse_active_connections(text: str) -> list[dict[str, str]]:
    rows: list[dict[str, str]] = []
    for line in (text or "").splitlines():
        parts = line.split(":")
        if len(parts) < 3:
            continue
        rows.append(
            {"name": parts[0], "type": parts[1], "device": parts[2]}
        )
    return rows


def wifi_site_up(nm: NmRun = _default_nmcli) -> bool:
    """True when wlan is associated to a real AP (not our setup hotspot)."""
    code, out, _err = nm(
        ["-t", "-f", "NAME,TYPE,DEVICE", "connection", "show", "--active"],
        5,
    )
    if code != 0:
        return False
    for row in parse_active_connections(out):
        typ = row["type"].lower()
        if typ in ("802-11-wireless", "wifi") and row["name"] != HOTSPOT_CON:
            return True
    return False


def hotspot_active(nm: NmRun = _default_nmcli) -> bool:
    code, out, _err = nm(
        ["-t", "-f", "NAME,TYPE,DEVICE", "connection", "show", "--active"],
        5,
    )
    if code != 0:
        return False
    return any(row["name"] == HOTSPOT_CON for row in parse_active_connections(out))


def hotspot_ip(nm: NmRun = _default_nmcli) -> str | None:
    code, out, _err = nm(["-g", "IP4.ADDRESS", "connection", "show", HOTSPOT_CON], 5)
    if code != 0:
        return None
    raw = (out or "").strip().split()[0] if out.strip() else ""
    if not raw:
        return None
    return raw.split("/", 1)[0]


def setup_state_path() -> Path:
    return Path(os.environ.get("VESYL_WIFI_STATE", str(SETUP_STATE_FILE)))


def load_setup_state() -> dict[str, Any]:
    path = setup_state_path()
    if not path.is_file():
        return {}
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}
    return data if isinstance(data, dict) else {}


def save_setup_state(**fields: Any) -> None:
    path = setup_state_path()
    data = load_setup_state()
    data.update(fields)
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(data), encoding="utf-8")
        os.chmod(path, 0o644)
    except OSError:
        pass


def is_secrets_error(err: str) -> bool:
    low = (err or "").lower()
    return any(
        tok in low
        for tok in (
            "secret",
            "password",
            "802-11-wireless-security",
            "(7)",
        )
    )


def is_not_found_error(err: str) -> bool:
    low = (err or "").lower()
    return any(tok in low for tok in ("not found", "no network", "no ap"))


def classify_join_error(err: str) -> str:
    """Map nmcli join failures to a short LCD / portal reason."""
    if is_secrets_error(err):
        return "Wrong password — rejoin setup Wi-Fi"
    if is_not_found_error(err) or "ssid" in (err or "").lower():
        return "Network not found — rejoin setup Wi-Fi"
    return short_hotspot_error(err) or "Join failed — rejoin setup Wi-Fi"


def short_hotspot_error(err: str) -> str:
    """Last line of nmcli noise, trimmed for the LCD."""
    line = (err or "hotspot failed").strip().splitlines()[-1].strip()
    line = re.sub(r"^Error:\s*", "", line, flags=re.I)
    line = re.sub(r"^Failed to setup a Wi-Fi hotspot:\s*", "", line, flags=re.I)
    return line[:56] or "hotspot failed"


def wifi_device_state(nm: NmRun, name: str) -> str:
    code, out, _err = nm(["-t", "-f", "DEVICE,STATE", "device", "status"], 5)
    if code != 0:
        return ""
    for line in out.splitlines():
        dev, _, state = line.partition(":")
        if dev.strip() == name:
            return state.strip().lower()
    return ""


def ensure_wifi_radio(nm: NmRun = _default_nmcli) -> tuple[bool, str]:
    """Turn the Wi-Fi radio on and wait until the NIC is usable."""
    nm(["radio", "wifi", "on"], 5)
    dev = first_wifi_device(nm)
    if not dev:
        return False, "no Wi-Fi device"
    for _ in range(16):
        state = wifi_device_state(nm, dev)
        if state and state not in ("unavailable", "unmanaged", "unrecognized"):
            return True, dev
        time.sleep(0.25)
    return False, f"{dev} still {wifi_device_state(nm, dev) or 'unavailable'}"


def captive_dns_config(ap_ip: str) -> str:
    """dnsmasq-shared snippet: sinkhole DNS + RFC 8910 captive-portal URI."""
    ip = (ap_ip or DEFAULT_AP_IP).strip() or DEFAULT_AP_IP
    return (
        "# vesyl-print setup hotspot - managed, do not edit\n"
        f"address=/#/{ip}\n"
        f"dhcp-option=114,http://{ip}/\n"
    )


def write_captive_dns(ap_ip: str, path: Path | None = None) -> Path:
    dest = path or Path(
        os.environ.get("VESYL_CAPTIVE_DNS", str(CAPTIVE_DNS_FILE))
    )
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_text(captive_dns_config(ap_ip), encoding="ascii")
    return dest


def apply_captive_redirects(
    dev: str,
    ap_ip: str,
    *,
    run: Callable[[list[str]], int] | None = None,
) -> None:
    """Send wlan TCP/80 and DNS to the portal so phones detect a captive network."""
    exe = run or _iptables
    ip = ap_ip or DEFAULT_AP_IP
    clear_captive_redirects(dev, ap_ip=ip, run=exe)
    for proto, port, dest in (
        ("tcp", "80", f"{ip}:80"),
        ("udp", "53", f"{ip}:53"),
        ("tcp", "53", f"{ip}:53"),
    ):
        exe(
            [
                "iptables",
                "-t",
                "nat",
                "-A",
                "PREROUTING",
                "-i",
                dev,
                "-p",
                proto,
                "--dport",
                port,
                "-m",
                "comment",
                "--comment",
                IPTABLES_COMMENT,
                "-j",
                "DNAT",
                "--to-destination",
                dest,
            ]
        )


def clear_captive_redirects(
    dev: str,
    *,
    ap_ip: str = DEFAULT_AP_IP,
    run: Callable[[list[str]], int] | None = None,
) -> None:
    exe = run or _iptables
    ip = ap_ip or DEFAULT_AP_IP
    for proto, port, dest in (
        ("tcp", "80", f"{ip}:80"),
        ("udp", "53", f"{ip}:53"),
        ("tcp", "53", f"{ip}:53"),
    ):
        rule = [
            "iptables",
            "-t",
            "nat",
            "-D",
            "PREROUTING",
            "-i",
            dev,
            "-p",
            proto,
            "--dport",
            port,
            "-m",
            "comment",
            "--comment",
            IPTABLES_COMMENT,
            "-j",
            "DNAT",
            "--to-destination",
            dest,
        ]
        for _ in range(8):
            if exe(rule) != 0:
                break


def _iptables(args: list[str]) -> int:
    try:
        r = subprocess.run(args, capture_output=True, text=True, timeout=5)
    except (OSError, subprocess.SubprocessError):
        return 1
    return r.returncode


def start_hotspot(
    ssid: str,
    password: str,
    *,
    nm: NmRun = _default_nmcli,
    clear_error: bool = True,
) -> dict[str, Any]:
    ok, detail = ensure_wifi_radio(nm)
    if not ok:
        return {"ok": False, "error": detail}
    dev = first_wifi_device(nm) or detail
    # NM's shared dnsmasq reads this when the hotspot comes up.
    try:
        write_captive_dns(DEFAULT_AP_IP)
    except OSError as e:
        log.warning("captive dns config: %s", e)
    # Replace any previous setup AP.
    nm(["-t", "connection", "delete", HOTSPOT_CON], 8)
    args = [
        "device",
        "wifi",
        "hotspot",
        "ifname",
        dev,
        "con-name",
        HOTSPOT_CON,
        "ssid",
        ssid,
        "password",
        password,
    ]
    code, out, err = nm(args, 30)
    if code != 0:
        return {"ok": False, "error": short_hotspot_error(err or out)}
    # IP can take a moment to appear.
    ip = None
    for _ in range(8):
        ip = hotspot_ip(nm)
        if ip:
            break
        time.sleep(0.25)
    if ip:
        if ip != DEFAULT_AP_IP:
            try:
                write_captive_dns(ip)
            except OSError:
                pass
        apply_captive_redirects(dev, ip)
    fields: dict[str, Any] = {"ssid": ssid, "pin": password}
    if clear_error:
        fields["last_error"] = None
    save_setup_state(**fields)
    return {
        "ok": True,
        "ssid": ssid,
        "device": dev,
        "ap_ip": ip,
        "connection": HOTSPOT_CON,
    }


def stop_hotspot(
    nm: NmRun = _default_nmcli, *, kill_portal: bool = True
) -> dict[str, Any]:
    if kill_portal:
        stop_portal()
    dev = first_wifi_device(nm) or "wlan0"
    clear_captive_redirects(dev, ap_ip=hotspot_ip(nm) or DEFAULT_AP_IP)
    nm(["-t", "connection", "down", HOTSPOT_CON], 8)
    nm(["-t", "connection", "delete", HOTSPOT_CON], 8)
    return {"ok": True}


def stop_portal() -> None:
    pid_path = Path(os.environ.get("VESYL_WIFI_PORTAL_PID", str(PORTAL_PID)))
    pid = 0
    if pid_path.is_file():
        try:
            pid = int(pid_path.read_text(encoding="ascii").strip())
        except (OSError, ValueError):
            pid = 0
        try:
            pid_path.unlink()
        except OSError:
            pass
    if pid > 1:
        try:
            os.kill(pid, signal.SIGTERM)
        except OSError:
            pass


def spawn_portal(ap_ip: str, *, port: int = 80) -> None:
    """Launch wifi_portal.py on the AP address (root / helper path)."""
    stop_portal()
    ip = (ap_ip or "").strip()
    if not ip or ip in ("0.0.0.0", "::"):
        return
    script = Path("/opt/vesyl-print/current/wifi_portal.py")
    if not script.is_file():
        script = Path(__file__).resolve().parent / "wifi_portal.py"
    if not script.is_file():
        log.warning("wifi_portal.py missing — LCD QR still works")
        return
    try:
        proc = subprocess.Popen(
            [sys.executable, str(script), "--bind", ip, "--port", str(port)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
    except OSError as e:
        log.warning("portal spawn failed: %s", e)
        return
    pid_path = Path(os.environ.get("VESYL_WIFI_PORTAL_PID", str(PORTAL_PID)))
    try:
        pid_path.write_text(str(proc.pid), encoding="ascii")
    except OSError:
        pass


def scan_networks(nm: NmRun = _default_nmcli) -> list[dict[str, Any]]:
    dev = first_wifi_device(nm)
    args = ["-t", "-f", "SSID,SIGNAL,SECURITY", "device", "wifi", "list"]
    if dev:
        args.extend(["ifname", dev])
    args.append("--rescan")
    args.append("yes")
    code, out, _err = nm(args, 20)
    if code != 0:
        return []
    seen: set[str] = set()
    rows: list[dict[str, Any]] = []
    for line in out.splitlines():
        parts = line.split(":")
        if not parts:
            continue
        ssid = parts[0].strip()
        if not ssid or ssid in seen:
            continue
        seen.add(ssid)
        signal = 0
        try:
            signal = int(parts[1]) if len(parts) > 1 else 0
        except ValueError:
            pass
        sec = parts[2] if len(parts) > 2 else ""
        rows.append({"ssid": ssid, "signal": signal, "security": sec})
    rows.sort(key=lambda r: r["signal"], reverse=True)
    return rows[:40]


def listed_ssids(nm: NmRun, dev: str | None) -> list[str]:
    args = ["-t", "-f", "SSID", "device", "wifi", "list"]
    if dev:
        args.extend(["ifname", dev])
    code, out, _err = nm(args, 15)
    if code != 0:
        return []
    rows: list[str] = []
    for line in (out or "").splitlines():
        ssid = line.replace("\\:", ":").strip()
        if ssid:
            rows.append(ssid)
    return rows


def wait_for_ssid(
    nm: NmRun,
    dev: str | None,
    ssid: str,
    *,
    attempts: int | None = None,
    pause_s: float | None = None,
) -> bool:
    """Rescan until ``ssid`` is visible. Needed after leaving AP mode."""
    n = SCAN_ATTEMPTS if attempts is None else attempts
    pause = SCAN_PAUSE_S if pause_s is None else pause_s
    for i in range(max(1, n)):
        if ssid in listed_ssids(nm, dev):
            return True
        args = ["device", "wifi", "rescan"]
        if dev:
            args.extend(["ifname", dev])
        nm(args, 12)
        if i + 1 < n and pause > 0:
            time.sleep(pause)
    return ssid in listed_ssids(nm, dev)


def connections_for_ssid(nm: NmRun, ssid: str) -> list[str]:
    """NM connection ids whose Wi-Fi SSID matches ``ssid``."""
    code, out, _err = nm(["-t", "-f", "NAME,TYPE", "connection", "show"], 8)
    if code != 0:
        return []
    names: list[str] = []
    for line in (out or "").splitlines():
        raw, sep, typ = line.rpartition(":")
        if not sep:
            continue
        name = raw.replace("\\:", ":").strip()
        kind = typ.strip().lower()
        if not name or name == HOTSPOT_CON:
            continue
        if kind not in ("802-11-wireless", "wifi"):
            continue
        c, val, _ = nm(
            ["-g", "802-11-wireless.ssid", "connection", "show", name], 5
        )
        if c == 0 and val.strip() == ssid:
            names.append(name)
    return names


def forget_ssid(nm: NmRun, ssid: str) -> list[str]:
    """Drop saved profiles for ``ssid`` so a new PSK is not overridden."""
    removed: list[str] = []
    for name in connections_for_ssid(nm, ssid):
        nm(["-t", "connection", "delete", name], 8)
        removed.append(name)
    return removed


def connect_site(
    ssid: str,
    password: str,
    *,
    nm: NmRun = _default_nmcli,
) -> dict[str, Any]:
    ssid = (ssid or "").strip()
    if not ssid:
        return {"ok": False, "error": "missing ssid"}
    save_setup_state(joining=True, last_error=None)
    try:
        return _connect_site_body(ssid, password, nm=nm)
    finally:
        save_setup_state(joining=False)


def _wait_sta_ready(nm: NmRun, dev: str | None) -> None:
    if not dev:
        return
    for _ in range(16):
        if not hotspot_active(nm):
            state = wifi_device_state(nm, dev)
            if state in ("disconnected", "unavailable", ""):
                break
        time.sleep(0.25)
    if wifi_device_state(nm, dev) not in ("disconnected", "unavailable", ""):
        nm(["device", "disconnect", dev], 8)
    if STA_SETTLE_S > 0:
        time.sleep(STA_SETTLE_S)


def _nm_connect(
    nm: NmRun, ssid: str, password: str, dev: str | None, *, hidden: bool
) -> tuple[int, str, str]:
    args = ["device", "wifi", "connect", ssid]
    if password:
        args.extend(["password", password])
    if dev:
        args.extend(["ifname", dev])
    if hidden:
        args.extend(["hidden", "yes"])
    return nm(args, 45)


def _recover_setup_ap(nm: NmRun, reason: str) -> bool:
    save_setup_state(last_error=reason)
    saved = load_setup_state()
    ap_ssid = str(saved.get("ssid") or "")
    ap_pin = str(saved.get("pin") or "")
    if not (ap_ssid and ap_pin):
        return False
    rec = start_hotspot(ap_ssid, ap_pin, nm=nm, clear_error=False)
    recovered = bool(rec.get("ok"))
    if recovered and rec.get("ap_ip"):
        spawn_portal(str(rec["ap_ip"]))
    save_setup_state(last_error=reason)
    return recovered


def _connect_site_body(
    ssid: str, password: str, *, nm: NmRun
) -> dict[str, Any]:
    ensure_wifi_radio(nm)
    # Do not SIGTERM the portal first — that can SIGHUP this join process.
    stop_hotspot(nm, kill_portal=False)
    dev = first_wifi_device(nm)
    _wait_sta_ready(nm, dev)
    ensure_wifi_radio(nm)
    # A prior bad password leaves a profile that beats a new good PSK.
    forget_ssid(nm, ssid)
    seen = wait_for_ssid(nm, dev, ssid)
    last = "connect failed"
    secret_tries = 0
    for attempt in range(max(1, CONNECT_TRIES)):
        code, out, err = _nm_connect(
            nm, ssid, password, dev, hidden=not seen and attempt == 0
        )
        if code == 0:
            stop_portal()
            save_setup_state(last_error=None)
            return {"ok": True, "ssid": ssid, "device": dev}
        last = (err or out or "connect failed").strip()
        if is_secrets_error(last):
            secret_tries += 1
            if secret_tries >= 2:
                break
            if STA_SETTLE_S > 0:
                time.sleep(min(STA_SETTLE_S, 2.0))
            continue
        if attempt + 1 < CONNECT_TRIES:
            seen = wait_for_ssid(nm, dev, ssid, attempts=4)
    reason = classify_join_error(last)
    recovered = _recover_setup_ap(nm, reason)
    return {"ok": False, "error": reason, "recovered": recovered}


def link_status(
    *,
    sys_class_net: str | Path = "/sys/class/net",
    nm: NmRun = _default_nmcli,
) -> dict[str, Any]:
    eth = sysinfo.ethernet_up(sys_class_net)
    site = wifi_site_up(nm)
    hot = hotspot_active(nm)
    return {
        "ok": True,
        "eth_up": eth,
        "wifi_site": site,
        "hotspot": hot,
        "uplink": bool(eth or site),
        "ap_ip": hotspot_ip(nm) if hot else None,
        "wifi_device": first_wifi_device(nm),
    }


def helper_status(
    *,
    sys_class_net: str | Path = "/sys/class/net",
    nm: NmRun = _default_nmcli,
) -> dict[str, Any]:
    st = link_status(sys_class_net=sys_class_net, nm=nm)
    st["hostname"] = sysinfo.hostname()
    err = load_setup_state().get("last_error")
    if err and st.get("hotspot"):
        st["last_error"] = err
    return st


# ── display-side controller ──────────────────────────────────────────────


@dataclass
class WifiSnapshot:
    phase: str = "idle"  # idle | starting | setup | connecting | failed | timed_out
    ssid: str = ""
    pin: str = ""
    ap_ip: str | None = None
    message: str = ""
    force: bool = False
    qr_payload: str = ""

    @property
    def show_setup(self) -> bool:
        return self.phase in ("starting", "setup", "connecting", "failed", "timed_out")


class WifiSetupController:
    """Poll link state; start/stop the setup AP. Safe to tick from a thread."""

    def __init__(
        self,
        *,
        helper: str | None = DEFAULT_HELPER,
        idle_s: float = SETUP_IDLE_S,
        run_helper: Callable[[list[str]], dict[str, Any]] | None = None,
        sys_class_net: str | Path = "/sys/class/net",
    ):
        self.helper = helper or DEFAULT_HELPER
        self.idle_s = idle_s
        self._run_helper = run_helper
        self.sys_class_net = sys_class_net
        self._lock = threading.Lock()
        self._snap = WifiSnapshot()
        self._started_mono: float | None = None
        self._retry_after: float | None = None

    def snapshot(self) -> WifiSnapshot:
        with self._lock:
            return WifiSnapshot(**asdict(self._snap))

    def request_setup(self) -> None:
        with self._lock:
            self._snap.force = True
            self._retry_after = None
            if self._snap.phase in ("failed", "timed_out"):
                self._snap.phase = "idle"
            self._snap.message = "Starting setup…"

    def _helper(self, args: list[str]) -> dict[str, Any]:
        if self._run_helper is not None:
            return self._run_helper(args)
        cmd = ["sudo", "-n", self.helper, *args]
        try:
            r = subprocess.run(
                cmd, capture_output=True, text=True, timeout=50
            )
        except (OSError, subprocess.SubprocessError) as e:
            return {"ok": False, "error": str(e)}
        text = (r.stdout or "").strip()
        if not text:
            err = (r.stderr or "helper failed").strip()
            return {"ok": False, "error": err, "code": r.returncode}
        try:
            data = json.loads(text)
        except json.JSONDecodeError:
            return {"ok": False, "error": text[:200]}
        if not isinstance(data, dict):
            return {"ok": False, "error": "bad helper json"}
        return data

    def tick(self, now_mono: float | None = None) -> WifiSnapshot:
        now = time.monotonic() if now_mono is None else now_mono
        st = self._helper(["status"])
        if load_setup_state().get("joining") and not st.get("eth_up"):
            with self._lock:
                self._snap.phase = "connecting"
                self._snap.message = "Joining network…"
            return self.snapshot()
        eth = bool(st.get("eth_up")) if "eth_up" in st else sysinfo.ethernet_up(
            self.sys_class_net
        )
        site = bool(st.get("wifi_site"))
        hot = bool(st.get("hotspot"))
        ap_ip = st.get("ap_ip")
        join_err = str(st.get("last_error") or "")

        action: str | None = None
        start_ssid = ""
        start_pin = ""
        with self._lock:
            snap = self._snap
            force = snap.force
            if eth and force:
                snap.force = False
                force = False
            want = should_enter_setup(eth_up=eth, wifi_site=site, force=force)

            setup_phases = (
                "setup", "starting", "connecting", "failed", "timed_out"
            )
            # Any real uplink (Ethernet or site Wi-Fi) closes the setup screen.
            if (eth or (site and not force)) and snap.phase in setup_phases:
                action = "stop"
            elif want and snap.phase == "idle":
                snap.phase = "starting"
                snap.ssid = setup_ssid()
                snap.pin = generate_pin()
                snap.qr_payload = wifi_qr_payload(snap.ssid, snap.pin)
                snap.message = "Starting setup…"
                snap.ap_ip = None
                self._started_mono = now
                start_ssid, start_pin = snap.ssid, snap.pin
                action = "start"
            elif want and snap.phase == "failed":
                due = self._retry_after
                if due is None or now >= due:
                    snap.phase = "starting"
                    snap.message = "Retrying hotspot…"
                    if not snap.ssid or not snap.pin:
                        snap.ssid = setup_ssid()
                        snap.pin = generate_pin()
                        snap.qr_payload = wifi_qr_payload(snap.ssid, snap.pin)
                    start_ssid, start_pin = snap.ssid, snap.pin
                    action = "start"
            elif want and snap.phase == "setup":
                if ap_ip:
                    snap.ap_ip = str(ap_ip)
                if join_err:
                    snap.message = join_err[:56]
                if (
                    self._started_mono is not None
                    and now - self._started_mono >= self.idle_s
                ):
                    action = "timeout"
            elif want and snap.phase == "connecting" and hot and join_err:
                snap.phase = "setup"
                snap.message = join_err[:56]
            elif snap.phase == "starting" and hot:
                snap.phase = "setup"
                snap.ap_ip = str(ap_ip) if ap_ip else snap.ap_ip
                snap.message = "Scan to set up Wi-Fi"

        if action == "start":
            result = self._helper(
                ["start-ap", "--ssid", start_ssid, "--password", start_pin]
            )
            with self._lock:
                if result.get("ok"):
                    self._snap.phase = "setup"
                    self._snap.ap_ip = result.get("ap_ip")
                    self._snap.message = "Scan to set up Wi-Fi"
                else:
                    self._snap.phase = "failed"
                    self._snap.message = short_hotspot_error(
                        str(result.get("error") or "setup failed")
                    )
                    self._retry_after = now + RETRY_S
        elif action in ("stop", "timeout"):
            self._helper(["stop-ap"])
            with self._lock:
                self._snap.phase = "timed_out" if action == "timeout" else "idle"
                self._snap.force = False
                self._snap.ap_ip = None
                self._snap.qr_payload = ""
                self._snap.message = (
                    "Timed out — plug Ethernet or retry"
                    if action == "timeout"
                    else ""
                )
                self._started_mono = None
                self._retry_after = None

        return self.snapshot()

    def mark_connecting(self, ssid: str) -> None:
        with self._lock:
            self._snap.phase = "connecting"
            self._snap.message = f"Connecting to {ssid}…"[:48]


def call_helper_cli(argv: list[str], *, nm: NmRun = _default_nmcli) -> int:
    """Entry for ``scripts/wifi-setup`` / installed helper."""
    if not argv:
        _print_json({"ok": False, "error": "missing command"})
        return 2
    cmd = argv[0]
    args = argv[1:]

    def opt(flag: str, default: str = "") -> str:
        if flag in args:
            i = args.index(flag)
            if i + 1 < len(args):
                return args[i + 1]
        return default

    if cmd == "status":
        _print_json(helper_status(nm=nm))
        return 0
    if cmd == "start-ap":
        ssid = opt("--ssid") or setup_ssid()
        password = opt("--password") or generate_pin()
        out = start_hotspot(ssid, password, nm=nm)
        out.setdefault("ssid", ssid)
        if out.get("ok") and out.get("ap_ip"):
            spawn_portal(str(out["ap_ip"]))
        _print_json(out)
        return 0 if out.get("ok") else 1
    if cmd == "stop-ap":
        _print_json(stop_hotspot(nm=nm))
        return 0
    if cmd == "scan":
        _print_json({"ok": True, "networks": scan_networks(nm=nm)})
        return 0
    if cmd == "connect":
        out = connect_site(opt("--ssid"), opt("--password"), nm=nm)
        _print_json(out)
        return 0 if out.get("ok") else 1
    _print_json({"ok": False, "error": f"unknown command: {cmd}"})
    return 2


def _print_json(data: dict[str, Any]) -> None:
    sys.stdout.write(json.dumps(data, separators=(",", ":")) + "\n")


def main(argv: list[str] | None = None) -> int:
    return call_helper_cli(list(sys.argv[1:] if argv is None else argv))


if __name__ == "__main__":
    raise SystemExit(main())
