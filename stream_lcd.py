#!/usr/bin/env python3
"""Stream the SPI LCD as MJPEG for local demos.

Used two ways:

1. **Embedded in the display app** (default) — ``main.py`` publishes each
   rendered frame and serves  http://<pi-ip>:8765/

2. **Standalone** (optional) — poll ``/dev/fb1`` without the display loop:

       python3 stream_lcd.py --port 8765 --fps 2 --scale 2

Needs group ``video`` for ``/dev/fb1``. Binds 0.0.0.0 by default (trusted LAN only).
"""

from __future__ import annotations

import argparse
import io
import logging
import signal
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any
from urllib.parse import urlparse

from PIL import Image

from framebuffer import Framebuffer

log = logging.getLogger("vesyl-print.stream")

BOUNDARY = b"frame"
DEFAULT_PORT = 8765
DEFAULT_FPS = 2.0
DEFAULT_SCALE = 2.0
DEFAULT_QUALITY = 80

HTML_PAGE = """\
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8"/>
  <meta name="viewport" content="width=device-width, initial-scale=1"/>
  <title>VESYL Print — LCD</title>
  <style>
    html, body {{
      margin: 0; height: 100%; background: #101218; color: #ebeef5;
      font-family: system-ui, sans-serif;
    }}
    .wrap {{
      min-height: 100%; display: flex; flex-direction: column;
      align-items: center; justify-content: center; gap: 12px; padding: 16px;
      box-sizing: border-box;
    }}
    h1 {{ font-size: 14px; font-weight: 600; letter-spacing: 0.04em;
          color: #8c94a5; margin: 0; text-transform: uppercase; }}
    img {{
      image-rendering: pixelated;
      image-rendering: crisp-edges;
      max-width: min(96vw, {css_max}px);
      height: auto;
      border: 1px solid #2a303c;
      border-radius: 6px;
      box-shadow: 0 12px 40px rgba(0,0,0,0.45);
      background: #000;
    }}
    .meta {{ font-size: 12px; color: #8c94a5; }}
    a {{ color: #edfc33; }}
  </style>
</head>
<body>
  <div class="wrap">
    <h1>VESYL Print LCD</h1>
    <img src="/stream.mjpg" alt="LCD stream" width="{w}" height="{h}"/>
    <div class="meta">{w}&times;{h} · ~{fps} fps ·
      <a href="/snapshot.jpg">snapshot</a>
    </div>
  </div>
</body>
</html>
"""


class FrameSource:
    """Thread-safe latest JPEG. Fed by ``publish()`` or a capture loop."""

    def __init__(
        self,
        *,
        scale: float = DEFAULT_SCALE,
        quality: int = DEFAULT_QUALITY,
        native_size: tuple[int, int] | None = None,
    ):
        self.scale = max(scale, 0.25)
        self.quality = max(40, min(quality, 95))
        self._native = native_size or (480, 320)
        self._jpeg = b""
        self._lock = threading.Lock()
        self._error: str | None = None

    @property
    def size(self) -> tuple[int, int]:
        w, h = self._native
        if self.scale != 1.0:
            return (max(1, round(w * self.scale)), max(1, round(h * self.scale)))
        return (w, h)

    def set_native_size(self, size: tuple[int, int]) -> None:
        self._native = size

    def publish(self, image: Image.Image) -> None:
        """Encode ``image`` to JPEG for stream clients."""
        try:
            self._native = image.size
            img = image
            if img.mode != "RGB":
                img = img.convert("RGB")
            if self.scale != 1.0:
                tw, th = self.size
                img = img.resize((tw, th), Image.NEAREST)
            buf = io.BytesIO()
            img.save(buf, format="JPEG", quality=self.quality, optimize=True)
            data = buf.getvalue()
            with self._lock:
                self._jpeg = data
                self._error = None
        except Exception as e:
            with self._lock:
                self._error = str(e)

    def latest_jpeg(self) -> bytes:
        with self._lock:
            return self._jpeg

    def error(self) -> str | None:
        with self._lock:
            return self._error


class FrameGrabber:
    """Background thread that captures /dev/fb1 into a FrameSource (standalone)."""

    def __init__(
        self,
        device: str,
        source: FrameSource,
        fps: float,
    ):
        self.fb = Framebuffer(device)
        source.set_native_size(self.fb.size)
        self.source = source
        self.interval = 1.0 / max(fps, 0.2)
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None

    def start(self) -> None:
        self._thread = threading.Thread(
            target=self._loop, name="lcd-grabber", daemon=True
        )
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        if self._thread and self._thread.is_alive():
            self._thread.join(timeout=2.0)

    def _loop(self) -> None:
        while not self._stop.is_set():
            t0 = time.monotonic()
            try:
                self.source.publish(self.fb.capture())
            except Exception as e:
                with self.source._lock:
                    self.source._error = str(e)
            elapsed = time.monotonic() - t0
            self._stop.wait(max(0.0, self.interval - elapsed))


def make_handler(source: FrameSource, fps: float) -> type[BaseHTTPRequestHandler]:
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, fmt: str, *args: Any) -> None:
            sys.stderr.write("%s - %s\n" % (self.address_string(), fmt % args))

        def do_GET(self) -> None:  # noqa: N802
            path = urlparse(self.path).path
            if path in ("/", "/index.html"):
                self._serve_html()
            elif path in ("/stream.mjpg", "/stream.mjpeg", "/mjpeg"):
                self._serve_mjpeg()
            elif path in ("/snapshot.jpg", "/snapshot.jpeg", "/snap.jpg"):
                self._serve_snapshot()
            elif path == "/health":
                self._serve_health()
            else:
                self.send_error(404, "not found")

        def _serve_html(self) -> None:
            w, h = source.size
            body = HTML_PAGE.format(
                w=w, h=h, fps=fps, css_max=max(w, 480) * 2
            ).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(body)

        def _serve_snapshot(self) -> None:
            data = source.latest_jpeg()
            if not data:
                self.send_error(503, source.error() or "no frame yet")
                return
            self.send_response(200)
            self.send_header("Content-Type", "image/jpeg")
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(data)

        def _serve_health(self) -> None:
            body = b"ok\n" if source.latest_jpeg() else b"warming\n"
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def _serve_mjpeg(self) -> None:
            self.send_response(200)
            self.send_header(
                "Content-Type",
                f"multipart/x-mixed-replace; boundary={BOUNDARY.decode()}",
            )
            self.send_header("Cache-Control", "no-store, no-cache, must-revalidate")
            self.send_header("Pragma", "no-cache")
            self.send_header("Connection", "close")
            self.end_headers()

            interval = 1.0 / max(fps, 0.2)
            try:
                while True:
                    data = source.latest_jpeg()
                    if data:
                        header = (
                            b"--" + BOUNDARY + b"\r\n"
                            b"Content-Type: image/jpeg\r\n"
                            b"Content-Length: " + str(len(data)).encode() + b"\r\n"
                            b"\r\n"
                        )
                        self.wfile.write(header)
                        self.wfile.write(data)
                        self.wfile.write(b"\r\n")
                        self.wfile.flush()
                    time.sleep(interval)
            except (BrokenPipeError, ConnectionResetError, OSError):
                return

    return Handler


class LcdStreamServer:
    """Background HTTP MJPEG server fed by ``publish(image)`` each display frame."""

    def __init__(
        self,
        *,
        host: str = "0.0.0.0",
        port: int = DEFAULT_PORT,
        fps: float = DEFAULT_FPS,
        scale: float = DEFAULT_SCALE,
        quality: int = DEFAULT_QUALITY,
        native_size: tuple[int, int] | None = None,
    ):
        self.host = host
        self.port = port
        self.fps = fps
        self.source = FrameSource(
            scale=scale, quality=quality, native_size=native_size
        )
        self._server: ThreadingHTTPServer | None = None
        self._thread: threading.Thread | None = None

    def start(self) -> None:
        if self._thread and self._thread.is_alive():
            return
        handler = make_handler(self.source, self.fps)
        try:
            self._server = ThreadingHTTPServer((self.host, self.port), handler)
        except OSError as e:
            log.warning("LCD stream not started on %s:%s: %s", self.host, self.port, e)
            self._server = None
            return
        # daemon thread so display service can exit on SIGTERM without hang
        self._thread = threading.Thread(
            target=self._serve, name="lcd-stream", daemon=True
        )
        self._thread.start()
        log.info(
            "LCD stream http://0.0.0.0:%s/ (scale=%s fps~%s)",
            self.port,
            self.source.scale,
            self.fps,
        )

    def _serve(self) -> None:
        assert self._server is not None
        try:
            self._server.serve_forever(poll_interval=0.5)
        except Exception:
            log.exception("LCD stream server stopped with error")

    def publish(self, image: Image.Image) -> None:
        self.source.publish(image)

    def stop(self) -> None:
        if self._server is not None:
            try:
                self._server.shutdown()
            except Exception:
                pass
            try:
                self._server.server_close()
            except Exception:
                pass
            self._server = None
        if self._thread and self._thread.is_alive():
            self._thread.join(timeout=2.0)
        self._thread = None


def main() -> None:
    ap = argparse.ArgumentParser(description="MJPEG stream of the vesyl-print LCD")
    ap.add_argument("--device", default="/dev/fb1", help="framebuffer device")
    ap.add_argument("--host", default="0.0.0.0", help="bind address")
    ap.add_argument("--port", type=int, default=DEFAULT_PORT, help="HTTP port")
    ap.add_argument(
        "--fps", type=float, default=DEFAULT_FPS, help="capture / stream rate"
    )
    ap.add_argument(
        "--scale",
        type=float,
        default=DEFAULT_SCALE,
        help="upscale factor (nearest-neighbor)",
    )
    ap.add_argument(
        "--quality", type=int, default=DEFAULT_QUALITY, help="JPEG quality 40-95"
    )
    args = ap.parse_args()

    try:
        source = FrameSource(scale=args.scale, quality=args.quality)
        grabber = FrameGrabber(args.device, source, args.fps)
    except (OSError, RuntimeError) as e:
        print(f"Cannot open {args.device}: {e}", file=sys.stderr)
        print("Is the display service running? Are you in group 'video'?", file=sys.stderr)
        raise SystemExit(1) from e

    grabber.start()
    for _ in range(50):
        if source.latest_jpeg():
            break
        time.sleep(0.05)

    handler = make_handler(source, args.fps)
    server = ThreadingHTTPServer((args.host, args.port), handler)

    def _shutdown(*_args: object) -> None:
        print("\nStopping…", flush=True)
        threading.Thread(target=server.shutdown, daemon=True).start()

    signal.signal(signal.SIGINT, _shutdown)
    signal.signal(signal.SIGTERM, _shutdown)

    w, h = source.size
    print(
        f"Streaming {args.device} ({grabber.fb.width}x{grabber.fb.height}"
        f" → {w}x{h} @ ~{args.fps} fps)",
        flush=True,
    )
    print(f"  Open  http://<pi-ip>:{args.port}/", flush=True)
    print(f"  Snap  http://<pi-ip>:{args.port}/snapshot.jpg", flush=True)
    try:
        server.serve_forever(poll_interval=0.5)
    finally:
        grabber.stop()
        server.server_close()


if __name__ == "__main__":
    main()
