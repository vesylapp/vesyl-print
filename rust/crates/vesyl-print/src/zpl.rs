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
use image::{DynamicImage, GrayImage, ImageDecoder, ImageFormat, ImageReader, Luma};
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
/// Most pixels one PDF page may rasterize to. A bigger page that is scaled
/// onto the label anyway is drawn at a lower dpi that keeps it within this
/// ([`page_resolution`]); one printed at its own size (`zpl_fit` none) fails
/// with `pdf_page_too_large`, before the renderer starts. pdftoppm holds the
/// page as one RGB bitmap, about 4 bytes a pixel (a 9000 pt page at 203 dpi
/// took it to 2.4 GB), and the agent decodes its PNG with about as much
/// again. At 50 MP each peaked near 200 MB, which a 1 GB Pi has to spare
/// beside CUPS and the display (scaling the page onto the label can add up
/// to 335 MB, see [`MAX_RESIZE_BYTES`]). It still takes US Legal at 600 dpi
/// (42.8 MP) at full resolution; a 4×6 label at 600 dpi is 8.6 MP. A page's
/// PNG on disk is at most ~150 MB.
const MAX_PAGE_PIXELS: u64 = 50_000_000;
/// Pillow's decompression-bomb limit (2 × `Image.MAX_IMAGE_PIXELS`): a
/// bigger image fails with `image_bad` from its header, before any pixel is
/// decoded, as under Python. The image crate only caps the decoded buffer
/// (512 MiB), which lets 8-bit gray through at three times this.
const MAX_IMAGE_PIXELS: u64 = 178_956_970;
/// ZPL's coordinate range: `^FO` and `^LL` take 0–32000 dots.
const MAX_ZPL_DOTS: i64 = 32_000;
/// Most dots one label graphic may have after fitting; a bigger one fails
/// with `label_too_large`. Each later copy (1-bit raster, hex, label text)
/// costs up to a byte a dot. A 4×6 label at 600 dpi is 8.8 MP, and a 4"
/// label of ZPL's full 32000-dot length is 39 MP at 300 dpi.
const MAX_LABEL_DOTS: u64 = 50_000_000;
/// Most bytes the Lanczos resize may hold in its float buffer (an RGBA f32
/// pixel, 16 bytes, per source column and output row); more fails with
/// `label_too_large`. It is the cap the image crate puts on a decoded image.
/// A PDF page within [`MAX_PAGE_PIXELS`] needs at most 335 MB to fit a
/// default label box (US Legal at 600 dpi: 291 MB).
const MAX_RESIZE_BYTES: u64 = 512 << 20;

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

/// Render one PDF page to PNG, at `dpi` (a page too large to print at its
/// own size fails). Prefers pdftoppm; falls back to Ghostscript.
pub fn pdf_to_png(
    pdf_path: &Path,
    out_dir: &Path,
    dpi: i64,
    page: i64,
) -> Result<PathBuf, ZplError> {
    render_page(&Renderer::find()?, pdf_path, out_dir, dpi, page, None).map(|p| p.png)
}

/// A PDF page rendered to PNG.
#[derive(Debug)]
struct DrawnPage {
    png: PathBuf,
    /// The resolution it was drawn at: the job's, or a lower one that kept
    /// it within [`MAX_PAGE_PIXELS`] (see [`page_resolution`]).
    dpi: i64,
}

impl DrawnPage {
    /// Label dots per pixel of the page at its own size at the job's `dpi`:
    /// 1, unless it was drawn below that.
    fn native_scale(&self, dpi: i64) -> f64 {
        max(72, dpi) as f64 / self.dpi as f64
    }
}

/// [`pdf_to_png`] with a given renderer, for a page scaled into the label
/// box `fitted` (`None`: printed at its own size). A page pdfinfo can size
/// is drawn at the resolution [`page_resolution`] gives it, and the PNG is
/// checked after (see [`finish_page`]).
fn render_page(
    renderer: &Renderer,
    pdf_path: &Path,
    out_dir: &Path,
    dpi: i64,
    page: i64,
    fitted: Option<(i64, i64)>,
) -> Result<DrawnPage, ZplError> {
    // pdftoppm draws page 1 for a page number below 1, the first box pdfinfo
    // lists then.
    let (res, expected) = match PdfInfo::read(pdf_path, page, page)
        .and_then(|info| info.media_boxes.first().copied())
    {
        Some((n, w, h)) => {
            let (res, size) = page_resolution(n, w, h, dpi, fitted)?;
            (res, Some(size))
        }
        None => (max(72, dpi), None),
    };
    fs::create_dir_all(out_dir).map_err(|e| ZplError::new(e.to_string(), "pdf_render"))?;
    let (png, stderr) = draw_page(renderer, pdf_path, out_dir, res, page)?;
    let drawn = DrawnPage { png, dpi: res };
    finish_page(
        renderer, pdf_path, out_dir, page, drawn, expected, &stderr, fitted,
    )
}

/// Run the renderer for `page` alone at `dpi`, into `out_dir`; returns the
/// PNG and the tool's stderr.
fn draw_page(
    renderer: &Renderer,
    pdf_path: &Path,
    out_dir: &Path,
    dpi: i64,
    page: i64,
) -> Result<(PathBuf, String), ZplError> {
    let stem_name = pdf_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = out_dir.join(format!("{stem_name}_p{page}"));
    let png = PathBuf::from(format!("{}.png", stem.display()));
    let res = dpi.to_string();
    let page_s = page.to_string();
    let pdf = pdf_path.display().to_string();

    let (tool, args): (&str, Vec<String>) = match renderer {
        // -singlefile writes stem.png
        Renderer::Pdftoppm(tool) => (
            tool,
            [
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
            .collect(),
        ),
        Renderer::Ghostscript(tool) => (
            tool,
            vec![
                "-dSAFER".into(),
                "-dBATCH".into(),
                "-dNOPAUSE".into(),
                format!("-dFirstPage={page}"),
                format!("-dLastPage={page}"),
                format!("-r{res}"),
                "-sDEVICE=pnggray".into(),
                format!("-sOutputFile={}", png.display()),
                pdf,
            ],
        ),
    };

    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = run_with_timeout(tool, &argv, Duration::from_secs(120))
        .map_err(|e| ZplError::new(format!("{tool} failed: {e}"), "pdf_render"))?;
    if !out.success || !png.is_file() {
        return Err(render_error(tool, out));
    }
    Ok((png, out.stderr))
}

/// Check page `page` as it was `drawn` ([`check_rendered`], against the
/// size it was `expected` at).
///
/// A page over [`MAX_PAGE_PIXELS`] once drawn, though pdfinfo's box was
/// within it or there was none to go by (gs scales by a /UserUnit pdfinfo
/// and pdftoppm ignore; no pdfinfo), is drawn again, alone, into `out_dir`,
/// at the resolution [`page_resolution`] would have given its real size,
/// when it is scaled into the label box `fitted`. gs bands its memory, so
/// the first drawing cost disk, not RAM.
#[allow(clippy::too_many_arguments)] // render_page's, and what the drawing gave
fn finish_page(
    renderer: &Renderer,
    pdf_path: &Path,
    out_dir: &Path,
    page: i64,
    drawn: DrawnPage,
    expected: Option<(u64, u64)>,
    stderr: &str,
    fitted: Option<(i64, i64)>,
) -> Result<DrawnPage, ZplError> {
    let refused = match check_rendered(renderer, &drawn.png, page.max(1), expected, stderr) {
        Ok(()) => return Ok(drawn),
        Err(e) => e,
    };
    let Some(label) = fitted.filter(|_| refused.code == "pdf_page_too_large") else {
        return Err(refused);
    };
    let Some((w, h)) = png_dimensions(&drawn.png) else {
        return Err(refused);
    };
    // Its size in points as this renderer sees it, a pixel over for gs's
    // rounding.
    let pt = |px: u32| (f64::from(px) + 1.0) * 72.0 / drawn.dpi as f64;
    let Some((dpi, _)) = within_page_budget(pt(w), pt(h), label) else {
        return Err(refused);
    };
    log::info!(
        target: LOG,
        "PDF page {} came out at {w} x {h} pixels at {} dpi; drawing it again at {dpi} dpi",
        page.max(1),
        drawn.dpi
    );
    let _ = fs::remove_file(&drawn.png);
    let (png, stderr) = draw_page(renderer, pdf_path, out_dir, dpi, page)?;
    check_rendered(renderer, &png, page.max(1), None, &stderr)?;
    Ok(DrawnPage { png, dpi })
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

/// What `pdfinfo` (poppler-utils, which also ships pdftoppm) reports about
/// a PDF: its page count and the MediaBox of each page asked for.
#[derive(Debug, Clone, Default, PartialEq)]
struct PdfInfo {
    pages: Option<i64>,
    /// `(page, width, height)` in points.
    media_boxes: Vec<(i64, f64, f64)>,
}

impl PdfInfo {
    /// `pdfinfo -f first -l last -box`; `None` without pdfinfo or when it
    /// fails (a page past the end, a damaged file).
    fn read(pdf_path: &Path, first: i64, last: i64) -> Option<PdfInfo> {
        let tool = which("pdfinfo")?;
        let (first_s, last_s) = (first.to_string(), last.to_string());
        let pdf = pdf_path.display().to_string();
        let argv = ["-f", &first_s, "-l", &last_s, "-box", &pdf];
        let out = run_with_timeout(&tool, &argv, Duration::from_secs(30)).ok()?;
        out.success.then(|| PdfInfo::parse(&out.stdout, first))
    }

    /// The renderers draw the MediaBox: the page "size" line is the CropBox,
    /// which can be far smaller. The box lines are numbered
    /// (`Page    2 MediaBox: …`) unless `-l` is 0, as for render_page's page
    /// 0: unnumbered, they are page `first`, or page 1 below that.
    ///
    /// Only lines after the last `Pages:` count: the ones before it are the
    /// document's own metadata (title, author …), which could fake a box.
    fn parse(stdout: &str, first: i64) -> PdfInfo {
        let mut info = PdfInfo::default();
        for line in stdout.lines() {
            if let Some(n) = line.strip_prefix("Pages:") {
                info = PdfInfo {
                    pages: n.trim().parse().ok(),
                    media_boxes: Vec::new(),
                };
                continue;
            }
            let Some((head, coords)) = line.split_once("MediaBox:") else {
                continue;
            };
            let page = match head.trim() {
                "" => first.max(1),
                head => match head.strip_prefix("Page").map(|n| n.trim().parse()) {
                    Some(Ok(n)) => n,
                    _ => continue,
                },
            };
            let v: Vec<f64> = coords
                .split_whitespace()
                .filter_map(|t| t.parse().ok())
                .collect();
            if let [x0, y0, x1, y1] = v[..] {
                info.media_boxes
                    .push((page, (x1 - x0).abs(), (y1 - y0).abs()));
            }
        }
        info
    }
}

/// The size pdftoppm draws a `w`×`h` pt page at `dpi` (it rounds up; gs
/// rounds).
fn drawn_size(w: f64, h: f64, dpi: f64) -> (f64, f64) {
    // Multiplied before dividing, as poppler does: 432 × (600 / 72) is a
    // hair over 3600.
    ((w * dpi / 72.0).ceil(), (h * dpi / 72.0).ceil())
}

/// Hold a `w`×`h` pt page to [`MAX_PAGE_PIXELS`] at `dpi` before rendering;
/// returns the size pdftoppm will draw it.
///
/// gs also scales by a page's /UserUnit, which pdfinfo and pdftoppm ignore:
/// such a page is caught once rendered (gs bands its memory, so rendering it
/// costs disk, not RAM).
fn check_page_budget(page: i64, w: f64, h: f64, dpi: i64) -> Result<(u64, u64), ZplError> {
    let (pw, ph) = drawn_size(w, h, max(72, dpi) as f64);
    let pixels = pw * ph;
    if pixels.is_nan() || pixels > MAX_PAGE_PIXELS as f64 {
        return Err(ZplError::new(
            format!(
                "PDF page {page} is {:.1} x {:.1} in: {pw} x {ph} pixels at {dpi} dpi, more \
                 than the {MAX_PAGE_PIXELS} a page may have",
                w / 72.0,
                h / 72.0,
            ),
            "pdf_page_too_large",
        ));
    }
    Ok((pw as u64, ph as u64))
}

/// The dpi to draw a PDF page at, and the size pdftoppm draws it there.
type Resolution = (i64, (u64, u64));

/// The resolution to draw a `w`×`h` pt page at: `dpi` (at least 72) when
/// [`check_page_budget`] passes.
///
/// A bigger page that is scaled into the label box `fitted` (any `zpl_fit`
/// but none) is drawn at the highest dpi that keeps it within
/// [`MAX_PAGE_PIXELS`] instead, as long as it still covers the box there
/// ([`within_page_budget`]): the fit then scales it down onto the label as
/// it would the page drawn at `dpi`, so the label loses nothing it could
/// print. A page printed at its own size (`fitted` is `None`), or one that
/// could not cover the box within the budget (only a box of more dots than
/// the budget has pixels, or a page kilometres across, could not be
/// covered), fails with `pdf_page_too_large`.
fn page_resolution(
    page: i64,
    w: f64,
    h: f64,
    dpi: i64,
    fitted: Option<(i64, i64)>,
) -> Result<Resolution, ZplError> {
    let refused = match check_page_budget(page, w, h, dpi) {
        Ok(size) => return Ok((max(72, dpi), size)),
        Err(e) => e,
    };
    let Some((res, size)) = fitted.and_then(|label| within_page_budget(w, h, label)) else {
        return Err(refused);
    };
    log::info!(
        target: LOG,
        "PDF page {page} is {:.1} x {:.1} in: drawing it at {res} dpi, not {dpi}, to stay \
         within the {MAX_PAGE_PIXELS} pixels a page may have",
        w / 72.0,
        h / 72.0
    );
    Ok((res, size))
}

/// The highest dpi at which a `w`×`h` pt page stays within
/// [`MAX_PAGE_PIXELS`] and still covers the label box `(label_w, label_h)`
/// (fills its width or its length: what the fit scales the page down to),
/// with the size pdftoppm draws it there; `None` when there is none.
fn within_page_budget(w: f64, h: f64, (label_w, label_h): (i64, i64)) -> Option<Resolution> {
    // From the area, then down past pdftoppm's rounding up.
    let mut dpi = (72.0 * (MAX_PAGE_PIXELS as f64 / (w * h)).sqrt()).floor();
    let (pw, ph) = loop {
        // A NaN comes from a broken box.
        if dpi.is_nan() || dpi < 1.0 {
            return None;
        }
        let (pw, ph) = drawn_size(w, h, dpi);
        if pw * ph <= MAX_PAGE_PIXELS as f64 {
            break (pw, ph);
        }
        dpi -= 1.0;
    };
    // Whole pixels only: gs rounds where pdftoppm rounds up. Either way
    // round: the box pdfinfo gives is the page before its /Rotate.
    let (whole_w, whole_h) = ((w * dpi / 72.0).floor(), (h * dpi / 72.0).floor());
    let covers = |(a, b): (f64, f64)| a >= label_w as f64 || b >= label_h as f64;
    (covers((whole_w, whole_h)) && covers((whole_h, whole_w)))
        .then_some((dpi as i64, (pw as u64, ph as u64)))
}

/// A PNG's size, from its header.
fn png_dimensions(png: &Path) -> Option<(u32, u32)> {
    ImageReader::open(png)
        .and_then(|r| r.with_guessed_format())
        .map_err(image::ImageError::IoError)
        .and_then(|r| r.into_dimensions())
        .ok()
}

/// Check a rendered page from its PNG header, before decoding it.
///
/// - Over [`MAX_PAGE_PIXELS`] (no pdfinfo to check it before, or gs and a
///   /UserUnit): `pdf_page_too_large`.
/// - pdftoppm exits 0 with a 1×1 PNG when it cannot allocate the page bitmap
///   ("Out of memory", "Bogus memory allocation size"). A size other than
///   pdfinfo's (either way round: /Rotate turns the page), or 1×1 when that
///   is unknown, is a failed render (`pdf_render`), not a blank label.
fn check_rendered(
    renderer: &Renderer,
    png: &Path,
    page: i64,
    expected: Option<(u64, u64)>,
    stderr: &str,
) -> Result<(), ZplError> {
    // An unreadable PNG fails to decode next, as image_bad.
    let Some((w, h)) = png_dimensions(png) else {
        return Ok(());
    };
    let (w64, h64) = (u64::from(w), u64::from(h));
    if w64 * h64 > MAX_PAGE_PIXELS {
        return Err(ZplError::new(
            format!(
                "PDF page {page} rendered at {w} x {h} pixels, more than the \
                 {MAX_PAGE_PIXELS} a page may have"
            ),
            "pdf_page_too_large",
        ));
    }
    let Renderer::Pdftoppm(tool) = renderer else {
        return Ok(());
    };
    let near = |ew: u64, eh: u64| w64.abs_diff(ew) <= 2 && h64.abs_diff(eh) <= 2;
    let (drawn, wanted) = match expected {
        Some((ew, eh)) => (near(ew, eh) || near(eh, ew), format!(", not {ew} x {eh}")),
        None => ((w, h) != (1, 1), String::new()),
    };
    if drawn {
        return Ok(());
    }
    let why = match stderr.trim() {
        "" => String::new(),
        msg => format!(": {msg}"),
    };
    Err(ZplError::new(
        format!("{tool} rendered page {page} at {w} x {h} pixels{wanted}{why}"),
        "pdf_render",
    ))
}

/// Render pages `first..=last` of a PDF at `dpi` into `dir` with one tool
/// run (both tools stop at the document's real last page). Returns the
/// PNGs in page order, and the tool's stderr.
fn draw_pages(
    renderer: &Renderer,
    pdf_path: &Path,
    dir: &Path,
    dpi: i64,
    first: i64,
    last: i64,
) -> Result<(Vec<PathBuf>, String), ZplError> {
    fs::create_dir_all(dir).map_err(|e| ZplError::new(e.to_string(), "pdf_render"))?;
    let root = dir.join("page").display().to_string();
    let res = dpi.to_string();
    let pdf = pdf_path.display().to_string();
    let (tool, args): (&str, Vec<String>) = match renderer {
        // page-1.png, page-2.png …: the page number, zero-padded to the
        // width of the page count.
        Renderer::Pdftoppm(tool) => (
            tool,
            vec![
                "-png".into(),
                "-r".into(),
                res,
                "-f".into(),
                first.to_string(),
                "-l".into(),
                last.to_string(),
                pdf,
                root,
            ],
        ),
        // page-1.png, page-2.png …, numbered from 1 whatever `first` is; a
        // literal % in the path must be doubled.
        Renderer::Ghostscript(tool) => (
            tool,
            vec![
                "-dSAFER".into(),
                "-dBATCH".into(),
                "-dNOPAUSE".into(),
                format!("-dFirstPage={first}"),
                format!("-dLastPage={last}"),
                format!("-r{res}"),
                "-sDEVICE=pnggray".into(),
                format!("-sOutputFile={}-%d.png", root.replace('%', "%%")),
                pdf,
            ],
        ),
    };
    // A single page keeps its 120 s; each further page adds 10 s.
    let timeout = Duration::from_secs(120 + 10 * (last - first).max(0) as u64);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = run_with_timeout(tool, &argv, timeout)
        .map_err(|e| ZplError::new(format!("{tool} failed: {e}"), "pdf_render"))?;
    if !out.success {
        return Err(render_error(tool, out));
    }
    // Sort by number: pdftoppm pads (page-01.png) and gs does not.
    let mut pages: Vec<(u64, PathBuf)> = fs::read_dir(dir)
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
    Ok((pages.into_iter().map(|(_, png)| png).collect(), out.stderr))
}

/// Render pages `1..=last` of a PDF into `out_dir`, each at the resolution
/// `plan` gives it (page → its dpi and the size pdfinfo puts it at there,
/// from [`page_resolution`]), the job's `dpi` for a page it lacks: one tool
/// run for each stretch of pages drawn alike, so one run for a document
/// whose pages all fit the budget. Each page is then checked, for the label
/// box `fitted` (see [`finish_page`]). Returns the pages in order.
fn pdf_pages_to_png(
    renderer: &Renderer,
    pdf_path: &Path,
    out_dir: &Path,
    dpi: i64,
    last: i64,
    plan: &[(i64, Resolution)],
    fitted: Option<(i64, i64)>,
) -> Result<Vec<DrawnPage>, ZplError> {
    let planned = |page: i64| plan.iter().find(|(p, _)| *p == page).map(|(_, r)| *r);
    // (first page, last page, dpi) of each run.
    let mut runs: Vec<(i64, i64, i64)> = Vec::new();
    for page in 1..=last {
        let res = planned(page).map_or(max(72, dpi), |(res, _)| res);
        match runs.last_mut() {
            Some((_, end, r)) if *r == res => *end = page,
            _ => runs.push((page, page, res)),
        }
    }
    let one_run = runs.len() == 1;
    let mut pages = Vec::new();
    for (first, end, res) in runs {
        // gs numbers each run's files from 1: a directory of its own.
        let dir = if one_run {
            out_dir.to_path_buf()
        } else {
            out_dir.join(first.to_string())
        };
        let (pngs, stderr) = draw_pages(renderer, pdf_path, &dir, res, first, end)?;
        for (page, png) in (first..).zip(pngs) {
            let expected = planned(page).map(|(_, size)| size);
            let drawn = DrawnPage { png, dpi: res };
            pages.push(finish_page(
                renderer, pdf_path, &dir, page, drawn, expected, &stderr, fitted,
            )?);
        }
    }
    Ok(pages)
}

/// Render every page of a PDF, in order, up to [`MAX_PDF_PAGES`], for the
/// label box `fitted` (see [`render_page`]), each held to
/// [`MAX_PAGE_PIXELS`] before any is rendered. `info` is what pdfinfo said
/// (empty without it): with no page count, one page past the limit is
/// rendered so an over-long document is still caught.
fn pdf_all_pages_to_png(
    renderer: &Renderer,
    pdf_path: &Path,
    out_dir: &Path,
    dpi: i64,
    info: &PdfInfo,
    fitted: Option<(i64, i64)>,
) -> Result<Vec<DrawnPage>, ZplError> {
    let too_many = |count: String| {
        ZplError::new(
            format!(
                "PDF has {count} pages; ZPL conversion handles at most {MAX_PDF_PAGES} \
                 (set zpl_page to print a single page)"
            ),
            "pdf_too_many_pages",
        )
    };
    let last = match info.pages {
        Some(n) if n > MAX_PDF_PAGES => return Err(too_many(n.to_string())),
        Some(n) if n >= 1 => n,
        _ => MAX_PDF_PAGES + 1,
    };
    let plan = info
        .media_boxes
        .iter()
        .map(|&(page, w, h)| Ok((page, page_resolution(page, w, h, dpi, fitted)?)))
        .collect::<Result<Vec<_>, ZplError>>()?;
    let pages = pdf_pages_to_png(renderer, pdf_path, out_dir, dpi, last, &plan, fitted)?;
    if pages.len() as i64 > MAX_PDF_PAGES {
        return Err(too_many(format!("more than {MAX_PDF_PAGES}")));
    }
    Ok(pages)
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
///
/// 8-bit gray is returned as is (it is its own luma: the weights sum to
/// 65536), and 8-bit RGB / RGBA are read in place rather than copied.
fn to_pil_luma(img: DynamicImage) -> GrayImage {
    let luma = |r: u8, g: u8, b: u8| {
        (r as u32 * 19595 + g as u32 * 38470 + b as u32 * 7471 + 0x8000) >> 16
    };
    let from_rgba = |rgba: &image::RgbaImage| {
        GrayImage::from_fn(rgba.width(), rgba.height(), |x, y| {
            let [r, g, b, a] = rgba.get_pixel(x, y).0;
            let (l, a) = (luma(r, g, b), a as u32);
            Luma([((l * a + 255 * (255 - a) + 127) / 255) as u8])
        })
    };
    let from_rgb = |rgb: &image::RgbImage| {
        GrayImage::from_fn(rgb.width(), rgb.height(), |x, y| {
            let [r, g, b] = rgb.get_pixel(x, y).0;
            Luma([luma(r, g, b) as u8])
        })
    };
    match img {
        DynamicImage::ImageLuma8(gray) => gray,
        DynamicImage::ImageRgb8(rgb) => from_rgb(&rgb),
        DynamicImage::ImageRgba8(rgba) => from_rgba(&rgba),
        img if img.color().has_alpha() => from_rgba(&img.to_rgba8()),
        img => from_rgb(&img.to_rgb8()),
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
    let (img, native) = if is_pdf {
        let work = tempfile::Builder::new()
            .prefix("vesyl-zpl-pdf-")
            .tempdir()
            .map_err(|e| ZplError::new(e.to_string(), "image_bad"))?;
        // The box the fit scales the page into; no length is no limit.
        let length = max_height_dots.filter(|h| *h != 0).unwrap_or(i64::MAX);
        let fitted = (fit != Fit::None).then_some((max_width_dots.max(8), length));
        let page = render_page(
            &Renderer::find()?,
            path,
            work.path(),
            DEFAULT_DPI,
            1,
            fitted,
        )?;
        (open_gray(&page.png)?, page.native_scale(DEFAULT_DPI))
    } else {
        (open_gray(path)?, 1.0)
    };
    to_mono_label(
        img,
        max_width_dots,
        max_height_dots,
        threshold,
        invert,
        fit,
        native,
    )
}

/// [`load_image_as_mono`] for a decoded image. `native` is the scale of the
/// image at its own size on the label: 1, or more for a PDF page drawn
/// below the job's dpi ([`DrawnPage::native_scale`]). `Fit::Contain` scales
/// down from that size and never up past it, so such a page comes out as
/// it would have at the job's dpi.
fn to_mono_label(
    mut img: GrayImage,
    max_width_dots: i64,
    max_height_dots: Option<i64>,
    threshold: i64,
    invert: bool,
    fit: Fit,
    native: f64,
) -> Result<GrayImage, ZplError> {
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
    let mut scale = native;
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
            if wf * native > max_w {
                scale = scale.min(max_w / wf);
            }
            if let Some(mh) = max_h {
                if hf * native > mh {
                    scale = scale.min(mh / hf);
                }
            }
        }
        Fit::None => {}
    }

    let resize = (scale - 1.0).abs() > 0.001;
    let (nw, nh) = if resize {
        (
            round_half_even(wf * scale).max(1),
            round_half_even(hf * scale).max(1),
        )
    } else {
        (w.into(), h.into())
    };
    check_label_size((w, h), (nw, nh))?;
    if resize {
        img = image::imageops::resize(&img, nw as u32, nh as u32, FilterType::Lanczos3);
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

/// Refuse a label graphic too large to build, before anything is allocated
/// for it: `nw`×`nh` dots over [`MAX_LABEL_DOTS`], or a resize from `src`
/// whose float buffer would pass [`MAX_RESIZE_BYTES`] (a same-size resize
/// is a plain copy). Huge label boxes otherwise abort the agent: a failed
/// allocation is not a panic that a job can contain.
fn check_label_size(src: (u32, u32), (nw, nh): (i64, i64)) -> Result<(), ZplError> {
    let too_large = |msg: String| {
        Err(ZplError::new(
            format!(
                "{msg} (check zpl_max_width_dots / zpl_max_height_dots / label_width_dots / \
                 label_height_dots / zpl_fit)"
            ),
            "label_too_large",
        ))
    };
    let (w, h) = (nw.max(1) as u64, nh.max(1) as u64);
    if w.saturating_mul(h) > MAX_LABEL_DOTS {
        return too_large(format!(
            "label graphic would be {nw} x {nh} dots, more than the {MAX_LABEL_DOTS} a label \
             may have"
        ));
    }
    let buffer = u64::from(src.0).saturating_mul(h).saturating_mul(16);
    if (w, h) != (src.0.into(), src.1.into()) && buffer > MAX_RESIZE_BYTES {
        return too_large(format!(
            "scaling the {} x {} image to {nw} x {nh} dots needs {} MiB, more than the {} MiB \
             a resize may use",
            src.0,
            src.1,
            buffer >> 20,
            MAX_RESIZE_BYTES >> 20
        ));
    }
    Ok(())
}

/// Decode by content, not file name (like Pillow's `Image.open`): carrier
/// TIFF or CDN WebP bytes arrive saved as `.png`.
fn open_gray(path: &Path) -> Result<GrayImage, ZplError> {
    let bad =
        |e: &dyn std::fmt::Display| ZplError::new(format!("open image failed: {e}"), "image_bad");
    let reader = ImageReader::open(path)
        .and_then(|r| r.with_guessed_format())
        .map_err(|e| bad(&e))?;
    let img = match decode_capped(reader)? {
        Ok(img) => img,
        Err(e) => {
            // Pillow prints PNGs with a missing IEND or a bad chunk CRC; the
            // png crate rejects them. Retry once with the framing repaired.
            let repaired = match fs::read(path).ok().and_then(|bytes| repair_png(&bytes)) {
                Some(png) => {
                    decode_capped(ImageReader::with_format(Cursor::new(png), ImageFormat::Png))?
                        .ok()
                }
                None => None,
            };
            match repaired {
                Some(img) => {
                    log::warn!(target: LOG, "{}: repaired damaged PNG ({e})", path.display());
                    img
                }
                None => return Err(bad(&e)),
            }
        }
    };
    Ok(to_pil_luma(img))
}

/// [`ImageReader::decode`], unless the header puts the image over
/// [`MAX_IMAGE_PIXELS`]: that refusal is the `Err`, made before any pixel
/// buffer exists. `Ok(Err(_))` is the decoder's own error.
fn decode_capped<R: std::io::BufRead + std::io::Seek>(
    reader: ImageReader<R>,
) -> Result<image::ImageResult<DynamicImage>, ZplError> {
    let mut decoder = match reader.into_decoder() {
        Ok(decoder) => decoder,
        Err(e) => return Ok(Err(e)),
    };
    let (w, h) = decoder.dimensions();
    if u64::from(w) * u64::from(h) > MAX_IMAGE_PIXELS {
        return Err(ZplError::new(
            format!(
                "open image failed: image is {w} x {h} pixels, more than the \
                 {MAX_IMAGE_PIXELS} an image may have"
            ),
            "image_bad",
        ));
    }
    // What `decode` does from here: the buffer counts against the default
    // limits (512 MiB) and the decoder gets what is left.
    let mut limits = image::Limits::default();
    Ok(limits
        .reserve(decoder.total_bytes())
        .and_then(|()| decoder.set_limits(limits))
        .and_then(|()| DynamicImage::from_decoder(decoder)))
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
/// `^PW` / `^LL` cover the bitmap plus its `x` / `y` offset; they and the
/// offsets stay within ZPL's 32000 dots. Print quantity is left to CUPS
/// `lp -n` unless copies > 1.
pub fn build_zpl_label(gfa: &Gfa, height_dots: Option<i64>, x: i64, y: i64, copies: i64) -> String {
    let copies = copies.max(1);
    let (x, y) = (x.min(MAX_ZPL_DOTS), y.min(MAX_ZPL_DOTS));
    let width_dots = (gfa.bytes_per_row as i64)
        .saturating_mul(8)
        .max(8)
        .saturating_add(x.max(0))
        .min(MAX_ZPL_DOTS);
    let mut parts = vec![
        "^XA".to_string(),
        "^LH0,0".into(),
        "^FWN".into(),
        format!("^PW{width_dots}"),
    ];
    if let Some(h) = height_dots.filter(|h| *h > 0) {
        parts.push(format!(
            "^LL{}",
            h.saturating_add(y.max(0)).min(MAX_ZPL_DOTS)
        ));
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
    // A box past ZPL's range is no bigger a label (`label_too_large` bounds
    // what fills it). Below 8 dots the width is 8, as in load_image_as_mono.
    max_w = max_w.clamp(8, MAX_ZPL_DOTS);
    max_h = max_h.min(MAX_ZPL_DOTS);
    let thr = opt_int(opts, "zpl_threshold", DEFAULT_THRESHOLD);
    let invert = get_truthy(opts, "zpl_invert") || get_truthy(opts, "invert");
    let x = opt_int(opts, "zpl_x", 0);
    let y = opt_int(opts, "zpl_y", DEFAULT_TOP_MARGIN_DOTS);
    // ^FO takes 0–32000 dots: past that the graphic is off any label. A
    // negative offset passes through, as it always has.
    for (key, offset) in [("zpl_x", x), ("zpl_y", y)] {
        if offset > MAX_ZPL_DOTS {
            return Err(ZplError::new(
                format!("{key} {offset} is past ZPL's {MAX_ZPL_DOTS}-dot range"),
                "zpl_error",
            ));
        }
    }

    // Leave room at the bottom so ^FO y-shift does not run off the label
    // (overflow onto the next gap looks like an extra blank label), and on
    // the right for an x-shift (^PW widens by it; past the media it clips).
    if y > 0 && max_h != 0 {
        max_h = (max_h - y).max(8);
    }
    if x > 0 {
        max_w = (max_w - x).max(8);
    }

    let is_pdf = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
    // `_work` keeps the rendered pages until they are converted. Each
    // source comes with its scale at its own size (see to_mono_label).
    let (sources, _work): (Vec<(PathBuf, f64)>, _) = if is_pdf {
        // DPI only affects PDF rasterization.
        let work = tempfile::Builder::new()
            .prefix("vesyl-zpl-pdf-")
            .tempdir()
            .map_err(|e| ZplError::new(e.to_string(), "pdf_render"))?;
        let renderer = Renderer::find()?;
        // A page over the page budget may be drawn smaller when it is
        // scaled into this box anyway (see page_resolution).
        let fitted = (fit != Fit::None).then_some((max_w, max_h));
        let pages = if opts.get("zpl_page").is_some_and(|v| !v.is_null()) {
            let page = opt_int(opts, "zpl_page", 1);
            vec![render_page(
                &renderer,
                path,
                work.path(),
                dpi,
                page,
                fitted,
            )?]
        } else {
            let info = PdfInfo::read(path, 1, MAX_PDF_PAGES + 1).unwrap_or_default();
            pdf_all_pages_to_png(&renderer, path, work.path(), dpi, &info, fitted)?
        };
        let sources = pages
            .into_iter()
            .map(|p| {
                let native = p.native_scale(dpi);
                (p.png, native)
            })
            .collect();
        (sources, Some(work))
    } else {
        (vec![(path.to_path_buf(), 1.0)], None)
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
    for (i, (source, native)) in sources.iter().enumerate() {
        let gray = open_gray(source)?;
        let img = to_mono_label(gray, max_w, Some(max_h), thr, invert, fit, *native)?;
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
        pdf_with_page_entries(page_w, page_h, "", contents)
    }

    /// [`pdf_with_content`] with `extra` (e.g. " /CropBox [0 0 288 432]")
    /// added to every page dictionary.
    fn pdf_with_page_entries(page_w: u32, page_h: u32, extra: &str, contents: &[&str]) -> Vec<u8> {
        let pages: Vec<_> = contents.iter().map(|c| (page_w, page_h, *c)).collect();
        pdf_with_pages(&pages, extra)
    }

    /// Minimal PDF: one page per `(width, height, content stream)`, in
    /// points, each page dictionary with `extra` added.
    fn pdf_with_pages(pages: &[(u32, u32, &str)], extra: &str) -> Vec<u8> {
        let n = pages.len();
        let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", 3 + 2 * i)).collect();
        let mut objs = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            format!("<< /Type /Pages /Kids [{}] /Count {n} >>", kids.join(" ")),
        ];
        for (i, (page_w, page_h, content)) in pages.iter().enumerate() {
            objs.push(format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {page_w} {page_h}]{extra} \
                 /Contents {} 0 R >>",
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

    /// `VESYL_PRINT_REQUIRE_RENDERERS=1` (set by CI's test job, which
    /// installs poppler-utils and ghostscript) makes a missing PDF tool fail
    /// the tests that need it: a runner without them must not pass those
    /// tests without running them.
    const REQUIRE_RENDERERS: &str = "VESYL_PRINT_REQUIRE_RENDERERS";

    /// `tool` (pdftoppm, pdfinfo, gs) on PATH. Without it, a check that
    /// needs it is skipped, unless [`REQUIRE_RENDERERS`] is set.
    fn pdf_tool(tool: &str) -> Option<String> {
        let found = which(tool);
        if found.is_none() {
            assert!(
                std::env::var_os(REQUIRE_RENDERERS).is_none_or(|v| v != "1"),
                "{tool} is not installed, but {REQUIRE_RENDERERS}=1"
            );
            eprintln!("no {tool} installed; skipping the PDF checks that need it");
        }
        found
    }

    /// Every rasterizer installed here (a machine may have neither).
    fn renderers() -> Vec<Renderer> {
        [
            pdf_tool("pdftoppm").map(Renderer::Pdftoppm),
            pdf_tool("gs").map(Renderer::Ghostscript),
        ]
        .into_iter()
        .flatten()
        .collect()
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
            let unknown = PdfInfo::default();
            let pages = pdf_all_pages_to_png(&renderer, &pdf, &out, 72, &unknown, None).unwrap();
            let widths: Vec<u32> = pages
                .iter()
                .map(|p| {
                    let (x0, _, x1, _) = content_bbox(&open_gray(&p.png).unwrap()).unwrap();
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
        let info = PdfInfo {
            pages: Some(51),
            ..PdfInfo::default()
        };
        let err =
            pdf_all_pages_to_png(&r, Path::new("x.pdf"), td.path(), 72, &info, None).unwrap_err();
        assert_eq!(err.code, "pdf_too_many_pages");
        assert!(err.message.contains("51 pages"), "{}", err.message);

        // Unknown count (no pdfinfo): one page past the limit is rendered.
        let boxes = vec![(2, 2, 8, 8); MAX_PDF_PAGES as usize + 1];
        let pdf = write_pdf(td.path(), "long.pdf", &pdf_with_boxes(18, 18, &boxes));
        for renderer in renderers() {
            let out = td
                .path()
                .join(format!("{renderer:?}").replace(['"', '/', ' '], "_"));
            let unknown = PdfInfo::default();
            let err = pdf_all_pages_to_png(&renderer, &pdf, &out, 72, &unknown, None).unwrap_err();
            assert_eq!(err.code, "pdf_too_many_pages", "{renderer:?}");
            if let Some(info) = PdfInfo::read(&pdf, 1, MAX_PDF_PAGES + 1) {
                assert_eq!(info.pages, Some(MAX_PDF_PAGES + 1));
                assert_eq!(info.media_boxes.len() as i64, MAX_PDF_PAGES + 1);
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

    // --- Size limits: refuse what would exhaust the Pi ------------------------

    /// The `^LL…` value of each label in a ZPL stream.
    fn label_lengths(zpl: &str) -> Vec<i64> {
        zpl.lines()
            .filter_map(|l| l.strip_prefix("^LL"))
            .map(|h| h.parse().unwrap())
            .collect()
    }

    /// The `^FO` x, y and the `^GFA` bytes per row of each label.
    fn label_fields(zpl: &str) -> Vec<(i64, i64, i64)> {
        zpl.lines()
            .filter_map(|l| l.strip_prefix("^FO"))
            .map(|l| {
                let (pos, gfa) = l.split_once("^GFA,").unwrap();
                let (x, y) = pos.split_once(',').unwrap();
                let row = gfa.split(',').nth(2).unwrap();
                (x.parse().unwrap(), y.parse().unwrap(), row.parse().unwrap())
            })
            .collect()
    }

    fn is_permanent(err: &ZplError) -> bool {
        crate::jobs::JobError::from(err.clone()).is_permanent()
    }

    #[test]
    fn page_budget_takes_label_and_office_pages_and_refuses_huge_ones() {
        // 4×6 and US Letter / Legal, up to 600 dpi.
        assert_eq!(
            check_page_budget(1, 288.0, 432.0, 203).unwrap(),
            (812, 1218)
        );
        assert_eq!(
            check_page_budget(1, 288.0, 432.0, 600).unwrap(),
            (2400, 3600)
        );
        assert_eq!(
            check_page_budget(1, 612.0, 792.0, 600).unwrap(),
            (5100, 6600)
        );
        assert_eq!(
            check_page_budget(1, 612.0, 1008.0, 600).unwrap(),
            (5100, 8400)
        );
        // pdftoppm rounds up (Letter at 203 dpi is 1725.5 dots wide).
        assert_eq!(
            check_page_budget(1, 612.0, 792.0, 203).unwrap(),
            (1726, 2233)
        );
        // A0, at 203 and 600 dpi; a 9000 pt page; a NaN from a broken box.
        for (w, h, dpi) in [
            (2384.0, 3370.0, 203),
            (2384.0, 3370.0, 600),
            (9000.0, 9000.0, 203),
            (f64::NAN, 432.0, 203),
            (f64::INFINITY, 432.0, 203),
        ] {
            let err = check_page_budget(3, w, h, dpi).unwrap_err();
            assert_eq!(err.code, "pdf_page_too_large", "{w}x{h}@{dpi}");
            assert!(err.message.starts_with("PDF page 3 is "), "{}", err.message);
            assert!(is_permanent(&err));
        }
        let err = check_page_budget(1, 9000.0, 9000.0, 203).unwrap_err();
        assert!(
            err.message
                .contains("125.0 x 125.0 in: 25375 x 25375 pixels at 203 dpi"),
            "{}",
            err.message
        );
    }

    #[test]
    fn pdfinfo_media_boxes_are_parsed_after_the_metadata() {
        // One page (`-f 2 -l 2`): its lines are numbered, and its "size" is
        // the CropBox, not what is rendered.
        let numbered = "Title:           x\nPages:           3\nPage    2 size:  288 x 432 pts\n\
                        Page    2 rot:   0\n\
                        Page    2 MediaBox:      0.00     0.00  9000.00  9000.00\n\
                        Page    2 CropBox:       0.00     0.00   288.00   432.00\n";
        assert_eq!(
            PdfInfo::parse(numbered, 2),
            PdfInfo {
                pages: Some(3),
                media_boxes: vec![(2, 9000.0, 9000.0)],
            }
        );
        // Page 0 (`-f 0 -l 0`): no page numbers, and the box is page 1's,
        // the page pdftoppm draws for it. (`-f 2 -l 0` would list page 2's.)
        let unnumbered = "Title:           x\nPages:           3\nPage size:       288 x 432 pts\n\
                          Page rot:        0\nMediaBox:            0.00     0.00  9000.00  9000.00\n\
                          CropBox:             0.00     0.00   288.00   432.00\n";
        assert_eq!(
            PdfInfo::parse(unnumbered, 0),
            PdfInfo {
                pages: Some(3),
                media_boxes: vec![(1, 9000.0, 9000.0)],
            }
        );
        assert_eq!(PdfInfo::parse(unnumbered, 2).media_boxes[0].0, 2);
        // Several pages, with an offset origin; a title faking a box and a
        // page count is ignored (it comes before the real `Pages:`).
        let many = "Title:           x\nPages:           1\nMediaBox: 0 0 1 1\nAuthor: y\n\
                    Pages:           2\nPage    1 size:  288 x 432 pts\n\
                    Page    1 MediaBox:     10.00    20.00   298.00   452.00\n\
                    Page    1 CropBox:      10.00    20.00   298.00   452.00\n\
                    Page    2 MediaBox:      0.00     0.00  2384.00  3370.00\n";
        assert_eq!(
            PdfInfo::parse(many, 1),
            PdfInfo {
                pages: Some(2),
                media_boxes: vec![(1, 288.0, 432.0), (2, 2384.0, 3370.0)],
            }
        );
        assert_eq!(PdfInfo::parse("", 1), PdfInfo::default());
    }

    /// A page over the budget fails before anything is rendered where it
    /// must print at its own size (zpl_fit none), or where no resolution
    /// within the budget covers the label box it is scaled into.
    #[test]
    fn huge_pdf_pages_fail_before_anything_is_rendered() {
        if pdf_tool("pdfinfo").is_none() {
            return;
        }
        let td = tempfile::tempdir().unwrap();
        // A renderer that cannot run: only a check before rendering passes.
        let absent = Renderer::Pdftoppm("/nonexistent/pdftoppm".into());
        let huge = write_pdf(
            td.path(),
            "huge.pdf",
            &pdf_with_boxes(9000, 9000, &[(10, 10, 200, 300)]),
        );
        // A 4×6 CropBox on a 9000 pt MediaBox renders 9000 pt.
        let cropped = write_pdf(
            td.path(),
            "cropped.pdf",
            &pdf_with_page_entries(
                9000,
                9000,
                " /CropBox [0 0 288 432]",
                &["0 g 10 10 200 300 re f"],
            ),
        );
        // Page 2 of 2 is A0.
        let content = "0 g 36 36 72 36 re f";
        let second = write_pdf(
            td.path(),
            "second.pdf",
            &pdf_with_pages(&[(288, 432, content), (2384, 3370, content)], ""),
        );
        // At their own size; or into a box 32000 dots square, which none of
        // them covers at the dpi that keeps it within the budget (7000 × 7000
        // pixels for the 9000 pt page, 5927 × 8379 for A0).
        for fitted in [None, Some((32_000, 32_000))] {
            for (pdf, page) in [(&huge, 1), (&cropped, 1), (&second, 2)] {
                let out = td.path().join("out");
                let info = PdfInfo::read(pdf, 1, MAX_PDF_PAGES + 1).unwrap();
                let err = pdf_all_pages_to_png(&absent, pdf, &out, 203, &info, fitted).unwrap_err();
                assert_eq!(
                    err.code,
                    "pdf_page_too_large",
                    "{} {fitted:?}: {}",
                    pdf.display(),
                    err.message
                );
                assert!(
                    err.message.starts_with(&format!("PDF page {page} is ")),
                    "{}",
                    err.message
                );
                assert!(!out.exists(), "nothing rendered for {}", pdf.display());

                // zpl_page selects the page and checks it the same way.
                let err = render_page(&absent, pdf, &out, 203, page, fitted).unwrap_err();
                assert_eq!(err.code, "pdf_page_too_large", "{}", err.message);
                assert!(!out.exists());
            }
        }
        // The other page of the two is fine on its own.
        let err = render_page(&absent, &second, &td.path().join("p1"), 203, 1, None).unwrap_err();
        assert_eq!(err.code, "pdf_render", "{}", err.message);

        // End to end, with the real renderer: an error, not a 2 GB bitmap.
        if !renderers().is_empty() {
            let huge_box = json!({"zpl_fit": "width", "zpl_max_width_dots": 32000,
                "zpl_max_height_dots": 32000});
            for o in [
                json!({"zpl_fit": "none"}),
                json!({"zpl_fit": "none", "zpl_page": 1}),
                huge_box,
            ] {
                let err = image_path_to_zpl(&huge, &opts(o.clone())).unwrap_err();
                assert_eq!(err.code, "pdf_page_too_large", "{o}");
                assert!(is_permanent(&err));
            }
        }
    }

    #[test]
    fn rendered_pages_are_checked_before_decoding() {
        let td = tempfile::tempdir().unwrap();
        let png = |name: &str, w: u32, h: u32| {
            let p = td.path().join(name);
            GrayImage::from_pixel(w, h, Luma([255])).save(&p).unwrap();
            p
        };
        let pdftoppm = Renderer::Pdftoppm("pdftoppm".into());
        let gs = Renderer::Ghostscript("gs".into());
        let label = png("label.png", 812, 1218);
        let blank = png("blank.png", 1, 1);

        // pdftoppm drew the size pdfinfo gave, either way round, give or
        // take its rounding.
        for want in [(812, 1218), (1218, 812), (810, 1220)] {
            check_rendered(&pdftoppm, &label, 1, Some(want), "").unwrap();
        }
        check_rendered(&pdftoppm, &label, 1, None, "").unwrap();
        // Its 1×1 "could not allocate" page is a failed render.
        let err =
            check_rendered(&pdftoppm, &blank, 2, Some((812, 1218)), "Out of memory\n").unwrap_err();
        assert_eq!(err.code, "pdf_render");
        assert_eq!(
            err.message,
            "pdftoppm rendered page 2 at 1 x 1 pixels, not 812 x 1218: Out of memory"
        );
        assert!(is_permanent(&err));
        let err = check_rendered(&pdftoppm, &blank, 1, None, "").unwrap_err();
        assert_eq!(err.message, "pdftoppm rendered page 1 at 1 x 1 pixels");
        let err = check_rendered(&pdftoppm, &label, 1, Some((830, 1218)), "").unwrap_err();
        assert_eq!(err.code, "pdf_render");
        // gs never draws that page, and a /UserUnit makes its size differ.
        check_rendered(&gs, &blank, 1, Some((812, 1218)), "").unwrap();
        // An unreadable PNG is left to the decoder (image_bad).
        let junk = td.path().join("junk.png");
        fs::write(&junk, b"not a png").unwrap();
        check_rendered(&pdftoppm, &junk, 1, Some((812, 1218)), "").unwrap();

        // Over the budget once drawn (gs and a /UserUnit, or no pdfinfo):
        // refused from the header, whatever the renderer.
        let big = td.path().join("big.png");
        fs::write(&big, png_header(10_000, 5_001)).unwrap();
        for r in [&pdftoppm, &gs] {
            let err = check_rendered(r, &big, 4, None, "").unwrap_err();
            assert_eq!(err.code, "pdf_page_too_large", "{r:?}");
            assert_eq!(
                err.message,
                "PDF page 4 rendered at 10000 x 5001 pixels, more than the 50000000 a page may have"
            );
        }
    }

    /// An 8-bit gray PNG that claims `w`×`h` pixels but holds no image data:
    /// its header is all a size check reads.
    fn png_header(w: u32, h: u32) -> Vec<u8> {
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
        ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        chunk(&mut png, b"IHDR", &ihdr);
        chunk(&mut png, b"IDAT", b"\x78\x9c\x03\x00");
        chunk(&mut png, b"IEND", &[]);
        png
    }

    /// Install `script` as the executable `dir/name`; returns its path.
    ///
    /// A child `cp` writes it, so this process never holds it open for
    /// writing: a child another test forks meanwhile would inherit that
    /// descriptor, and running the script would then fail with ETXTBSY.
    fn install_script(dir: &Path, name: &str, script: &str) -> String {
        use std::os::unix::fs::PermissionsExt as _;
        let source = dir.join(format!("{name}.sh"));
        fs::write(&source, script).unwrap();
        let tool = dir.join(name);
        let (src, dst) = (source.display().to_string(), tool.display().to_string());
        let cp = run_with_timeout("cp", &[&src, &dst], Duration::from_secs(30)).unwrap();
        assert!(cp.success, "{}", cp.stderr);
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        dst
    }

    /// A stand-in pdftoppm that cannot allocate the page bitmap: like the
    /// real one it then writes a 1×1 PNG, says "Out of memory" and exits 0.
    fn out_of_memory_pdftoppm(dir: &Path) -> Renderer {
        let one = dir.join("one.png");
        image::RgbImage::from_pixel(1, 1, image::Rgb([255, 255, 255]))
            .save(&one)
            .unwrap();
        let script = format!(
            "#!/bin/sh\n\
             for last; do :; done\n\
             case \" $* \" in *' -singlefile '*) out=\"$last.png\" ;; *) out=\"$last-1.png\" ;; esac\n\
             cp '{}' \"$out\"\n\
             echo 'Out of memory' >&2\n",
            one.display()
        );
        Renderer::Pdftoppm(install_script(dir, "pdftoppm", &script))
    }

    /// `renderer` through a script that logs the arguments of each run, a
    /// line each, to `dir/runs.log`.
    fn logged(dir: &Path, renderer: &Renderer) -> Renderer {
        let (Renderer::Pdftoppm(tool) | Renderer::Ghostscript(tool)) = renderer;
        let log = dir.join("runs.log").display().to_string();
        let script = format!("#!/bin/sh\necho \"$*\" >> '{log}'\nexec '{tool}' \"$@\"\n");
        let tool = install_script(dir, "renderer", &script);
        match renderer {
            Renderer::Pdftoppm(_) => Renderer::Pdftoppm(tool),
            Renderer::Ghostscript(_) => Renderer::Ghostscript(tool),
        }
    }

    /// `(first page, last page, dpi)` of each run [`logged`] logged in
    /// `dir`; the log starts over.
    fn logged_runs(dir: &Path) -> Vec<(i64, i64, i64)> {
        let log = dir.join("runs.log");
        let runs = fs::read_to_string(&log).unwrap_or_default();
        let _ = fs::remove_file(&log);
        runs.lines()
            .map(|line| {
                let args: Vec<&str> = line.split(' ').collect();
                // pdftoppm's `-f 2`, gs's `-dFirstPage=2`.
                let arg = |flag: &str, prefix: &str| -> i64 {
                    let value = match args.iter().position(|a| *a == flag) {
                        Some(i) => args[i + 1],
                        None => args.iter().find_map(|a| a.strip_prefix(prefix)).unwrap(),
                    };
                    value.parse().unwrap()
                };
                (
                    arg("-f", "-dFirstPage="),
                    arg("-l", "-dLastPage="),
                    arg("-r", "-r"),
                )
            })
            .collect()
    }

    #[test]
    fn a_page_pdftoppm_could_not_allocate_fails_instead_of_printing_blank() {
        let td = tempfile::tempdir().unwrap();
        let oom = out_of_memory_pdftoppm(td.path());
        let pdf = write_pdf(
            td.path(),
            "label.pdf",
            &pdf_with_boxes(288, 432, &[(36, 36, 72, 36)]),
        );
        // Sized by pdfinfo when it is installed; else the 1×1 alone tells.
        // Scaled onto the label or not, it is not drawn again.
        let info = PdfInfo::read(&pdf, 1, MAX_PDF_PAGES + 1).unwrap_or_default();
        let label = Some((812, 1186));
        let all = pdf_all_pages_to_png(&oom, &pdf, &td.path().join("all"), 203, &info, label);
        let one = render_page(&oom, &pdf, &td.path().join("one"), 203, 1, None);
        for err in [all.unwrap_err(), one.unwrap_err()] {
            assert_eq!(err.code, "pdf_render", "{}", err.message);
            assert!(
                err.message.contains("rendered page 1 at 1 x 1 pixels"),
                "{}",
                err.message
            );
            assert!(err.message.ends_with(": Out of memory"), "{}", err.message);
        }
    }

    /// gs scales a page by its /UserUnit, which pdfinfo (and pdftoppm) never
    /// see: such a page is checked once rendered. At its own size it fails;
    /// scaled onto the label, it is drawn again within the budget.
    #[test]
    fn gs_user_unit_pages_are_checked_once_rendered() {
        let Some(gs) = pdf_tool("gs") else {
            return;
        };
        let td = tempfile::tempdir().unwrap();
        // 4×6 in pt, but 25 times that in user units: 7200 × 10800 at 72 dpi
        // for gs.
        let pdf = write_pdf(
            td.path(),
            "uu.pdf",
            &pdf_with_page_entries(288, 432, " /UserUnit 25", &["0 g 10 10 200 300 re f"]),
        );
        let info = PdfInfo::read(&pdf, 1, MAX_PDF_PAGES + 1).unwrap_or_default();
        let r = Renderer::Ghostscript(gs);
        let all = td.path().join("all");
        let err = pdf_all_pages_to_png(&r, &pdf, &all, 72, &info, None).unwrap_err();
        assert_eq!(err.code, "pdf_page_too_large", "{}", err.message);
        assert!(
            err.message.contains("7200 x 10800 pixels"),
            "{}",
            err.message
        );
        let err = render_page(&r, &pdf, &td.path().join("one"), 72, 1, None).unwrap_err();
        assert_eq!(err.code, "pdf_page_too_large", "{}", err.message);

        // Into a 4×6 label box: drawn again at 57 dpi, the most within the
        // budget.
        let label = Some((812, 1186));
        let all = td.path().join("all-fitted");
        let pages = pdf_all_pages_to_png(&r, &pdf, &all, 72, &info, label).unwrap();
        assert_eq!(pages.len(), 1);
        let one = render_page(&r, &pdf, &td.path().join("one-fitted"), 72, 1, label).unwrap();
        for page in [&pages[0], &one] {
            assert_eq!(page.dpi, 57, "{page:?}");
            assert_eq!(png_dimensions(&page.png), Some((5700, 8550)), "{page:?}");
        }
    }

    /// A page over the budget at the job's dpi that is scaled onto the label
    /// anyway is drawn at the highest dpi within the budget: A0 at 203 dpi
    /// (63.9 MP), Tabloid and A3 at 600 dpi (67.3 and 69.6 MP), a 9000 pt
    /// page. At its own size it is refused as before.
    #[test]
    fn pages_over_the_budget_scaled_onto_the_label_are_drawn_at_a_lower_dpi() {
        let label = Some((812, 1186));
        let label_600 = Some((2454, 3568));
        // Within the budget: the job's dpi, scaled or not.
        for fitted in [None, label] {
            assert_eq!(
                page_resolution(1, 288.0, 432.0, 203, fitted).unwrap(),
                (203, (812, 1218))
            );
            assert_eq!(
                page_resolution(1, 612.0, 1008.0, 600, fitted).unwrap(),
                (600, (5100, 8400))
            );
        }
        for (w, h, dpi, fitted, want) in [
            (2384.0, 3370.0, 203, label, (179, (5927, 8379))),
            (792.0, 1224.0, 600, label_600, (517, (5687, 8789))),
            (842.0, 1191.0, 600, label_600, (508, (5941, 8404))),
            (9000.0, 9000.0, 203, label, (56, (7000, 7000))),
            // A banner covers the box with its width alone.
            (14400.0, 72.0, 600, label_600, (500, (100_000, 500))),
        ] {
            let (res, (pw, ph)) = page_resolution(2, w, h, dpi, fitted).unwrap();
            assert_eq!((res, (pw, ph)), want, "{w} x {h} pt at {dpi} dpi");
            assert!(pw * ph <= MAX_PAGE_PIXELS);
            // One dpi more would be over it.
            let (pw, ph) = drawn_size(w, h, (res + 1) as f64);
            assert!(pw * ph > MAX_PAGE_PIXELS as f64, "{w} x {h} pt");
            let err = page_resolution(2, w, h, dpi, None).unwrap_err();
            assert_eq!(err.code, "pdf_page_too_large");
            let unchanged = check_page_budget(2, w, h, dpi).unwrap_err();
            assert_eq!(err.message, unchanged.message);
        }
        // Refused too where no dpi within the budget covers the label box,
        // and for a broken box.
        for (w, h, fitted) in [
            (2384.0, 3370.0, Some((32_000, 32_000))),
            (9000.0, 9000.0, Some((32_000, 32_000))),
            (f64::NAN, 432.0, label),
            (f64::INFINITY, 432.0, label),
        ] {
            let err = page_resolution(3, w, h, 203, fitted).unwrap_err();
            assert_eq!(err.code, "pdf_page_too_large", "{w} x {h} pt");
            assert!(err.message.starts_with("PDF page 3 is "), "{}", err.message);
            assert!(is_permanent(&err));
        }
    }

    /// A page drawn below the job's dpi keeps its size on the label: with
    /// `zpl_fit` contain it is brought to the size it has at the job's dpi,
    /// and only scaled down from there; width fills the box as ever.
    #[test]
    fn a_page_drawn_below_the_jobs_dpi_keeps_its_size_on_the_label() {
        // A 100 × 150 design drawn at half the job's dpi, and the same
        // design at the job's dpi.
        let half = GrayImage::from_pixel(100, 150, Luma([0]));
        let full = GrayImage::from_pixel(200, 300, Luma([0]));
        let mono = |img: &GrayImage, fit, native, (w, h): (i64, i64)| {
            to_mono_label(img.clone(), w, Some(h), 128, false, fit, native)
                .unwrap()
                .dimensions()
        };
        for fit in [Fit::Contain, Fit::Width] {
            for label in [(812, 1186), (152, 1186), (812, 240)] {
                assert_eq!(
                    mono(&half, fit, 2.0, label),
                    mono(&full, fit, 1.0, label),
                    "{fit:?} into {label:?}"
                );
            }
        }
        assert_eq!(mono(&half, Fit::Contain, 2.0, (812, 1186)), (200, 300));
        assert_eq!(mono(&half, Fit::Contain, 1.0, (812, 1186)), (104, 150));
    }

    /// An A0 page (2384 × 3370 pt) with a 3 × 4 in label in its top left
    /// corner, which lands on whole pixels at any dpi.
    const A0_LABEL: (u32, u32, &str) = (2384, 3370, "0 g 0 3082 216 288 re f");

    /// The label of [`A0_LABEL`] on a 4×6 page of its own.
    const PAGE_LABEL: (u32, u32, &str) = (288, 432, "0 g 0 144 216 288 re f");

    /// The A0 page with [`A0_LABEL`] (`a0.pdf`) converts to a label of the
    /// size the 4×6 page with [`PAGE_LABEL`] does, `width` dots wide, with
    /// `zpl_fit` `fit` on a 203 dpi queue.
    fn assert_a0_label_size(dir: &Path, fit: &str, width: i64) {
        let page = write_pdf(dir, "page.pdf", &pdf_with_pages(&[PAGE_LABEL], ""));
        let o = opts(json!({"cups_name": "Zebra_ZD421-203dpi_ZPL", "zpl_fit": fit}));
        let big = image_path_to_zpl(&dir.join("a0.pdf"), &o).unwrap();
        let small = image_path_to_zpl(&page, &o).unwrap();
        assert_eq!(label_widths(&big), [width], "{fit}");
        assert_eq!(label_widths(&big), label_widths(&small), "{fit}");
        assert_eq!(label_lengths(&big), label_lengths(&small), "{fit}");
        assert_eq!(label_fields(&big), label_fields(&small), "{fit}");
    }

    /// An A0 page on a 203 dpi queue (63.9 MP there), fitted to the width
    /// as Python printed it (the page budget refused it): drawn at 179 dpi,
    /// within the budget, it prints the label a 4×6 page with the same
    /// design prints. At its own size it is still refused.
    #[test]
    fn an_a0_page_fitted_to_the_width_prints_a_normal_label() {
        if pdf_tool("pdfinfo").is_none() {
            return;
        }
        let td = tempfile::tempdir().unwrap();
        let a0 = write_pdf(td.path(), "a0.pdf", &pdf_with_pages(&[A0_LABEL], ""));
        let info = PdfInfo::read(&a0, 1, MAX_PDF_PAGES + 1).unwrap();
        // The 4×6 box at 203 dpi, below the 32-dot top margin.
        let label = Some((812, 1186));
        for renderer in renderers() {
            let dir = td
                .path()
                .join(format!("{renderer:?}").replace(['"', '/', ' '], "_"));
            let all = pdf_all_pages_to_png(&renderer, &a0, &dir.join("all"), 203, &info, label);
            let one = render_page(&renderer, &a0, &dir.join("one"), 203, 1, label).unwrap();
            for page in [&all.unwrap()[0], &one] {
                assert_eq!(page.dpi, 179, "{renderer:?}");
                // pdftoppm rounds up, gs rounds.
                let (w, h) = png_dimensions(&page.png).unwrap();
                assert!(w == 5927 && (8378..=8379).contains(&h), "{w} x {h}");
                assert!(u64::from(w) * u64::from(h) <= MAX_PAGE_PIXELS);
            }
        }
        if renderers().is_empty() {
            return;
        }
        assert_a0_label_size(td.path(), "width", 816);
        let native = json!({"cups_name": "Zebra_ZD421-203dpi_ZPL", "zpl_fit": "none"});
        let err = image_path_to_zpl(&a0, &opts(native)).unwrap_err();
        assert_eq!(err.code, "pdf_page_too_large");
    }

    /// With `zpl_fit` contain, the default, the label on the A0 page drawn
    /// at 179 dpi prints at the size it has at 203 dpi, not 179/203 of it.
    #[test]
    fn a_label_on_an_a0_page_keeps_its_size() {
        if renderers().is_empty() || pdf_tool("pdfinfo").is_none() {
            return;
        }
        let td = tempfile::tempdir().unwrap();
        write_pdf(td.path(), "a0.pdf", &pdf_with_pages(&[A0_LABEL], ""));
        assert_a0_label_size(td.path(), "contain", 616);
    }

    /// A page drawn below the job's dpi gets a tool run of its own, before
    /// any is drawn at the job's dpi: the pages around it are drawn exactly
    /// as in a document without it, which takes a single run.
    #[test]
    fn a_page_over_the_budget_is_drawn_in_a_run_of_its_own() {
        if pdf_tool("pdfinfo").is_none() {
            return;
        }
        let td = tempfile::tempdir().unwrap();
        // 4×6 pages with a 1, 2 and 3 in wide box, and A0 as page 2.
        let small = [
            "0 g 36 36 72 36 re f",
            "0 g 36 36 144 36 re f",
            "0 g 36 36 216 36 re f",
        ];
        let pages = [
            (288, 432, small[0]),
            A0_LABEL,
            (288, 432, small[1]),
            (288, 432, small[2]),
        ];
        let mixed = write_pdf(td.path(), "mixed.pdf", &pdf_with_pages(&pages, ""));
        let without = write_pdf(td.path(), "small.pdf", &pdf_with_content(288, 432, &small));
        let label = Some((812, 1186));
        for (i, renderer) in renderers().iter().enumerate() {
            let out = td.path().join(i.to_string());
            fs::create_dir_all(&out).unwrap();
            let renderer = logged(&out, renderer);
            let draw = |pdf: &Path, dir: &str| {
                let info = PdfInfo::read(pdf, 1, MAX_PDF_PAGES + 1).unwrap();
                pdf_all_pages_to_png(&renderer, pdf, &out.join(dir), 203, &info, label).unwrap()
            };
            let with = draw(&mixed, "mixed");
            let runs = [(1, 1, 203), (2, 2, 179), (3, 4, 203)];
            assert_eq!(logged_runs(&out), runs, "{renderer:?}");
            let without = draw(&without, "small");
            assert_eq!(logged_runs(&out), [(1, 3, 203)], "{renderer:?}");

            let dpis: Vec<i64> = with.iter().map(|p| p.dpi).collect();
            assert_eq!(dpis, [203, 179, 203, 203], "{renderer:?}");
            assert_eq!(png_dimensions(&with[1].png).map(|(w, _)| w), Some(5927));
            let png = |p: &DrawnPage| fs::read(&p.png).unwrap();
            for (i, j) in [(0, 0), (2, 1), (3, 2)] {
                assert!(
                    png(&with[i]) == png(&without[j]),
                    "{renderer:?}: page {} differs",
                    i + 1
                );
            }
        }
    }

    /// The conversion `to_pil_luma` replaced: every image copied to 8-bit
    /// RGB(A) first.
    fn luma_by_copy(img: &image::DynamicImage) -> GrayImage {
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

    #[test]
    fn luma_without_copies_matches_the_copying_conversion() {
        let (w, h) = (37, 23);
        let rgba = image::RgbaImage::from_raw(w, h, noise((w * h * 4) as usize, 3)).unwrap();
        let base = image::DynamicImage::ImageRgba8(rgba);
        // Every pixel format a decoder hands over.
        let variants = [
            image::DynamicImage::ImageLuma8(base.to_luma8()),
            image::DynamicImage::ImageLumaA8(base.to_luma_alpha8()),
            image::DynamicImage::ImageRgb8(base.to_rgb8()),
            base.clone(),
            image::DynamicImage::ImageLuma16(base.to_luma16()),
            image::DynamicImage::ImageLumaA16(base.to_luma_alpha16()),
            image::DynamicImage::ImageRgb16(base.to_rgb16()),
            image::DynamicImage::ImageRgba16(base.to_rgba16()),
            image::DynamicImage::ImageRgb32F(base.to_rgb32f()),
            image::DynamicImage::ImageRgba32F(base.to_rgba32f()),
        ];
        for img in variants {
            let color = img.color();
            assert_eq!(to_pil_luma(img.clone()), luma_by_copy(&img), "{color:?}");
        }
        // Gray is its own luma: every level maps to itself.
        let levels = GrayImage::from_fn(256, 1, |x, _| Luma([x as u8]));
        assert_eq!(
            to_pil_luma(image::DynamicImage::ImageLuma8(levels.clone())),
            levels
        );
    }

    #[test]
    fn images_over_pillows_pixel_limit_fail_from_their_header() {
        let td = tempfile::tempdir().unwrap();
        let open = |name: &str, bytes: &[u8]| {
            let p = td.path().join(name);
            fs::write(&p, bytes).unwrap();
            open_gray(&p)
        };
        // 21146² 8-bit gray (447 MP): what gs renders of a 7500 pt page at
        // 203 dpi. The image crate's 512 MiB buffer cap would decode it.
        let err = open("bomb.png", &png_header(21_146, 21_146)).unwrap_err();
        assert_eq!(err.code, "image_bad");
        assert_eq!(
            err.message,
            "open image failed: image is 21146 x 21146 pixels, more than the 178956970 an \
             image may have"
        );
        // Just over and just under the limit: the second is decoded (and
        // fails on its empty image data, not on its size).
        let err = open("over.png", &png_header(13_378, 13_378)).unwrap_err();
        assert!(
            err.message.contains("13378 x 13378 pixels"),
            "{}",
            err.message
        );
        let err = open("under.png", &png_header(13_377, 13_377)).unwrap_err();
        assert_eq!(err.code, "image_bad");
        assert!(!err.message.contains("pixels"), "{}", err.message);

        // A damaged PNG is repaired and checked again.
        let mut damaged = png_header(21_146, 21_146);
        damaged[29] ^= 0xFF; // IHDR CRC
        let unrepaired = ImageReader::with_format(Cursor::new(&damaged), ImageFormat::Png);
        assert!(unrepaired.into_dimensions().is_err());
        let err = open("damaged.png", &damaged).unwrap_err();
        assert!(
            err.message.contains("21146 x 21146 pixels"),
            "{}",
            err.message
        );
    }

    #[test]
    fn huge_label_boxes_fail_instead_of_aborting() {
        let td = tempfile::tempdir().unwrap();
        let label = test_label("png");
        let png = tiny_png(td.path());
        // Each had the resize fill a box tens of thousands of dots square:
        // gigabytes, and a failed allocation aborted the whole agent (it is
        // no panic a job can contain).
        for (path, o) in [
            (
                &label,
                json!({"zpl_fit": "width", "zpl_max_width_dots": 200000, "zpl_max_height_dots": 200000}),
            ),
            (
                &label,
                json!({"zpl_fit": "width", "label_width_dots": 60000, "label_height_dots": "60000"}),
            ),
            (
                &png,
                json!({"zpl_fit": "width", "zpl_max_width_dots": 1e300, "zpl_max_height_dots": 1e300}),
            ),
        ] {
            let err = image_path_to_zpl(path, &opts(o.clone())).unwrap_err();
            assert_eq!(err.code, "label_too_large", "{o}");
            assert!(
                err.message.contains("zpl_max_width_dots"),
                "{}",
                err.message
            );
            assert!(is_permanent(&err));
        }
        // A huge box on one side only changes nothing for a normal label: the
        // other side still bounds it.
        let default = image_path_to_zpl(&label, &JsonObject::new()).unwrap();
        let wide = image_path_to_zpl(&label, &opts(json!({"label_width_dots": 200000}))).unwrap();
        assert_eq!(wide, default);
        let tall = json!({"zpl_fit": "width", "zpl_max_height_dots": 200000});
        let bounded = json!({"zpl_fit": "width", "zpl_max_height_dots": 32000});
        assert_eq!(
            image_path_to_zpl(&label, &opts(tall)).unwrap(),
            image_path_to_zpl(&label, &opts(bounded)).unwrap()
        );

        // A box past ZPL's 32000 dots is clamped to them: a bar fitted to the
        // width makes a label the full 32000 dots long or wide, as in a
        // 32000-dot box. Unclamped, either bar would be 80 M dots
        // (label_too_large).
        let bar = |name: &str, w: u32, h: u32| {
            let p = td.path().join(name);
            GrayImage::from_pixel(w, h, Luma([0])).save(&p).unwrap();
            p
        };
        let huge = opts(json!({
            "zpl_fit": "width", "zpl_max_width_dots": 200000, "zpl_max_height_dots": 200000
        }));
        let full = opts(json!({
            "zpl_fit": "width", "zpl_max_width_dots": 32000, "zpl_max_height_dots": 32000
        }));
        let wide_bar = bar("wide.png", 500, 1);
        let zpl = image_path_to_zpl(&wide_bar, &huge).unwrap();
        assert_eq!(zpl, image_path_to_zpl(&wide_bar, &full).unwrap());
        assert_eq!(label_widths(&zpl), [32000]);
        // 31968 dots of bar below the 32-dot top margin.
        let tall_bar = bar("tall.png", 1, 500);
        let zpl = image_path_to_zpl(&tall_bar, &huge).unwrap();
        assert_eq!(zpl, image_path_to_zpl(&tall_bar, &full).unwrap());
        assert_eq!(label_lengths(&zpl), [32000]);
        assert_eq!(label_fields(&zpl), [(0, 32, 8)]);
    }

    #[test]
    fn label_size_limits() {
        // The largest normal resize: US Legal at 600 dpi onto a 4×6 label
        // (291 MB of float buffer), and the 600 dpi label itself.
        check_label_size((5100, 8400), (2166, 3568)).unwrap();
        check_label_size((2400, 3600), (2454, 3681)).unwrap();
        // Unscaled (or a resize that rounds back to the same size, a plain
        // copy): only the label's own size counts, though resampling this
        // width would take 720 MB.
        check_label_size((9000, 5000), (9000, 5000)).unwrap();

        let err = check_label_size((7072, 7072), (7072, 7072)).unwrap_err();
        assert_eq!(err.code, "label_too_large");
        assert!(
            err.message.starts_with(
                "label graphic would be 7072 x 7072 dots, more than the 50000000 a label may have"
            ),
            "{}",
            err.message
        );
        // A wide image scaled onto a long label: 9000 × 3800 × 16 B = 547 MB.
        let err = check_label_size((9000, 13000), (2454, 3800)).unwrap_err();
        assert_eq!(err.code, "label_too_large");
        assert!(
            err.message.starts_with(
                "scaling the 9000 x 13000 image to 2454 x 3800 dots needs 521 MiB, more than \
                 the 512 MiB a resize may use"
            ),
            "{}",
            err.message
        );
        // Nothing overflows on absurd sizes.
        check_label_size((u32::MAX, u32::MAX), (i64::MAX, i64::MAX)).unwrap_err();
    }

    #[test]
    fn label_length_and_offsets_stay_in_zpl_range() {
        let gfa = Gfa {
            hex: "80".into(),
            total_bytes: 1,
            bytes_per_row: 1,
            height: 1,
        };
        // h + y overflowed: a panic in debug builds, a negative ^LL in release.
        for (h, y) in [(8, i64::MAX), (i64::MAX, 32), (i64::MAX, i64::MAX)] {
            let zpl = build_zpl_label(&gfa, Some(h), 0, y, 1);
            let ll = label_lengths(&zpl)[0];
            assert!((1..=32000).contains(&ll), "{h} + {y}: ^LL{ll}");
            let (_, fo_y, _) = label_fields(&zpl)[0];
            assert!(fo_y <= 32000, "{zpl}");
        }
        assert!(build_zpl_label(&gfa, Some(1218), 0, 32000, 1).contains("^LL32000"));
        // x past the range too; a negative offset is left alone.
        let zpl = build_zpl_label(&gfa, Some(8), i64::MAX, i64::MIN, 1);
        assert_eq!(label_widths(&zpl), [32000]);
        assert_eq!(label_lengths(&zpl), [8]);
        assert!(zpl.contains(&format!("^FO32000,{}^GFA", i64::MIN)), "{zpl}");

        // From job options: past 32000 dots is refused (1e300 saturated to
        // i64::MAX), up to it is printed.
        let td = tempfile::tempdir().unwrap();
        let png = tiny_png(td.path());
        for o in [
            json!({"zpl_y": 1e300}),
            json!({"zpl_y": "9223372036854775807"}),
            json!({"zpl_y": 40000}),
            json!({"zpl_x": 32001}),
            json!({"zpl_x": 1e300}),
        ] {
            let err = image_path_to_zpl(&png, &opts(o.clone())).unwrap_err();
            assert_eq!(err.code, "zpl_error", "{o}");
            assert!(
                err.message.contains("past ZPL's 32000-dot range"),
                "{}",
                err.message
            );
            assert!(is_permanent(&err));
        }
        let zpl = image_path_to_zpl(&png, &opts(json!({"zpl_y": 32000}))).unwrap();
        assert_eq!(label_lengths(&zpl), [32000]);
        assert_eq!(label_fields(&zpl)[0].1, 32000);
        let zpl = image_path_to_zpl(&png, &opts(json!({"zpl_y": -1e300}))).unwrap();
        assert_eq!(label_lengths(&zpl), [1]);
    }

    #[test]
    fn x_offset_widens_the_print_width_instead_of_clipping() {
        let gfa = Gfa {
            hex: "80".into(),
            total_bytes: 1,
            bytes_per_row: 1,
            height: 1,
        };
        assert_eq!(
            label_widths(&build_zpl_label(&gfa, Some(16), 20, 0, 1)),
            [28]
        );
        assert_eq!(label_widths(&build_zpl_label(&gfa, Some(16), 0, 0, 1)), [8]);
        assert_eq!(
            label_widths(&build_zpl_label(&gfa, Some(16), -5, 0, 1)),
            [8]
        );

        // A full-width 4×6 design (812 dots): shifted 20 dots right it is
        // fitted into the 792 left, so ^PW covers it and stays on the media.
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("full.png");
        let mut img = GrayImage::from_pixel(812, 1186, Luma([255]));
        for y in 0..1186 {
            img.put_pixel(0, y, Luma([0]));
            img.put_pixel(811, y, Luma([0]));
        }
        img.save(&p).unwrap();
        let unshifted = image_path_to_zpl(&p, &JsonObject::new()).unwrap();
        assert_eq!(label_widths(&unshifted), [816]);
        let zpl = image_path_to_zpl(&p, &opts(json!({"zpl_x": 20}))).unwrap();
        let (pw, (x, _, row)) = (label_widths(&zpl)[0], label_fields(&zpl)[0]);
        assert_eq!(x, 20);
        assert!(
            x + row * 8 <= pw,
            "graphic {x}+{} dots past ^PW{pw}",
            row * 8
        );
        assert!(pw <= 812, "^PW{pw} wider than the 4\" media");

        // A negative offset leaves the box alone, as it always has: a dot
        // fitted to the width fills the same 812 dots, and only ^FO moves.
        let dot = tiny_png(td.path());
        let fitted = |x: i64| {
            image_path_to_zpl(&dot, &opts(json!({"zpl_fit": "width", "zpl_x": x}))).unwrap()
        };
        assert_eq!(fitted(-5), fitted(0).replace("^FO0,32^", "^FO-5,32^"));
    }

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
        // Another release (as on CI's runner) is skipped, but not none.
        let Some(tool) = pdf_tool("pdftoppm") else {
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
