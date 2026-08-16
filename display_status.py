"""LCD messaging helpers (no Pillow / framebuffer dependency).

Maps agent + OTA status into short labels the display loop can paint.
Also owns paired-page ordering and idle-home logic for touch navigation.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Any

import update as update_mod
from config import AGENT_VERSION

# RGB tuples kept here so tests can assert colors without importing main.
OK = (80, 220, 120)
DOWN = (232, 72, 72)
WARN = (255, 180, 60)

# Paired multi-page navigation (touch cycles; idle returns to ops).
PAGE_OPS = "ops"
PAGE_NETWORK = "network"
PAGE_SYSTEM = "system"
PAIRED_PAGES: tuple[str, ...] = (PAGE_OPS, PAGE_NETWORK, PAGE_SYSTEM)
IDLE_HOME_SECONDS = 10.0

# Long-press overlay (not part of the page cycle).
PAGE_TEST = "test"
LONG_PRESS_SECONDS = 3.0
TEST_IDLE_SECONDS = 30.0


def format_agent_version(version: str | None) -> str:
    """Normalize to a short ``vX.Y.Z`` label for the footer."""
    v = (version or AGENT_VERSION or "").strip()
    if not v:
        return ""
    if not v.lower().startswith("v"):
        v = f"v{v}"
    return v


def normalize_page(page: str | None) -> str:
    """Return a valid paired page id (default ops)."""
    p = (page or PAGE_OPS).strip().lower()
    if p in PAIRED_PAGES:
        return p
    return PAGE_OPS


def advance_page(current: str | None) -> str:
    """Next page in the paired cycle (wraps)."""
    pages = PAIRED_PAGES
    cur = normalize_page(current)
    try:
        i = pages.index(cur)
    except ValueError:
        return pages[0]
    return pages[(i + 1) % len(pages)]


def page_after_idle(
    current: str | None,
    last_input_mono: float | None,
    now_mono: float,
    *,
    idle_seconds: float = IDLE_HOME_SECONDS,
) -> str:
    """Snap to Ops after idle_seconds without input; otherwise keep current."""
    cur = normalize_page(current)
    if cur == PAGE_OPS:
        return PAGE_OPS
    if last_input_mono is None:
        return PAGE_OPS
    if now_mono - last_input_mono >= idle_seconds:
        return PAGE_OPS
    return cur


class PageState:
    """Paired multi-page cursor: tap advances; idle returns to Ops.

    Unpaired callers should not call ``note_tap`` (or pass ``paired=False``).
    """

    def __init__(
        self,
        initial: str = PAGE_OPS,
        *,
        idle_seconds: float = IDLE_HOME_SECONDS,
    ):
        self.page = normalize_page(initial)
        self.last_input_mono: float | None = None
        self.idle_seconds = idle_seconds

    def note_tap(self, *, paired: bool, now_mono: float) -> str:
        if not paired:
            return self.page
        self.page = advance_page(self.page)
        self.last_input_mono = now_mono
        return self.page

    def set_page(self, page: str, *, now_mono: float | None = None) -> str:
        self.page = normalize_page(page)
        if self.page != PAGE_OPS and now_mono is not None:
            self.last_input_mono = now_mono
        return self.page

    def sync(self, *, paired: bool, now_mono: float) -> str:
        """Reset when unpaired; apply idle-home when paired."""
        if not paired:
            self.page = PAGE_OPS
            self.last_input_mono = None
            return self.page
        self.page = page_after_idle(
            self.page,
            self.last_input_mono,
            now_mono,
            idle_seconds=self.idle_seconds,
        )
        return self.page


def test_print_formats(supports_raw: bool) -> tuple[str, ...]:
    """Formats offered for a queue: PDF+ZPL on raw/Zebra, PDF only otherwise."""
    if supports_raw:
        return ("pdf", "zpl")
    return ("pdf",)


def test_print_default_format(supports_raw: bool) -> str:
    """LCD Test button: native ZPL on raw/Zebra, PDF everywhere else."""
    return "zpl" if supports_raw else "pdf"


@dataclass
class HitRect:
    """Screen-space tap target produced while rendering the test overlay."""

    id: str
    x: int
    y: int
    w: int
    h: int
    payload: dict[str, Any] = field(default_factory=dict)

    def contains(self, px: int, py: int, pad: int = 0) -> bool:
        p = max(0, int(pad))
        return (
            self.x - p <= px < self.x + self.w + p
            and self.y - p <= py < self.y + self.h + p
        )


def hit_test(
    rects: list[HitRect],
    x: int | None,
    y: int | None,
    *,
    pad: int = 0,
) -> HitRect | None:
    if x is None or y is None:
        return None
    px, py = int(x), int(y)
    for r in rects:
        if r.contains(px, py, pad=pad):
            return r
    return None


def coarse_test_action(
    x: int | None,
    y: int | None,
    w: int,
    h: int,
) -> str | None:
    """Fallback zones when a tap misses the painted buttons.

    Bottom band: left = Back, right = Test. Side strips: prev / next.
    """
    if x is None or y is None or w < 1 or h < 1:
        return None
    px, py = int(x), int(y)
    if py >= h - 88:
        return "close" if px < w // 2 else "test"
    if px <= 64:
        return "prev"
    if px >= w - 64:
        return "next"
    return None


def layout_test_print(
    *,
    w: int,
    h: int,
    body_top: int,
    printer: dict[str, Any] | None,
    footer_reserve: int = 56,
    btn_h: int = 48,
    pad: int = 16,
    gap: int = 10,
    nav_y: int | None = None,
    nav_size: int = 52,
    show_nav: bool = True,
) -> list[HitRect]:
    """Hit targets: ‹ › beside the printer name, Back | Test on the bottom row."""
    inner_w = max(40, w - 2 * pad)
    col_w = max(36, (inner_w - gap) // 2)
    btn_y = h - footer_reserve - btn_h
    if btn_y < body_top:
        btn_y = max(body_top, 0)
    raw = bool(printer and printer.get("supports_raw"))
    fmt = test_print_default_format(raw)
    ny = body_top if nav_y is None else nav_y
    ns = max(40, int(nav_size))
    rects: list[HitRect] = []
    if show_nav:
        rects.append(
            HitRect(
                id="prev",
                x=pad,
                y=ny,
                w=ns,
                h=ns,
                payload={"kind": "prev", "label": "‹"},
            )
        )
        rects.append(
            HitRect(
                id="next",
                x=max(pad + ns, w - pad - ns),
                y=ny,
                w=ns,
                h=ns,
                payload={"kind": "next", "label": "›"},
            )
        )
    rects += [
        HitRect(
            id="back",
            x=pad,
            y=btn_y,
            w=col_w,
            h=btn_h,
            payload={"kind": "back", "label": "Back"},
        ),
        HitRect(
            id="test",
            x=pad + col_w + gap,
            y=btn_y,
            w=col_w,
            h=btn_h,
            payload={
                "kind": "test",
                "format": fmt,
                "label": f"Test {fmt.upper()}",
            },
        ),
    ]
    return rects


class TestPrintState:
    """Modal test-print overlay opened by a 3s screen hold.

    One printer at a time. Side ‹ › buttons cycle the queue; Test sends the
    default format for that queue; Back returns home. Missed taps stay here
    (page cycling is disabled while open).
    """

    def __init__(self, *, idle_seconds: float = TEST_IDLE_SECONDS):
        self.open = False
        self.index = 0
        self.selected: dict[str, Any] | None = None
        self.message: str | None = None
        self.busy = False
        self.last_input_mono: float | None = None
        self.idle_seconds = idle_seconds

    def open_panel(self, now_mono: float) -> None:
        self.open = True
        self.index = 0
        self.selected = None
        self.message = None
        self.busy = False
        self.last_input_mono = now_mono

    def close(self) -> None:
        self.open = False
        self.index = 0
        self.selected = None
        self.message = None
        self.busy = False
        self.last_input_mono = None

    def note_input(self, now_mono: float) -> None:
        self.last_input_mono = now_mono

    def sync_printers(self, printers: list[dict[str, Any]]) -> dict[str, Any] | None:
        """Clamp index and refresh ``selected`` from the live inventory."""
        if not printers:
            self.index = 0
            self.selected = None
            return None
        if self.index < 0 or self.index >= len(printers):
            self.index = 0
        self.selected = printers[self.index]
        return self.selected

    def cycle(self, delta: int, count: int) -> None:
        if count <= 0:
            self.index = 0
            return
        self.index = (self.index + int(delta)) % count
        self.message = None

    def sync_idle(self, now_mono: float) -> bool:
        """Close after idle (unless a print is in flight). Returns ``open``."""
        if not self.open:
            return False
        if self.busy:
            return True
        if self.last_input_mono is None:
            self.close()
            return False
        if now_mono - self.last_input_mono >= self.idle_seconds:
            self.close()
            return False
        return True


def apply_test_hit(
    state: TestPrintState,
    hit: HitRect | None,
    now_mono: float,
) -> str | None:
    """Handle a tap on the overlay. Misses stay put (do not close or page-cycle)."""
    if not state.open:
        return None
    state.note_input(now_mono)
    if state.busy:
        return None
    kind = hit.payload.get("kind") if hit is not None else None
    if kind == "back":
        return "close"
    if kind == "test":
        raw = bool(state.selected and state.selected.get("supports_raw"))
        fmt = str(hit.payload.get("format") or test_print_default_format(raw))
        if fmt not in ("pdf", "zpl"):
            fmt = test_print_default_format(raw)
        return f"print:{fmt}"
    if kind == "prev":
        return "prev"
    if kind == "next":
        return "next"
    return None


def apply_test_swipe(
    state: TestPrintState,
    direction: str,
    count: int,
    now_mono: float,
) -> None:
    """Swipe left → next printer, swipe right → previous."""
    if not state.open:
        return
    state.note_input(now_mono)
    if state.busy or count <= 0:
        return
    if direction == "left":
        state.cycle(1, count)
    elif direction == "right":
        state.cycle(-1, count)


def identity_line(
    *,
    warehouse_name: str | None = None,
    organization_name: str | None = None,
    node_name: str | None = None,
) -> str:
    """One-line identity for the Ops header (warehouse · node)."""
    left = (warehouse_name or organization_name or "").strip() or "—"
    right = (node_name or "").strip()
    if right:
        return f"{left} · {right}"
    return left


def heartbeat_age_label(
    last_heartbeat_at: str | None,
    *,
    now: datetime | None = None,
) -> str:
    """Human age of last heartbeat, e.g. ``12s ago``, or ``—`` if unknown."""
    if not last_heartbeat_at:
        return "—"
    raw = last_heartbeat_at.strip()
    if not raw:
        return "—"
    try:
        # Accept trailing Z
        if raw.endswith("Z"):
            raw = raw[:-1] + "+00:00"
        ts = datetime.fromisoformat(raw)
        if ts.tzinfo is None:
            ts = ts.replace(tzinfo=timezone.utc)
    except ValueError:
        return "—"

    now_dt = now or datetime.now(timezone.utc)
    if now_dt.tzinfo is None:
        now_dt = now_dt.replace(tzinfo=timezone.utc)
    age = max(0, int((now_dt - ts).total_seconds()))
    if age < 60:
        return f"{age}s ago"
    if age < 3600:
        return f"{age // 60}m ago"
    if age < 86400:
        return f"{age // 3600}h ago"
    return f"{age // 86400}d ago"


# Local mute for unknown printer status (avoid importing main).
_MUTED = (140, 148, 165)


def printer_status_color(status: str | None) -> tuple[int, int, int]:
    """Dot color for a CUPS/IPP status string."""
    s = (status or "").strip().lower()
    if s in ("idle", "online"):
        return OK
    if s in ("printing", "processing"):
        return WARN
    if s in ("stopped", "offline", "error"):
        return DOWN
    if not s or s == "unknown":
        return _MUTED
    return WARN


def printer_status_label(
    status: str | None, status_message: str | None = None
) -> str:
    """Short right-hand text for an Ops printer row."""
    msg = (status_message or "").strip()
    if msg:
        return msg
    s = (status or "").strip().lower()
    if not s:
        return "unknown"
    return s


def jobs_strip_label(queued: int) -> str:
    """Compact jobs line for Ops."""
    n = max(0, int(queued))
    if n == 0:
        return "JOBS  queue 0 · idle"
    return f"JOBS  queue {n}"


def count_queue_jobs(queue_dir: Any) -> int:
    """Count ``*.json`` job files under the durable queue directory."""
    from pathlib import Path

    p = Path(queue_dir) if queue_dir is not None else None
    if p is None or not p.is_dir():
        return 0
    try:
        return sum(1 for f in p.iterdir() if f.is_file() and f.suffix == ".json")
    except OSError:
        return 0


def _looks_like_post_activate_glitch(ust: update_mod.UpdateStatus) -> bool:
    """True when activate likely succeeded but status was marked failed (self-restart).

    Classic case: ``apply-update restart`` SIGTERMs the agent while it is still
    waiting; status becomes ``failed`` even though ``current`` already points at
    the new release. LCD should show Verifying…, not Update failed.
    """
    target = (ust.target_version or "").strip()
    if not target:
        return False
    err = (ust.last_error or "").lower()
    if "sigterm" in err or "apply-update" in err and "restart" in err:
        return True
    # After activate we set current_version == target before restart.
    cur = (ust.current_version or "").strip()
    if cur and update_mod.version_cmp(cur, target) == 0:
        return True
    return False


def ota_display_message(
    ust: update_mod.UpdateStatus | None,
) -> tuple[str, tuple[int, int, int]] | None:
    """Map update_status → (footer label, color) for the LCD, or None if idle."""
    if ust is None:
        return None
    target = (ust.target_version or "").strip().lstrip("v")
    s = ust.status

    if s == update_mod.STATUS_DOWNLOADING:
        label = f"Updating {target}…".strip() if target else "Updating…"
        return label, WARN
    if s == update_mod.STATUS_INSTALLING:
        label = f"Installing {target}…".strip() if target else "Installing…"
        return label, WARN
    if s == update_mod.STATUS_PENDING_HEALTH:
        label = f"Verifying {target}…".strip() if target else "Verifying…"
        return label, WARN
    if s == update_mod.STATUS_FAILED:
        # Don't flash red "Update failed" for self-restart false negatives.
        if _looks_like_post_activate_glitch(ust):
            label = f"Verifying {target}…".strip() if target else "Verifying…"
            return label, WARN
        return "Update failed", DOWN
    if s == update_mod.STATUS_ROLLED_BACK:
        return "Rolled back", WARN
    return None
