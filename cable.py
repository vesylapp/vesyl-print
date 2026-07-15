"""Minimal ActionCable client for PrintNodeChannel (/print/cable).

Protocol (Rails ActionCable):
  server → welcome | ping | confirm_subscription | reject_subscription | message
  client → subscribe | unsubscribe | message (perform)

Connect with short-lived ticket:  {cable_url}?token={ws_ticket}

Requires the ``websocket-client`` package (``pip install websocket-client`` or
Debian ``python3-websocket``). If missing, ``available`` is False and the
agent stays on HTTPS pull only.
"""

from __future__ import annotations

import json
import logging
import threading
import time
from typing import Any, Callable
from urllib.parse import parse_qsl, urlencode, urlparse, urlunparse

log = logging.getLogger("vesyl-print.cable")

CHANNEL_NAME = "PrintNodeChannel"

try:
    import websocket  # type: ignore[import-untyped]

    _HAS_WS = True
except ImportError:  # pragma: no cover
    websocket = None  # type: ignore[assignment]
    _HAS_WS = False


def websocket_available() -> bool:
    return _HAS_WS


def build_cable_connect_url(cable_url: str, ticket: str) -> str:
    """Append ?token=ticket (or &token=) to the cable WebSocket URL."""
    parts = urlparse(cable_url)
    q = dict(parse_qsl(parts.query, keep_blank_values=True))
    q["token"] = ticket
    return urlunparse(parts._replace(query=urlencode(q)))


def channel_identifier() -> str:
    return json.dumps({"channel": CHANNEL_NAME}, separators=(",", ":"))


MessageHandler = Callable[[dict[str, Any]], None]
EventHandler = Callable[[], None]


class ActionCableClient:
    """Background WebSocket client for PrintNodeChannel.

    Callbacks run on the WS thread — keep them short or enqueue work.
    ``stop()`` never blocks the caller more than a short join timeout.
    """

    def __init__(
        self,
        url: str,
        *,
        on_message: MessageHandler | None = None,
        on_connected: EventHandler | None = None,
        on_disconnected: EventHandler | None = None,
        on_subscribed: EventHandler | None = None,
    ):
        self._url = url
        self._on_message = on_message
        self._on_connected = on_connected
        self._on_disconnected = on_disconnected
        self._on_subscribed = on_subscribed

        self._ws: Any = None
        self._thread: threading.Thread | None = None
        self._stop = threading.Event()
        self._subscribed = threading.Event()
        self._connected = threading.Event()
        self._identifier = channel_identifier()
        self._send_lock = threading.Lock()

    @property
    def connected(self) -> bool:
        return self._connected.is_set() and not self._stop.is_set()

    @property
    def subscribed(self) -> bool:
        return self._subscribed.is_set() and self.connected

    def start(self) -> None:
        if not _HAS_WS:
            raise RuntimeError(
                "websocket-client not installed "
                "(apt install python3-websocket or pip install websocket-client)"
            )
        if self._thread and self._thread.is_alive():
            return
        self._stop.clear()
        self._subscribed.clear()
        self._connected.clear()
        self._thread = threading.Thread(
            target=self._run, name="vesyl-print-cable", daemon=True
        )
        self._thread.start()

    def stop(self, timeout: float = 2.0) -> None:
        """Signal the socket to close. Never hang the agent main loop."""
        self._stop.set()
        self._subscribed.clear()
        ws = self._ws
        if ws is not None:
            try:
                # Prefer closing the underlying sock; WebSocketApp.close can hang.
                sock = getattr(ws, "sock", None)
                if sock is not None:
                    try:
                        sock.shutdown(2)  # SHUT_RDWR
                    except Exception:
                        pass
                    try:
                        sock.close()
                    except Exception:
                        pass
                ws.keep_running = False
                try:
                    ws.close()
                except Exception:
                    pass
            except Exception:
                pass
        thr = self._thread
        if thr and thr.is_alive() and thr is not threading.current_thread():
            thr.join(timeout=timeout)
        self._connected.clear()
        self._thread = None
        self._ws = None

    def wait_subscribed(self, timeout: float = 10.0) -> bool:
        return self._subscribed.wait(timeout=timeout)

    def perform(self, action: str, **data: Any) -> None:
        """Invoke a channel action (heartbeat, ack_job, job_state, …)."""
        payload = {"action": action, **{k: v for k, v in data.items() if v is not None}}
        frame = {
            "command": "message",
            "identifier": self._identifier,
            "data": json.dumps(payload, separators=(",", ":")),
        }
        self._send(frame)

    def _send(self, obj: dict[str, Any]) -> None:
        raw = json.dumps(obj, separators=(",", ":"))
        with self._send_lock:
            ws = self._ws
            if ws is None or self._stop.is_set():
                raise RuntimeError("cable not connected")
            ws.send(raw)

    def _run(self) -> None:
        assert websocket is not None
        if self._stop.is_set():
            return
        log.info("cable connecting")
        try:
            self._ws = websocket.WebSocketApp(
                self._url,
                header=[],
                on_open=self._on_open,
                on_message=self._on_ws_message,
                on_error=self._on_error,
                on_close=self._on_close,
            )
            # Short ping keeps dead peers from hanging forever; sockopt timeouts help.
            self._ws.run_forever(
                ping_interval=25,
                ping_timeout=10,
                skip_utf8_validation=True,
            )
        except Exception:
            if not self._stop.is_set():
                log.exception("cable connection error")
        finally:
            self._connected.clear()
            self._subscribed.clear()
            self._ws = None
            if self._on_disconnected and not self._stop.is_set():
                try:
                    self._on_disconnected()
                except Exception:
                    log.debug("on_disconnected failed", exc_info=True)

    def _on_open(self, _ws: Any) -> None:
        log.info("cable socket open — waiting for welcome")

    def _on_close(self, _ws: Any, status: Any = None, msg: Any = None) -> None:
        log.info("cable closed status=%s", status)
        self._connected.clear()
        self._subscribed.clear()

    def _on_error(self, _ws: Any, error: Any) -> None:
        if not self._stop.is_set():
            log.warning("cable error: %s", error)

    def _on_ws_message(self, _ws: Any, message: str) -> None:
        try:
            data = json.loads(message)
        except json.JSONDecodeError:
            log.warning("cable non-JSON frame")
            return
        if not isinstance(data, dict):
            return

        typ = data.get("type")
        if typ == "welcome":
            self._connected.set()
            if self._on_connected:
                try:
                    self._on_connected()
                except Exception:
                    log.debug("on_connected failed", exc_info=True)
            self._subscribe()
            return
        if typ == "ping":
            return
        if typ == "disconnect":
            log.warning("cable disconnect: %s", data.get("reason"))
            self._stop.set()
            try:
                if self._ws:
                    self._ws.close()
            except Exception:
                pass
            return
        if typ == "confirm_subscription":
            if data.get("identifier") == self._identifier:
                log.info("cable subscribed to %s", CHANNEL_NAME)
                self._subscribed.set()
                if self._on_subscribed:
                    try:
                        self._on_subscribed()
                    except Exception:
                        log.debug("on_subscribed failed", exc_info=True)
            return
        if typ == "reject_subscription":
            log.error("cable subscription rejected")
            self._subscribed.clear()
            return

        if "message" in data:
            msg = data["message"]
            if isinstance(msg, dict) and self._on_message:
                try:
                    self._on_message(msg)
                except Exception:
                    log.exception("cable message handler failed")

    def _subscribe(self) -> None:
        frame = {
            "command": "subscribe",
            "identifier": self._identifier,
        }
        try:
            self._send(frame)
        except Exception:
            log.exception("cable subscribe send failed")


class PrintCableSession:
    """High-level session: ticket → connect → PrintNodeChannel."""

    def __init__(
        self,
        *,
        cable_url: str,
        get_ticket: Callable[[], dict[str, Any]],
        on_print_job: MessageHandler,
        on_revoke: EventHandler | None = None,
        on_job_canceled: Callable[[str], None] | None = None,
        on_subscribed: EventHandler | None = None,
        on_disconnected: EventHandler | None = None,
    ):
        self._cable_url = cable_url.rstrip("/")
        self._get_ticket = get_ticket
        self._on_print_job = on_print_job
        self._on_revoke = on_revoke
        self._on_job_canceled = on_job_canceled
        self._on_subscribed = on_subscribed
        self._on_disconnected = on_disconnected
        self._client: ActionCableClient | None = None
        # RLock: start() stops any existing client while already holding the lock.
        self._lock = threading.RLock()

    @property
    def subscribed(self) -> bool:
        c = self._client
        return bool(c and c.subscribed)

    @property
    def connected(self) -> bool:
        c = self._client
        return bool(c and c.connected)

    def start(self) -> bool:
        """Fetch ticket and start client (non-blocking handshake).

        Returns False if WS unavailable or ticket fails. Does **not** wait for
        subscription — poll ``subscribed`` or call ``wait_subscribed``.
        """
        if not _HAS_WS:
            log.warning("cable: websocket-client not installed — push disabled")
            return False
        with self._lock:
            self._stop_unlocked()
            try:
                ticket_payload = self._get_ticket()
            except Exception as e:
                log.warning("cable: ws_ticket failed: %s", e)
                return False
            ticket = ticket_payload.get("ticket")
            if not ticket:
                log.warning("cable: ws_ticket response missing ticket")
                return False
            url = build_cable_connect_url(self._cable_url, str(ticket))
            self._client = ActionCableClient(
                url,
                on_message=self._dispatch_message,
                on_subscribed=self._on_subscribed,
                on_disconnected=self._on_disconnected,
            )
            try:
                self._client.start()
            except Exception as e:
                log.warning("cable: start failed: %s", e)
                self._client = None
                return False
        return True

    def _stop_unlocked(self) -> None:
        if self._client:
            self._client.stop(timeout=1.5)
            self._client = None

    def stop(self) -> None:
        with self._lock:
            self._stop_unlocked()

    def perform(self, action: str, **data: Any) -> bool:
        c = self._client
        if not c or not c.subscribed:
            return False
        try:
            c.perform(action, **data)
            return True
        except Exception as e:
            log.warning("cable perform %s failed: %s", action, e)
            return False

    def wait_subscribed(self, timeout: float = 10.0) -> bool:
        c = self._client
        if not c:
            return False
        return c.wait_subscribed(timeout=timeout)

    def _dispatch_message(self, msg: dict[str, Any]) -> None:
        typ = msg.get("type")
        if typ == "print_job":
            job = msg.get("job")
            if isinstance(job, dict):
                self._on_print_job(job)
            else:
                log.warning("print_job message missing job object")
        elif typ == "job_canceled":
            jid = msg.get("job_id")
            if jid and self._on_job_canceled:
                self._on_job_canceled(str(jid))
        elif typ == "revoke":
            log.warning("cable: revoke received")
            if self._on_revoke:
                self._on_revoke()
        elif typ == "error":
            log.warning(
                "cable error from server: %s %s",
                msg.get("code"),
                msg.get("message"),
            )
        else:
            log.debug("cable unknown message type=%s", typ)
