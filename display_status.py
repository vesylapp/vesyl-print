"""LCD messaging helpers and the display's glue to the Rust agent.

Maps agent + OTA status into short labels the display loop can paint.
Also owns paired-page ordering and idle-home logic for touch navigation.
No Pillow / framebuffer dependency.

The display shares files and a CLI with the agent, never Python modules:

- ``update_status.json`` (the agent's OTA state): :func:`read_update_status`
  and the ``STATUS_*`` values;
- printer rows built from the agent's ``printers.json`` (:func:`printer_rows`);
- the ``vesyl-print`` binary of this release slot (:data:`VESYL_PRINT_BIN`),
  run with ``--json`` for the test print (:func:`submit_test_print`) and the
  stream page's claim (:func:`run_vesyl_print`).
"""

from __future__ import annotations

import json
import logging
import subprocess
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from config import AGENT_VERSION

log = logging.getLogger("vesyl-print.display")

# The running release slot (``/opt/vesyl-print/releases/<ver>/``, symlinks
# resolved): main.py, VERSION and the vesyl-print binary sit side by side.
SLOT_DIR = Path(__file__).resolve().parent
VESYL_PRINT_BIN = SLOT_DIR / "vesyl-print"
TEST_PRINT_TIMEOUT_S = 90.0

# update_status.json ``status`` values (written by the agent's OTA code).
STATUS_IDLE = "idle"
STATUS_DOWNLOADING = "downloading"
STATUS_INSTALLING = "installing"
STATUS_PENDING_HEALTH = "pending_health"
STATUS_FAILED = "failed"
STATUS_ROLLED_BACK = "rolled_back"

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
PAGE_WIFI = "wifi"
LONG_PRESS_SECONDS = 3.0
TEST_IDLE_SECONDS = 30.0


def network_status_color(up: bool) -> tuple[int, int, int]:
    return OK if up else DOWN


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
    """Hit targets: ‹ › beside the name; Back | Wi-Fi | Test on the bottom row."""
    inner_w = max(40, w - 2 * pad)
    col_w = max(32, (inner_w - 2 * gap) // 3)
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
            id="wifi",
            x=pad + col_w + gap,
            y=btn_y,
            w=col_w,
            h=btn_h,
            payload={"kind": "wifi", "label": "Wi-Fi"},
        ),
        HitRect(
            id="test",
            x=pad + 2 * (col_w + gap),
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
    if kind == "wifi":
        return "wifi"
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
    p = Path(queue_dir) if queue_dir is not None else None
    if p is None or not p.is_dir():
        return 0
    try:
        return sum(1 for f in p.iterdir() if f.is_file() and f.suffix == ".json")
    except OSError:
        return 0


def printer_rows(items: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Ops / test-print rows from the agent's printer inventory items."""
    return [
        {
            "name": str(item.get("display_name") or item.get("cups_name") or "—"),
            "cups_name": str(item.get("cups_name") or ""),
            "status": item.get("status"),
            "message": item.get("status_message"),
            "supports_raw": bool(item.get("supports_raw")),
        }
        for item in items
    ]


# ── vesyl-print CLI ─────────────────────────────────────────────────


class CliError(Exception):
    """A failed ``vesyl-print`` call; ``message`` is short and safe to show."""

    def __init__(self, message: str, *, code: str | None = None):
        super().__init__(message)
        self.message = message
        self.code = code


def cli_option(flag: str, value: str) -> list[str]:
    """``[flag, value]``, or ``[flag=value]`` when the value starts with ``-``
    (clap would otherwise read it as another option)."""
    if value.startswith("-"):
        return [f"{flag}={value}"]
    return [flag, value]


def _json_object(text: str) -> dict[str, Any] | None:
    """The JSON object a ``--json`` command printed (whole output, else the
    last line holding one)."""
    candidates = [text.strip()] + [ln.strip() for ln in reversed(text.splitlines())]
    for chunk in candidates:
        if not chunk.startswith("{"):
            continue
        try:
            data = json.loads(chunk)
        except ValueError:
            continue
        if isinstance(data, dict):
            return data
    return None


def _stderr_reason(stderr: str) -> str:
    lines = [ln.strip() for ln in stderr.splitlines() if ln.strip()]
    for ln in lines:
        if ln.lower().startswith("error"):
            return ln[:200]
    return lines[-1][:200] if lines else ""


def run_vesyl_print(
    args: list[str],
    *,
    timeout: float,
    env: dict[str, str] | None = None,
    binary: str | Path | None = None,
) -> dict[str, Any]:
    """Run ``vesyl-print <args>`` (a ``--json`` command) and return its JSON object.

    Not being able to run it, a timeout, or output without a JSON object also
    come back as ``{"ok": False, "error": <short reason>}``, so callers handle
    one shape. A JSON ``ok: true`` with a non-zero exit status counts as failed.
    """
    exe = str(binary or VESYL_PRINT_BIN)
    what = args[0] if args else "vesyl-print"
    try:
        proc = subprocess.run(
            [exe, *args],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=timeout,
            env=env,
            check=False,
        )
    except subprocess.TimeoutExpired:
        log.warning("vesyl-print %s timed out after %gs", what, timeout)
        return {"ok": False, "error": f"timed out after {timeout:g}s"}
    except FileNotFoundError:
        log.warning("vesyl-print %s: %s not found", what, exe)
        return {"ok": False, "error": "vesyl-print not found"}
    except OSError as e:
        log.warning("vesyl-print %s: cannot run %s: %s", what, exe, e)
        return {"ok": False, "error": f"cannot run vesyl-print: {e.strerror or e}"}
    except ValueError as e:  # e.g. an argument holding a NUL byte
        log.warning("vesyl-print %s: bad arguments: %s", what, e)
        return {"ok": False, "error": f"cannot run vesyl-print: {e}"}

    data = _json_object(proc.stdout or "")
    if data is None or (data.get("ok") is True and proc.returncode != 0):
        reason = _stderr_reason(proc.stderr or "")
        if not reason:
            if proc.returncode < 0:
                reason = f"vesyl-print killed by signal {-proc.returncode}"
            else:
                reason = f"vesyl-print exited with status {proc.returncode}"
        log.warning(
            "vesyl-print %s failed (exit %s): %s",
            what,
            proc.returncode,
            (proc.stderr or proc.stdout or "").strip()[-500:],
        )
        return {"ok": False, "error": reason}
    if data.get("ok") is not True:
        log.info("vesyl-print %s: %s", what, data.get("error"))
    return data


def submit_test_print(
    cups_name: str,
    fmt: str,
    *,
    binary: str | Path | None = None,
    timeout: float = TEST_PRINT_TIMEOUT_S,
) -> str:
    """Print the sample label: ``vesyl-print test-print --queue Q --format F --json``.

    The binary queues it in a private job store and returns once ``lp`` took
    it. Returns the job state (``delivered``); raises :class:`CliError`.
    """
    queue = (cups_name or "").strip()
    kind = (fmt or "").strip().lower()
    out = run_vesyl_print(
        [
            "test-print",
            *cli_option("--queue", queue),
            *cli_option("--format", kind),
            "--json",
        ],
        timeout=timeout,
        binary=binary,
    )
    if out.get("ok") is not True:
        code = out.get("code")
        raise CliError(
            str(out.get("error") or "") or "test print failed",
            code=str(code) if code else None,
        )
    return str(out.get("state") or "delivered")


# ── update_status.json (agent OTA state) ────────────────────────────


@dataclass
class UpdateStatus:
    status: str = STATUS_IDLE
    current_version: str = ""
    target_version: str | None = None
    last_error: str | None = None
    last_checked_at: str | None = None
    channel: str | None = None
    previous_version: str | None = None


def package_version() -> str:
    """This slot's version: the VERSION file next to main.py, else AGENT_VERSION."""
    try:
        text = (SLOT_DIR / "VERSION").read_text(encoding="utf-8").strip()
    except (OSError, ValueError):
        text = ""
    return text or AGENT_VERSION


def parse_version(v: str) -> tuple[int, ...]:
    core = v.split("-", 1)[0].split("+", 1)[0]
    out: list[int] = []
    for p in core.split("."):
        try:
            out.append(int(p))
        except ValueError:
            out.append(0)
    return tuple(out)


def version_cmp(a: str, b: str) -> int:
    """Return -1 if a<b, 0 if equal, 1 if a>b (numeric semver-ish)."""
    ta, tb = parse_version(a), parse_version(b)
    n = max(len(ta), len(tb))
    ta = ta + (0,) * (n - len(ta))
    tb = tb + (0,) * (n - len(tb))
    if ta < tb:
        return -1
    if ta > tb:
        return 1
    return 0


def read_update_status(path: Path | str) -> UpdateStatus | None:
    """``update_status.json``, or None when missing or unreadable.

    ``status`` defaults to ``idle`` and ``current_version`` to
    :func:`package_version`; the other fields are passed through as stored.
    """
    path = Path(path)
    if not path.is_file():
        return None
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    if not isinstance(data, dict):
        return None
    return UpdateStatus(
        status=str(data.get("status") or STATUS_IDLE),
        current_version=str(data.get("current_version") or package_version()),
        target_version=data.get("target_version"),
        last_error=data.get("last_error"),
        last_checked_at=data.get("last_checked_at"),
        channel=data.get("channel"),
        previous_version=data.get("previous_version"),
    )


def _looks_like_post_activate_glitch(ust: UpdateStatus) -> bool:
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
    if cur and version_cmp(cur, target) == 0:
        return True
    return False


def ota_display_message(
    ust: UpdateStatus | None,
) -> tuple[str, tuple[int, int, int]] | None:
    """Map update_status → (footer label, color) for the LCD, or None if idle."""
    if ust is None:
        return None
    target = (ust.target_version or "").strip().lstrip("v")
    s = ust.status

    if s == STATUS_DOWNLOADING:
        label = f"Updating {target}…".strip() if target else "Updating…"
        return label, WARN
    if s == STATUS_INSTALLING:
        label = f"Installing {target}…".strip() if target else "Installing…"
        return label, WARN
    if s == STATUS_PENDING_HEALTH:
        label = f"Verifying {target}…".strip() if target else "Verifying…"
        return label, WARN
    if s == STATUS_FAILED:
        # Don't flash red "Update failed" for self-restart false negatives.
        if _looks_like_post_activate_glitch(ust):
            label = f"Verifying {target}…".strip() if target else "Verifying…"
            return label, WARN
        return "Update failed", DOWN
    if s == STATUS_ROLLED_BACK:
        return "Rolled back", WARN
    return None
