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
use std::path::{Path, PathBuf};
use std::time::Duration;

use image::imageops::FilterType;
use image::{GrayImage, ImageReader, Luma};
use serde_json::Value;

use crate::printers::{run_with_timeout, which};
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
        let msg = if !out.stderr.trim().is_empty() {
            out.stderr
        } else {
            out.stdout
        };
        let msg = msg.trim();
        return Err(ZplError::new(
            if msg.is_empty() {
                format!("{tool} failed")
            } else {
                msg.to_string()
            },
            "pdf_render",
        ));
    }
    Ok(png)
}

fn max(a: i64, b: i64) -> i64 {
    a.max(b)
}

fn dpi_from_name(name: &str, dpi: i64) -> i64 {
    if name.contains("300dpi") {
        300
    } else if name.contains("203dpi") || name.contains("200dpi") {
        203
    } else {
        dpi
    }
}

/// Guess printable width from the CUPS queue name (model / dpi token).
///
/// Default is **4×6** (812 dots @ 203 dpi).
pub fn infer_media_width_dots(cups_name: Option<&str>, dpi: i64) -> i64 {
    let name = cups_name.unwrap_or_default().to_lowercase();
    let dpi = dpi_from_name(&name, dpi);
    // ZD220 / ZD230 / ZD421 / ZD621 are 4" desktop printers (not 2").
    const FOUR_INCH: &[&str] = &[
        "zd220", "zd230", "zd421", "zd621", "zt410", "zt411", "gk420", "gx430",
    ];
    if FOUR_INCH.iter().any(|t| name.contains(t)) {
        return if dpi <= 203 {
            812
        } else {
            round_half_even(4.09 * dpi as f64)
        };
    }
    DEFAULT_MAX_WIDTH_DOTS
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

/// Grayscale like Pillow `convert("L")` (ITU-R 601-2, alpha ignored).
fn to_pil_luma(img: &image::DynamicImage) -> GrayImage {
    let rgb = img.to_rgb8();
    GrayImage::from_fn(rgb.width(), rgb.height(), |x, y| {
        let p = rgb.get_pixel(x, y).0;
        let l = (p[0] as u32 * 19595 + p[1] as u32 * 38470 + p[2] as u32 * 7471 + 0x8000) >> 16;
        Luma([l as u8])
    })
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

fn open_gray(path: &Path) -> Result<GrayImage, ZplError> {
    let img = ImageReader::open(path)
        .and_then(|r| r.with_guessed_format())
        .map_err(|e| ZplError::new(format!("open image failed: {e}"), "image_bad"))?
        .decode()
        .map_err(|e| ZplError::new(format!("open image failed: {e}"), "image_bad"))?;
    Ok(to_pil_luma(&img))
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
pub fn image_path_to_zpl(path: &Path, opts: &JsonObject) -> Result<String, ZplError> {
    let dpi = opt_int(opts, "zpl_dpi", DEFAULT_DPI);
    let cups = opts
        .get("cups_name")
        .filter(|v| truthy(v))
        .map(py_str)
        .unwrap_or_default();
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
    let img = if is_pdf {
        // DPI only affects PDF rasterization.
        let work = tempfile::Builder::new()
            .prefix("vesyl-zpl-pdf-")
            .tempdir()
            .map_err(|e| ZplError::new(e.to_string(), "pdf_render"))?;
        let png = pdf_to_png(path, work.path(), dpi, opt_int(opts, "zpl_page", 1))?;
        load_image_as_mono(&png, max_w, Some(max_h), thr, invert, fit)?
    } else {
        load_image_as_mono(path, max_w, Some(max_h), thr, invert, fit)?
    };

    let gfa = mono_image_to_gfa_hex(&img);
    log::info!(
        target: LOG,
        "ZPL graphic {}x{} dots, {} bytes/row, {} total (from {})",
        img.width(),
        gfa.height,
        gfa.bytes_per_row,
        gfa.total_bytes,
        path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
    );
    // Quantity is handled by lp -n, so ^PQ stays 1.
    Ok(build_zpl_label(&gfa, Some(gfa.height as i64), x, y, 1))
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

pub fn is_graphic_path(path: &Path) -> bool {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .is_some_and(|e| matches!(e.as_str(), "pdf" | "png" | "jpg" | "jpeg" | "bmp" | "gif"))
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
}
