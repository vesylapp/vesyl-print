"""Captive portal for the vesyl-print setup hotspot.

Bind only to the AP address (never 0.0.0.0 on Ethernet). Port 80 when
launched by the root helper; 8088 is fine for tests.
"""

from __future__ import annotations

import html
import json
import logging
import os
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, Callable
from urllib.parse import parse_qs, urlparse

log = logging.getLogger("vesyl-print.wifi-portal")

HELPER = os.environ.get(
    "VESYL_WIFI_HELPER", "/usr/local/lib/vesyl-print/wifi-setup"
)

_CAPTIVE = {
    "/generate_204",
    "/gen_204",
    "/hotspot-detect.html",
    "/canonical.html",
    "/ncsi.txt",
    "/connecttest.txt",
    "/success.txt",
    "/library/test/success.html",
    "/kindle-wifi/wifiredirect.html",
    "/kindle-wifi/wifistub.html",
}

ConnectFn = Callable[[str, str], dict[str, Any]]
ScanFn = Callable[[], list[dict[str, Any]]]


def _default_helper(args: list[str]) -> dict[str, Any]:
    cmd = [HELPER, *args] if os.geteuid() == 0 else ["sudo", "-n", HELPER, *args]
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=50)
    except (OSError, subprocess.SubprocessError) as e:
        return {"ok": False, "error": str(e)}
    text = (r.stdout or "").strip()
    if not text:
        return {"ok": False, "error": (r.stderr or "helper failed").strip()}
    try:
        data = json.loads(text)
    except json.JSONDecodeError:
        return {"ok": False, "error": text[:200]}
    return data if isinstance(data, dict) else {"ok": False, "error": "bad json"}


def default_scan() -> list[dict[str, Any]]:
    data = _default_helper(["scan"])
    nets = data.get("networks") if data.get("ok") else None
    return list(nets) if isinstance(nets, list) else []


def mark_joining() -> None:
    """Tell the LCD tick to leave the radio alone during a join."""
    try:
        import wifi_setup
    except ImportError:
        return
    wifi_setup.save_setup_state(joining=True, last_error=None)


def default_connect(ssid: str, password: str) -> dict[str, Any]:
    """Start the join in its own session so killing the portal cannot abort it."""
    mark_joining()
    cmd = [HELPER, "connect", "--ssid", ssid]
    if password:
        cmd.extend(["--password", password])
    if os.geteuid() != 0:
        cmd = ["sudo", "-n", *cmd]
    try:
        subprocess.Popen(
            cmd,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
    except OSError as e:
        return {"ok": False, "error": str(e)}
    return {"ok": True, "scheduled": True}


def portal_html(
    networks: list[dict[str, Any]],
    *,
    message: str = "",
    error: str = "",
) -> str:
    opts = []
    for n in networks:
        ssid = html.escape(str(n.get("ssid") or ""))
        if not ssid:
            continue
        sig = n.get("signal")
        sec = html.escape(str(n.get("security") or ""))
        label = ssid
        extra = []
        if sig not in (None, ""):
            extra.append(f"{sig}%")
        if sec:
            extra.append(sec)
        if extra:
            label = f"{ssid} ({', '.join(extra)})"
        opts.append(f'<option value="{ssid}">{label}</option>')
    select = "\n".join(opts) or '<option value="">(no networks yet)</option>'
    banner = ""
    if error:
        banner = f'<p class="err">{html.escape(error)}</p>'
    elif message:
        banner = f'<p class="ok">{html.escape(message)}</p>'
    return f"""<!DOCTYPE html>
<html lang="en"><head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>VESYL — Set up Wi-Fi</title>
<style>
  body {{ font-family: system-ui, sans-serif; background:#101218; color:#ebeef5;
         margin:0; padding:24px; }}
  h1 {{ font-size:20px; }}
  label {{ display:block; margin:14px 0 6px; color:#8c94a5; font-size:12px;
           text-transform:uppercase; letter-spacing:.06em; }}
  select, input {{ width:100%; max-width:420px; padding:10px; border-radius:8px;
                   border:1px solid #2a303c; background:#181c24; color:#ebeef5; }}
  button {{ margin-top:18px; background:#edfc33; color:#101218; border:0;
            font-weight:700; padding:12px 22px; border-radius:10px; }}
  .err {{ color:#e84848; }} .ok {{ color:#50dc78; }}
  .hint {{ color:#8c94a5; font-size:13px; }}
</style></head><body>
<h1>Set up Wi-Fi</h1>
<p class="hint">Choose the warehouse network. This device will leave setup mode after it connects.</p>
{banner}
<form method="post" action="/connect">
  <label for="ssid">Network</label>
  <select id="ssid" name="ssid">{select}</select>
  <label for="ssid_other">Or type SSID</label>
  <input id="ssid_other" name="ssid_other" type="text" autocomplete="off"/>
  <label for="password">Password</label>
  <input id="password" name="password" type="password"/>
  <button type="submit">Connect</button>
</form>
<p class="hint"><a href="/" style="color:#edfc33">Refresh list</a></p>
</body></html>
"""


CONNECT_DELAY_S = float(os.environ.get("VESYL_WIFI_CONNECT_DELAY", "2.0"))


def success_html(ssid: str) -> str:
    name = html.escape(ssid)
    return f"""<!DOCTYPE html>
<html lang="en"><head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>VESYL — Connected</title>
<style>
  body {{ font-family: system-ui, sans-serif; background:#101218; color:#ebeef5;
         margin:0; padding:32px 24px; }}
  h1 {{ font-size:22px; color:#edfc33; margin:0 0 12px; }}
  p {{ line-height:1.45; color:#c8cdd8; max-width:28em; }}
  .ok {{ color:#50dc78; }}
</style></head><body>
<h1>Trying to join</h1>
<p class="ok">This print node is joining <strong>{name}</strong>.</p>
<p>The setup Wi-Fi will drop for a few seconds. That is expected.</p>
<p>If the password is wrong, <strong>VESYL-…</strong> will come back.
Rejoin it (same PIN as the LCD) and try again. Close this page either way.</p>
</body></html>
"""


def parse_connect_body(raw: bytes) -> tuple[str, str]:
    qs = parse_qs(raw.decode("utf-8", errors="replace"), keep_blank_values=True)
    typed = (qs.get("ssid_other") or [""])[0].strip()
    picked = (qs.get("ssid") or [""])[0].strip()
    password = (qs.get("password") or [""])[0]
    return typed or picked, password


def make_handler(
    *,
    scan: ScanFn = default_scan,
    connect: ConnectFn = default_connect,
    connect_delay_s: float | None = None,
) -> type[BaseHTTPRequestHandler]:
    delay = CONNECT_DELAY_S if connect_delay_s is None else float(connect_delay_s)

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, fmt: str, *args: Any) -> None:
            log.info("%s - %s", self.address_string(), fmt % args)

        def _html(self, status: int, body: str, *, close: bool = False) -> None:
            data = body.encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Cache-Control", "no-store")
            if close:
                self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(data)
            try:
                self.wfile.flush()
            except OSError:
                pass

        def _redirect(self, loc: str, status: int = 302) -> None:
            self.send_response(status)
            self.send_header("Location", loc)
            self.send_header("Content-Length", "0")
            self.end_headers()

        def do_GET(self) -> None:  # noqa: N802
            path = urlparse(self.path).path
            # Never 204 / "Success" — phones only pop the login sheet if
            # connectivity checks fail and land on this HTML.
            if path in ("/", "/index.html", "/setup"):
                self._html(200, portal_html(scan()))
                return
            self._redirect("/")

        def do_HEAD(self) -> None:  # noqa: N802
            self._redirect("/")

        def do_POST(self) -> None:  # noqa: N802
            path = urlparse(self.path).path
            if path != "/connect":
                self.send_error(404, "not found")
                return
            try:
                length = int(self.headers.get("Content-Length") or "0")
            except ValueError:
                length = 0
            raw = self.rfile.read(max(0, min(length, 8192))) if length else b""
            ssid, password = parse_connect_body(raw)
            if not ssid:
                self._html(400, portal_html(scan(), error="Choose a network"))
                return
            # Flag before the delay so the LCD does not start a new AP.
            mark_joining()
            # Reply first. Switching off the setup AP drops this connection;
            # if we connect() before the 200, the phone shows a navigation error.
            self._html(200, success_html(ssid), close=True)
            timer = threading.Timer(
                max(0.0, delay),
                lambda: _run_connect(connect, ssid, password),
            )
            timer.daemon = True
            timer.start()

    return Handler


def _run_connect(connect: ConnectFn, ssid: str, password: str) -> None:
    try:
        result = connect(ssid, password)
    except Exception:
        log.exception("delayed Wi-Fi connect failed")
        return
    if result.get("ok"):
        log.info("joining %s", ssid)
    else:
        log.warning("join %s failed: %s", ssid, result.get("error"))


def serve(
    host: str,
    port: int = 80,
    *,
    scan: ScanFn = default_scan,
    connect: ConnectFn = default_connect,
    connect_delay_s: float | None = None,
) -> ThreadingHTTPServer:
    if host in ("0.0.0.0", "", "::"):
        raise ValueError("refusing to bind portal on all interfaces")
    httpd = ThreadingHTTPServer(
        (host, port),
        make_handler(scan=scan, connect=connect, connect_delay_s=connect_delay_s),
    )
    return httpd


def main(argv: list[str] | None = None) -> int:
    import argparse

    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(message)s")
    ap = argparse.ArgumentParser(description="VESYL Wi-Fi setup portal")
    ap.add_argument("--bind", required=True, help="AP IPv4 (not 0.0.0.0)")
    ap.add_argument("--port", type=int, default=80)
    args = ap.parse_args(argv)
    try:
        httpd = serve(args.bind, args.port)
    except ValueError as e:
        log.error("%s", e)
        return 2
    log.info("wifi portal http://%s:%s/", args.bind, args.port)
    try:
        httpd.serve_forever(poll_interval=0.5)
    except KeyboardInterrupt:
        pass
    finally:
        httpd.server_close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
