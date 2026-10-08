//! Convert PDF / raster images to ZPL for raw thermal (Zebra) queues.
//!
//! Pipeline:
//!   1. PDF → PNG via `pdftoppm` (poppler) or Ghostscript fallback
//!   2. Raster → 1-bit monochrome
//!   3. Encode as ZPL `^GFA` hex graphic field (ASCII hex stream)
//!
//! `~DY` download-to-printer is not used: each job embeds the graphic in a
//! self-contained `^XA`…`^XZ` label so CUPS raw queues stay stateless.

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::Duration;

use image::imageops::FilterType;
use image::{GrayImage, ImageFormat, ImageReader, Luma};
use serde_json::Value;

use crate::printers::{run_with_timeout, which, CmdOutput};
use crate::util::{get_truthy, py_int, py_str, truthy};
use crate::{BoxError, JsonObject};

const LOG: &str = "vesyl-print.zpl";

/// 203 dpi desktop Zebra. Default media is 4"×6" shipping labels.
pub const DEFAULT_DPI: i64 = 203;
pub const DEFAULT_MAX_WIDTH_DOTS: i64 = 812; // 4.00" × 203
pub const DEFAULT_MAX_HEIGHT_DOTS: i64 = 1218; // 6.00" × 203
pub const DEFAULT_THRESHOLD: i64 = 128;
/// Shift graphic down so it isn't clipped by the top of the label / printhead.
/// 32 dots ≈ 4 mm at 203 dpi.
pub const DEFAULT_TOP_MARGIN_DOTS: i64 = 32;
/// Highest `zpl_dpi` honored: Zebra heads top out at 600 dpi, and more only
/// costs memory when rasterizing.
const MAX_DPI: i64 = 600;
/// Most PDF pages converted for one job. A longer document fails instead of
/// building a huge raw stream (and temp files) on the Pi.
const MAX_PDF_PAGES: i64 = 50;

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct ZplError {
    pub message: String,
    pub code: &'static str,
}

impl ZplError {
    fn new(message: impl Into<String>, code: &'static str) -> Self {
        ZplError {
            message: message.into(),
            code,
        }
    }
}

fn opt_int(options: &JsonObject, key: &str, default: i64) -> i64 {
    match options.get(key) {
        None | Some(Value::Null) => default,
        Some(v) => py_int(v).unwrap_or(default),
    }
}

/// Python 3 `round()` (half to even) — keeps resize targets identical.
fn round_half_even(x: f64) -> i64 {
    let r = x.round();
    if (x - x.trunc()).abs() == 0.5 && (r as i64) % 2 != 0 {
        (r - x.signum()) as i64
    } else {
        r as i64
    }
}

/// Render one PDF page to PNG. Prefers pdftoppm; falls back to Ghostscript.
pub fn pdf_to_png(
    pdf_path: &Path,
    out_dir: &Path,
    dpi: i64,
    page: i64,
) -> Result<PathBuf, ZplError> {
    fs::create_dir_all(out_dir).map_err(|e| ZplError::new(e.to_string(), "pdf_render"))?;
    let stem_name = pdf_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = out_dir.join(format!("{stem_name}_p{page}"));
    let png = PathBuf::from(format!("{}.png", stem.display()));
    let res = max(72, dpi).to_string();
    let page_s = page.to_string();
    let pdf = pdf_path.display().to_string();

    let (tool, args): (String, Vec<String>) = if let Some(pdftoppm) = which("pdftoppm") {
        // -singlefile writes stem.png
        let args = [
            "-png",
            "-r",
            &res,
            "-f",
            &page_s,
            "-l",
            &page_s,
            "-singlefile",
            &pdf,
        ]
        .iter()
        .map(|s| s.to_string())
        .chain([stem.display().to_string()])
        .collect();
        (pdftoppm, args)
    } else if let Some(gs) = which("gs") {
        let args = vec![
            "-dSAFER".into(),
            "-dBATCH".into(),
            "-dNOPAUSE".into(),
            format!("-dFirstPage={page}"),
            format!("-dLastPage={page}"),
            format!("-r{res}"),
            "-sDEVICE=pnggray".into(),
            format!("-sOutputFile={}", png.display()),
            pdf,
        ];
        (gs, args)
    } else {
        return Err(ZplError::new(
            "PDF→image needs pdftoppm (poppler-utils) or ghostscript",
            "pdf_render",
        ));
    };

    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = run_with_timeout(&tool, &argv, Duration::from_secs(120))
        .map_err(|e| ZplError::new(format!("{tool} failed: {e}"), "pdf_render"))?;
    if !out.success || !png.is_file() {
        return Err(render_error(&tool, out));
    }
    Ok(png)
}

/// `pdf_render` error from a failed tool run: stderr, else stdout.
fn render_error(tool: &str, out: CmdOutput) -> ZplError {
    let msg = if !out.stderr.trim().is_empty() {
        out.stderr
    } else {
        out.stdout
    };
    let msg = msg.trim();
    ZplError::new(
        if msg.is_empty() {
            format!("{tool} failed")
        } else {
            msg.to_string()
        },
        "pdf_render",
    )
}

/// A PDF rasterizer on PATH: pdftoppm (poppler) preferred, else Ghostscript.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Renderer {
    Pdftoppm(String),
    Ghostscript(String),
}

impl Renderer {
    fn find() -> Result<Self, ZplError> {
        if let Some(tool) = which("pdftoppm") {
            Ok(Renderer::Pdftoppm(tool))
        } else if let Some(tool) = which("gs") {
            Ok(Renderer::Ghostscript(tool))
        } else {
            Err(ZplError::new(
                "PDF→image needs pdftoppm (poppler-utils) or ghostscript",
                "pdf_render",
            ))
        }
    }
}

/// Render pages `1..=last` of a PDF into `out_dir` with one tool run (both
/// tools stop at the document's real last page). Returns the PNGs in page
/// order.
fn pdf_pages_to_png(
    renderer: &Renderer,
    pdf_path: &Path,
    out_dir: &Path,
    dpi: i64,
    last: i64,
) -> Result<Vec<PathBuf>, ZplError> {
    fs::create_dir_all(out_dir).map_err(|e| ZplError::new(e.to_string(), "pdf_render"))?;
    let root = out_dir.join("page").display().to_string();
    let res = max(72, dpi).to_string();
    let pdf = pdf_path.display().to_string();
    let (tool, args): (&str, Vec<String>) = match renderer {
        // page-1.png, page-2.png …, zero-padded to the width of the page count.
        Renderer::Pdftoppm(tool) => (
            tool,
            vec![
                "-png".into(),
                "-r".into(),
                res,
                "-f".into(),
                "1".into(),
                "-l".into(),
                last.to_string(),
                pdf,
                root,
            ],
        ),
        // page-1.png, page-2.png …; a literal % in the path must be doubled.
        Renderer::Ghostscript(tool) => (
            tool,
            vec![
                "-dSAFER".into(),
                "-dBATCH".into(),
                "-dNOPAUSE".into(),
                "-dFirstPage=1".into(),
                format!("-dLastPage={last}"),
                format!("-r{res}"),
                "-sDEVICE=pnggray".into(),
                format!("-sOutputFile={}-%d.png", root.replace('%', "%%")),
                pdf,
            ],
        ),
    };
    // A single page keeps its 120 s; each further page adds 10 s.
    let timeout = Duration::from_secs(120 + 10 * (last.max(1) as u64 - 1));
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = run_with_timeout(tool, &argv, timeout)
        .map_err(|e| ZplError::new(format!("{tool} failed: {e}"), "pdf_render"))?;
    if !out.success {
        return Err(render_error(tool, out));
    }
    // Sort by page number: pdftoppm pads (page-01.png) and gs does not.
    let mut pages: Vec<(u64, PathBuf)> = fs::read_dir(out_dir)
        .map_err(|e| ZplError::new(e.to_string(), "pdf_render"))?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let page = name
                .to_str()?
                .strip_prefix("page-")?
                .strip_suffix(".png")?
                .parse()
                .ok()?;
            Some((page, e.path()))
        })
        .collect();
    pages.sort();
    if pages.is_empty() {
        return Err(ZplError::new(
            format!("{tool} rendered no pages"),
            "pdf_render",
        ));
    }
    Ok(pages.into_iter().map(|(_, png)| png).collect())
}

/// Page count from `pdfinfo` (poppler-utils, which also ships pdftoppm).
fn pdf_page_count(pdf_path: &Path) -> Option<i64> {
    let tool = which("pdfinfo")?;
    let pdf = pdf_path.display().to_string();
    let out = run_with_timeout(&tool, &[&pdf], Duration::from_secs(30)).ok()?;
    if !out.success {
        return None;
    }
    out.stdout
        .lines()
        .find_map(|l| l.strip_prefix("Pages:"))
        .and_then(|n| n.trim().parse().ok())
}

/// Render every page of a PDF, in order, up to [`MAX_PDF_PAGES`]. `pages` is
/// the page count when known; otherwise one page past the limit is rendered
/// so an over-long document is still caught.
fn pdf_all_pages_to_png(
    renderer: &Renderer,
    pdf_path: &Path,
    out_dir: &Path,
    dpi: i64,
    pages: Option<i64>,
) -> Result<Vec<PathBuf>, ZplError> {
    let too_many = |count: String| {
        ZplError::new(
            format!(
                "PDF has {count} pages; ZPL conversion handles at most {MAX_PDF_PAGES} \
                 (set zpl_page to print a single page)"
            ),
            "pdf_too_many_pages",
        )
    };
    let last = match pages {
        Some(n) if n > MAX_PDF_PAGES => return Err(too_many(n.to_string())),
        Some(n) if n >= 1 => n,
        _ => MAX_PDF_PAGES + 1,
    };
    let pngs = pdf_pages_to_png(renderer, pdf_path, out_dir, dpi, last)?;
    if pngs.len() as i64 > MAX_PDF_PAGES {
        return Err(too_many(format!("more than {MAX_PDF_PAGES}")));
    }
    Ok(pngs)
}

fn max(a: i64, b: i64) -> i64 {
    a.max(b)
}

/// Head resolution from a model/queue name token (`Zebra_ZD421-300dpi_ZPL`).
fn dpi_from_name(name: &str, dpi: i64) -> i64 {
    if name.contains("600dpi") {
        600
    } else if name.contains("300dpi") {
        300
    } else if name.contains("203dpi") || name.contains("200dpi") {
        203
    } else {
        dpi
    }
}

/// Rasterization dpi for a job: an explicit `zpl_dpi` (clamped to 72–600),
/// else the queue name's dpi token, else 203. Rendering a PDF at the head's
/// own resolution keeps the label at its real size: a 300 dpi head printing
/// a 203 dpi raster prints it at 68%.
fn effective_dpi(opts: &JsonObject, cups_name: &str) -> i64 {
    match opts.get("zpl_dpi").and_then(py_int) {
        Some(dpi) if dpi > 0 => dpi.clamp(72, MAX_DPI),
        _ => dpi_from_name(&cups_name.to_lowercase(), DEFAULT_DPI),
    }
}

/// Guess printable width from the CUPS queue name (model / dpi token).
///
/// Default is **4×6** (812 dots @ 203 dpi); a model we don't know keeps the
/// 4" width at higher resolutions (1200 dots @ 300 dpi).
pub fn infer_media_width_dots(cups_name: Option<&str>, dpi: i64) -> i64 {
    let name = cups_name.unwrap_or_default().to_lowercase();
    let dpi = dpi_from_name(&name, dpi);
    if dpi <= DEFAULT_DPI {
        return DEFAULT_MAX_WIDTH_DOTS;
    }
    // ZD220 / ZD230 / ZD421 / ZD621 are 4" desktop printers (not 2").
    const FOUR_INCH: &[&str] = &[
        "zd220", "zd230", "zd421", "zd621", "zt410", "zt411", "gk420", "gx430",
    ];
    if FOUR_INCH.iter().any(|t| name.contains(t)) {
        round_half_even(4.09 * dpi as f64)
    } else {
        round_half_even(DEFAULT_MAX_WIDTH_DOTS as f64 * dpi as f64 / DEFAULT_DPI as f64)
    }
}

/// Guess label length. Default 6" (1218 @ 203) for 4×6 stock.
pub fn infer_media_height_dots(cups_name: Option<&str>, dpi: i64) -> i64 {
    let name = cups_name.unwrap_or_default().to_lowercase();
    let dpi = dpi_from_name(&name, dpi);
    if dpi <= 203 {
        DEFAULT_MAX_HEIGHT_DOTS
    } else {
        round_half_even(6.0 * dpi as f64)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    /// Scale up or down so the image fills the width (capped by height).
    Width,
    /// Only scale down to fit.
    Contain,
    /// Leave size alone.
    None,
}

impl Fit {
    fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "width" => Fit::Width,
            "contain" => Fit::Contain,
            _ => Fit::None,
        }
    }
}

/// Grayscale like Pillow `convert("L")` (ITU-R 601-2), after compositing any
/// transparency onto white: a transparent background stored as black
/// (canvas exports, palette/GIF transparency) is paper, not a solid black
/// label.
fn to_pil_luma(img: &image::DynamicImage) -> GrayImage {
    let luma = |r: u8, g: u8, b: u8| {
        (r as u32 * 19595 + g as u32 * 38470 + b as u32 * 7471 + 0x8000) >> 16
    };
    if img.color().has_alpha() {
        let rgba = img.to_rgba8();
        GrayImage::from_fn(rgba.width(), rgba.height(), |x, y| {
            let [r, g, b, a] = rgba.get_pixel(x, y).0;
            let (l, a) = (luma(r, g, b), a as u32);
            Luma([((l * a + 255 * (255 - a) + 127) / 255) as u8])
        })
    } else {
        let rgb = img.to_rgb8();
        GrayImage::from_fn(rgb.width(), rgb.height(), |x, y| {
            let [r, g, b] = rgb.get_pixel(x, y).0;
            Luma([luma(r, g, b) as u8])
        })
    }
}

/// Bounding box of pixels darker than 250 → (x0, y0, x1, y1) exclusive.
fn content_bbox(img: &GrayImage) -> Option<(u32, u32, u32, u32)> {
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    for (x, y, p) in img.enumerate_pixels() {
        if p.0[0] < 250 {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x + 1);
            y1 = y1.max(y + 1);
        }
    }
    (x0 != u32::MAX).then_some((x0, y0, x1, y1))
}

/// Load image/PDF path → 1-bit image as a `GrayImage` (black = 0, white = 255),
/// width padded to a multiple of 8.
pub fn load_image_as_mono(
    path: &Path,
    max_width_dots: i64,
    max_height_dots: Option<i64>,
    threshold: i64,
    invert: bool,
    fit: Fit,
) -> Result<GrayImage, ZplError> {
    let is_pdf = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
    let mut img = if is_pdf {
        let work = tempfile::Builder::new()
            .prefix("vesyl-zpl-pdf-")
            .tempdir()
            .map_err(|e| ZplError::new(e.to_string(), "image_bad"))?;
        let png = pdf_to_png(path, work.path(), DEFAULT_DPI, 1)?;
        open_gray(&png)?
    } else {
        open_gray(path)?
    };

    // Optional invert (white-on-black PDF backgrounds).
    if invert {
        image::imageops::invert(&mut img);
    }

    // Crop empty page margins so a 4×6 design on a larger PDF page still fills.
    if let Some((x0, y0, x1, y1)) = content_bbox(&img) {
        img = image::imageops::crop_imm(&img, x0, y0, x1 - x0, y1 - y0).to_image();
    }

    let (mut w, mut h) = img.dimensions();
    if w < 1 || h < 1 {
        return Err(ZplError::new("empty image", "image_bad"));
    }

    let max_w = max_width_dots.max(8) as f64;
    let max_h = max_height_dots.filter(|h| *h != 0).map(|h| h as f64);
    let (wf, hf) = (w as f64, h as f64);
    let mut scale = 1.0f64;
    match fit {
        Fit::Width => {
            scale = max_w / wf;
            if let Some(mh) = max_h {
                if hf * scale > mh {
                    scale = mh / hf;
                }
            }
        }
        Fit::Contain => {
            if wf > max_w {
                scale = scale.min(max_w / wf);
            }
            if let Some(mh) = max_h {
                if hf > mh {
                    scale = scale.min(mh / hf);
                }
            }
        }
        Fit::None => {}
    }

    if (scale - 1.0).abs() > 0.001 {
        let nw = round_half_even(wf * scale).max(1) as u32;
        let nh = round_half_even(hf * scale).max(1) as u32;
        img = image::imageops::resize(&img, nw, nh, FilterType::Lanczos3);
        (w, h) = img.dimensions();
    }

    let pad_w = (8 - (w % 8)) % 8;
    let thr = threshold.clamp(0, 255) as u8;
    // 1-bit: black (print) = 0.
    Ok(GrayImage::from_fn(w + pad_w, h, |x, y| {
        let v = if x < w { img.get_pixel(x, y).0[0] } else { 255 };
        Luma([if v < thr { 0 } else { 255 }])
    }))
}

/// Decode by content, not file name (like Pillow's `Image.open`): carrier
/// TIFF or CDN WebP bytes arrive saved as `.png`.
fn open_gray(path: &Path) -> Result<GrayImage, ZplError> {
    let bad =
        |e: &dyn std::fmt::Display| ZplError::new(format!("open image failed: {e}"), "image_bad");
    let decoded = ImageReader::open(path)
        .and_then(|r| r.with_guessed_format())
        .map_err(|e| bad(&e))?
        .decode();
    let img = match decoded {
        Ok(img) => img,
        Err(e) => {
            // Pillow prints PNGs with a missing IEND or a bad chunk CRC; the
            // png crate rejects them. Retry once with the framing repaired.
            let repaired = fs::read(path)
                .ok()
                .and_then(|bytes| repair_png(&bytes))
                .and_then(|png| {
                    ImageReader::with_format(Cursor::new(png), ImageFormat::Png)
                        .decode()
                        .ok()
                });
            match repaired {
                Some(img) => {
                    log::warn!(target: LOG, "{}: repaired damaged PNG ({e})", path.display());
                    img
                }
                None => return Err(bad(&e)),
            }
        }
    };
    Ok(to_pil_luma(&img))
}

/// Re-frame a damaged PNG the way Pillow tolerates it: every chunk CRC
/// recomputed, a truncated trailing chunk dropped and a missing IEND added.
/// `None` when the bytes are not a PNG or have nothing to repair.
fn repair_png(bytes: &[u8]) -> Option<Vec<u8>> {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    let crc = |type_and_data: &[u8]| {
        let mut crc = flate2::Crc::new();
        crc.update(type_and_data);
        crc.sum().to_be_bytes()
    };
    let mut rest = bytes.strip_prefix(SIGNATURE)?;
    let mut out = SIGNATURE.to_vec();
    let mut changed = false;
    while rest.len() >= 8 {
        let len = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
        let (data_end, crc_end) = (len.saturating_add(8), len.saturating_add(12));
        // Chunk type + data; a chunk cut short is dropped.
        let Some(body) = rest.get(4..data_end) else {
            break;
        };
        let sum = crc(body);
        changed |= rest.get(data_end..crc_end) != Some(&sum[..]);
        out.extend_from_slice(&rest[..4]);
        out.extend_from_slice(body);
        out.extend_from_slice(&sum);
        if &body[..4] == b"IEND" {
            return changed.then_some(out);
        }
        rest = rest.get(crc_end..).unwrap_or_default();
    }
    // Ran out of chunks before IEND.
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(b"IEND");
    out.extend_from_slice(&crc(b"IEND"));
    Some(out)
}

/// Encoded `^GFA` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gfa {
    pub hex: String,
    pub total_bytes: usize,
    pub bytes_per_row: usize,
    pub height: usize,
}

/// Encode a mono image (0 = black) as ZPL ^GFA hex.
///
/// Bit 1 = black (print); MSB first within each byte (Zebra convention).
pub fn mono_image_to_gfa_hex(img: &GrayImage) -> Gfa {
    let (width, height) = (img.width() as usize, img.height() as usize);
    let row_bytes = width.div_ceil(8);
    let mut hex = String::with_capacity(row_bytes * height * 2);
    for y in 0..height {
        for bx in 0..row_bytes {
            let mut byte = 0u8;
            for bit in 0..8 {
                let x = bx * 8 + bit;
                if x < width && img.get_pixel(x as u32, y as u32).0[0] == 0 {
                    byte |= 1 << (7 - bit);
                }
            }
            hex.push_str(&format!("{byte:02X}"));
        }
    }
    Gfa {
        hex,
        total_bytes: row_bytes * height,
        bytes_per_row: row_bytes,
        height,
    }
}

/// Build a complete ZPL label with one ^GFA graphic.
///
/// `^PW` / `^LL` match the bitmap (plus `y` offset). Print quantity is
/// left to CUPS `lp -n` unless copies > 1.
pub fn build_zpl_label(gfa: &Gfa, height_dots: Option<i64>, x: i64, y: i64, copies: i64) -> String {
    let copies = copies.max(1);
    let width_dots = (gfa.bytes_per_row as i64 * 8).max(8);
    let mut parts = vec![
        "^XA".to_string(),
        "^LH0,0".into(),
        "^FWN".into(),
        format!("^PW{width_dots}"),
    ];
    if let Some(h) = height_dots.filter(|h| *h > 0) {
        parts.push(format!("^LL{}", h + y.max(0)));
    }
    parts.push(format!(
        "^FO{x},{y}^GFA,{t},{t},{r},{hex}^FS",
        t = gfa.total_bytes,
        r = gfa.bytes_per_row,
        hex = gfa.hex
    ));
    if copies > 1 {
        parts.push(format!("^PQ{copies}"));
    }
    parts.push("^XZ".into());
    parts.join("\n") + "\n"
}

/// Convert a PDF/PNG/JPEG path to a ZPL string (embedded ^GFA).
///
/// Each PDF page becomes its own `^XA`…`^XZ` label, the way CUPS prints every
/// page on a filtered queue; `zpl_page` selects a single page instead.
pub fn image_path_to_zpl(path: &Path, opts: &JsonObject) -> Result<String, ZplError> {
    let cups = opts
        .get("cups_name")
        .filter(|v| truthy(v))
        .map(py_str)
        .unwrap_or_default();
    // One dpi for the PDF raster and the media size (whose queue-name token,
    // the head's real resolution, still decides the label box when present).
    let dpi = effective_dpi(opts, &cups);
    let default_w = infer_media_width_dots(Some(&cups), dpi);
    let default_h = infer_media_height_dots(Some(&cups), dpi);
    let mut max_w = opt_int(opts, "zpl_max_width_dots", default_w);
    // Aliases
    if opts.contains_key("label_width_dots") {
        max_w = opt_int(opts, "label_width_dots", max_w);
    }
    let fit = Fit::parse(
        &opts
            .get("zpl_fit")
            .filter(|v| truthy(v))
            .map(py_str)
            .unwrap_or("contain".into()),
    );
    let mut max_h = opt_int(opts, "zpl_max_height_dots", default_h);
    if opts.get("label_height_dots").is_some_and(|v| !v.is_null()) {
        max_h = opt_int(opts, "label_height_dots", max_h);
    }
    if max_h <= 0 {
        max_h = default_h;
    }
    let thr = opt_int(opts, "zpl_threshold", DEFAULT_THRESHOLD);
    let invert = get_truthy(opts, "zpl_invert") || get_truthy(opts, "invert");
    let x = opt_int(opts, "zpl_x", 0);
    let y = opt_int(opts, "zpl_y", DEFAULT_TOP_MARGIN_DOTS);

    // Leave room at the bottom so ^FO y-shift does not run off the label
    // (overflow onto the next gap looks like an extra blank label).
    if y > 0 && max_h != 0 {
        max_h = (max_h - y).max(8);
    }

    let is_pdf = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
    // `_work` keeps the rendered pages until they are converted.
    let (sources, _work) = if is_pdf {
        // DPI only affects PDF rasterization.
        let work = tempfile::Builder::new()
            .prefix("vesyl-zpl-pdf-")
            .tempdir()
            .map_err(|e| ZplError::new(e.to_string(), "pdf_render"))?;
        let pngs = if opts.get("zpl_page").is_some_and(|v| !v.is_null()) {
            vec![pdf_to_png(
                path,
                work.path(),
                dpi,
                opt_int(opts, "zpl_page", 1),
            )?]
        } else {
            let renderer = Renderer::find()?;
            pdf_all_pages_to_png(&renderer, path, work.path(), dpi, pdf_page_count(path))?
        };
        (pngs, Some(work))
    } else {
        (vec![path.to_path_buf()], None)
    };

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let pages = sources.len();
    if pages > 1 {
        log::info!(target: LOG, "{name}: {pages} PDF pages, one ZPL label each");
    }
    let mut zpl = String::new();
    for (i, source) in sources.iter().enumerate() {
        let img = load_image_as_mono(source, max_w, Some(max_h), thr, invert, fit)?;
        let gfa = mono_image_to_gfa_hex(&img);
        let page = if pages > 1 {
            format!(" page {}/{pages}", i + 1)
        } else {
            String::new()
        };
        log::info!(
            target: LOG,
            "ZPL graphic {}x{} dots, {} bytes/row, {} total (from {name}{page})",
            img.width(),
            gfa.height,
            gfa.bytes_per_row,
            gfa.total_bytes,
        );
        // Quantity is handled by lp -n (repeating the whole document), so
        // ^PQ stays 1.
        zpl.push_str(&build_zpl_label(&gfa, Some(gfa.height as i64), x, y, 1));
    }
    Ok(zpl)
}

/// Convert `path` to a temp `.zpl` file under `dest_dir`.
pub fn write_zpl_file(
    path: &Path,
    dest_dir: &Path,
    job_id: &str,
    opts: &JsonObject,
) -> Result<PathBuf, ZplError> {
    let zpl = image_path_to_zpl(path, opts)?;
    let io_err = |e: std::io::Error| ZplError::new(e.to_string(), "zpl_error");
    fs::create_dir_all(dest_dir).map_err(io_err)?;
    let out = dest_dir.join(format!("{job_id}.zpl"));
    fs::write(&out, zpl).map_err(io_err)?;
    Ok(out)
}

/// Files [`image_path_to_zpl`] can rasterize (TIFF and WebP decode too, so a
/// file saved under those names is converted rather than sent raw).
pub fn is_graphic_path(path: &Path) -> bool {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .is_some_and(|e| {
            matches!(
                e.as_str(),
                "pdf" | "png" | "jpg" | "jpeg" | "bmp" | "gif" | "tif" | "tiff" | "webp"
            )
        })
}

/// True when a graphic file should be converted for a raw thermal queue.
///
/// `supports_raw` is the CUPS probe (normally [`crate::printers::queue_supports_raw`]);
/// if it errors we fall back to a queue-name heuristic.
pub fn should_convert_to_zpl(
    path: &Path,
    cups_name: &str,
    opts: &JsonObject,
    force_raw: bool,
    supports_raw: &dyn Fn(&str) -> Result<bool, BoxError>,
) -> bool {
    if get_truthy(opts, "no_zpl_convert") || opts.get("zpl_convert") == Some(&Value::Bool(false)) {
        return false;
    }
    if opts.get("zpl_convert") == Some(&Value::Bool(true)) || get_truthy(opts, "force_zpl") {
        return is_graphic_path(path);
    }
    if !is_graphic_path(path) {
        return false;
    }
    // Explicit raw flag with a PDF/image file → convert
    if force_raw {
        return true;
    }
    match supports_raw(cups_name) {
        Ok(v) => v,
        Err(e) => {
            log::debug!(target: LOG, "queue_supports_raw failed for {cups_name}: {e}");
            // Heuristic: queue name looks like Zebra
            let low = cups_name.to_lowercase();
            ["zebra", "zpl", "zd2", "zd4"]
                .iter()
                .any(|t| low.contains(t))
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// 8×2 grayscale PNG with a black top-left pixel.
    pub(crate) fn tiny_png(dir: &Path) -> PathBuf {
        let mut img = GrayImage::from_pixel(8, 2, Luma([255]));
        img.put_pixel(0, 0, Luma([0]));
        let p = dir.join("dot.png");
        img.save(&p).unwrap();
        p
    }

    fn no_probe(_: &str) -> Result<bool, BoxError> {
        Err("no cups".into())
    }

    #[test]
    fn mono_hex_black_msb() {
        let mut img = GrayImage::from_pixel(8, 1, Luma([255]));
        img.put_pixel(0, 0, Luma([0]));
        let gfa = mono_image_to_gfa_hex(&img);
        assert_eq!((gfa.bytes_per_row, gfa.total_bytes, gfa.height), (1, 1, 1));
        assert_eq!(gfa.hex, "80");
    }

    #[test]
    fn build_label_contains_gfa() {
        let gfa = Gfa {
            hex: "80".into(),
            total_bytes: 1,
            bytes_per_row: 1,
            height: 1,
        };
        let zpl = build_zpl_label(&gfa, Some(20), 0, 0, 1);
        assert!(zpl.starts_with("^XA"));
        assert!(zpl.contains("^GFA,1,1,1,80"));
        assert!(zpl.contains("^PW8"));
        assert!(zpl.contains("^LL20"));
        assert!(!zpl.contains("^PQ"));
        assert!(zpl.trim_end().ends_with("^XZ"));
        assert!(build_zpl_label(&gfa, Some(1218), 0, 0, 1).contains("^LL1218"));
        // y offset extends the label length.
        assert!(build_zpl_label(&gfa, Some(100), 0, 32, 2).contains("^LL132"));
        assert!(build_zpl_label(&gfa, Some(100), 0, 32, 2).contains("^PQ2"));
    }

    #[test]
    fn fit_width_scales_up() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("tiny.png");
        GrayImage::from_pixel(16, 8, Luma([200])).save(&p).unwrap();
        let img = load_image_as_mono(&p, 64, None, DEFAULT_THRESHOLD, false, Fit::Width).unwrap();
        assert_eq!(img.width(), 64);
        assert_eq!(img.height(), 32);
    }

    #[test]
    fn crops_margins_and_pads_to_byte() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("m.png");
        let mut img = GrayImage::from_pixel(40, 40, Luma([255]));
        for x in 10..13 {
            img.put_pixel(x, 20, Luma([0]));
        }
        img.save(&p).unwrap();
        let mono = load_image_as_mono(&p, 812, None, DEFAULT_THRESHOLD, false, Fit::None).unwrap();
        assert_eq!(mono.dimensions(), (8, 1));
        assert_eq!(mono_image_to_gfa_hex(&mono).hex, "E0");
    }

    #[test]
    fn infer_4x6_zd220_and_zd421() {
        assert_eq!(
            infer_media_width_dots(Some("Zebra_ZD220-203dpi_ZPL"), DEFAULT_DPI),
            812
        );
        assert_eq!(
            infer_media_height_dots(Some("Zebra_ZD220-203dpi_ZPL"), DEFAULT_DPI),
            1218
        );
        assert_eq!(infer_media_width_dots(None, DEFAULT_DPI), 812);
        assert_eq!(
            infer_media_width_dots(Some("Zebra_ZD421-203dpi_ZPL"), DEFAULT_DPI),
            812
        );
        assert_eq!(
            infer_media_width_dots(Some("Zebra_ZD421-300dpi"), DEFAULT_DPI),
            1227
        );
        assert_eq!(
            infer_media_height_dots(Some("Zebra_ZD421-300dpi"), DEFAULT_DPI),
            1800
        );
    }

    #[test]
    fn png_to_zpl_file() {
        let td = tempfile::tempdir().unwrap();
        let png = tiny_png(td.path());
        let out = write_zpl_file(&png, td.path(), "job1", &JsonObject::new()).unwrap();
        let text = fs::read_to_string(out).unwrap();
        assert!(text.contains("^GFA,"));
        assert!(text.contains("^XA"));
        assert!(text.contains("^FO0,32"));
    }

    #[test]
    fn should_convert_cases() {
        let none = JsonObject::new();
        assert!(should_convert_to_zpl(
            Path::new("label.pdf"),
            "OfficeJet",
            &none,
            true,
            &no_probe
        ));

        let opt_out = json!({"no_zpl_convert": true});
        assert!(!should_convert_to_zpl(
            Path::new("label.pdf"),
            "Zebra_ZD220",
            opt_out.as_object().unwrap(),
            false,
            &no_probe
        ));

        // Probe failure → queue-name heuristic.
        assert!(should_convert_to_zpl(
            Path::new("x.png"),
            "Zebra_ZD220-203dpi_ZPL",
            &none,
            false,
            &no_probe
        ));
        assert!(!should_convert_to_zpl(
            Path::new("x.png"),
            "Brother",
            &none,
            false,
            &no_probe
        ));

        assert!(!should_convert_to_zpl(
            Path::new("label.zpl"),
            "Zebra_ZD220",
            &none,
            true,
            &no_probe
        ));

        let forced = json!({"zpl_convert": true});
        assert!(should_convert_to_zpl(
            Path::new("x.jpg"),
            "Brother",
            forced.as_object().unwrap(),
            false,
            &|_| Ok(false)
        ));
    }

    #[test]
    fn python_rounding() {
        assert_eq!(round_half_even(2.5), 2);
        assert_eq!(round_half_even(3.5), 4);
        assert_eq!(round_half_even(1227.0), 1227);
        assert_eq!(round_half_even(2.6), 3);
    }

    // --- PDF fixtures --------------------------------------------------------

    /// Minimal PDF: one `page_w`×`page_h` pt page per box, each page filled
    /// with its black `(x, y, w, h)` box (points, origin bottom-left).
    fn pdf_with_boxes(page_w: u32, page_h: u32, boxes: &[(u32, u32, u32, u32)]) -> Vec<u8> {
        let contents: Vec<String> = boxes
            .iter()
            .map(|(x, y, w, h)| format!("0 g {x} {y} {w} {h} re f"))
            .collect();
        let contents: Vec<&str> = contents.iter().map(String::as_str).collect();
        pdf_with_content(page_w, page_h, &contents)
    }

    /// Minimal PDF: one `page_w`×`page_h` pt page per content stream.
    fn pdf_with_content(page_w: u32, page_h: u32, contents: &[&str]) -> Vec<u8> {
        let n = contents.len();
        let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 3 + 2 * i)).collect();
        let mut objs = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            format!("<< /Type /Pages /Kids [{}] /Count {n} >>", kids.join(" ")),
        ];
        for (i, content) in contents.iter().enumerate() {
            objs.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {page_w} {page_h}] /Contents {} 0 R >>",
                4 + 2 * i
            ));
            objs.push(format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            ));
        }
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in objs.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let xref = pdf.len();
        let size = objs.len() + 1;
        pdf.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
        for off in offsets {
            pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n")
                .as_bytes(),
        );
        pdf
    }

    fn write_pdf(dir: &Path, name: &str, pdf: &[u8]) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, pdf).unwrap();
        p
    }

    /// Every rasterizer installed here (CI may have neither).
    fn renderers() -> Vec<Renderer> {
        let found: Vec<Renderer> = [
            which("pdftoppm").map(Renderer::Pdftoppm),
            which("gs").map(Renderer::Ghostscript),
        ]
        .into_iter()
        .flatten()
        .collect();
        if found.is_empty() {
            eprintln!("no pdftoppm or gs installed; skipping PDF rendering checks");
        }
        found
    }

    /// The `^PW…` value of each label in a ZPL stream.
    fn label_widths(zpl: &str) -> Vec<i64> {
        zpl.lines()
            .filter_map(|l| l.strip_prefix("^PW"))
            .map(|w| w.parse().unwrap())
            .collect()
    }

    fn opts(v: Value) -> JsonObject {
        v.as_object().unwrap().clone()
    }

    // --- C26: every PDF page becomes a label ---------------------------------

    #[test]
    fn multi_page_pdf_prints_every_page() {
        if renderers().is_empty() {
            return;
        }
        let td = tempfile::tempdir().unwrap();
        // 4×6 in pages: a 1 in wide box, then a 2 in wide box.
        let pdf = write_pdf(
            td.path(),
            "two.pdf",
            &pdf_with_boxes(288, 432, &[(36, 36, 72, 36), (36, 36, 144, 72)]),
        );
        let all = image_path_to_zpl(&pdf, &JsonObject::new()).unwrap();
        assert_eq!(all.matches("^XA").count(), 2, "{all:.200}");
        assert_eq!(all.matches("^XZ").count(), 2);
        let widths = label_widths(&all);
        assert_eq!(widths.len(), 2);
        assert!(widths[0] < widths[1], "pages out of order: {widths:?}");

        // zpl_page still selects one page, identical to that page's label.
        let one = image_path_to_zpl(&pdf, &opts(json!({"zpl_page": 1}))).unwrap();
        let two = image_path_to_zpl(&pdf, &opts(json!({"zpl_page": "2"}))).unwrap();
        assert_eq!(one.matches("^XA").count(), 1);
        assert_eq!(format!("{one}{two}"), all);
    }

    #[test]
    fn pdf_pages_come_back_in_page_order() {
        // 12 pages: pdftoppm writes page-01…page-12, gs page-1…page-12 (a
        // name sort would put page-10 before page-2).
        let boxes: Vec<_> = (1..=12).map(|i| (4, 4, 10 * i, 20)).collect();
        let td = tempfile::tempdir().unwrap();
        let pdf = write_pdf(td.path(), "twelve.pdf", &pdf_with_boxes(144, 36, &boxes));
        for renderer in renderers() {
            let out = td
                .path()
                .join(format!("{renderer:?}").replace(['"', '/', ' '], "_"));
            let pngs = pdf_all_pages_to_png(&renderer, &pdf, &out, 72, None).unwrap();
            let widths: Vec<u32> = pngs
                .iter()
                .map(|p| {
                    let (x0, _, x1, _) = content_bbox(&open_gray(p).unwrap()).unwrap();
                    x1 - x0
                })
                .collect();
            assert_eq!(widths.len(), 12, "{renderer:?}");
            assert!(
                widths.windows(2).all(|w| w[0] < w[1]),
                "{renderer:?} page order: {widths:?}"
            );
        }
    }

    #[test]
    fn over_long_pdf_fails_instead_of_dropping_pages() {
        let td = tempfile::tempdir().unwrap();
        // A known page count fails before anything is rendered.
        let r = Renderer::Pdftoppm("/nonexistent/pdftoppm".into());
        let err =
            pdf_all_pages_to_png(&r, Path::new("x.pdf"), td.path(), 72, Some(51)).unwrap_err();
        assert_eq!(err.code, "pdf_too_many_pages");
        assert!(err.message.contains("51 pages"), "{}", err.message);

        // Unknown count (no pdfinfo): one page past the limit is rendered.
        let boxes = vec![(2, 2, 8, 8); MAX_PDF_PAGES as usize + 1];
        let pdf = write_pdf(td.path(), "long.pdf", &pdf_with_boxes(18, 18, &boxes));
        for renderer in renderers() {
            let out = td
                .path()
                .join(format!("{renderer:?}").replace(['"', '/', ' '], "_"));
            let err = pdf_all_pages_to_png(&renderer, &pdf, &out, 72, None).unwrap_err();
            assert_eq!(err.code, "pdf_too_many_pages", "{renderer:?}");
            if let Some(n) = pdf_page_count(&pdf) {
                assert_eq!(n, MAX_PDF_PAGES + 1);
            }
        }
    }

    // --- C08: render at the head's resolution --------------------------------

    #[test]
    fn effective_dpi_prefers_option_then_queue_name() {
        let none = JsonObject::new();
        assert_eq!(effective_dpi(&none, "Zebra_ZD421-300dpi_ZPL"), 300);
        assert_eq!(effective_dpi(&none, "Zebra_ZD220-203dpi_ZPL"), 203);
        assert_eq!(effective_dpi(&none, "Zebra_ZT411-600dpi_ZPL"), 600);
        assert_eq!(effective_dpi(&none, "Brother"), DEFAULT_DPI);
        let explicit = opts(json!({"zpl_dpi": "300"}));
        assert_eq!(effective_dpi(&explicit, "Zebra_ZD220"), 300);
        assert_eq!(
            effective_dpi(&opts(json!({"zpl_dpi": 203})), "Z-300dpi"),
            203
        );
        // Unusable values fall back to the name; absurd ones are clamped.
        assert_eq!(
            effective_dpi(&opts(json!({"zpl_dpi": null})), "Z-300dpi"),
            300
        );
        assert_eq!(effective_dpi(&opts(json!({"zpl_dpi": 0})), "Z-300dpi"), 300);
        assert_eq!(effective_dpi(&opts(json!({"zpl_dpi": "x"})), "Z"), 203);
        assert_eq!(effective_dpi(&opts(json!({"zpl_dpi": 5000})), "Z"), MAX_DPI);
    }

    #[test]
    fn unknown_models_keep_four_inches_at_higher_dpi() {
        assert_eq!(
            infer_media_width_dots(Some("Zebra_ZD420-300dpi_ZPL"), 203),
            1200
        );
        assert_eq!(infer_media_width_dots(Some("Zebra_ZT230"), 300), 1200);
        assert_eq!(infer_media_width_dots(Some("Zebra_ZT230"), 203), 812);
        assert_eq!(
            infer_media_width_dots(Some("Zebra_ZT411-600dpi_ZPL"), 203),
            2454
        );
        assert_eq!(
            infer_media_height_dots(Some("Zebra_ZT411-600dpi_ZPL"), 203),
            3600
        );
    }

    #[test]
    fn pdf_on_300dpi_queue_prints_full_size() {
        if renderers().is_empty() {
            return;
        }
        let td = tempfile::tempdir().unwrap();
        // A 3.8 × 5.8 in design on a 4 × 6 in page.
        let pdf = write_pdf(
            td.path(),
            "4x6.pdf",
            &pdf_with_boxes(288, 432, &[(7, 7, 274, 418)]),
        );
        let width = |o: Value| label_widths(&image_path_to_zpl(&pdf, &opts(o)).unwrap())[0];
        let w203 = width(json!({"cups_name": "Zebra_ZD421-203dpi_ZPL"}));
        assert!((760..=780).contains(&w203), "{w203}");
        // Same physical size on a 300 dpi head: ~1.48× the dots (was 760).
        for queue in ["Zebra_ZD421-300dpi_ZPL", "Zebra_ZD420-300dpi_ZPL"] {
            let w300 = width(json!({ "cups_name": queue }));
            assert!((1130..=1160).contains(&w300), "{queue}: {w300}");
        }
        // An explicit zpl_dpi sets the raster resolution, and the label box
        // too when the queue name has no dpi token…
        let tokenless = width(json!({"cups_name": "Zebra_ZT230", "zpl_dpi": 300}));
        assert!((1130..=1160).contains(&tokenless), "{tokenless}");
        // …but the name's token is the head's real resolution: a 300 dpi
        // raster is scaled into the 203 dpi box rather than printed at 148%.
        let named = width(json!({"cups_name": "Zebra_ZD220-203dpi_ZPL", "zpl_dpi": 300}));
        assert!(named <= 816, "{named}");
    }

    // --- C27: transparency is paper ------------------------------------------

    /// 40×30 image, fully transparent *black* except an opaque black 10×5
    /// box at (5, 5): a canvas export.
    fn transparent_black_canvas() -> image::RgbaImage {
        let mut img = image::RgbaImage::from_pixel(40, 30, image::Rgba([0, 0, 0, 0]));
        for x in 5..15 {
            for y in 5..10 {
                img.put_pixel(x, y, image::Rgba([0, 0, 0, 255]));
            }
        }
        img
    }

    /// Palette PNG (bit depth 8) with a `tRNS` chunk, built by hand: the
    /// image encoder cannot write palette images.
    fn palette_png(w: u32, h: u32, palette: &[[u8; 3]], trns: &[u8], idx: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        let chunk = |out: &mut Vec<u8>, ty: &[u8], data: &[u8]| {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            let mut crc = flate2::Crc::new();
            crc.update(ty);
            crc.update(data);
            out.extend_from_slice(ty);
            out.extend_from_slice(data);
            out.extend_from_slice(&crc.sum().to_be_bytes());
        };
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 3, 0, 0, 0]);
        let mut raw = Vec::new();
        for row in idx.chunks(w as usize) {
            raw.push(0); // filter: none
            raw.extend_from_slice(row);
        }
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(&raw).unwrap();
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        chunk(&mut png, b"IHDR", &ihdr);
        chunk(&mut png, b"PLTE", palette.concat().as_slice());
        chunk(&mut png, b"tRNS", trns);
        chunk(&mut png, b"IDAT", &z.finish().unwrap());
        chunk(&mut png, b"IEND", &[]);
        png
    }

    #[test]
    fn transparent_background_prints_as_paper() {
        let td = tempfile::tempdir().unwrap();
        let mono = |p: &Path| load_image_as_mono(p, 812, None, 128, false, Fit::None).unwrap();
        let only_box = |img: &GrayImage| {
            // 10×5 box, padded to 16 dots: 10 black then 6 white per row.
            assert_eq!(img.dimensions(), (16, 5));
            assert!(img
                .enumerate_pixels()
                .all(|(x, _, p)| (p.0[0] == 0) == (x < 10)));
        };

        let rgba = td.path().join("canvas.png");
        transparent_black_canvas().save(&rgba).unwrap();
        only_box(&mono(&rgba));

        let la = td.path().join("la.png");
        image::DynamicImage::ImageRgba8(transparent_black_canvas())
            .to_luma_alpha8()
            .save(&la)
            .unwrap();
        only_box(&mono(&la));

        // Palette PNG whose transparent index 0 is black; index 1 is opaque black.
        let idx: Vec<u8> = (0..30 * 40)
            .map(|i| u8::from((5..15).contains(&(i % 40)) && (5..10).contains(&(i / 40))))
            .collect();
        let pal = td.path().join("palette.png");
        fs::write(
            &pal,
            palette_png(40, 30, &[[0, 0, 0], [0, 0, 0]], &[0], &idx),
        )
        .unwrap();
        only_box(&mono(&pal));

        // Opaque images are unchanged: white stays paper, black stays ink.
        let mut opaque = GrayImage::from_pixel(40, 30, Luma([255]));
        opaque.put_pixel(3, 3, Luma([0]));
        let op = td.path().join("opaque.png");
        opaque.save(&op).unwrap();
        assert_eq!(mono(&op).dimensions(), (8, 1));
    }

    // --- C25: decoders Pillow had --------------------------------------------

    /// White 60×40 label with a black 20×10 box at (10, 10).
    fn boxed_label() -> image::DynamicImage {
        let mut img = image::RgbImage::from_pixel(60, 40, image::Rgb([255, 255, 255]));
        for x in 10..30 {
            for y in 10..20 {
                img.put_pixel(x, y, image::Rgb([0, 0, 0]));
            }
        }
        image::DynamicImage::ImageRgb8(img)
    }

    fn encoded(img: &image::DynamicImage, format: ImageFormat) -> Vec<u8> {
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), format).unwrap();
        buf
    }

    /// 64×40 1-bit TIFF, CCITT Group 4 (USPS-style), black box (8,10)–(40,30);
    /// written by Pillow `save(format="TIFF", compression="group4")`.
    const GROUP4_TIFF_B64: &str = "SUkqACAAAAAmoHhv///Io3////////////+P//8AEAEJAAABAwABAAAAQAAAAAEBAwABAAAAKAAAAAIBAwABAAAAAQAAAAMBAwABAAAABAAAAAYBAwABAAAAAQAAABEBBAABAAAACAAAABYBAwABAAAAKAAAABcBBAABAAAAGAAAABwBAwABAAAAAQAAAAAAAAA=";

    #[test]
    fn tiff_and_webp_bytes_named_png_convert() {
        use base64::Engine as _;
        let td = tempfile::tempdir().unwrap();
        let label = boxed_label();
        let real_png = td.path().join("real.png");
        fs::write(&real_png, encoded(&label, ImageFormat::Png)).unwrap();
        let expected = image_path_to_zpl(&real_png, &JsonObject::new()).unwrap();

        for format in [ImageFormat::Tiff, ImageFormat::WebP] {
            // sniff_suffix only knows PDF/PNG/JPEG, so these arrive as .png.
            let p = td.path().join(format!("{format:?}.png"));
            fs::write(&p, encoded(&label, format)).unwrap();
            let zpl = image_path_to_zpl(&p, &JsonObject::new())
                .unwrap_or_else(|e| panic!("{format:?}: {e}"));
            assert_eq!(zpl, expected, "{format:?}");
        }

        // Saved under their own names (a sniffer that knows them), they are
        // still graphics to convert, not raw bytes for the printer.
        assert!(is_graphic_path(Path::new("label.TIFF")));
        assert!(is_graphic_path(Path::new("label.webp")));

        let g4 = td.path().join("usps.png");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(GROUP4_TIFF_B64)
            .unwrap();
        fs::write(&g4, bytes).unwrap();
        let mono = load_image_as_mono(&g4, 812, None, 128, false, Fit::None).unwrap();
        assert_eq!(mono.dimensions(), (32, 20));
        assert!(mono.pixels().all(|p| p.0[0] == 0));
    }

    #[test]
    fn damaged_png_prints_like_pillow() {
        let td = tempfile::tempdir().unwrap();
        let png = encoded(&boxed_label(), ImageFormat::Png);
        assert_eq!(repair_png(&png), None, "intact PNG needs no repair");
        assert_eq!(repair_png(b"GIF89a"), None);
        let mono = |bytes: &[u8], name: &str| {
            let p = td.path().join(name);
            fs::write(&p, bytes).unwrap();
            load_image_as_mono(&p, 812, None, 128, false, Fit::None)
        };
        let expected = mono(&png, "ok.png").unwrap();

        // IEND cut off (a truncated download that still has every row).
        let no_iend = &png[..png.len() - 12];
        assert_eq!(mono(no_iend, "no_iend.png").unwrap(), expected);

        // A corrupt IDAT CRC.
        let at = png.windows(4).position(|w| w == b"IDAT").unwrap();
        let len = u32::from_be_bytes(png[at - 4..at].try_into().unwrap()) as usize;
        let mut bad_crc = png.clone();
        bad_crc[at + 4 + len] ^= 0xFF;
        assert_eq!(mono(&bad_crc, "bad_crc.png").unwrap(), expected);

        // Missing rows still fail cleanly.
        let err = mono(&png[..at + 10], "cut.png").unwrap_err();
        assert_eq!(err.code, "image_bad");
    }

    // --- Golden output: labels stay byte-identical ---------------------------

    /// The repository's real 4×6 test label (`assets/test-labels/`).
    fn test_label(ext: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../assets/test-labels")
            .join(format!("vesyl-roadrunner-4x6.{ext}"))
    }

    /// Deterministic pseudo-random bytes (an LCG): photo-like noise.
    fn noise(n: usize, seed: u32) -> Vec<u8> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                (x >> 16) as u8
            })
            .collect()
    }

    /// 120×80 RGBA canvas on fully transparent black: an opaque black box,
    /// a box fading out in alpha, and gray ink at half alpha.
    fn alpha_canvas() -> image::RgbaImage {
        image::RgbaImage::from_fn(120, 80, |x, y| match (x, y) {
            (10..40, 10..30) => image::Rgba([0, 0, 0, 255]),
            (50..110, 10..30) => image::Rgba([0, 0, 0, (255 - (x - 50) * 4) as u8]),
            (10..110, 40..70) => image::Rgba([90, 60, 30, 128]),
            _ => image::Rgba([0, 0, 0, 0]),
        })
    }

    /// Every raster fixture: the real label in each pixel format and codec
    /// the conversion decodes differently, plus small synthetic images.
    fn raster_fixtures(dir: &Path) -> Vec<(&'static str, PathBuf)> {
        use base64::Engine as _;
        use image::codecs::jpeg::JpegEncoder;
        let label = image::open(test_label("png")).unwrap();
        let jpeg = |img: &image::DynamicImage| {
            let mut buf = Vec::new();
            img.write_with_encoder(JpegEncoder::new_with_quality(&mut buf, 85))
                .unwrap();
            buf
        };
        let canvas = image::DynamicImage::ImageRgba8(alpha_canvas());
        let gray16 = image::ImageBuffer::<Luma<u16>, Vec<u16>>::from_fn(64, 48, |x, y| {
            Luma([((x * 1024 + y * 700) % 65536) as u16])
        });
        let idx: Vec<u8> = (0..30 * 40)
            .map(|i| match (i % 40, i / 40) {
                (5..15, 5..10) => 1,
                (20..35, 5..25) => 2,
                _ => 0,
            })
            .collect();
        let rgb_noise =
            image::RgbImage::from_raw(300, 200, noise(300 * 200 * 3, 7)).expect("noise buffer");
        let files: Vec<(&'static str, Vec<u8>)> = vec![
            ("label.png", fs::read(test_label("png")).unwrap()),
            (
                "label-rgb.png",
                encoded(
                    &image::DynamicImage::ImageRgb8(label.to_rgb8()),
                    ImageFormat::Png,
                ),
            ),
            (
                "label.jpg",
                jpeg(&image::DynamicImage::ImageRgb8(label.to_rgb8())),
            ),
            ("label-gray.jpg", jpeg(&label)),
            ("canvas.png", encoded(&canvas, ImageFormat::Png)),
            ("canvas.gif", encoded(&canvas, ImageFormat::Gif)),
            (
                "canvas-la.png",
                encoded(
                    &image::DynamicImage::ImageLumaA8(canvas.to_luma_alpha8()),
                    ImageFormat::Png,
                ),
            ),
            (
                "palette.png",
                palette_png(40, 30, &[[0, 0, 0], [0, 0, 0], [120, 120, 120]], &[0], &idx),
            ),
            (
                "gray16.png",
                encoded(&image::DynamicImage::ImageLuma16(gray16), ImageFormat::Png),
            ),
            (
                "usps.tif",
                base64::engine::general_purpose::STANDARD
                    .decode(GROUP4_TIFF_B64)
                    .unwrap(),
            ),
            ("boxed.webp", encoded(&boxed_label(), ImageFormat::WebP)),
            ("boxed.bmp", encoded(&boxed_label(), ImageFormat::Bmp)),
            (
                "noise.png",
                encoded(&image::DynamicImage::ImageRgb8(rgb_noise), ImageFormat::Png),
            ),
        ];
        files
            .into_iter()
            .map(|(name, bytes)| {
                let p = dir.join(name);
                fs::write(&p, bytes).unwrap();
                (name, p)
            })
            .collect()
    }

    /// Options every fixture is converted with: queue names without and with
    /// a dpi token, each fit, label boxes (both spellings), offsets, and
    /// threshold/invert.
    const GOLDEN_OPTIONS: &[&str] = &[
        r#"{}"#,
        r#"{"cups_name":"Zebra_Raw"}"#,
        r#"{"cups_name":"Zebra_ZD421-300dpi_ZPL"}"#,
        r#"{"zpl_fit":"width"}"#,
        r#"{"zpl_fit":"none"}"#,
        r#"{"zpl_fit":"height"}"#,
        r#"{"zpl_max_width_dots":400,"zpl_max_height_dots":600}"#,
        r#"{"label_width_dots":200,"label_height_dots":300,"zpl_fit":"width"}"#,
        r#"{"zpl_x":0,"zpl_y":0}"#,
        r#"{"zpl_x":-5,"zpl_y":-10}"#,
        r#"{"zpl_y":10}"#,
        r#"{"zpl_threshold":200,"zpl_invert":true}"#,
    ];

    /// Extra options for PDFs: rendering resolution and page selection.
    const GOLDEN_PDF_OPTIONS: &[&str] = &[
        r#"{"cups_name":"Zebra_ZD421-203dpi_ZPL"}"#,
        r#"{"zpl_dpi":300}"#,
        r#"{"zpl_page":1}"#,
        r#"{"zpl_page":"2"}"#,
    ];

    /// Every fixture with every option set.
    fn golden_cases<'a>(
        fixtures: &'a [(&'a str, PathBuf)],
        options: &'a [&'a str],
    ) -> impl Iterator<Item = (&'a str, &'a Path, &'a str)> {
        fixtures
            .iter()
            .flat_map(move |(name, path)| options.iter().map(move |o| (*name, path.as_path(), *o)))
    }

    /// `fixture options digest` for each conversion: the first 16 hex digits
    /// of the SHA-256 of the ZPL, or `err:<code>`.
    fn golden_rows(cases: &[(&str, &Path, &str)]) -> Vec<String> {
        use sha2::{Digest as _, Sha256};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let row = |(name, path, o): (&str, &Path, &str)| {
            let v: Value = serde_json::from_str(o).unwrap();
            let got = match image_path_to_zpl(path, v.as_object().unwrap()) {
                Ok(zpl) => Sha256::digest(zpl.as_bytes())[..8]
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect(),
                Err(e) => format!("err:{}", e.code),
            };
            format!("{name} {o} {got}")
        };
        // A few workers share the cases: the debug build resamples slowly.
        let next = AtomicUsize::new(0);
        let workers = std::thread::available_parallelism().map_or(2, |n| n.get().min(6));
        let mut rows: Vec<(usize, String)> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..workers)
                .map(|_| {
                    s.spawn(|| {
                        let mut done = Vec::new();
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            let Some(case) = cases.get(i) else {
                                return done;
                            };
                            done.push((i, row(*case)));
                        }
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap())
                .collect()
        });
        rows.sort();
        rows.into_iter().map(|(_, row)| row).collect()
    }

    fn assert_golden(expected: &str, actual: &[String]) {
        let want: Vec<&str> = expected
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        let changed: Vec<String> = actual
            .iter()
            .zip(want.iter().copied().chain(std::iter::repeat("<missing>")))
            .filter(|(got, want)| got.as_str() != *want)
            .map(|(got, want)| format!("  want {want}\n   got {got}"))
            .collect();
        assert!(
            changed.is_empty() && want.len() == actual.len(),
            "ZPL output changed:\n{}\nfull table:\n{}",
            changed.join("\n"),
            actual.join("\n")
        );
    }

    /// Recorded from the conversion as it was before its size limits: a
    /// normal label must never change. After a deliberate output change, the
    /// failure message prints the new table.
    const RASTER_GOLDEN: &str = r#"
label.png {} 6144278d5f7eaae6
label.png {"cups_name":"Zebra_Raw"} 6144278d5f7eaae6
label.png {"cups_name":"Zebra_ZD421-300dpi_ZPL"} 6144278d5f7eaae6
label.png {"zpl_fit":"width"} 77a7db60c7a396b8
label.png {"zpl_fit":"none"} 6144278d5f7eaae6
label.png {"zpl_fit":"height"} 6144278d5f7eaae6
label.png {"zpl_max_width_dots":400,"zpl_max_height_dots":600} abd6b15f82656e2f
label.png {"label_width_dots":200,"label_height_dots":300,"zpl_fit":"width"} ea2ee3aec94647db
label.png {"zpl_x":0,"zpl_y":0} bf6d1dbf18e61349
label.png {"zpl_x":-5,"zpl_y":-10} dc6a38955c86d326
label.png {"zpl_y":10} f42edcaffacb7666
label.png {"zpl_threshold":200,"zpl_invert":true} fe5b7f273a8d8daf
label-rgb.png {} 6144278d5f7eaae6
label-rgb.png {"cups_name":"Zebra_Raw"} 6144278d5f7eaae6
label-rgb.png {"cups_name":"Zebra_ZD421-300dpi_ZPL"} 6144278d5f7eaae6
label-rgb.png {"zpl_fit":"width"} 77a7db60c7a396b8
label-rgb.png {"zpl_fit":"none"} 6144278d5f7eaae6
label-rgb.png {"zpl_fit":"height"} 6144278d5f7eaae6
label-rgb.png {"zpl_max_width_dots":400,"zpl_max_height_dots":600} abd6b15f82656e2f
label-rgb.png {"label_width_dots":200,"label_height_dots":300,"zpl_fit":"width"} ea2ee3aec94647db
label-rgb.png {"zpl_x":0,"zpl_y":0} bf6d1dbf18e61349
label-rgb.png {"zpl_x":-5,"zpl_y":-10} dc6a38955c86d326
label-rgb.png {"zpl_y":10} f42edcaffacb7666
label-rgb.png {"zpl_threshold":200,"zpl_invert":true} fe5b7f273a8d8daf
label.jpg {} ae3c5faabac29458
label.jpg {"cups_name":"Zebra_Raw"} ae3c5faabac29458
label.jpg {"cups_name":"Zebra_ZD421-300dpi_ZPL"} ae3c5faabac29458
label.jpg {"zpl_fit":"width"} 70a776165ade4dfe
label.jpg {"zpl_fit":"none"} ae3c5faabac29458
label.jpg {"zpl_fit":"height"} ae3c5faabac29458
label.jpg {"zpl_max_width_dots":400,"zpl_max_height_dots":600} 217918e8320d7294
label.jpg {"label_width_dots":200,"label_height_dots":300,"zpl_fit":"width"} c965339eff924c26
label.jpg {"zpl_x":0,"zpl_y":0} a90379b7b95b6493
label.jpg {"zpl_x":-5,"zpl_y":-10} bce3e984bdcdac57
label.jpg {"zpl_y":10} b6d8565773141fe1
label.jpg {"zpl_threshold":200,"zpl_invert":true} 4a4a999664d69c3d
label-gray.jpg {} ae3c5faabac29458
label-gray.jpg {"cups_name":"Zebra_Raw"} ae3c5faabac29458
label-gray.jpg {"cups_name":"Zebra_ZD421-300dpi_ZPL"} ae3c5faabac29458
label-gray.jpg {"zpl_fit":"width"} 70a776165ade4dfe
label-gray.jpg {"zpl_fit":"none"} ae3c5faabac29458
label-gray.jpg {"zpl_fit":"height"} ae3c5faabac29458
label-gray.jpg {"zpl_max_width_dots":400,"zpl_max_height_dots":600} 217918e8320d7294
label-gray.jpg {"label_width_dots":200,"label_height_dots":300,"zpl_fit":"width"} c965339eff924c26
label-gray.jpg {"zpl_x":0,"zpl_y":0} a90379b7b95b6493
label-gray.jpg {"zpl_x":-5,"zpl_y":-10} bce3e984bdcdac57
label-gray.jpg {"zpl_y":10} b6d8565773141fe1
label-gray.jpg {"zpl_threshold":200,"zpl_invert":true} 4a4a999664d69c3d
canvas.png {} 2c2961c5636ae306
canvas.png {"cups_name":"Zebra_Raw"} 2c2961c5636ae306
canvas.png {"cups_name":"Zebra_ZD421-300dpi_ZPL"} 2c2961c5636ae306
canvas.png {"zpl_fit":"width"} a90387a70c8788c5
canvas.png {"zpl_fit":"none"} 2c2961c5636ae306
canvas.png {"zpl_fit":"height"} 2c2961c5636ae306
canvas.png {"zpl_max_width_dots":400,"zpl_max_height_dots":600} 2c2961c5636ae306
canvas.png {"label_width_dots":200,"label_height_dots":300,"zpl_fit":"width"} da0cd4429d78c684
canvas.png {"zpl_x":0,"zpl_y":0} 6a8866443aaab52f
canvas.png {"zpl_x":-5,"zpl_y":-10} 411e085856fd1d5d
canvas.png {"zpl_y":10} 03b2fe7ed8b70bfb
canvas.png {"zpl_threshold":200,"zpl_invert":true} 2968c1e8dc83e67c
canvas.gif {} 34da79daaf1623ca
canvas.gif {"cups_name":"Zebra_Raw"} 34da79daaf1623ca
canvas.gif {"cups_name":"Zebra_ZD421-300dpi_ZPL"} 34da79daaf1623ca
canvas.gif {"zpl_fit":"width"} 76cce02b7b5afd97
canvas.gif {"zpl_fit":"none"} 34da79daaf1623ca
canvas.gif {"zpl_fit":"height"} 34da79daaf1623ca
canvas.gif {"zpl_max_width_dots":400,"zpl_max_height_dots":600} 34da79daaf1623ca
canvas.gif {"label_width_dots":200,"label_height_dots":300,"zpl_fit":"width"} 4704fcc437a6b713
canvas.gif {"zpl_x":0,"zpl_y":0} a9cbeca7a746517c
canvas.gif {"zpl_x":-5,"zpl_y":-10} 74c22a91d087a907
canvas.gif {"zpl_y":10} e4871908e0f5a6a4
canvas.gif {"zpl_threshold":200,"zpl_invert":true} 255f47f89b53684f
canvas-la.png {} 2c2961c5636ae306
canvas-la.png {"cups_name":"Zebra_Raw"} 2c2961c5636ae306
canvas-la.png {"cups_name":"Zebra_ZD421-300dpi_ZPL"} 2c2961c5636ae306
canvas-la.png {"zpl_fit":"width"} a90387a70c8788c5
canvas-la.png {"zpl_fit":"none"} 2c2961c5636ae306
canvas-la.png {"zpl_fit":"height"} 2c2961c5636ae306
canvas-la.png {"zpl_max_width_dots":400,"zpl_max_height_dots":600} 2c2961c5636ae306
canvas-la.png {"label_width_dots":200,"label_height_dots":300,"zpl_fit":"width"} da0cd4429d78c684
canvas-la.png {"zpl_x":0,"zpl_y":0} 6a8866443aaab52f
canvas-la.png {"zpl_x":-5,"zpl_y":-10} 411e085856fd1d5d
canvas-la.png {"zpl_y":10} 03b2fe7ed8b70bfb
canvas-la.png {"zpl_threshold":200,"zpl_invert":true} 2968c1e8dc83e67c
palette.png {} 1b3a720a224aee5b
palette.png {"cups_name":"Zebra_Raw"} 1b3a720a224aee5b
palette.png {"cups_name":"Zebra_ZD421-300dpi_ZPL"} 1b3a720a224aee5b
palette.png {"zpl_fit":"width"} 95f18f507a506815
palette.png {"zpl_fit":"none"} 1b3a720a224aee5b
palette.png {"zpl_fit":"height"} 1b3a720a224aee5b
palette.png {"zpl_max_width_dots":400,"zpl_max_height_dots":600} 1b3a720a224aee5b
palette.png {"label_width_dots":200,"label_height_dots":300,"zpl_fit":"width"} 39f9d07fa124c22a
palette.png {"zpl_x":0,"zpl_y":0} 0b6f4bd679644587
palette.png {"zpl_x":-5,"zpl_y":-10} 1fd022051d1d3cd0
palette.png {"zpl_y":10} 82c69bae3a2c20ff
palette.png {"zpl_threshold":200,"zpl_invert":true} 1a7a9aa8f8872580
gray16.png {} d25696e31d8c9669
gray16.png {"cups_name":"Zebra_Raw"} d25696e31d8c9669
gray16.png {"cups_name":"Zebra_ZD421-300dpi_ZPL"} d25696e31d8c9669
gray16.png {"zpl_fit":"width"} d0b1743a0d693193
gray16.png {"zpl_fit":"none"} d25696e31d8c9669
gray16.png {"zpl_fit":"height"} d25696e31d8c9669
gray16.png {"zpl_max_width_dots":400,"zpl_max_height_dots":600} d25696e31d8c9669
gray16.png {"label_width_dots":200,"label_height_dots":300,"zpl_fit":"width"} c2ccee69d0a3648e
gray16.png {"zpl_x":0,"zpl_y":0} fa6aa602df0e3f0c
gray16.png {"zpl_x":-5,"zpl_y":-10} bcfc66edd9a32e63
gray16.png {"zpl_y":10} 0b01c2b37b01e4fa
gray16.png {"zpl_threshold":200,"zpl_invert":true} aa1f59b6d89a36e6
usps.tif {} 350fcf193ab7c414
usps.tif {"cups_name":"Zebra_Raw"} 350fcf193ab7c414
usps.tif {"cups_name":"Zebra_ZD421-300dpi_ZPL"} 350fcf193ab7c414
usps.tif {"zpl_fit":"width"} c01850051ffab54c
usps.tif {"zpl_fit":"none"} 350fcf193ab7c414
usps.tif {"zpl_fit":"height"} 350fcf193ab7c414
usps.tif {"zpl_max_width_dots":400,"zpl_max_height_dots":600} 350fcf193ab7c414
usps.tif {"label_width_dots":200,"label_height_dots":300,"zpl_fit":"width"} 2a643502ed476343
usps.tif {"zpl_x":0,"zpl_y":0} 2e11961cb1699f2a
usps.tif {"zpl_x":-5,"zpl_y":-10} fc05fed08e8c22b1
usps.tif {"zpl_y":10} f4e549fb551ff453
usps.tif {"zpl_threshold":200,"zpl_invert":true} 8065aa50d0384d60
boxed.webp {} 9f024988f5a4fafd
boxed.webp {"cups_name":"Zebra_Raw"} 9f024988f5a4fafd
boxed.webp {"cups_name":"Zebra_ZD421-300dpi_ZPL"} 9f024988f5a4fafd
boxed.webp {"zpl_fit":"width"} 6eabe9b93cafaf02
boxed.webp {"zpl_fit":"none"} 9f024988f5a4fafd
boxed.webp {"zpl_fit":"height"} 9f024988f5a4fafd
boxed.webp {"zpl_max_width_dots":400,"zpl_max_height_dots":600} 9f024988f5a4fafd
boxed.webp {"label_width_dots":200,"label_height_dots":300,"zpl_fit":"width"} d2a3e150a012611e
boxed.webp {"zpl_x":0,"zpl_y":0} 6b53dcaf9ff5106a
boxed.webp {"zpl_x":-5,"zpl_y":-10} 2fc90aff4ce40963
boxed.webp {"zpl_y":10} fe8e46842b985173
boxed.webp {"zpl_threshold":200,"zpl_invert":true} aceff9d7a8692263
boxed.bmp {} 9f024988f5a4fafd
boxed.bmp {"cups_name":"Zebra_Raw"} 9f024988f5a4fafd
boxed.bmp {"cups_name":"Zebra_ZD421-300dpi_ZPL"} 9f024988f5a4fafd
boxed.bmp {"zpl_fit":"width"} 6eabe9b93cafaf02
boxed.bmp {"zpl_fit":"none"} 9f024988f5a4fafd
boxed.bmp {"zpl_fit":"height"} 9f024988f5a4fafd
boxed.bmp {"zpl_max_width_dots":400,"zpl_max_height_dots":600} 9f024988f5a4fafd
boxed.bmp {"label_width_dots":200,"label_height_dots":300,"zpl_fit":"width"} d2a3e150a012611e
boxed.bmp {"zpl_x":0,"zpl_y":0} 6b53dcaf9ff5106a
boxed.bmp {"zpl_x":-5,"zpl_y":-10} 2fc90aff4ce40963
boxed.bmp {"zpl_y":10} fe8e46842b985173
boxed.bmp {"zpl_threshold":200,"zpl_invert":true} aceff9d7a8692263
noise.png {} ad644ac40398808c
noise.png {"cups_name":"Zebra_Raw"} ad644ac40398808c
noise.png {"cups_name":"Zebra_ZD421-300dpi_ZPL"} ad644ac40398808c
noise.png {"zpl_fit":"width"} fd94e2c664b9f0ae
noise.png {"zpl_fit":"none"} ad644ac40398808c
noise.png {"zpl_fit":"height"} ad644ac40398808c
noise.png {"zpl_max_width_dots":400,"zpl_max_height_dots":600} ad644ac40398808c
noise.png {"label_width_dots":200,"label_height_dots":300,"zpl_fit":"width"} 38db9d65314616ba
noise.png {"zpl_x":0,"zpl_y":0} fcb8f039fbcf78c2
noise.png {"zpl_x":-5,"zpl_y":-10} 619b230715d56d76
noise.png {"zpl_y":10} f095df32903d6911
noise.png {"zpl_threshold":200,"zpl_invert":true} afb8acc9cf545f0c
"#;

    #[test]
    fn raster_labels_are_byte_identical() {
        let td = tempfile::tempdir().unwrap();
        let fixtures = raster_fixtures(td.path());
        let cases: Vec<_> = golden_cases(&fixtures, GOLDEN_OPTIONS).collect();
        assert_golden(RASTER_GOLDEN, &golden_rows(&cases));
    }

    /// The renderer the PDF goldens were recorded with: another poppler
    /// release may anti-alias differently, so those rows are skipped there.
    const GOLDEN_PDFTOPPM: &str = "pdftoppm version 26.08.0";

    const PDF_GOLDEN: &str = r#"
roadrunner.pdf {"cups_name":"Zebra_ZT411-600dpi_ZPL"} ed873ed64b203c67
roadrunner.pdf {} 35479131dc264911
roadrunner.pdf {"cups_name":"Zebra_Raw"} 35479131dc264911
roadrunner.pdf {"cups_name":"Zebra_ZD421-300dpi_ZPL"} 0315a81aa73f8b42
roadrunner.pdf {"zpl_fit":"width"} b585e4e654cf3fce
roadrunner.pdf {"zpl_fit":"none"} 35479131dc264911
roadrunner.pdf {"zpl_fit":"height"} 35479131dc264911
roadrunner.pdf {"zpl_max_width_dots":400,"zpl_max_height_dots":600} 521519a84f7768e4
roadrunner.pdf {"label_width_dots":200,"label_height_dots":300,"zpl_fit":"width"} d8524b753c622350
roadrunner.pdf {"zpl_x":0,"zpl_y":0} cf81fd5d5e7ae990
roadrunner.pdf {"zpl_x":-5,"zpl_y":-10} 71bbd7a8a67bbe08
roadrunner.pdf {"zpl_y":10} 709b199368749d67
roadrunner.pdf {"zpl_threshold":200,"zpl_invert":true} e0ea7beb2efde3be
roadrunner.pdf {"cups_name":"Zebra_ZD421-203dpi_ZPL"} 35479131dc264911
roadrunner.pdf {"zpl_dpi":300} 0315a81aa73f8b42
roadrunner.pdf {"zpl_page":1} 35479131dc264911
roadrunner.pdf {"zpl_page":"2"} err:pdf_render
two.pdf {} e225d4a5b85205b1
two.pdf {"cups_name":"Zebra_Raw"} e225d4a5b85205b1
two.pdf {"cups_name":"Zebra_ZD421-300dpi_ZPL"} 6c65afad4188d15c
two.pdf {"zpl_fit":"width"} 239a3681e90605cc
two.pdf {"zpl_fit":"none"} e225d4a5b85205b1
two.pdf {"zpl_fit":"height"} e225d4a5b85205b1
two.pdf {"zpl_max_width_dots":400,"zpl_max_height_dots":600} 906257693c4749e3
two.pdf {"label_width_dots":200,"label_height_dots":300,"zpl_fit":"width"} 434cf312c551e065
two.pdf {"zpl_x":0,"zpl_y":0} ba600e44fac98a47
two.pdf {"zpl_x":-5,"zpl_y":-10} 37da5191ff31d081
two.pdf {"zpl_y":10} cabc6a64be37be31
two.pdf {"zpl_threshold":200,"zpl_invert":true} ce3f277c7f228b51
two.pdf {"cups_name":"Zebra_ZD421-203dpi_ZPL"} e225d4a5b85205b1
two.pdf {"zpl_dpi":300} 6c65afad4188d15c
two.pdf {"zpl_page":1} 8c2a186829b91735
two.pdf {"zpl_page":"2"} 84dadba5fc6786a1
letter.pdf {} 76d0e0277f4eedde
letter.pdf {"cups_name":"Zebra_Raw"} 76d0e0277f4eedde
letter.pdf {"cups_name":"Zebra_ZD421-300dpi_ZPL"} 3a836c0973436a1f
letter.pdf {"zpl_fit":"width"} 76d0e0277f4eedde
letter.pdf {"zpl_fit":"none"} 41829c9e79e0c4a0
letter.pdf {"zpl_fit":"height"} 41829c9e79e0c4a0
letter.pdf {"zpl_max_width_dots":400,"zpl_max_height_dots":600} 101242fa0f2eaae8
letter.pdf {"label_width_dots":200,"label_height_dots":300,"zpl_fit":"width"} 72541df7eadcccbe
letter.pdf {"zpl_x":0,"zpl_y":0} 4eb3c040476f89b6
letter.pdf {"zpl_x":-5,"zpl_y":-10} 25ff39832d3ac240
letter.pdf {"zpl_y":10} c574156144ff5867
letter.pdf {"zpl_threshold":200,"zpl_invert":true} d936d9d8f4b0cee2
letter.pdf {"cups_name":"Zebra_ZD421-203dpi_ZPL"} 76d0e0277f4eedde
letter.pdf {"zpl_dpi":300} 3a836c0973436a1f
letter.pdf {"zpl_page":1} 76d0e0277f4eedde
letter.pdf {"zpl_page":"2"} err:pdf_render
"#;

    #[test]
    fn pdf_labels_are_byte_identical() {
        let Some(tool) = which("pdftoppm") else {
            eprintln!("no pdftoppm installed; skipping PDF golden output");
            return;
        };
        let out = run_with_timeout(&tool, &["-v"], Duration::from_secs(30)).unwrap();
        let version = format!("{}{}", out.stdout, out.stderr);
        if !version.contains(GOLDEN_PDFTOPPM) {
            eprintln!(
                "{} is not {GOLDEN_PDFTOPPM}; skipping PDF golden output",
                version.trim()
            );
            return;
        }
        let td = tempfile::tempdir().unwrap();
        // Frame and boxes on a Letter page (cropped to its content), and a
        // two-page 4×6 document.
        let frame = "0 g 100 100 288 6 re f 100 100 6 432 re f 382 100 6 432 re f \
                     100 526 288 6 re f 150 300 120 80 re f 140 150 3 120 re f";
        let fixtures = vec![
            ("roadrunner.pdf", test_label("pdf")),
            (
                "two.pdf",
                write_pdf(
                    td.path(),
                    "two.pdf",
                    &pdf_with_boxes(288, 432, &[(36, 36, 72, 36), (36, 36, 144, 72)]),
                ),
            ),
            (
                "letter.pdf",
                write_pdf(
                    td.path(),
                    "letter.pdf",
                    &pdf_with_content(612, 792, &[frame]),
                ),
            ),
        ];
        let options: Vec<&str> = GOLDEN_OPTIONS
            .iter()
            .chain(GOLDEN_PDF_OPTIONS)
            .copied()
            .collect();
        // The real label on a 600 dpi head too: the largest normal raster.
        // First, so the slowest case does not start last.
        let mut cases = vec![(
            fixtures[0].0,
            fixtures[0].1.as_path(),
            r#"{"cups_name":"Zebra_ZT411-600dpi_ZPL"}"#,
        )];
        cases.extend(golden_cases(&fixtures, &options));
        assert_golden(PDF_GOLDEN, &golden_rows(&cases));
    }
}
