"""Resistive touch (MHS-3.5 / ADS7846 / XPT2046).

Stdlib-only: reads Linux ``struct input_event`` from ``/dev/input/event*``.

A short press+release is a tap (with mapped screen coordinates). A mostly
horizontal drag becomes ``swipe_left`` / ``swipe_right``. Holding for
``long_press_s`` (default 3s) emits a long-press — used to open the LCD test
print panel.

Default axis map matches ``dtoverlay=mhs35:rotate=90`` (swap XY, invert Y).
Override with ``VESYL_TOUCH_SWAP_XY`` / ``VESYL_TOUCH_INVERT_X`` /
``VESYL_TOUCH_INVERT_Y`` if a panel is flipped.
"""

from __future__ import annotations

import array
import fcntl
import logging
import os
import select
import struct
import threading
import time
from dataclasses import dataclass
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
ABS_X = 0x00
ABS_Y = 0x01
ABS_PRESSURE = 0x18
ABS_MT_POSITION_X = 0x35
ABS_MT_POSITION_Y = 0x36

# Debounce: ignore releases sooner than this after a prior tap.
_DEFAULT_DEBOUNCE_S = 0.12
DEFAULT_LONG_PRESS_S = 3.0
DEFAULT_SWIPE_PX = 140
# ADS7846 reports 0–4095; the usable plate is inset (tslib/xorg typically ~200–3900).
_DEFAULT_ABS_MAX = 4095
_CAL_ABS_MIN = 200
_CAL_ABS_MAX = 3900


def _event_formats() -> list[tuple[str, int]]:
    """Prefer native long size, then the other width."""
    native = struct.calcsize("l")
    if native == 8:
        return [(_EVENT_FMT_64, _EVENT_SIZE_64), (_EVENT_FMT_32, _EVENT_SIZE_32)]
    return [(_EVENT_FMT_32, _EVENT_SIZE_32), (_EVENT_FMT_64, _EVENT_SIZE_64)]


def _env_bool(name: str, default: bool) -> bool:
    raw = os.environ.get(name)
    if raw is None or raw.strip() == "":
        return default
    return raw.strip().lower() in ("1", "true", "yes", "on")


@dataclass(frozen=True)
class TouchTransform:
    """Map raw ABS_X/ABS_Y onto framebuffer pixels."""

    swap_xy: bool = True
    invert_x: bool = True
    invert_y: bool = False
    xmin: int = _CAL_ABS_MIN
    xmax: int = _CAL_ABS_MAX
    ymin: int = _CAL_ABS_MIN
    ymax: int = _CAL_ABS_MAX

    @classmethod
    def mhs35_rotate90(cls) -> TouchTransform:
        """MHS-3.5 with ``dtoverlay=mhs35:rotate=90`` (480×320 landscape).

        Field-calibrated from ADS7846 logs: screen_x = 1−raw_y, screen_y = raw_x
        (swap + invert X). A tap on › at raw (1501, 688) maps onto the button
        instead of the top bar. Override with ``VESYL_TOUCH_SWAP_XY`` /
        ``VESYL_TOUCH_INVERT_X`` / ``VESYL_TOUCH_INVERT_Y`` if a panel differs.
        """
        return cls(
            swap_xy=_env_bool("VESYL_TOUCH_SWAP_XY", True),
            invert_x=_env_bool("VESYL_TOUCH_INVERT_X", True),
            invert_y=_env_bool("VESYL_TOUCH_INVERT_Y", False),
        )

    def with_abs_range(
        self,
        x_range: tuple[int, int] | None,
        y_range: tuple[int, int] | None,
    ) -> TouchTransform:
        xmin, xmax = self.xmin, self.xmax
        ymin, ymax = self.ymin, self.ymax
        # 0–4095 is the raw ADC span, not a measured plate calibration.
        if x_range is not None and x_range not in ((0, 4095), (0, 4096)):
            xmin, xmax = x_range
        if y_range is not None and y_range not in ((0, 4095), (0, 4096)):
            ymin, ymax = y_range
        if xmax < xmin:
            xmin, xmax = xmax, xmin
        if ymax < ymin:
            ymin, ymax = ymax, ymin
        return TouchTransform(
            swap_xy=self.swap_xy,
            invert_x=self.invert_x,
            invert_y=self.invert_y,
            xmin=xmin,
            xmax=xmax,
            ymin=ymin,
            ymax=ymax,
        )


def _norm(value: int, lo: int, hi: int) -> float:
    if hi <= lo:
        return 0.5
    return max(0.0, min(1.0, (value - lo) / (hi - lo)))


def map_touch_to_screen(
    raw_x: int,
    raw_y: int,
    screen_w: int,
    screen_h: int,
    transform: TouchTransform | None = None,
) -> tuple[int, int]:
    """Project raw ABS coordinates onto a ``screen_w`` × ``screen_h`` framebuffer."""
    tf = transform or TouchTransform.mhs35_rotate90()
    nx = _norm(raw_x, tf.xmin, tf.xmax)
    ny = _norm(raw_y, tf.ymin, tf.ymax)
    if tf.swap_xy:
        nx, ny = ny, nx
    if tf.invert_x:
        nx = 1.0 - nx
    if tf.invert_y:
        ny = 1.0 - ny
    w = max(1, int(screen_w))
    h = max(1, int(screen_h))
    x = int(round(nx * (w - 1)))
    y = int(round(ny * (h - 1)))
    return max(0, min(w - 1, x)), max(0, min(h - 1, y))


def classify_swipe(
    start_x: int | None,
    start_y: int | None,
    end_x: int | None,
    end_y: int | None,
    *,
    min_px: int = DEFAULT_SWIPE_PX,
) -> str | None:
    """Return ``left`` / ``right`` for a horizontal swipe, else None."""
    if start_x is None or start_y is None or end_x is None or end_y is None:
        return None
    dx = int(end_x) - int(start_x)
    dy = int(end_y) - int(start_y)
    if abs(dx) < max(8, int(min_px)):
        return None
    # Must be clearly horizontal so a tap on Test/Back is not a swipe.
    if abs(dx) < abs(dy) * 1.2:
        return None
    return "right" if dx > 0 else "left"


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


def _ioc_read(nr: int, size: int = 24) -> int:
    """``_IOR('E', nr, size)`` for EVIOCGABS."""
    return (2 << 30) | (size << 16) | (ord("E") << 8) | nr


def read_abs_range(fd: int, axis: int) -> tuple[int, int] | None:
    """``EVIOCGABS`` → (minimum, maximum), or None if the ioctl fails."""
    buf = array.array("i", [0] * 6)
    req = _ioc_read(0x40 + axis, 24)
    try:
        fcntl.ioctl(fd, req, buf, True)
    except OSError:
        return None
    lo, hi = int(buf[1]), int(buf[2])
    if lo == 0 and hi == 0:
        return None
    return lo, hi


@dataclass(frozen=True)
class TouchEvent:
    """One classified contact: tap, long-press, or horizontal swipe."""

    kind: str  # "tap" | "long_press" | "swipe_left" | "swipe_right"
    x: int | None = None
    y: int | None = None
    duration: float = 0.0
    raw_x: int | None = None
    raw_y: int | None = None
    dx: int = 0
    dy: int = 0
    direction: str | None = None


class TouchListener:
    """Background reader that signals taps / long-presses.

    Thread-safe: ``poll_event()`` returns the next event. Extra taps between
    polls are queued (long-press is never coalesced away). ``poll_tap()``
    remains a tap-only boolean for older callers.
    """

    def __init__(
        self,
        device: Path | str | None = None,
        *,
        debounce_s: float = _DEFAULT_DEBOUNCE_S,
        on_tap: Callable[[], None] | None = None,
        long_press_s: float = DEFAULT_LONG_PRESS_S,
        screen_size: tuple[int, int] | None = None,
        transform: TouchTransform | None = None,
        swipe_px: int = DEFAULT_SWIPE_PX,
    ):
        self.device = Path(device) if device else None
        self.debounce_s = debounce_s
        self.on_tap = on_tap
        self.long_press_s = max(0.2, float(long_press_s))
        self.screen_size = screen_size or (480, 320)
        self.transform = transform or TouchTransform.mhs35_rotate90()
        self.swipe_px = max(8, int(swipe_px))
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None
        self._pending = False
        self._events: list[TouchEvent] = []
        self._lock = threading.Lock()
        self._pressed = False
        self._press_mono = 0.0
        self._long_fired = False
        self._last_tap_mono = 0.0
        self._raw_x: int | None = None
        self._raw_y: int | None = None
        self._press_x: int | None = None
        self._press_y: int | None = None
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
        """Return True once if a short tap occurred since the last poll."""
        with self._lock:
            for i, ev in enumerate(self._events):
                if ev.kind == "tap":
                    self._events.pop(i)
                    self._pending = any(e.kind == "tap" for e in self._events)
                    return True
            if self._pending:
                self._pending = False
                return True
            return False

    def poll_event(self) -> TouchEvent | None:
        """Pop the next tap or long-press, or None."""
        with self._lock:
            if not self._events:
                return None
            ev = self._events.pop(0)
            self._pending = any(e.kind == "tap" for e in self._events)
            return ev

    def inject_tap(self) -> None:
        """Test/helper: record a tap without hardware."""
        self._emit("tap", duration=0.0)

    def inject_event(
        self,
        kind: str,
        *,
        x: int | None = None,
        y: int | None = None,
        duration: float = 0.0,
        dx: int = 0,
        dy: int = 0,
        direction: str | None = None,
    ) -> None:
        """Test/helper: queue a classified event."""
        self._emit(
            kind, x=x, y=y, duration=duration, dx=dx, dy=dy, direction=direction
        )

    def _mapped_xy(self) -> tuple[int | None, int | None]:
        if self._raw_x is None or self._raw_y is None:
            return None, None
        w, h = self.screen_size
        return map_touch_to_screen(
            self._raw_x, self._raw_y, w, h, self.transform
        )

    def _emit(
        self,
        kind: str,
        *,
        x: int | None = None,
        y: int | None = None,
        duration: float = 0.0,
        dx: int = 0,
        dy: int = 0,
        direction: str | None = None,
    ) -> None:
        now = time.monotonic()
        if kind == "tap" and now - self._last_tap_mono < self.debounce_s:
            return
        if kind == "tap":
            self._last_tap_mono = now
        mx, my = x, y
        if mx is None and my is None:
            mx, my = self._mapped_xy()
        ev = TouchEvent(
            kind=kind,
            x=mx,
            y=my,
            duration=duration,
            raw_x=self._raw_x,
            raw_y=self._raw_y,
            dx=dx,
            dy=dy,
            direction=direction,
        )
        with self._lock:
            self._events.append(ev)
            if kind == "tap":
                self._pending = True
        log.info(
            "touch %s raw=(%s,%s) screen=(%s,%s) dur=%.2f",
            kind,
            self._raw_x,
            self._raw_y,
            mx,
            my,
            duration,
        )
        if kind == "tap" and self.on_tap is not None:
            try:
                self.on_tap()
            except Exception:
                log.exception("touch on_tap failed")

    def _record_tap(self) -> None:
        # Back-compat name used by older tests via press/release path.
        self._emit("tap", duration=0.0)

    def _maybe_long_press(self) -> None:
        if not self._pressed or self._long_fired:
            return
        held = time.monotonic() - self._press_mono
        if held >= self.long_press_s:
            self._long_fired = True
            self._emit("long_press", duration=held)

    def _capture_press_xy(self) -> None:
        mx, my = self._mapped_xy()
        if mx is not None and my is not None:
            self._press_x, self._press_y = mx, my

    def _begin_press(self) -> None:
        self._pressed = True
        self._press_mono = time.monotonic()
        self._long_fired = False
        self._press_x, self._press_y = None, None
        self._capture_press_xy()

    def _end_press(self) -> None:
        if not self._pressed:
            return
        held = time.monotonic() - self._press_mono
        self._pressed = False
        end_x, end_y = self._mapped_xy()
        if self._long_fired:
            return
        if held >= self.long_press_s:
            self._long_fired = True
            self._emit("long_press", duration=held)
            return
        direction = classify_swipe(
            self._press_x,
            self._press_y,
            end_x,
            end_y,
            min_px=self.swipe_px,
        )
        if direction:
            dx = (end_x or 0) - (self._press_x or 0)
            dy = (end_y or 0) - (self._press_y or 0)
            self._emit(
                f"swipe_{direction}",
                x=end_x,
                y=end_y,
                duration=held,
                dx=dx,
                dy=dy,
                direction=direction,
            )
            return
        self._emit("tap", x=end_x, y=end_y, duration=held)

    def _run(self) -> None:
        assert self.device is not None
        try:
            fd = os.open(str(self.device), os.O_RDONLY | os.O_NONBLOCK)
        except OSError as e:
            log.warning("touch: open %s failed: %s", self.device, e)
            return
        try:
            xr = read_abs_range(fd, ABS_X)
            yr = read_abs_range(fd, ABS_Y)
            if xr or yr:
                self.transform = self.transform.with_abs_range(xr, yr)
                log.info(
                    "touch abs range x=%s y=%s swap=%s inv=(%s,%s)",
                    (self.transform.xmin, self.transform.xmax),
                    (self.transform.ymin, self.transform.ymax),
                    self.transform.swap_xy,
                    self.transform.invert_x,
                    self.transform.invert_y,
                )
            fmts = _event_formats()
            self._fmt, self._size = fmts[0]
            residual = b""
            while not self._stop.is_set():
                try:
                    r, _, _ = select.select([fd], [], [], 0.2)
                except (OSError, ValueError):
                    break
                self._maybe_long_press()
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
        if etype == EV_ABS:
            if code in (ABS_X, ABS_MT_POSITION_X):
                self._raw_x = int(value)
                if self._pressed and self._press_x is None:
                    self._capture_press_xy()
                return
            if code in (ABS_Y, ABS_MT_POSITION_Y):
                self._raw_y = int(value)
                if self._pressed and self._press_y is None:
                    self._capture_press_xy()
                return
            if code == ABS_PRESSURE:
                if value > 0:
                    if not self._pressed:
                        self._begin_press()
                elif self._pressed:
                    self._end_press()
                return
            return
        if etype == EV_KEY and code == BTN_TOUCH:
            if value == 1:
                self._begin_press()
            elif value == 0:
                self._end_press()


def open_touch(
    *,
    device: str | None = None,
    debounce_s: float = _DEFAULT_DEBOUNCE_S,
    long_press_s: float = DEFAULT_LONG_PRESS_S,
    screen_size: tuple[int, int] | None = None,
    transform: TouchTransform | None = None,
) -> TouchListener:
    """Create a listener for override path or auto-discovered device."""
    path = find_touch_device(override=device)
    return TouchListener(
        path,
        debounce_s=debounce_s,
        long_press_s=long_press_s,
        screen_size=screen_size,
        transform=transform,
    )
