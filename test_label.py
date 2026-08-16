"""4×6 VESYL sample shipping label (PDF + ZPL).

Layout (203 dpi, 812×1218 dots):
  • VESYL logo top-left
  • TO: Road Runner + desert address
  • FROM: VESYL
  • Code 128 tracking barcode + human-readable number

Run:  python3 test_label.py
Writes assets/test-labels/vesyl-roadrunner-4x6.{pdf,zpl}
"""

from __future__ import annotations

from pathlib import Path

from PIL import Image, ImageDraw, ImageFont, ImageOps

import zpl as zpl_mod

ROOT = Path(__file__).resolve().parent
LOGO_PATH = ROOT / "assets" / "logo.png"
OUT_DIR = ROOT / "assets" / "test-labels"
PDF_NAME = "vesyl-roadrunner-4x6.pdf"
ZPL_NAME = "vesyl-roadrunner-4x6.zpl"

DPI = 203
WIDTH = 812
HEIGHT = 1218
TRACKING = "1Z999VES014200042"
RECIPIENT = "Road Runner"
ADDR_LINES = (
    "1 Acme Canyon Road",
    "Desert Valley, AZ 86038",
    "UNITED STATES",
)
FONT = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"
FONT_BOLD = "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"


def _font(path: str, size: int) -> ImageFont.FreeTypeFont:
    try:
        return ImageFont.truetype(path, size)
    except OSError:
        return ImageFont.load_default()


# Code 128-B (subset B) — enough for tracking / ASCII.
_CODE128_B_START = 104
_CODE128_STOP = 106
# 107 patterns: 6 bars/spaces, last is stop (7 elements). Values 0–106.
_PAT = (
    "212222", "222122", "222221", "121223", "121322", "131222", "122213",
    "122312", "132212", "221213", "221312", "231212", "112232", "122132",
    "122231", "113222", "123122", "123221", "223211", "221132", "221231",
    "213212", "223112", "312131", "311222", "321122", "321221", "312212",
    "322112", "322211", "212123", "212321", "232121", "111323", "131123",
    "131321", "112313", "132113", "132311", "211313", "231113", "231311",
    "112133", "112331", "132131", "113123", "113321", "133121", "313121",
    "211331", "231131", "213113", "213311", "213131", "311123", "311321",
    "331121", "312113", "312311", "332111", "314111", "221411", "431111",
    "111224", "111422", "121124", "121421", "141122", "141221", "112214",
    "112412", "122114", "122411", "142112", "142211", "241211", "221114",
    "413111", "241112", "134111", "111242", "121142", "121241", "114212",
    "124112", "124211", "411212", "421112", "421211", "212141", "214121",
    "412121", "111143", "111341", "131141", "114113", "114311", "411113",
    "411311", "113141", "114131", "311141", "411131", "211412", "211214",
    "211232", "2331112",
)


def code128_values(text: str) -> list[int]:
    vals = [_CODE128_B_START]
    checksum = _CODE128_B_START
    for i, ch in enumerate(text, start=1):
        v = ord(ch) - 32
        if v < 0 or v > 95:
            raise ValueError(f"Code 128-B cannot encode {ch!r}")
        vals.append(v)
        checksum += i * v
    vals.append(checksum % 103)
    vals.append(_CODE128_STOP)
    return vals


def draw_code128(
    draw: ImageDraw.ImageDraw,
    text: str,
    *,
    x: int,
    y: int,
    height: int,
    module: int = 2,
    fill: int = 0,
) -> int:
    """Draw Code 128-B; returns width in pixels."""
    vals = code128_values(text)
    cx = x
    for v in vals:
        pat = _PAT[v]
        bar = True
        for ch in pat:
            w = int(ch) * module
            if bar:
                draw.rectangle([cx, y, cx + w - 1, y + height - 1], fill=fill)
            cx += w
            bar = not bar
    # quiet zone
    return cx - x + 10 * module


def _logo_mark() -> Image.Image:
    """Black-on-white mark for thermal / PDF.

    Brand art is yellow/white on a dark field. Flatten onto black so
    transparent pixels stay paper-white after invert.
    """
    if LOGO_PATH.is_file():
        raw = Image.open(LOGO_PATH).convert("RGBA")
        bg = Image.new("RGB", raw.size, (0, 0, 0))
        bg.paste(raw, mask=raw.split()[-1])
        gray = ImageOps.invert(bg.convert("L"))
        gray = gray.point(lambda p: 0 if p < 80 else 255)
        return gray
    img = Image.new("L", (200, 48), 255)
    d = ImageDraw.Draw(img)
    d.rectangle([0, 0, 40, 40], fill=0)
    d.text((50, 8), "VESYL", font=_font(FONT_BOLD, 28), fill=0)
    return img


def render_pdf_image() -> Image.Image:
    img = Image.new("L", (WIDTH, HEIGHT), 255)
    d = ImageDraw.Draw(img)
    f_sm = _font(FONT, 22)
    f_md = _font(FONT, 28)
    f_lg = _font(FONT_BOLD, 40)
    f_xl = _font(FONT_BOLD, 48)
    f_tiny = _font(FONT, 18)

    logo = _logo_mark()
    lw = 280
    lh = max(1, round(logo.height * lw / logo.width))
    logo = logo.resize((lw, lh), Image.Resampling.LANCZOS)
    img.paste(logo, (28, 24))

    d.text((28, 24 + lh + 16), "TEST LABEL  ·  4 × 6", font=f_tiny, fill=80)
    y = 24 + lh + 48
    d.line([(28, y), (WIDTH - 28, y)], fill=0, width=3)
    y += 20

    d.text((28, y), "FROM", font=f_tiny, fill=80)
    y += 22
    d.text((28, y), "VESYL Shipping", font=f_md, fill=0)
    y += 34
    d.text((28, y), "Warehouse test desk", font=f_sm, fill=0)
    y += 50

    d.text((28, y), "TO", font=f_tiny, fill=80)
    y += 24
    d.text((28, y), RECIPIENT, font=f_xl, fill=0)
    y += 56
    for line in ADDR_LINES:
        d.text((28, y), line, font=f_md, fill=0)
        y += 34

    y += 16
    d.line([(28, y), (WIDTH - 28, y)], fill=0, width=2)
    y += 24
    d.text((28, y), "TRACKING", font=f_tiny, fill=80)
    y += 22
    d.text((28, y), TRACKING, font=f_lg, fill=0)
    y += 56

    bc_h = 140
    draw_code128(d, TRACKING, x=36, y=y, height=bc_h, module=2)
    y += bc_h + 16
    d.text((28, y), TRACKING, font=f_md, fill=0)

    d.text(
        (28, HEIGHT - 40),
        "VESYL print-node sample  ·  not a real shipment",
        font=f_tiny,
        fill=80,
    )
    return img


def write_pdf(path: Path | None = None) -> Path:
    out = path or (OUT_DIR / PDF_NAME)
    out.parent.mkdir(parents=True, exist_ok=True)
    img = render_pdf_image()
    rgb = img.convert("RGB")
    rgb.save(out, "PDF", resolution=float(DPI))
    return out


def _logo_gfa(width: int = 280) -> tuple[str, int, int, int, int]:
    """Return (^GFA hex, total, row_bytes, width, height) for the logo mark."""
    logo = _logo_mark()
    lh = max(1, round(logo.height * width / logo.width))
    logo = logo.resize((width, lh), Image.Resampling.LANCZOS)
    pad = (8 - (logo.width % 8)) % 8
    if pad:
        canvas = Image.new("L", (logo.width + pad, logo.height), 255)
        canvas.paste(logo, (0, 0))
        logo = canvas
    bw = logo.point(lambda p: 0 if p < 128 else 255, mode="1")
    hex_data, total, row_b, h = zpl_mod.mono_image_to_gfa_hex(bw)
    return hex_data, total, row_b, logo.width, h


def write_zpl(path: Path | None = None) -> Path:
    """Native ZPL (logo as ^GFA, Code 128 via ^BC) — not a PDF conversion."""
    out = path or (OUT_DIR / ZPL_NAME)
    out.parent.mkdir(parents=True, exist_ok=True)
    hex_data, total, row_b, _lw, lh = _logo_gfa(280)
    y = 24 + lh + 16
    lines = [
        "^XA",
        "^LH0,0",
        "^FWN",
        f"^PW{WIDTH}",
        f"^LL{HEIGHT}",
        f"^FO28,24^GFA,{total},{total},{row_b},{hex_data}^FS",
        f"^FO28,{y}^A0N,22,22^FDTEST LABEL  4x6^FS",
    ]
    y += 36
    lines.append(f"^FO28,{y}^GB756,3,3^FS")
    y += 20
    lines += [
        f"^FO28,{y}^A0N,22,22^FDFROM^FS",
        f"^FO28,{y + 30}^A0N,36,36^FDVESYL Shipping^FS",
        f"^FO28,{y + 80}^A0N,28,28^FDWarehouse test desk^FS",
        f"^FO28,{y + 140}^A0N,22,22^FDTO^FS",
        f"^FO28,{y + 170}^A0N,56,56^FD{RECIPIENT}^FS",
        f"^FO28,{y + 240}^A0N,32,32^FD{ADDR_LINES[0]}^FS",
        f"^FO28,{y + 280}^A0N,32,32^FD{ADDR_LINES[1]}^FS",
        f"^FO28,{y + 320}^A0N,32,32^FD{ADDR_LINES[2]}^FS",
    ]
    y += 380
    lines += [
        f"^FO28,{y}^GB756,2,2^FS",
        f"^FO28,{y + 20}^A0N,22,22^FDTRACKING^FS",
        f"^FO28,{y + 50}^A0N,40,40^FD{TRACKING}^FS",
        f"^FO36,{y + 110}^BY2,3,140",
        f"^BCN,140,Y,N,N^FD{TRACKING}^FS",
        "^FO28,1180^A0N,20,20^FDVESYL print-node sample  -  not a real shipment^FS",
        "^XZ",
    ]
    out.write_text("\n".join(lines) + "\n", encoding="ascii")
    return out


def ensure_test_labels() -> tuple[Path, Path]:
    """Create PDF + ZPL if missing; return (pdf_path, zpl_path)."""
    pdf = OUT_DIR / PDF_NAME
    zpl = OUT_DIR / ZPL_NAME
    if not pdf.is_file():
        write_pdf(pdf)
    if not zpl.is_file():
        write_zpl(zpl)
    return pdf, zpl


def submit_test_label(
    cups_name: str,
    fmt: str,
    *,
    lp=None,
    wait_cups: bool = False,
) -> str:
    """Print the sample label through the local job pipeline.

    ``fmt`` is ``pdf`` (any queue; converted to ZPL graphics on raw/Zebra)
    or ``zpl`` (native ^BC payload; raw queues only).
    Uses a private JobStore so LCD test prints do not collide with the agent.
    """
    import tempfile
    import shutil

    import jobs

    kind = (fmt or "").strip().lower()
    if kind not in ("pdf", "zpl"):
        raise jobs.JobError(f"unsupported test format: {fmt}", code="invalid_job")
    queue = (cups_name or "").strip()
    if not queue:
        raise jobs.JobError("missing cups_name", code="invalid_job")

    pdf, zpl = ensure_test_labels()
    path = zpl if kind == "zpl" else pdf
    job = jobs.job_from_local_file(
        path,
        queue,
        title=f"VESYL test {kind.upper()} {TRACKING}",
        raw=(kind == "zpl"),
    )
    td = Path(tempfile.mkdtemp(prefix="vesyl-test-print-"))
    try:
        store = jobs.JobStore(queue_dir=td / "q", processed_dir=td / "p")
        runner = lp if lp is not None else jobs.default_lp
        return jobs.receive_job(job, store, lp=runner, wait_cups=wait_cups)
    finally:
        shutil.rmtree(td, ignore_errors=True)


def main() -> None:
    pdf = write_pdf()
    zpl = write_zpl()
    print(f"wrote {pdf}")
    print(f"wrote {zpl}")


if __name__ == "__main__":
    main()
