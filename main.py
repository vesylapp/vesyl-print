"""VESYL print device — multi-page LCD status display.

Paired: Ops (default) → Network → System; tap cycles; 10s idle returns to Ops.
Unpaired / revoked: engineer network view for connect + claim.

Renders to the 3.5" LCD (/dev/fb1) at ~1 Hz.
Run with:  python3 main.py
"""

from __future__ import annotations

import argparse
import logging
import os
import signal
import threading
import time
from pathlib import Path
from typing import Any

from PIL import Image, ImageDraw, ImageFont

import printers
import statusio
import sysinfo
import touch as touch_mod
import update as update_mod
from config import AGENT_VERSION, load_config
from display_status import (
    DOWN,
    IDLE_HOME_SECONDS,
    OK,
    PAGE_NETWORK,
    PAGE_OPS,
    PAGE_SYSTEM,
    PAIRED_PAGES,
    WARN,
    PageState,
    count_queue_jobs,
    format_agent_version,
    heartbeat_age_label,
    identity_line,
    jobs_strip_label,
    ota_display_message,
    printer_status_color,
    printer_status_label,
)
from framebuffer import Framebuffer
from stream_lcd import (
    DEFAULT_FPS,
    DEFAULT_PORT,
    DEFAULT_QUALITY,
    DEFAULT_SCALE,
    LcdStreamServer,
)

log = logging.getLogger("vesyl-print.display")

BASE_DIR = os.path.dirname(os.path.abspath(__file__))
FONT_PATH = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"
FONT_BOLD = "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"
LOGO_PATH = os.path.join(BASE_DIR, "assets", "logo.png")

# logo header sizing
LOGO_WIDTH = 240  # rendered width on screen (aspect preserved)
LOGO_TOP = 14
LOGO_LEFT = 16

# colors
BG = (16, 18, 24)
ACCENT = (237, 252, 51)  # VESYL yellow-green, matches logo mark
FG = (235, 238, 245)
MUTED = (140, 148, 165)

# Background inventory refresh (CUPS/IPP can be slow).
_PRINTER_REFRESH_S = 8.0


def load_font(path: str, size: int) -> ImageFont.FreeTypeFont:
    return ImageFont.truetype(path, size)


class InfoScreen:
    def __init__(
        self,
        fb: Framebuffer,
        status_path: str | None = None,
        update_status_path: str | None = None,
        queue_dir: str | Path | None = None,
        *,
        initial_page: str = PAGE_OPS,
        idle_home_seconds: float = IDLE_HOME_SECONDS,
    ):
        self.fb = fb
        self.w, self.h = fb.size
        self.f_clock = load_font(FONT_BOLD, 28)
        self.f_date = load_font(FONT_PATH, 14)
        self.f_head = load_font(FONT_BOLD, 42)
        self.f_label = load_font(FONT_BOLD, 18)
        self.f_value = load_font(FONT_PATH, 26)
        self.f_footer = load_font(FONT_PATH, 16)
        self.f_hint = load_font(FONT_PATH, 15)
        self.logo = self._load_logo()
        # Filled by the background CUPS discovery / status thread.
        self.printer_names: list[str] = []
        self.printer_rows: list[dict[str, Any]] = []
        self.status_path = status_path
        self.queue_dir = Path(queue_dir) if queue_dir else None
        self.idle_home_seconds = idle_home_seconds
        self._pages = PageState(
            initial_page, idle_seconds=idle_home_seconds
        )
        self._page_lock = threading.Lock()
        if update_status_path:
            self.update_status_path = update_status_path
        elif status_path:
            self.update_status_path = str(
                Path(status_path).parent / "update_status.json"
            )
        else:
            self.update_status_path = None

    def _load_logo(self) -> Image.Image | None:
        try:
            logo = Image.open(LOGO_PATH).convert("RGBA")
        except OSError:
            return None
        h = round(LOGO_WIDTH * logo.height / logo.width)
        return logo.resize((LOGO_WIDTH, h), Image.LANCZOS)

    def _agent_status(self) -> statusio.AgentStatus | None:
        if not self.status_path:
            return None
        return statusio.read_status(self.status_path)

    def _update_status(self) -> update_mod.UpdateStatus | None:
        if not self.update_status_path:
            return None
        return update_mod.read_update_status(Path(self.update_status_path))

    def _display_version(self, st: statusio.AgentStatus | None) -> str:
        if st and st.agent_version:
            return format_agent_version(st.agent_version)
        try:
            return format_agent_version(update_mod.package_version())
        except Exception:
            return format_agent_version(AGENT_VERSION)

    # ── page navigation ──────────────────────────────────────────────

    @property
    def page(self) -> str:
        return self._pages.page

    def note_tap(self, *, paired: bool | None = None) -> None:
        """Advance page on touch when paired. Unpaired ignores taps."""
        if paired is None:
            st = self._agent_status()
            paired = bool(st and st.pairing == "paired")
        with self._page_lock:
            self._pages.note_tap(paired=paired, now_mono=time.monotonic())

    def set_page(self, page: str) -> None:
        with self._page_lock:
            self._pages.set_page(page, now_mono=time.monotonic())

    def _sync_page_idle(self, pairing: str) -> str:
        """Apply idle-home and return the page to render (ops if not paired)."""
        with self._page_lock:
            return self._pages.sync(
                paired=(pairing == "paired"),
                now_mono=time.monotonic(),
            )

    def render(self) -> Image.Image:
        """Live screen: pairing-aware multi-page UI."""
        img, d = self._new()
        body_top = self._live_header(img, d)
        st = self._agent_status()
        pairing = st.pairing if st else "unpaired"
        cloud = st.cloud if st else "unknown"

        if pairing == "revoked":
            return self._render_revoked(img, d, body_top, st)
        if pairing != "paired":
            return self._render_unpaired(img, d, body_top, st)

        page = self._sync_page_idle(pairing)
        if page == PAGE_NETWORK:
            return self._render_network(img, d, body_top, st, cloud)
        if page == PAGE_SYSTEM:
            return self._render_system(img, d, body_top, st, cloud)
        return self._render_ops(img, d, body_top, st, cloud)

    # ── unpaired / revoked (engineer network) ───────────────────────

    def _render_unpaired(
        self, img, d, body_top: int, st: statusio.AgentStatus | None = None
    ) -> Image.Image:
        y = body_top
        ota = ota_display_message(self._update_status())
        if ota:
            label, color = ota
            self._centered(d, label, self.f_label, y=y, fill=color)
            y += 28
        self._centered(d, "Unpaired", self.f_label, y=y, fill=WARN)
        y += 26
        self._centered(
            d, "claim: vesyl-print claim <CODE>", self.f_hint, y=y, fill=MUTED
        )
        y += 32
        self._row(d, "HOSTNAME", sysinfo.hostname(), y=y)
        y += 52
        self._row(d, "IP ADDRESS", sysinfo.primary_ip(), y=y)
        y += 52
        self._row(d, "TAILSCALE", sysinfo.tailscale_ip(), y=y)

        self._draw_footer(d, st, default_label="unpaired", default_color=WARN)
        return img

    def _render_revoked(
        self, img, d, body_top: int, st: statusio.AgentStatus | None
    ) -> Image.Image:
        y = body_top
        ota = ota_display_message(self._update_status())
        if ota:
            label, color = ota
            self._centered(d, label, self.f_label, y=y, fill=color)
            y += 28
        self._centered(d, "Revoked", self.f_label, y=y, fill=DOWN)
        y += 24
        self._centered(d, "re-pair required", self.f_hint, y=y, fill=MUTED)
        y += 22
        self._centered(
            d, "vesyl-print claim <CODE>", self.f_hint, y=y, fill=MUTED
        )
        y += 28
        if st and st.organization_name:
            self._row(d, "WAS", st.organization_name, y=y)
            y += 48
        self._row(d, "IP ADDRESS", sysinfo.primary_ip(), y=y)
        y += 48
        self._row(d, "TAILSCALE", sysinfo.tailscale_ip(), y=y)

        self._draw_footer(d, st, default_label="revoked", default_color=DOWN)
        return img

    # ── paired pages ─────────────────────────────────────────────────

    def _ota_banner(self, d, y: int) -> int:
        """Paint OTA banner if active; return new y."""
        ust = self._update_status()
        ota = ota_display_message(ust)
        if not ota:
            return y
        label, color = ota
        self._centered(d, label, self.f_label, y=y, fill=color)
        y += 26
        if ust and ust.last_error and ust.status in (
            update_mod.STATUS_FAILED,
            update_mod.STATUS_ROLLED_BACK,
        ):
            err = self._fit(d, ust.last_error, self.f_hint, self.w - 32)
            self._centered(d, err, self.f_hint, y=y, fill=MUTED)
            y += 20
        return y + 4

    def _cloud_footer(
        self, d, st: statusio.AgentStatus | None, cloud: str
    ) -> None:
        ota = ota_display_message(self._update_status())
        if ota:
            self._draw_footer(d, st, default_label=ota[0], default_color=ota[1])
        elif cloud == "online":
            self._draw_footer(d, st, default_label="cloud", default_color=OK)
        else:
            self._draw_footer(
                d, st, default_label="cloud offline", default_color=DOWN
            )

    def _render_ops(
        self,
        img,
        d,
        body_top: int,
        st: statusio.AgentStatus | None,
        cloud: str,
    ) -> Image.Image:
        y = self._ota_banner(d, body_top)

        ident = identity_line(
            warehouse_name=st.warehouse_name if st else None,
            organization_name=st.organization_name if st else None,
            node_name=st.name if st else None,
        )
        ident = self._fit(d, ident, self.f_value, self.w - 32)
        d.text((16, y), ident, font=self.f_value, fill=FG)
        y += 34

        d.text((16, y), "PRINTERS", font=self.f_label, fill=MUTED)
        y += 24

        rows = self._ops_printer_rows()
        if not rows:
            d.text((16, y), "No printers", font=self.f_footer, fill=MUTED)
            y += 22
        else:
            line_h = 22
            # Leave room for jobs strip + footer.
            max_rows = max(1, (self.h - 48 - y - 28) // line_h)
            for row in rows[:max_rows]:
                name = str(row.get("name") or "—")
                status = row.get("status")
                message = row.get("message")
                color = printer_status_color(
                    str(status) if status is not None else None
                )
                label = printer_status_label(
                    str(status) if status is not None else None,
                    str(message) if message else None,
                )
                # Dot
                d.ellipse([16, y + 2, 28, y + 14], fill=color)
                name_fit = self._fit(d, name, self.f_footer, self.w // 2 - 20)
                d.text((34, y), name_fit, font=self.f_footer, fill=FG)
                right = self._fit(d, label, self.f_footer, self.w // 2 - 24)
                tw = d.textlength(right, font=self.f_footer)
                d.text(
                    (self.w - 16 - tw, y),
                    right,
                    font=self.f_footer,
                    fill=MUTED,
                )
                y += line_h

        # Jobs strip near footer
        queued = count_queue_jobs(self.queue_dir) if self.queue_dir else 0
        jobs = jobs_strip_label(queued)
        d.text((16, self.h - 52), jobs, font=self.f_footer, fill=MUTED)

        self._cloud_footer(d, st, cloud)
        return img

    def _ops_printer_rows(self) -> list[dict[str, Any]]:
        if self.printer_rows:
            return self.printer_rows
        return [
            {"name": n, "status": None, "message": None}
            for n in self.printer_names
        ]

    def _render_network(
        self,
        img,
        d,
        body_top: int,
        st: statusio.AgentStatus | None,
        cloud: str,
    ) -> Image.Image:
        y = self._ota_banner(d, body_top)
        if st and (st.warehouse_name or st.organization_name or st.name):
            ident = identity_line(
                warehouse_name=st.warehouse_name,
                organization_name=st.organization_name,
                node_name=st.name,
            )
            ident = self._fit(d, ident, self.f_hint, self.w - 32)
            self._centered(d, ident, self.f_hint, y=y, fill=MUTED)
            y += 22

        row_pitch = 52
        rows = [
            ("HOSTNAME", sysinfo.hostname()),
            ("IP ADDRESS", sysinfo.primary_ip()),
            ("TAILSCALE", sysinfo.tailscale_ip()),
        ]
        for i, (label, value) in enumerate(rows):
            self._row(d, label, value, y=y + i * row_pitch)

        self._centered(
            d,
            "tap · next  ·  home in 10s",
            self.f_hint,
            y=self.h - 52,
            fill=MUTED,
        )
        self._cloud_footer(d, st, cloud)
        return img

    def _render_system(
        self,
        img,
        d,
        body_top: int,
        st: statusio.AgentStatus | None,
        cloud: str,
    ) -> Image.Image:
        y = self._ota_banner(d, body_top)
        version = self._display_version(st) or "—"
        hb = heartbeat_age_label(st.last_heartbeat_at if st else None)
        temp = sysinfo.cpu_temp_c()
        cloud_label = "online" if cloud == "online" else (
            "offline" if cloud == "offline" else "unknown"
        )

        row_pitch = 50
        rows = [
            ("VERSION", version),
            ("HEARTBEAT", hb),
            ("CPU TEMP", temp),
            ("CLOUD", cloud_label),
        ]
        for i, (label, value) in enumerate(rows):
            self._row(d, label, value, y=y + i * row_pitch)

        err_y = y + len(rows) * row_pitch
        if st and st.last_error and err_y < self.h - 56:
            err = self._fit(d, st.last_error, self.f_hint, self.w - 32)
            d.text((16, err_y), "ERROR", font=self.f_label, fill=MUTED)
            d.text((16, err_y + 22), err, font=self.f_hint, fill=DOWN)

        self._centered(
            d,
            "tap · next  ·  home in 10s",
            self.f_hint,
            y=self.h - 52,
            fill=MUTED,
        )
        self._cloud_footer(d, st, cloud)
        return img

    # ── chrome helpers ───────────────────────────────────────────────

    def render_splash(self) -> Image.Image:
        """Static boot splash (logo + 'booting…')."""
        img, d = self._new()
        self._header(img, d, accent=ACCENT)
        self._centered(d, "booting...", self.f_head, y=128, fill=FG)
        return img

    def render_offline(self, message: str = "display service stopped") -> Image.Image:
        """Static screen shown when the refresh loop is not running."""
        img, d = self._new()
        self._header(img, d, accent=MUTED)

        self._centered(d, "DISCONNECTED", self.f_head, y=128, fill=MUTED)
        self._centered(d, message, self.f_footer, y=186, fill=MUTED)

        self._status(d, "offline", DOWN)
        return img

    def _new(self):
        img = Image.new("RGB", (self.w, self.h), BG)
        return img, ImageDraw.Draw(img)

    def _header(self, img, d, accent):
        """Centered logo (splash/offline) with a divider in the accent color."""
        if self.logo is not None:
            lx = (self.w - self.logo.width) // 2
            img.paste(self.logo, (lx, LOGO_TOP), self.logo)
            divider_y = LOGO_TOP + self.logo.height + 12
        else:
            self._centered(d, "VESYL PRINT", self.f_label, y=20, fill=accent)
            divider_y = 52
        d.rectangle([40, divider_y, self.w - 40, divider_y + 2], fill=accent)

    def _live_header(self, img, d) -> int:
        """Logo left + compact clock/date right; returns y where body rows start."""
        now = sysinfo.now()
        clock = now.strftime("%H:%M:%S")
        date = now.strftime("%a, %d %b %Y")

        if self.logo is not None:
            img.paste(self.logo, (LOGO_LEFT, LOGO_TOP), self.logo)
            header_bottom = LOGO_TOP + self.logo.height
        else:
            d.text((LOGO_LEFT, LOGO_TOP + 4), "VESYL PRINT",
                   font=self.f_label, fill=ACCENT)
            header_bottom = LOGO_TOP + 28

        clock_bbox = d.textbbox((0, 0), clock, font=self.f_clock)
        date_bbox = d.textbbox((0, 0), date, font=self.f_date)
        clock_h = clock_bbox[3] - clock_bbox[1]
        date_h = date_bbox[3] - date_bbox[1]
        gap = 8
        stack_h = clock_h + gap + date_h
        band_top = LOGO_TOP
        band_h = max(header_bottom - LOGO_TOP, stack_h)
        stack_y = band_top + (band_h - stack_h) // 2
        self._right(d, clock, self.f_clock, y=stack_y, fill=FG, pad=16)
        self._right(
            d, date, self.f_date, y=stack_y + clock_h + gap, fill=MUTED, pad=16
        )

        header_bottom = max(header_bottom, stack_y + stack_h)
        divider_y = header_bottom + 10
        d.rectangle([40, divider_y, self.w - 40, divider_y + 2], fill=ACCENT)
        return divider_y + 16

    def _draw_footer(
        self,
        d,
        st: statusio.AgentStatus | None,
        *,
        default_label: str,
        default_color: tuple[int, int, int],
    ) -> None:
        """Footer right cluster: ``vX.Y.Z  ● status`` (version left of status dot)."""
        ota = ota_display_message(self._update_status())
        if ota:
            label, color = ota
        else:
            label, color = default_label, default_color

        version = self._display_version(st)
        ver_w = d.textlength(version, font=self.f_footer) if version else 0
        # Room for optional version + gap + dot (12px) + gap before label.
        reserved = (ver_w + 8 + 12 + 8) if version else (12 + 8)
        max_status_w = self.w - 16 - 16 - reserved
        label = self._fit(d, label, self.f_footer, max(40, max_status_w))
        tw = d.textlength(label, font=self.f_footer)

        # Right edge: status text; immediately left: colored dot; left of that: version.
        tx = self.w - 16 - tw
        dot_l, dot_r = tx - 20, tx - 8
        d.text((tx, self.h - 28), label, font=self.f_footer, fill=MUTED)
        d.ellipse([dot_l, self.h - 26, dot_r, self.h - 14], fill=color)
        if version:
            d.text(
                (dot_l - 8 - ver_w, self.h - 28),
                version,
                font=self.f_footer,
                fill=MUTED,
            )

    def _status(self, d, text, color):
        """Right-aligned footer without version (splash/offline helpers)."""
        tw = d.textlength(text, font=self.f_footer)
        tx = self.w - 16 - tw
        d.text((tx, self.h - 28), text, font=self.f_footer, fill=MUTED)
        d.ellipse([tx - 20, self.h - 26, tx - 8, self.h - 14], fill=color)

    def _fit(self, d, text, font, max_w):
        """Truncate text with an ellipsis so it fits within max_w pixels."""
        if d.textlength(text, font=font) <= max_w:
            return text
        while text and d.textlength(text + "…", font=font) > max_w:
            text = text[:-1]
        return text + "…"

    def _centered(self, d, text, font, y, fill):
        w = d.textlength(text, font=font)
        d.text(((self.w - w) / 2, y), text, font=font, fill=fill)

    def _right(self, d, text, font, y, fill, pad: int = 16):
        w = d.textlength(text, font=font)
        d.text((self.w - pad - w, y), text, font=font, fill=fill)

    def _row(self, d, label, value, y):
        d.text((16, y), label, font=self.f_label, fill=MUTED)
        d.text((16, y + 24), value, font=self.f_value, fill=FG)


def open_framebuffer(device: str, wait: float = 0.0) -> Framebuffer:
    """Open the framebuffer, optionally retrying until it appears."""
    deadline = time.monotonic() + wait
    while True:
        try:
            return Framebuffer(device)
        except (OSError, RuntimeError):
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.5)


def _refresh_printers(screen: InfoScreen, stop: threading.Event) -> None:
    """Discover queues once, then periodically refresh status for Ops."""
    try:
        screen.printer_names = printers.ensure_printers()
    except Exception:
        log.exception("printer discovery failed")

    while not stop.is_set():
        try:
            inv = printers.inventory_payload()
            rows = [
                {
                    "name": str(item.get("display_name") or item.get("cups_name") or "—"),
                    "status": item.get("status"),
                    "message": item.get("status_message"),
                }
                for item in inv
            ]
            screen.printer_rows = rows
            if rows:
                screen.printer_names = [str(r["name"]) for r in rows]
        except Exception:
            log.debug("printer inventory refresh failed", exc_info=True)
        stop.wait(_PRINTER_REFRESH_S)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--device", default="/dev/fb1")
    ap.add_argument(
        "--interval", type=float, default=1.0, help="refresh seconds"
    )
    ap.add_argument("--once", action="store_true", help="render once and exit")
    ap.add_argument(
        "--offline",
        action="store_true",
        help="render the static disconnected screen once and exit",
    )
    ap.add_argument(
        "--splash",
        action="store_true",
        help="render the boot splash once and exit",
    )
    ap.add_argument(
        "--page",
        choices=list(PAIRED_PAGES),
        default=PAGE_OPS,
        help="initial paired page (default: ops); also used with --once",
    )
    ap.add_argument(
        "--touch-device",
        default=None,
        help="input event path (default: auto-detect ADS7846/touchscreen)",
    )
    ap.add_argument(
        "--no-touch",
        action="store_true",
        help="disable touch page cycling",
    )
    ap.add_argument(
        "--idle-home",
        type=float,
        default=IDLE_HOME_SECONDS,
        help=f"seconds without touch before returning to ops (default {IDLE_HOME_SECONDS:g})",
    )
    ap.add_argument(
        "--stream",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="serve LCD MJPEG stream during the live loop (default: on)",
    )
    ap.add_argument(
        "--stream-port",
        type=int,
        default=DEFAULT_PORT,
        help=f"MJPEG HTTP port (default {DEFAULT_PORT})",
    )
    ap.add_argument(
        "--stream-fps",
        type=float,
        default=DEFAULT_FPS,
        help="client stream rate (default 2)",
    )
    ap.add_argument(
        "--stream-scale",
        type=float,
        default=DEFAULT_SCALE,
        help="upscale for viewing (default 1)",
    )
    ap.add_argument(
        "--stream-quality",
        type=int,
        default=DEFAULT_QUALITY,
        help="JPEG quality (default 80)",
    )
    args = ap.parse_args()

    cfg = load_config()
    status_path = str(cfg.status_path)
    update_status_path = str(cfg.update_status_path)
    queue_dir = cfg.queue_dir

    manual = args.once or args.offline or args.splash
    fb = open_framebuffer(args.device, wait=0.0 if manual else 30.0)
    screen = InfoScreen(
        fb,
        status_path=status_path,
        update_status_path=update_status_path,
        queue_dir=queue_dir,
        initial_page=args.page,
        idle_home_seconds=args.idle_home,
    )

    if args.offline:
        fb.show(screen.render_offline())
        return
    if args.splash:
        fb.show(screen.render_splash())
        return

    running = {"go": True}
    signal.signal(signal.SIGINT, lambda *_: running.update(go=False))
    signal.signal(signal.SIGTERM, lambda *_: running.update(go=False))

    if args.once:
        fb.show(screen.render())
        return

    stop_bg = threading.Event()
    threading.Thread(
        target=_refresh_printers,
        args=(screen, stop_bg),
        daemon=True,
        name="vesyl-printers",
    ).start()

    listener: touch_mod.TouchListener | None = None
    if not args.no_touch:
        listener = touch_mod.open_touch(device=args.touch_device)
        listener.start()

    streamer: LcdStreamServer | None = None
    if args.stream:
        streamer = LcdStreamServer(
            port=args.stream_port,
            fps=args.stream_fps,
            scale=args.stream_scale,
            quality=args.stream_quality,
            native_size=fb.size,
        )
        streamer.start()

    try:
        while running["go"]:
            start = time.monotonic()
            if listener is not None and listener.poll_tap():
                screen.note_tap()
            frame = screen.render()
            fb.show(frame)
            if streamer is not None:
                streamer.publish(frame)
            elapsed = time.monotonic() - start
            # Wake sooner when a tap may be pending so page changes feel snappy.
            sleep_for = max(0.0, args.interval - elapsed)
            if listener is not None and sleep_for > 0.05:
                # Slice sleep so we can pick up taps mid-interval.
                end = time.monotonic() + sleep_for
                while running["go"] and time.monotonic() < end:
                    if listener.poll_tap():
                        screen.note_tap()
                        break
                    time.sleep(min(0.05, end - time.monotonic()))
            elif sleep_for > 0:
                time.sleep(sleep_for)
    finally:
        stop_bg.set()
        if listener is not None:
            listener.stop()
        if streamer is not None:
            streamer.stop()
        try:
            fb.show(screen.render_offline())
        except Exception:
            pass


if __name__ == "__main__":
    main()
