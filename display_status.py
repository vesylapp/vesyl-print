"""LCD messaging helpers (no Pillow / framebuffer dependency).

Maps agent + OTA status into short labels the display loop can paint.
Also owns paired-page ordering and idle-home logic for touch navigation.
"""

from __future__ import annotations

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
