"""Resistive touch (MHS-3.5 / ADS7846 / XPT2046) → whole-screen taps.

Stdlib-only: reads Linux ``struct input_event`` from ``/dev/input/event*``.
A tap is press then release (BTN_TOUCH or ABS_PRESSURE). Coordinates are
ignored in v1 — any contact advances the LCD page.
"""

from __future__ import annotations

import logging
import os
import select
import struct
import threading
import time
from pathlib import Path
from typing import Callable

log = logging.getLogger("vesyl-print.touch")

# Linux input_event: timeval (2× long) + type, code, value
# On 64-bit: long is 8 bytes → 24-byte event; on 32-bit: 16-byte.
_EVENT_FMT_64 = "llHHi"
_EVENT_SIZE_64 = struct.calcsize(_EVENT_FMT_64)
_EVENT_FMT_32 = "iiHHi"
_EVENT_SIZE_32 = struct.calcsize(_EVENT_FMT_32)

EV_SYN = 0x00
EV_KEY = 0x01
EV_ABS = 0x03
BTN_TOUCH = 0x14A
ABS_PRESSURE = 0x18

# Debounce: ignore releases sooner than this after a prior tap.
_DEFAULT_DEBOUNCE_S = 0.12


def _event_formats() -> list[tuple[str, int]]:
    """Prefer native long size, then the other width."""
    native = struct.calcsize("l")
    if native == 8:
        return [(_EVENT_FMT_64, _EVENT_SIZE_64), (_EVENT_FMT_32, _EVENT_SIZE_32)]
    return [(_EVENT_FMT_32, _EVENT_SIZE_32), (_EVENT_FMT_64, _EVENT_SIZE_64)]


def find_touch_device(
    *,
    override: str | None = None,
    input_dir: str | Path = "/dev/input",
) -> Path | None:
    """Locate the ADS7846/XPT2046 (or generic) touchscreen event node.

    ``override`` is used when set and the path exists. Otherwise scans
    ``/dev/input/event*`` via sysfs name (``ADS7846``, ``Touchscreen``,
    ``XPT2046``, ``Goodix``, etc.).
    """
    if override:
        p = Path(override)
        return p if p.exists() else None

    base = Path(input_dir)
    if not base.is_dir():
        return None

    name_hints = (
        "ads7846",
        "xpt2046",
        "touchscreen",
        "goodix",
        "ft5x",
        "stmpe",
    )
    candidates: list[Path] = []
    for event in sorted(base.glob("event*")):
        name = _device_name(event)
        if not name:
            continue
        low = name.lower()
        if any(h in low for h in name_hints):
            candidates.append(event)

    if candidates:
        return candidates[0]

    # Last resort: first event node (some images only expose one input).
    events = sorted(base.glob("event*"))
    return events[0] if len(events) == 1 else None


def _device_name(event_path: Path) -> str | None:
    """Read input device name from sysfs."""
    # /dev/input/eventN → /sys/class/input/eventN/device/name
    try:
        sysfs = Path("/sys/class/input") / event_path.name / "device" / "name"
        if sysfs.is_file():
            return sysfs.read_text(encoding="utf-8", errors="replace").strip()
    except OSError:
        pass
    return None


def parse_events(
    buf: bytes, fmt: str, size: int
) -> list[tuple[int, int, int]]:
    """Decode raw bytes into (type, code, value) tuples."""
    out: list[tuple[int, int, int]] = []
    for i in range(0, len(buf) - size + 1, size):
        chunk = buf[i : i + size]
        if len(chunk) < size:
            break
        try:
            _sec, _usec, etype, code, value = struct.unpack(fmt, chunk)
        except struct.error:
            break
        out.append((etype, code, value))
    return out


class TouchListener:
    """Background reader that signals whole-screen taps via a callback or queue.

    Thread-safe: ``poll_tap()`` returns True at most once per physical tap
    (extra taps between polls are coalesced to a single pending flag).
    """

    def __init__(
        self,
        device: Path | str | None = None,
        *,
        debounce_s: float = _DEFAULT_DEBOUNCE_S,
        on_tap: Callable[[], None] | None = None,
    ):
        self.device = Path(device) if device else None
        self.debounce_s = debounce_s
        self.on_tap = on_tap
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None
        self._pending = False
        self._lock = threading.Lock()
        self._pressed = False
        self._last_tap_mono = 0.0
        self._fmt: str | None = None
        self._size = 0

    @property
    def available(self) -> bool:
        return self.device is not None and self.device.exists()

    def start(self) -> bool:
        """Open device and start reader thread. Returns False if unavailable."""
        if not self.available:
            log.info("touch: no device (page cycle disabled)")
            return False
        self._stop.clear()
        self._thread = threading.Thread(
            target=self._run, name="vesyl-touch", daemon=True
        )
        self._thread.start()
        log.info("touch: listening on %s", self.device)
        return True

    def stop(self) -> None:
        self._stop.set()
        t = self._thread
        if t is not None and t.is_alive():
            t.join(timeout=1.5)
        self._thread = None

    def poll_tap(self) -> bool:
        """Return True once if a tap occurred since the last poll."""
        with self._lock:
            if self._pending:
                self._pending = False
                return True
            return False

    def inject_tap(self) -> None:
        """Test/helper: record a tap without hardware."""
        self._record_tap()

    def _record_tap(self) -> None:
        now = time.monotonic()
        if now - self._last_tap_mono < self.debounce_s:
            return
        self._last_tap_mono = now
        with self._lock:
            self._pending = True
        if self.on_tap is not None:
            try:
                self.on_tap()
            except Exception:
                log.exception("touch on_tap failed")

    def _run(self) -> None:
        assert self.device is not None
        try:
            fd = os.open(str(self.device), os.O_RDONLY | os.O_NONBLOCK)
        except OSError as e:
            log.warning("touch: open %s failed: %s", self.device, e)
            return
        try:
            # Probe event size on first full read
            fmts = _event_formats()
            self._fmt, self._size = fmts[0]
            residual = b""
            while not self._stop.is_set():
                try:
                    r, _, _ = select.select([fd], [], [], 0.25)
                except (OSError, ValueError):
                    break
                if not r:
                    continue
                try:
                    chunk = os.read(fd, self._size * 32)
                except BlockingIOError:
                    continue
                except OSError as e:
                    log.warning("touch: read failed: %s", e)
                    break
                if not chunk:
                    continue
                residual += chunk
                residual = self._consume(residual, fmts)
        finally:
            try:
                os.close(fd)
            except OSError:
                pass

    def _consume(
        self, buf: bytes, fmts: list[tuple[str, int]]
    ) -> bytes:
        """Process buffer; return unconsumed tail."""
        # Auto-detect size if we have enough bytes and events look wrong.
        if self._fmt is None or self._size == 0:
            self._fmt, self._size = fmts[0]

        size = self._size
        fmt = self._fmt
        # If buffer length is only compatible with the other format, switch.
        if len(buf) >= max(s for _, s in fmts):
            # Prefer exact multiple of chosen size
            if len(buf) % size != 0:
                for f, s in fmts:
                    if len(buf) % s == 0:
                        fmt, size = f, s
                        self._fmt, self._size = f, s
                        break

        while len(buf) >= size:
            events = parse_events(buf[:size], fmt, size)
            buf = buf[size:]
            for etype, code, value in events:
                self._handle_event(etype, code, value)
        return buf

    def _handle_event(self, etype: int, code: int, value: int) -> None:
        # Press / release via BTN_TOUCH
        if etype == EV_KEY and code == BTN_TOUCH:
            if value == 1:
                self._pressed = True
            elif value == 0 and self._pressed:
                self._pressed = False
                self._record_tap()
            return
        # Some drivers only report ABS_PRESSURE
        if etype == EV_ABS and code == ABS_PRESSURE:
            if value > 0:
                self._pressed = True
            elif self._pressed:
                self._pressed = False
                self._record_tap()


def open_touch(
    *,
    device: str | None = None,
    debounce_s: float = _DEFAULT_DEBOUNCE_S,
) -> TouchListener:
    """Create a listener for override path or auto-discovered device."""
    path = find_touch_device(override=device)
    return TouchListener(path, debounce_s=debounce_s)
