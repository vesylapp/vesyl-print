"""Convert PDF / raster images to ZPL for raw thermal (Zebra) queues.

Pipeline:
  1. PDF → PNG via ``pdftoppm`` (poppler) or Ghostscript fallback
  2. Raster → 1-bit monochrome (Pillow)
  3. Encode as ZPL ``^GFA`` hex graphic field (ASCII hex stream)

``~DY`` download-to-printer is not used: each job embeds the graphic in a
self-contained ``^XA``…``^XZ`` label so CUPS raw queues stay stateless.
"""

from __future__ import annotations

import logging
import shutil
import subprocess
import tempfile
from pathlib import Path
from typing import Any

log = logging.getLogger("vesyl-print.zpl")

# Common Zebra desktop defaults (ZD220 / ZD421 203dpi, 2" media).
DEFAULT_DPI = 203
DEFAULT_MAX_WIDTH_DOTS = 448  # ~2.2" at 203 dpi
DEFAULT_THRESHOLD = 128


class ZplError(Exception):
    def __init__(self, message: str, *, code: str = "zpl_error"):
        super().__init__(message)
        self.message = message
        self.code = code


def _opt_int(options: dict[str, Any] | None, key: str, default: int) -> int:
    if not options or key not in options or options[key] is None:
        return default
    try:
        return int(options[key])
    except (TypeError, ValueError):
        return default


def pdf_to_png(
    pdf_path: Path,
    out_dir: Path,
    *,
    dpi: int = DEFAULT_DPI,
    page: int = 1,
) -> Path:
    """Render one PDF page to PNG. Prefers pdftoppm; falls back to Ghostscript."""
    out_dir.mkdir(parents=True, exist_ok=True)
    stem = out_dir / f"{pdf_path.stem}_p{page}"
    pdftoppm = shutil.which("pdftoppm")
    if pdftoppm:
        # -singlefile writes stem.png
        cmd = [
            pdftoppm,
            "-png",
            "-r",
            str(max(72, dpi)),
            "-f",
            str(page),
            "-l",
            str(page),
            "-singlefile",
            str(pdf_path),
            str(stem),
        ]
        try:
            result = subprocess.run(
                cmd, capture_output=True, text=True, timeout=120
            )
        except (OSError, subprocess.SubprocessError) as e:
            raise ZplError(f"pdftoppm failed: {e}", code="pdf_render") from e
        png = Path(str(stem) + ".png")
        if result.returncode != 0 or not png.is_file():
            err = (result.stderr or result.stdout or "pdftoppm failed").strip()
            raise ZplError(err, code="pdf_render")
        return png

    gs = shutil.which("gs")
    if not gs:
        raise ZplError(
            "PDF→image needs pdftoppm (poppler-utils) or ghostscript",
            code="pdf_render",
        )
    png = Path(str(stem) + ".png")
    cmd = [
        gs,
        "-dSAFER",
        "-dBATCH",
        "-dNOPAUSE",
        f"-dFirstPage={page}",
        f"-dLastPage={page}",
        f"-r{max(72, dpi)}",
        "-sDEVICE=pnggray",
        f"-sOutputFile={png}",
        str(pdf_path),
    ]
    try:
        result = subprocess.run(
            cmd, capture_output=True, text=True, timeout=120
        )
    except (OSError, subprocess.SubprocessError) as e:
        raise ZplError(f"ghostscript failed: {e}", code="pdf_render") from e
    if result.returncode != 0 or not png.is_file():
        err = (result.stderr or result.stdout or "ghostscript failed").strip()
        raise ZplError(err, code="pdf_render")
    return png


def load_image_as_mono(
    path: Path,
    *,
    max_width_dots: int = DEFAULT_MAX_WIDTH_DOTS,
    max_height_dots: int | None = None,
    threshold: int = DEFAULT_THRESHOLD,
    invert: bool = False,
):
    """Load image/PDF path → Pillow 1-bit image (black=0) scaled to fit width."""
    try:
        from PIL import Image, ImageOps
    except ImportError as e:
        raise ZplError(
            "Pillow required for ZPL image conversion (python3-pil)",
            code="no_pillow",
        ) from e

    path = Path(path)
    work_dir: Path | None = None
    img_path = path
    try:
        if path.suffix.lower() == ".pdf":
            work_dir = Path(tempfile.mkdtemp(prefix="vesyl-zpl-pdf-"))
            img_path = pdf_to_png(path, work_dir, dpi=DEFAULT_DPI, page=1)

        with Image.open(img_path) as im:
            img = im.convert("L")
    except ZplError:
        raise
    except Exception as e:
        raise ZplError(f"open image failed: {e}", code="image_bad") from e
    finally:
        if work_dir is not None:
            try:
                for p in work_dir.iterdir():
                    p.unlink(missing_ok=True)
                work_dir.rmdir()
            except OSError:
                pass

    # Optional invert (white-on-black PDF backgrounds).
    if invert:
        img = ImageOps.invert(img)

    w, h = img.size
    if w < 1 or h < 1:
        raise ZplError("empty image", code="image_bad")

    max_w = max(8, int(max_width_dots))
    max_h = int(max_height_dots) if max_height_dots else None
    scale = 1.0
    if w > max_w:
        scale = min(scale, max_w / w)
    if max_h and h > max_h:
        scale = min(scale, max_h / h)
    if scale < 1.0:
        nw = max(1, int(round(w * scale)))
        nh = max(1, int(round(h * scale)))
        img = img.resize((nw, nh), Image.Resampling.LANCZOS)

    # 1-bit: black (print) = 0 in PIL mode "1"
    thr = max(0, min(255, int(threshold)))
    bw = img.point(lambda x: 0 if x < thr else 255, mode="1")
    return bw


def mono_image_to_gfa_hex(img) -> tuple[str, int, int, int]:
    """Encode a mode-``1`` image as ZPL ^GFA hex.

    Returns ``(hex_data, total_bytes, bytes_per_row, height)``.
    Bit 1 = black (print); MSB first within each byte (Zebra convention).
    """
    if img.mode != "1":
        raise ZplError(f"expected 1-bit image, got {img.mode}", code="image_bad")
    width, height = img.size
    row_bytes = (width + 7) // 8
    total = row_bytes * height
    pixels = img.load()
    out = bytearray(total)
    i = 0
    for y in range(height):
        for bx in range(row_bytes):
            byte = 0
            for bit in range(8):
                x = bx * 8 + bit
                if x < width:
                    # PIL "1": 0 = black, 255 = white
                    if pixels[x, y] == 0:
                        byte |= 1 << (7 - bit)
            out[i] = byte
            i += 1
    return out.hex().upper(), total, row_bytes, height


def build_zpl_label(
    hex_data: str,
    *,
    total_bytes: int,
    bytes_per_row: int,
    x: int = 0,
    y: int = 0,
    copies: int = 1,
) -> str:
    """Build a complete ZPL label with one ^GFA graphic."""
    copies = max(1, int(copies))
    # ^PQ = print quantity inside format
    body = (
        f"^XA\n"
        f"^FO{x},{y}^GFA,{total_bytes},{total_bytes},{bytes_per_row},{hex_data}^FS\n"
        f"^PQ{copies}\n"
        f"^XZ\n"
    )
    return body


def image_path_to_zpl(
    path: Path,
    *,
    options: dict[str, Any] | None = None,
    copies: int = 1,
) -> str:
    """Convert a PDF/PNG/JPEG path to a ZPL string (embedded ^GFA)."""
    opts = options or {}
    max_w = _opt_int(opts, "zpl_max_width_dots", DEFAULT_MAX_WIDTH_DOTS)
    # Aliases
    if "label_width_dots" in (opts or {}):
        max_w = _opt_int(opts, "label_width_dots", max_w)
    max_h = None
    if opts.get("zpl_max_height_dots") is not None or opts.get("label_height_dots") is not None:
        max_h = _opt_int(
            opts,
            "zpl_max_height_dots",
            _opt_int(opts, "label_height_dots", 0) or 0,
        )
        if max_h <= 0:
            max_h = None
    thr = _opt_int(opts, "zpl_threshold", DEFAULT_THRESHOLD)
    invert = bool(opts.get("zpl_invert") or opts.get("invert"))
    x = _opt_int(opts, "zpl_x", 0)
    y = _opt_int(opts, "zpl_y", 0)

    # DPI only affects PDF rasterization.
    dpi = _opt_int(opts, "zpl_dpi", DEFAULT_DPI)
    path = Path(path)
    if path.suffix.lower() == ".pdf":
        # Temporarily override DEFAULT via env-less path: re-render at requested dpi
        work = Path(tempfile.mkdtemp(prefix="vesyl-zpl-pdf-"))
        try:
            png = pdf_to_png(path, work, dpi=dpi, page=_opt_int(opts, "zpl_page", 1))
            img = load_image_as_mono(
                png,
                max_width_dots=max_w,
                max_height_dots=max_h,
                threshold=thr,
                invert=invert,
            )
        finally:
            try:
                for p in work.iterdir():
                    p.unlink(missing_ok=True)
                work.rmdir()
            except OSError:
                pass
    else:
        img = load_image_as_mono(
            path,
            max_width_dots=max_w,
            max_height_dots=max_h,
            threshold=thr,
            invert=invert,
        )

    hex_data, total, row_bytes, height = mono_image_to_gfa_hex(img)
    log.info(
        "ZPL graphic %sx%s dots, %d bytes/row, %d total (from %s)",
        img.size[0],
        height,
        row_bytes,
        total,
        path.name,
    )
    # copies in ^PQ; caller may also pass copies to lp — use 1 in ZPL if lp multiplies
    return build_zpl_label(
        hex_data,
        total_bytes=total,
        bytes_per_row=row_bytes,
        x=x,
        y=y,
        copies=1,  # quantity handled by lp -n when possible
    )


def write_zpl_file(
    path: Path,
    dest_dir: Path,
    job_id: str,
    *,
    options: dict[str, Any] | None = None,
) -> Path:
    """Convert *path* to a temp ``.zpl`` file under *dest_dir*."""
    zpl = image_path_to_zpl(path, options=options)
    dest_dir.mkdir(parents=True, exist_ok=True)
    out = dest_dir / f"{job_id}.zpl"
    out.write_text(zpl, encoding="ascii")
    return out


def is_graphic_path(path: Path) -> bool:
    return path.suffix.lower() in {".pdf", ".png", ".jpg", ".jpeg", ".bmp", ".gif"}


def should_convert_to_zpl(
    path: Path,
    *,
    cups_name: str,
    job_options: dict[str, Any] | None = None,
    force_raw: bool = False,
) -> bool:
    """True when a graphic file should be converted for a raw thermal queue."""
    opts = job_options or {}
    if opts.get("no_zpl_convert") or opts.get("zpl_convert") is False:
        return False
    if opts.get("zpl_convert") is True or opts.get("force_zpl"):
        return is_graphic_path(path)
    if not is_graphic_path(path):
        return False
    # Explicit raw flag with a PDF/image file → convert
    if force_raw:
        return True
    try:
        from printers import queue_supports_raw

        return bool(queue_supports_raw(cups_name))
    except Exception:
        log.debug("queue_supports_raw failed for %s", cups_name, exc_info=True)
        # Heuristic: queue name looks like Zebra
        low = cups_name.lower()
        return "zebra" in low or "zpl" in low or "zd2" in low or "zd4" in low
