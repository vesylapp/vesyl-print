"""VESYL print device — system info display.

Renders time, hostname and IP address to the 3.5" LCD (/dev/fb1) and
refreshes once a second. Run with:  python3 main.py
"""

from __future__ import annotations

import argparse
import os
import signal
import time

from PIL import Image, ImageDraw, ImageFont

import sysinfo
from framebuffer import Framebuffer

BASE_DIR = os.path.dirname(os.path.abspath(__file__))
FONT_PATH = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"
FONT_BOLD = "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"
LOGO_PATH = os.path.join(BASE_DIR, "assets", "logo.png")

# logo header sizing
LOGO_WIDTH = 270  # rendered width on screen (aspect preserved)
LOGO_TOP = 16

# colors
BG = (16, 18, 24)
ACCENT = (237, 252, 51)  # VESYL yellow-green, matches logo mark
FG = (235, 238, 245)
MUTED = (140, 148, 165)
OK = (80, 220, 120)  # online status
DOWN = (232, 72, 72)  # disconnected status


def load_font(path: str, size: int) -> ImageFont.FreeTypeFont:
    return ImageFont.truetype(path, size)


class InfoScreen:
    def __init__(self, fb: Framebuffer):
        self.fb = fb
        self.w, self.h = fb.size
        self.f_clock = load_font(FONT_BOLD, 60)
        self.f_head = load_font(FONT_BOLD, 42)
        self.f_label = load_font(FONT_BOLD, 18)
        self.f_value = load_font(FONT_PATH, 26)
        self.f_footer = load_font(FONT_PATH, 16)
        self.logo = self._load_logo()

    def _load_logo(self) -> Image.Image | None:
        try:
            logo = Image.open(LOGO_PATH).convert("RGBA")
        except OSError:
            return None
        h = round(LOGO_WIDTH * logo.height / logo.width)
        return logo.resize((LOGO_WIDTH, h), Image.LANCZOS)

    def render(self) -> Image.Image:
        """Live screen: clock, hostname, IP and an 'online' status."""
        img, d = self._new()
        self._header(img, d, accent=ACCENT)

        now = sysinfo.now()
        self._centered(d, now.strftime("%H:%M:%S"), self.f_clock, y=96, fill=FG)
        self._centered(
            d, now.strftime("%A, %d %B %Y"), self.f_footer, y=162, fill=MUTED
        )
        self._row(d, "HOSTNAME", sysinfo.hostname(), y=200)
        self._row(d, "IP ADDRESS", sysinfo.primary_ip(), y=252)

        self._status(d, "online", OK)
        return img

    def render_splash(self) -> Image.Image:
        """Static boot splash (logo + 'booting…').

        This is what the Plymouth boot screen shows: it is rendered once to
        assets/plymouth-splash.png and installed as the Plymouth theme image
        by setup.sh. Keeping it here lets the asset be regenerated if the
        logo or theme changes.
        """
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
        """Logo (centered) with a divider line in the given accent color."""
        if self.logo is not None:
            lx = (self.w - self.logo.width) // 2
            img.paste(self.logo, (lx, LOGO_TOP), self.logo)
            divider_y = LOGO_TOP + self.logo.height + 12
        else:
            self._centered(d, "VESYL PRINT", self.f_label, y=20, fill=accent)
            divider_y = 52
        d.rectangle([40, divider_y, self.w - 40, divider_y + 2], fill=accent)

    def _status(self, d, text, color):
        """Right-aligned footer: label with a colored dot to its left."""
        tw = d.textlength(text, font=self.f_footer)
        tx = self.w - 16 - tw
        d.text((tx, self.h - 28), text, font=self.f_footer, fill=MUTED)
        d.ellipse([tx - 20, self.h - 26, tx - 8, self.h - 14], fill=color)

    def _centered(self, d, text, font, y, fill):
        w = d.textlength(text, font=font)
        d.text(((self.w - w) / 2, y), text, font=font, fill=fill)

    def _row(self, d, label, value, y):
        d.text((16, y), label, font=self.f_label, fill=MUTED)
        d.text((16, y + 22), value, font=self.f_value, fill=FG)


def open_framebuffer(device: str, wait: float = 0.0) -> Framebuffer:
    """Open the framebuffer, optionally retrying until it appears.

    During early boot the splash can start before the kernel has created
    /dev/fb1, so wait up to `wait` seconds for it.
    """
    deadline = time.monotonic() + wait
    while True:
        try:
            return Framebuffer(device)
        except (OSError, RuntimeError):
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.5)


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
    args = ap.parse_args()

    # The LCD's SPI driver loads late in boot (fb1 appears ~15-20s in), so the
    # service can start before the device exists — wait for it. Manual one-shot
    # modes fail fast instead of hanging.
    manual = args.once or args.offline or args.splash
    fb = open_framebuffer(args.device, wait=0.0 if manual else 30.0)
    screen = InfoScreen(fb)

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

    # The Plymouth splash covers the boot window; the app goes straight to the
    # live info screen and refreshes on the interval.
    try:
        while running["go"]:
            start = time.monotonic()
            fb.show(screen.render())
            elapsed = time.monotonic() - start
            time.sleep(max(0.0, args.interval - elapsed))
    finally:
        # Graceful stop or unhandled crash: leave a disconnected screen behind.
        # (A hard kill -9 / segfault can't run this — systemd ExecStopPost
        # paints the same screen for that case.)
        try:
            fb.show(screen.render_offline())
        except Exception:
            pass


if __name__ == "__main__":
    main()
