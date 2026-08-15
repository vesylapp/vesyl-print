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

# 203 dpi desktop Zebra. Default media is 4"×6" shipping labels.
DEFAULT_DPI = 203
DEFAULT_MAX_WIDTH_DOTS = 812   # 4.00" × 203
DEFAULT_MAX_HEIGHT_DOTS = 1218  # 6.00" × 203
DEFAULT_THRESHOLD = 128
# Shift graphic down so it isn't clipped by the top of the label / printhead.
# 32 dots ≈ 4 mm at 203 dpi.
DEFAULT_TOP_MARGIN_DOTS = 32


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


def infer_media_width_dots(cups_name: str | None, *, dpi: int = DEFAULT_DPI) -> int:
    """Guess printable width from the CUPS queue name (model / dpi token).

    Default is **4×6** (812 dots @ 203 dpi). Only well-known 2" models
    (ZD220 / ZD230) are capped at 448 dots.
    """
    name = (cups_name or "").lower()
    if "300dpi" in name:
        dpi = 300
    elif "203dpi" in name or "200dpi" in name:
        dpi = 203
    # ZD220 / ZD230 / ZD421 / ZD621 are 4" desktop printers (not 2").
    if any(
        t in name
        for t in (
            "zd220",
            "zd230",
            "zd421",
            "zd621",
            "zt410",
            "zt411",
            "gk420",
            "gx430",
        )
    ):
        return 812 if dpi <= 203 else int(round(4.09 * dpi))
    return DEFAULT_MAX_WIDTH_DOTS


def infer_media_height_dots(cups_name: str | None, *, dpi: int = DEFAULT_DPI) -> int:
    """Guess label length. Default 6" (1218 @ 203) for 4×6 stock."""
    name = (cups_name or "").lower()
    if "300dpi" in name:
        dpi = 300
    elif "203dpi" in name or "200dpi" in name:
        dpi = 203
    return DEFAULT_MAX_HEIGHT_DOTS if dpi <= 203 else int(round(6.0 * dpi))


def load_image_as_mono(
    path: Path,
    *,
    max_width_dots: int = DEFAULT_MAX_WIDTH_DOTS,
    max_height_dots: int | None = None,
    threshold: int = DEFAULT_THRESHOLD,
    invert: bool = False,
    fit: str = "width",
):
    """Load image/PDF path → Pillow 1-bit image (black=0).

    ``fit=width`` scales up or down so the image fills ``max_width_dots``.
    """
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

    # Crop empty page margins so a 4×6 design on a larger PDF page still fills.
    try:
        # Darker than ~250 counts as content
        mask = img.point(lambda x: 255 if x < 250 else 0)
        bbox = mask.getbbox()
        if bbox:
            img = img.crop(bbox)
    except Exception:
        pass

    w, h = img.size
    if w < 1 or h < 1:
        raise ZplError("empty image", code="image_bad")

    max_w = max(8, int(max_width_dots))
    max_h = int(max_height_dots) if max_height_dots else None
    fit_mode = (fit or "width").lower()

    scale = 1.0
    if fit_mode == "width":
        scale = max_w / w
        if max_h and h * scale > max_h:
            scale = max_h / h
    elif fit_mode == "contain":
        if w > max_w:
            scale = min(scale, max_w / w)
        if max_h and h > max_h:
            scale = min(scale, max_h / h)

    if abs(scale - 1.0) > 0.001:
        nw = max(1, int(round(w * scale)))
        nh = max(1, int(round(h * scale)))
        img = img.resize((nw, nh), Image.Resampling.LANCZOS)
        w, h = img.size

    pad_w = (8 - (w % 8)) % 8
    if pad_w:
        canvas = Image.new("L", (w + pad_w, h), 255)
        canvas.paste(img, (0, 0))
        img = canvas

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
    height_dots: int | None = None,
    x: int = 0,
    y: int = 0,
    copies: int = 1,
) -> str:
    """Build a complete ZPL label with one ^GFA graphic.

    ``^PW`` / ``^LL`` match the bitmap (plus ``y`` offset). Print quantity is
    left to CUPS ``lp -n`` unless copies > 1.
    """
    copies = max(1, int(copies))
    width_dots = max(8, int(bytes_per_row) * 8)
    parts = [
        "^XA",
        "^LH0,0",
        "^FWN",
        f"^PW{width_dots}",
    ]
    if height_dots and height_dots > 0:
        parts.append(f"^LL{int(height_dots) + max(0, int(y))}")
    parts.append(f"^FO{x},{y}^GFA,{total_bytes},{total_bytes},{bytes_per_row},{hex_data}^FS")
    if copies > 1:
        parts.append(f"^PQ{copies}")
    parts.append("^XZ")
    return "\n".join(parts) + "\n"


def image_path_to_zpl(
    path: Path,
    *,
    options: dict[str, Any] | None = None,
    copies: int = 1,
) -> str:
    """Convert a PDF/PNG/JPEG path to a ZPL string (embedded ^GFA)."""
    opts = options or {}
    dpi = _opt_int(opts, "zpl_dpi", DEFAULT_DPI)
    cups = str(opts.get("cups_name") or "")
    default_w = infer_media_width_dots(cups, dpi=dpi)
    default_h = infer_media_height_dots(cups, dpi=dpi)
    max_w = _opt_int(opts, "zpl_max_width_dots", default_w)
    # Aliases
    if "label_width_dots" in opts:
        max_w = _opt_int(opts, "label_width_dots", max_w)
    fit = str(opts.get("zpl_fit") or "contain").lower()
    max_h = _opt_int(opts, "zpl_max_height_dots", default_h)
    if opts.get("label_height_dots") is not None:
        max_h = _opt_int(opts, "label_height_dots", max_h)
    if max_h <= 0:
        max_h = default_h
    thr = _opt_int(opts, "zpl_threshold", DEFAULT_THRESHOLD)
    invert = bool(opts.get("zpl_invert") or opts.get("invert"))
    x = _opt_int(opts, "zpl_x", 0)
    y = _opt_int(opts, "zpl_y", DEFAULT_TOP_MARGIN_DOTS)

    # Leave room at the bottom so ^FO y-shift does not run off the label
    # (overflow onto the next gap looks like an extra blank label).
    if y > 0 and max_h:
        max_h = max(8, max_h - y)

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
                fit=fit,
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
            fit=fit,
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
        height_dots=height,
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
