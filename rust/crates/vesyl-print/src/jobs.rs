//! Local print jobs: durable queue, content fetch, CUPS submit + completion.
//!
//! Ordering (at-least-once, crash-safe):
//!
//! 1. If processed/<job_id> exists → already finished (idempotent), skip print
//! 2. If no queue file → write + fsync full job JSON to queue/<job_id>.json
//! 3. Only then call ack callback (when cloud supports it)
//! 4. Materialize content → lp -d <cups_name> (`-o raw` for raw_*/ZPL)
//! 5. Report **delivered** when lp accepts the job
//! 6. Optionally poll CUPS → report **printed** or **error**
//!    (`WaitCups::Async` does this in the background so the next job can
//!    be `lp`'d immediately; CUPS FIFO keeps page order on one queue)
//! 7. On success path: write processed/<job_id>, delete queue file
//!
//! On agent start: [`Pipeline::drain`] recovers queue/*.json left from crashes.
//!
//! `raw_uri` / `raw_base64` write a temp `.zpl`/`.raw` file (no PDF/PNG
//! magic sniff) and submit with `lp -o raw`. PDF/PNG/JPEG sent to a raw Zebra
//! queue are converted to ZPL `^GFA` graphics (see [`crate::zpl`]).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use regex::Regex;
use serde_json::{json, Value};

use crate::config::WaitCups;
use crate::printers::run_with_timeout;
use crate::util::{py_int, py_str, truthy, utc_now_iso, write_durable};
use crate::{zpl, BoxError, JsonObject};

const LOG: &str = "vesyl-print.jobs";

const RAW_TYPES: &[&str] = &["raw_uri", "raw_base64"];

/// content_type → declared file suffix for temp materialization.
fn declared_suffix(content_type: &str) -> Option<&'static str> {
    Some(match content_type {
        "pdf_uri" | "pdf_base64" => ".pdf",
        "png_uri" | "png_base64" => ".png",
        "jpeg_uri" | "jpeg_base64" | "jpg_uri" | "jpg_base64" => ".jpg",
        "raw_uri" | "raw_base64" => ".raw",
        _ => return None,
    })
}

fn is_supported(content_type: &str) -> bool {
    content_type == "local_path" || declared_suffix(content_type).is_some()
}

/// Print / queue failure with a short machine-friendly code.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct JobError {
    pub message: String,
    pub code: String,
}

impl JobError {
    pub fn new(message: impl Into<String>, code: &str) -> Self {
        JobError {
            message: message.into(),
            code: code.into(),
        }
    }
}

impl From<zpl::ZplError> for JobError {
    fn from(e: zpl::ZplError) -> Self {
        JobError::new(e.message, e.code)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PrintJob {
    pub id: String,
    pub cups_name: String,
    pub content_type: String,
    pub content: String,
    pub title: Option<String>,
    pub printer_id: Option<String>,
    pub options: JsonObject,
    /// Original payload, preserved verbatim in the durable queue.
    pub raw: JsonObject,
}

impl PrintJob {
    pub fn from_dict(data: &JsonObject) -> Result<Self, JobError> {
        let first = |a: &str, b: &str| {
            data.get(a)
                .filter(|v| truthy(v))
                .or_else(|| data.get(b).filter(|v| truthy(v)))
        };
        let id =
            first("id", "job_id").ok_or_else(|| JobError::new("job missing id", "invalid_job"))?;
        let cups = first("cups_name", "cups_queue")
            .ok_or_else(|| JobError::new("job missing cups_name", "invalid_job"))?;
        let ctype = first("content_type", "type")
            .ok_or_else(|| JobError::new("job missing content_type", "invalid_job"))?;
        let content = match data.get("content") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if s.is_empty() => None,
            Some(v) => Some(py_str(v)),
        }
        .ok_or_else(|| JobError::new("job missing content", "invalid_job"))?;
        let options = match data.get("options") {
            Some(Value::Object(o)) => o.clone(),
            _ => JsonObject::new(),
        };
        Ok(PrintJob {
            id: py_str(id),
            cups_name: py_str(cups),
            content_type: py_str(ctype).to_lowercase(),
            content,
            title: data.get("title").filter(|v| truthy(v)).map(py_str),
            printer_id: data.get("printer_id").filter(|v| truthy(v)).map(py_str),
            options,
            raw: data.clone(),
        })
    }

    /// Serialize for durable queue (prefer original payload if present).
    pub fn to_dict(&self) -> JsonObject {
        if !self.raw.is_empty() {
            let mut out = self.raw.clone();
            out.entry("id").or_insert_with(|| json!(self.id));
            out.entry("cups_name")
                .or_insert_with(|| json!(self.cups_name));
            out.entry("content_type")
                .or_insert_with(|| json!(self.content_type));
            out.entry("content").or_insert_with(|| json!(self.content));
            return out;
        }
        let v = json!({
            "id": self.id,
            "cups_name": self.cups_name,
            "content_type": self.content_type,
            "content": self.content,
            "title": self.title,
            "printer_id": self.printer_id,
            "options": self.options,
        });
        v.as_object().cloned().expect("object")
    }

    /// Copies from options (`int(options.get("copies") or 1)`, min 1).
    fn copies(&self) -> i64 {
        self.options
            .get("copies")
            .filter(|v| truthy(v))
            .and_then(py_int)
            .unwrap_or(1)
            .max(1)
    }
}

/// Job lifecycle states reported to the cloud.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Printing,
    Delivered,
    Printed,
    Error,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Printing => "printing",
            JobState::Delivered => "delivered",
            JobState::Printed => "printed",
            JobState::Error => "error",
        }
    }
}

/// Final local result of [`Pipeline::process`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobOutcome {
    /// CUPS reported completion (or the job was already processed).
    Printed,
    /// `lp` accepted it; completion not tracked (or tracked in the background).
    Delivered,
}

impl JobOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            JobOutcome::Printed => "printed",
            JobOutcome::Delivered => "delivered",
        }
    }
}

/// Result of polling CUPS for a submitted request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CupsOutcome {
    /// Job left not-completed without cancel/abort markers.
    Printed,
    /// CUPS reports canceled/aborted (best-effort).
    Error,
    /// Hard timeout, or CUPS queries failed repeatedly.
    Unknown,
}

#[derive(Debug, Clone, Copy)]
pub struct LpArgs<'a> {
    pub title: Option<&'a str>,
    pub copies: i64,
    pub raw: bool,
}

pub type LpFn = Arc<dyn Fn(&str, &Path, &LpArgs) -> Result<Option<String>, JobError> + Send + Sync>;
pub type AckFn = Arc<dyn Fn(&PrintJob) -> Result<(), BoxError> + Send + Sync>;
pub type StateFn =
    Arc<dyn Fn(&PrintJob, JobState, Option<&str>) -> Result<(), BoxError> + Send + Sync>;
pub type FetchFn = Arc<dyn Fn(&str) -> Result<Vec<u8>, BoxError> + Send + Sync>;
pub type TickFn = Arc<dyn Fn() + Send + Sync>;
pub type WaitFn = Arc<dyn Fn(&str, Option<&TickFn>) -> CupsOutcome + Send + Sync>;
pub type RawProbeFn = Arc<dyn Fn(&str) -> Result<bool, BoxError> + Send + Sync>;

fn lp_request_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)request id is\s+(\S+)").expect("regex"))
}

/// `lp` argv (without the program name).
pub fn lp_args(cups_name: &str, path: &Path, args: &LpArgs) -> Vec<String> {
    let mut cmd = vec!["-d".to_string(), cups_name.to_string()];
    if args.copies > 1 {
        cmd.extend(["-n".into(), args.copies.to_string()]);
    }
    if let Some(t) = args.title.filter(|t| !t.is_empty()) {
        cmd.extend(["-t".into(), t.to_string()]);
    }
    if args.raw {
        cmd.extend(["-o".into(), "raw".into()]);
    }
    cmd.push(path.display().to_string());
    cmd
}

/// CUPS request id (e.g. `Zebra_1-42`) from `lp` output.
pub fn parse_lp_request_id(text: &str) -> Option<String> {
    lp_request_re()
        .captures(text)
        .map(|c| c[1].trim_end_matches(['(', ')']).to_string())
}

/// Submit a file to CUPS via `lp`.
///
/// When `raw` is set, passes `-o raw` so CUPS does not filter/transform
/// the payload (required for ZPL/EPL on raw thermal queues).
pub fn default_lp(cups_name: &str, path: &Path, args: &LpArgs) -> Result<Option<String>, JobError> {
    let argv = lp_args(cups_name, path, args);
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let out = run_with_timeout("lp", &argv, Duration::from_secs(60))
        .map_err(|e| JobError::new(format!("lp failed: {e}"), "lp_error"))?;
    if !out.success {
        let err = if !out.stderr.trim().is_empty() {
            &out.stderr
        } else {
            &out.stdout
        };
        let err = err.trim();
        return Err(JobError::new(
            if err.is_empty() { "lp failed" } else { err },
            "lp_error",
        ));
    }
    Ok(parse_lp_request_id(&format!(
        "{}\n{}",
        out.stdout, out.stderr
    )))
}

/// Paper-out / jam recovery can take a long time; keep watching CUPS while the
/// job remains in not-completed so we still report `printed` after refill.
pub const DEFAULT_CUPS_WAIT: Duration = Duration::from_secs(24 * 60 * 60);
const CUPS_WAIT_LOG_EVERY: Duration = Duration::from_secs(60);

/// Poll CUPS until the job leaves the active (not-completed) queue.
pub fn wait_cups_job(
    request_id: &str,
    timeout: Duration,
    poll: Duration,
    on_tick: Option<&TickFn>,
) -> CupsOutcome {
    let Some(job_key) = request_id.split_whitespace().next() else {
        return CupsOutcome::Unknown;
    };
    let started = Instant::now();
    let deadline = started + timeout.max(Duration::from_secs(30));
    let mut failures = 0;
    let mut last_log: Option<Instant> = None;

    while Instant::now() < deadline {
        let active = match run_with_timeout(
            "lpstat",
            &["-W", "not-completed"],
            Duration::from_secs(15),
        ) {
            Ok(o) => {
                failures = 0;
                o
            }
            Err(e) => {
                failures += 1;
                log::debug!(target: LOG, "lpstat not-completed failed: {e}");
                if failures >= 5 {
                    log::warn!(target: LOG, "CUPS lpstat failed {failures} times for {job_key} — leaving delivered");
                    return CupsOutcome::Unknown;
                }
                thread::sleep(poll.max(Duration::from_millis(500)));
                continue;
            }
        };

        if !format!("{}{}", active.stdout, active.stderr).contains(job_key) {
            // Not in active queue — check completed for abort markers if possible.
            let done = run_with_timeout(
                "lpstat",
                &["-W", "completed", "-l"],
                Duration::from_secs(15),
            )
            .map(|o| o.stdout + &o.stderr)
            .unwrap_or_default();
            return completed_outcome(&done, job_key);
        }

        if last_log.is_none_or(|t| t.elapsed() >= CUPS_WAIT_LOG_EVERY) {
            log::info!(
                target: LOG,
                "CUPS job {job_key} still active after {:.0}s (waiting for printer)",
                started.elapsed().as_secs_f64()
            );
            last_log = Some(Instant::now());
        }
        // Keep admin inventory fresh while this thread is blocked on paper-out.
        if let Some(tick) = on_tick {
            tick();
        }
        thread::sleep(poll.max(Duration::from_millis(250)));
    }
    log::warn!(
        target: LOG,
        "CUPS job {job_key} still active after {:.0}s — leaving delivered",
        timeout.as_secs_f64()
    );
    CupsOutcome::Unknown
}

/// Crude: canceled/aborted within 400 bytes after the job id → error.
fn completed_outcome(done_out: &str, job_key: &str) -> CupsOutcome {
    if let Some(idx) = done_out.find(job_key) {
        let snippet: String = done_out[idx..]
            .chars()
            .take(400)
            .collect::<String>()
            .to_lowercase();
        if ["canceled", "cancelled", "aborted"]
            .iter()
            .any(|k| snippet.contains(k))
        {
            return CupsOutcome::Error;
        }
    }
    CupsOutcome::Printed
}

/// Durable queue + processed markers under state_dir.
#[derive(Debug, Clone)]
pub struct JobStore {
    pub queue_dir: PathBuf,
    pub processed_dir: PathBuf,
}

impl JobStore {
    pub fn new(queue_dir: PathBuf, processed_dir: PathBuf) -> Self {
        JobStore {
            queue_dir,
            processed_dir,
        }
    }

    pub fn from_config(cfg: &crate::config::Config) -> Self {
        JobStore::new(cfg.queue_dir(), cfg.processed_dir())
    }

    pub fn ensure(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.queue_dir)?;
        fs::create_dir_all(&self.processed_dir)
    }

    pub fn queue_path(&self, job_id: &str) -> PathBuf {
        self.queue_dir.join(format!("{job_id}.json"))
    }

    pub fn processed_path(&self, job_id: &str) -> PathBuf {
        self.processed_dir.join(job_id)
    }

    pub fn is_processed(&self, job_id: &str) -> bool {
        self.processed_path(job_id).is_file()
    }

    pub fn has_queue_file(&self, job_id: &str) -> bool {
        self.queue_path(job_id).is_file()
    }

    /// Write job JSON with fsync (file + dir). Idempotent if file already exists.
    pub fn write_queue(&self, job: &PrintJob) -> std::io::Result<PathBuf> {
        self.ensure()?;
        let path = self.queue_path(&job.id);
        if path.is_file() {
            return Ok(path);
        }
        let mut raw =
            serde_json::to_string_pretty(&job.to_dict()).map_err(std::io::Error::other)?;
        raw.push('\n');
        write_durable(&path, raw.as_bytes(), 0o600, true)?;
        Ok(path)
    }

    pub fn mark_processed(&self, job_id: &str) -> std::io::Result<()> {
        self.ensure()?;
        let path = self.processed_path(job_id);
        fs::write(&path, utc_now_iso() + "\n")?;
        let _ = crate::util::set_mode(&path, 0o644);
        Ok(())
    }

    pub fn delete_queue(&self, job_id: &str) {
        let _ = fs::remove_file(self.queue_path(job_id));
    }

    pub fn load_queued(&self, job_id: &str) -> Result<PrintJob, JobError> {
        let path = self.queue_path(job_id);
        let corrupt = |detail: String| {
            JobError::new(
                format!("corrupt queue file {}{detail}", path.display()),
                "corrupt_queue",
            )
        };
        let raw = fs::read_to_string(&path).map_err(|e| corrupt(format!(": {e}")))?;
        match serde_json::from_str::<Value>(&raw) {
            Ok(Value::Object(data)) => PrintJob::from_dict(&data),
            Ok(_) => Err(corrupt(String::new())),
            Err(e) => Err(corrupt(format!(": {e}"))),
        }
    }

    pub fn list_queued_ids(&self) -> Vec<String> {
        let _ = self.ensure();
        let mut ids: Vec<String> = fs::read_dir(&self.queue_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                e.file_name()
                    .to_str()
                    .and_then(|n| n.strip_suffix(".json"))
                    .map(String::from)
            })
            .collect();
        ids.sort();
        ids
    }

    /// True if any durable queue file exists (job mid-print or crash recovery).
    pub fn has_pending_work(&self) -> bool {
        !self.list_queued_ids().is_empty()
    }
}

/// Detect file type from magic bytes (overrides wrong content_type labels).
///
/// Not used for `raw_*` — thermal payloads must not be reclassified as PDF/PNG.
pub fn sniff_suffix(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"%PDF") {
        Some(".pdf")
    } else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(".png")
    } else if data.starts_with(b"\xff\xd8\xff") {
        Some(".jpg")
    } else {
        None
    }
}

/// Pick a temp extension for raw thermal bytes (ZPL → `.zpl`, else `.raw`).
pub fn raw_file_suffix(data: &[u8]) -> &'static str {
    let start = data
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(data.len());
    let head = &data[start..data.len().min(start + 64)];
    // ZPL labels commonly start with ^XA (format start).
    if head.windows(3).any(|w| w == b"^XA") {
        ".zpl"
    } else {
        ".raw"
    }
}

/// True when the job must take the CUPS raw path (`lp -o raw`).
pub fn is_raw_job(job: &PrintJob) -> bool {
    if RAW_TYPES.contains(&job.content_type.as_str()) {
        return true;
    }
    // CLI print-test --raw / local overrides.
    match job.options.get("raw") {
        Some(Value::Bool(b)) => *b,
        Some(v @ Value::Number(_)) => py_int(v) == Some(1),
        Some(Value::String(s)) => matches!(s.to_lowercase().as_str(), "1" | "true" | "yes"),
        _ => false,
    }
}

/// Lenient base64 like Python `b64decode(validate=False)`: drops non-alphabet chars.
fn b64decode_lenient(payload: &str) -> Result<Vec<u8>, base64::DecodeError> {
    let cleaned: String = payload
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
        .collect();
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
    );
    engine.decode(cleaned.trim_end_matches('='))
}

/// Write bytes and name by sniffed type so CUPS filters see the real format.
fn write_temp_content(
    work_dir: &Path,
    job_id: &str,
    declared: &str,
    data: &[u8],
) -> std::io::Result<PathBuf> {
    let real = sniff_suffix(data);
    if let Some(r) = real.filter(|r| *r != declared) {
        log::warn!(target: LOG, "job {job_id} content magic is {r} but content_type implied {declared} — using magic");
    }
    let out = work_dir.join(format!("{job_id}{}", real.unwrap_or(declared)));
    fs::write(&out, data)?;
    Ok(out)
}

/// Write raw thermal bytes without PDF/PNG magic sniffing.
fn write_raw_content(work_dir: &Path, job_id: &str, data: &[u8]) -> std::io::Result<PathBuf> {
    let out = work_dir.join(format!("{job_id}{}", raw_file_suffix(data)));
    fs::write(&out, data)?;
    Ok(out)
}

/// Return `(path, is_temp)`. Caller must delete temp files when `is_temp`.
///
/// Supports pdf/png/jpeg/raw uri|base64 and local_path. Image/PDF extensions
/// are corrected from magic bytes; `raw_*` never sniffs as PDF/PNG.
pub fn materialize_content(
    job: &PrintJob,
    work_dir: Option<&Path>,
    fetch_url: &FetchFn,
) -> Result<(PathBuf, bool), JobError> {
    let ctype = job.content_type.as_str();
    if !is_supported(ctype) {
        return Err(JobError::new(
            format!("unsupported content_type: {ctype}"),
            "unsupported_content",
        ));
    }

    if ctype == "local_path" {
        let path = expand_user(&job.content);
        if !path.is_file() {
            return Err(JobError::new(
                format!("local file not found: {}", path.display()),
                "content_missing",
            ));
        }
        return Ok((path, false));
    }

    let io_err = |e: std::io::Error| JobError::new(e.to_string(), "job_error");
    let work_dir = match work_dir {
        Some(d) => {
            fs::create_dir_all(d).map_err(io_err)?;
            d.to_path_buf()
        }
        None => tempfile::Builder::new()
            .prefix("vesyl-print-")
            .tempdir()
            .map_err(io_err)?
            .keep(),
    };

    let declared = declared_suffix(ctype).unwrap_or(".bin");
    let is_raw = RAW_TYPES.contains(&ctype);

    let data = if ctype.ends_with("_base64") {
        let mut payload = job.content.as_str();
        // Tolerate data-url prefix.
        if payload.trim().to_lowercase().starts_with("data:") {
            if let Some((_, rest)) = payload.split_once(',') {
                payload = rest;
            }
        }
        let data = b64decode_lenient(payload)
            .map_err(|e| JobError::new(format!("invalid base64 content: {e}"), "content_bad"))?;
        if data.is_empty() {
            return Err(JobError::new("empty base64 content", "content_bad"));
        }
        data
    } else {
        let data = fetch_url(&job.content)
            .map_err(|e| JobError::new(format!("fetch failed: {e}"), "content_fetch"))?;
        if data.is_empty() {
            return Err(JobError::new("empty content from uri", "content_bad"));
        }
        data
    };

    let path = if is_raw {
        write_raw_content(&work_dir, &job.id, &data)
    } else {
        write_temp_content(&work_dir, &job.id, declared, &data)
    }
    .map_err(io_err)?;
    Ok((path, true))
}

fn expand_user(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return Path::new(&home).join(rest);
        }
    }
    PathBuf::from(p)
}

fn http_agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(60)))
            .http_status_as_error(false)
            .user_agent("vesyl-print-agent")
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .into()
    })
}

/// Default content fetcher for `*_uri` jobs.
pub fn http_get(url: &str) -> Result<Vec<u8>, BoxError> {
    let mut resp = http_agent()
        .get(url)
        .header("Accept", "*/*")
        .call()
        .map_err(|e| {
            JobError::new(
                format!("network error fetching content: {e}"),
                "content_fetch",
            )
        })?;
    let status = resp.status().as_u16();
    if status >= 400 {
        return Err(Box::new(JobError::new(
            format!("HTTP {status} fetching content"),
            "content_fetch",
        )));
    }
    Ok(resp
        .body_mut()
        .with_config()
        .limit(256 * 1024 * 1024)
        .read_to_vec()?)
}

/// Injectable hooks for the job pipeline. `Pipeline::default()` talks to real
/// CUPS with no cloud callbacks (Phase B local-only behavior).
#[derive(Clone)]
pub struct Pipeline {
    pub lp: LpFn,
    pub ack: AckFn,
    pub report_state: StateFn,
    pub fetch_url: FetchFn,
    /// Invoked periodically while waiting on CUPS completion (e.g. paper-out
    /// recovery) so the agent can keep reporting printer inventory.
    pub on_wait_tick: Option<TickFn>,
    pub wait_cups: WaitCups,
    pub wait_cups_job: WaitFn,
    pub supports_raw: RawProbeFn,
    pub work_dir: Option<PathBuf>,
}

impl Default for Pipeline {
    fn default() -> Self {
        Pipeline {
            lp: Arc::new(default_lp),
            ack: Arc::new(|_| Ok(())),
            report_state: Arc::new(|_, _, _| Ok(())),
            fetch_url: Arc::new(http_get),
            on_wait_tick: None,
            wait_cups: WaitCups::Sync,
            wait_cups_job: Arc::new(|id, tick| {
                wait_cups_job(id, DEFAULT_CUPS_WAIT, Duration::from_secs(2), tick)
            }),
            supports_raw: Arc::new(|q| Ok(crate::printers::queue_supports_raw(q, None))),
            work_dir: None,
        }
    }
}

impl Pipeline {
    fn report(&self, job: &PrintJob, state: JobState, detail: Option<&str>) {
        if let Err(e) = (self.report_state)(job, state, detail) {
            log::debug!(target: LOG, "report_state {} failed: {e}", state.as_str());
        }
    }

    /// Run the full durable pipeline for one job.
    ///
    /// Returns `Printed`, `Delivered` (CUPS not tracked or tracked in the
    /// background), or a `JobError` after reporting `error`.
    pub fn process(&self, job: &PrintJob, store: &JobStore) -> Result<JobOutcome, JobError> {
        let io_err = |e: std::io::Error| JobError::new(e.to_string(), "job_error");
        store.ensure().map_err(io_err)?;
        let job_id = job.id.as_str();

        // 1. Already finished — idempotent success (drop any leftover queue file)
        if store.is_processed(job_id) {
            log::info!(target: LOG, "job {job_id} already processed — skip");
            store.delete_queue(job_id);
            self.report(job, JobState::Printed, Some("already_processed"));
            return Ok(JobOutcome::Printed);
        }

        // 2. Durable receive before any ack / print
        store.write_queue(job).map_err(io_err)?;

        // 3. Ack only after disk durability. Non-fatal: cloud can redeliver.
        if let Err(e) = (self.ack)(job) {
            log::warn!(target: LOG, "ack failed for job {job_id}: {e}");
        }

        // 4–6. Materialize + submit + optional CUPS completion wait
        self.report(job, JobState::Printing, None);
        let mut temp: Option<PathBuf> = None;
        let result = self.submit(job, store, &mut temp);

        if let Some(path) = &temp {
            let _ = fs::remove_file(path);
            // Clean the single-file temp dir we created.
            if self.work_dir.is_none() {
                if let Some(parent) = path.parent() {
                    if parent
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("vesyl-print-"))
                    {
                        let _ = fs::remove_dir(parent);
                    }
                }
            }
        }

        match result {
            Ok(outcome) => Ok(outcome),
            Err(e) => {
                self.report(job, JobState::Error, Some(&e.message));
                log::error!(target: LOG, "job {job_id} error: {}", e.message);
                Err(e)
            }
        }
    }

    /// Steps 4–7; `temp` receives any temp file the caller must delete.
    fn submit(
        &self,
        job: &PrintJob,
        store: &JobStore,
        temp: &mut Option<PathBuf>,
    ) -> Result<JobOutcome, JobError> {
        let job_id = job.id.as_str();
        let (mut path, is_temp) =
            materialize_content(job, self.work_dir.as_deref(), &self.fetch_url)?;
        if is_temp {
            *temp = Some(path.clone());
        }
        let copies = job.copies();
        let mut use_raw = is_raw_job(job);

        // PDF/PNG/JPEG → ZPL graphic when targeting a raw thermal (Zebra) queue.
        if zpl::should_convert_to_zpl(
            &path,
            &job.cups_name,
            &job.options,
            use_raw,
            &*self.supports_raw,
        ) {
            let conv_dir = if is_temp {
                path.parent().map(Path::to_path_buf).unwrap_or_default()
            } else {
                tempfile::Builder::new()
                    .prefix("vesyl-print-zpl-")
                    .tempdir()
                    .map_err(|e| JobError::new(format!("ZPL conversion failed: {e}"), "zpl_error"))?
                    .keep()
            };
            let mut zpl_opts = job.options.clone();
            zpl_opts
                .entry("cups_name")
                .or_insert_with(|| json!(job.cups_name));
            let zpl_path = zpl::write_zpl_file(&path, &conv_dir, job_id, &zpl_opts)?;
            log::info!(
                target: LOG,
                "job {job_id} converted {} → ZPL for raw queue {}",
                path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(),
                job.cups_name
            );
            // Drop original temp graphic; keep ZPL temp.
            if is_temp && path != zpl_path {
                let _ = fs::remove_file(&path);
            }
            path = zpl_path;
            *temp = Some(path.clone());
            use_raw = true;
        }

        let cups_id = (self.lp)(
            &job.cups_name,
            &path,
            &LpArgs {
                title: job.title.as_deref(),
                copies,
                raw: use_raw,
            },
        )?;
        let cups_job = cups_id.filter(|s| !s.trim().is_empty());
        self.report(job, JobState::Delivered, cups_job.as_deref());

        let mut outcome = JobOutcome::Delivered;
        match (&cups_job, self.wait_cups) {
            (Some(cid), WaitCups::Sync) => {
                match (self.wait_cups_job)(cid, self.on_wait_tick.as_ref()) {
                    CupsOutcome::Printed => {
                        self.report(job, JobState::Printed, Some(cid));
                        outcome = JobOutcome::Printed;
                    }
                    CupsOutcome::Error => {
                        return Err(JobError::new(
                            format!("CUPS job {cid} failed"),
                            "cups_job_failed",
                        ));
                    }
                    CupsOutcome::Unknown => {
                        log::info!(target: LOG, "job {job_id} CUPS tracking timed out for {cid} — left delivered");
                    }
                }
            }
            (Some(cid), WaitCups::Async) => self.watch_cups_async(job, cid),
            (Some(_), WaitCups::Off) => {}
            (None, _) => {
                log::info!(target: LOG, "job {job_id} no CUPS request id — left delivered")
            }
        }

        store
            .mark_processed(job_id)
            .map_err(|e| JobError::new(e.to_string(), "job_error"))?;
        store.delete_queue(job_id);
        log::info!(target: LOG, "job {job_id} {} → {}", outcome.as_str(), job.cups_name);
        Ok(outcome)
    }

    /// Report printed/error after CUPS completes, without blocking the next lp.
    fn watch_cups_async(&self, job: &PrintJob, cups_id: &str) {
        let (job, cups_id) = (job.clone(), cups_id.to_string());
        let this = self.clone();
        let name = format!("cups-wait-{}", job.id.chars().take(8).collect::<String>());
        let spawned = thread::Builder::new().name(name).spawn(move || {
            match (this.wait_cups_job)(&cups_id, this.on_wait_tick.as_ref()) {
                CupsOutcome::Printed => this.report(&job, JobState::Printed, Some(&cups_id)),
                CupsOutcome::Error => this.report(
                    &job,
                    JobState::Error,
                    Some(&format!("CUPS job {cups_id} failed")),
                ),
                CupsOutcome::Unknown => log::info!(
                    target: LOG,
                    "job {} CUPS tracking timed out for {cups_id} — left delivered",
                    job.id
                ),
            }
        });
        if let Err(e) = spawned {
            log::error!(target: LOG, "async CUPS wait failed to start: {e}");
        }
    }

    /// Process every queue/*.json (crash recovery). Returns `[(job_id, result)]`
    /// where result is `printed`, `delivered` or `error:<code>`.
    pub fn drain(&self, store: &JobStore) -> Vec<(String, String)> {
        let _ = store.ensure();
        let mut results = Vec::new();
        for job_id in store.list_queued_ids() {
            let job = match store.load_queued(&job_id) {
                Ok(j) => j,
                Err(e) => {
                    log::error!(target: LOG, "skip corrupt queue {job_id}: {}", e.message);
                    results.push((job_id, format!("error:{}", e.code)));
                    continue;
                }
            };
            let r = match self.process(&job, store) {
                Ok(o) => o.as_str().to_string(),
                Err(e) => format!("error:{}", e.code),
            };
            results.push((job_id, r));
        }
        results
    }
}

/// Build a PrintJob that prints an existing file (CLI print-test).
///
/// Pass `raw = true` to submit with `lp -o raw`.
pub fn job_from_local_file(
    path: &Path,
    cups_name: &str,
    job_id: Option<&str>,
    title: Option<&str>,
    copies: i64,
    raw: bool,
) -> Result<PrintJob, JobError> {
    let p = fs::canonicalize(expand_user(&path.display().to_string()))
        .ok()
        .filter(|p| p.is_file())
        .ok_or_else(|| {
            JobError::new(
                format!("file not found: {}", path.display()),
                "content_missing",
            )
        })?;
    let mut options = JsonObject::new();
    options.insert("copies".into(), json!(copies));
    if raw {
        options.insert("raw".into(), json!(true));
    }
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(PrintJob {
        id: job_id
            .map(String::from)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        cups_name: cups_name.to_string(),
        content_type: "local_path".into(),
        content: p.display().to_string(),
        title: Some(
            title
                .map(String::from)
                .unwrap_or_else(|| format!("vesyl-print test {name}")),
        ),
        printer_id: None,
        options,
        raw: JsonObject::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Mutex};

    const PNG_1X1_B64: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

    fn b64(data: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(data)
    }

    fn store(td: &Path) -> JobStore {
        let s = JobStore::new(td.join("queue"), td.join("processed"));
        s.ensure().unwrap();
        s
    }

    fn job(id: &str, cups: &str, ctype: &str, content: String) -> PrintJob {
        let mut options = JsonObject::new();
        options.insert("copies".into(), json!(1));
        PrintJob {
            id: id.into(),
            cups_name: cups.into(),
            content_type: ctype.into(),
            content,
            title: Some("unit test".into()),
            printer_id: None,
            options,
            raw: JsonObject::new(),
        }
    }

    fn png_job(id: &str) -> PrintJob {
        job(id, "TestPrinter", "png_base64", PNG_1X1_B64.into())
    }

    /// Pipeline that never touches real CUPS.
    fn test_pipeline() -> Pipeline {
        Pipeline {
            lp: Arc::new(|_, _, _| Ok(None)),
            supports_raw: Arc::new(|_| Ok(false)),
            wait_cups_job: Arc::new(|_, _| panic!("wait_cups_job should not run")),
            fetch_url: Arc::new(|_| Err("no network in tests".into())),
            ..Pipeline::default()
        }
    }

    type Events = Arc<Mutex<Vec<String>>>;

    fn recording_state(events: &Events) -> StateFn {
        let ev = events.clone();
        Arc::new(move |_, st, _| {
            ev.lock().unwrap().push(format!("state:{}", st.as_str()));
            Ok(())
        })
    }

    #[test]
    fn write_queue_before_ack_before_print() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let events: Events = Arc::default();
        let j = png_job("job-1");

        let (s1, e1) = (st.clone(), events.clone());
        let (s2, e2) = (st.clone(), events.clone());
        let p = Pipeline {
            ack: Arc::new(move |j| {
                assert!(s1.has_queue_file(&j.id));
                e1.lock().unwrap().push("ack".into());
                Ok(())
            }),
            lp: Arc::new(move |_, path, args| {
                assert!(s2.has_queue_file("job-1"));
                assert!(e2.lock().unwrap().contains(&"ack".to_string()));
                assert!(!s2.is_processed("job-1"));
                assert!(path.is_file());
                assert!(!args.raw);
                e2.lock().unwrap().push("lp".into());
                Ok(None)
            }),
            report_state: recording_state(&events),
            ..test_pipeline()
        };
        let result = p.process(&j, &st).unwrap();
        assert_eq!(result, JobOutcome::Delivered);
        let ev = events.lock().unwrap();
        assert_eq!(ev[0], "ack");
        let ack = ev.iter().position(|e| e == "ack").unwrap();
        let lp = ev.iter().position(|e| e == "lp").unwrap();
        assert!(ack < lp);
        assert!(st.is_processed("job-1"));
        assert!(!st.has_queue_file("job-1"));
        // Temp dir created by materialize is cleaned up.
        assert!(ev.contains(&"state:delivered".to_string()));
    }

    #[test]
    fn already_processed_skips_print() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let j = png_job("job-1");
        st.mark_processed(&j.id).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let (c1, c2) = (calls.clone(), calls.clone());
        let p = Pipeline {
            lp: Arc::new(move |_, _, _| {
                c1.fetch_add(1, Ordering::SeqCst);
                Ok(None)
            }),
            ack: Arc::new(move |_| {
                c2.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
            ..test_pipeline()
        };
        assert_eq!(p.process(&j, &st).unwrap(), JobOutcome::Printed);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn failed_print_keeps_queue_file() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let j = png_job("job-1");
        let events: Events = Arc::default();
        let p = Pipeline {
            lp: Arc::new(|_, _, _| Err(JobError::new("printer offline", "lp_error"))),
            report_state: recording_state(&events),
            ..test_pipeline()
        };
        let err = p.process(&j, &st).unwrap_err();
        assert_eq!(err.code, "lp_error");
        assert!(st.has_queue_file("job-1"));
        assert!(!st.is_processed("job-1"));
        assert_eq!(events.lock().unwrap().last().unwrap(), "state:error");
    }

    #[test]
    fn queue_fsync_write_readable() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let path = st.write_queue(&png_job("job-fsync")).unwrap();
        let data: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(data["id"], "job-fsync");
        assert_eq!(data["cups_name"], "TestPrinter");
        assert_eq!(
            st.load_queued("job-fsync").unwrap().content_type,
            "png_base64"
        );
    }

    #[test]
    fn queue_preserves_original_payload() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let payload = json!({
            "job_id": "j9", "cups_queue": "Label_1", "type": "PDF_URI",
            "content": "https://x/a.pdf", "expires_at": "2026-07-15T20:00:00Z"
        });
        let j = PrintJob::from_dict(payload.as_object().unwrap()).unwrap();
        assert_eq!(
            (j.id.as_str(), j.cups_name.as_str(), j.content_type.as_str()),
            ("j9", "Label_1", "pdf_uri")
        );
        st.write_queue(&j).unwrap();
        let data: Value =
            serde_json::from_str(&fs::read_to_string(st.queue_path("j9")).unwrap()).unwrap();
        assert_eq!(data["expires_at"], "2026-07-15T20:00:00Z");
        assert_eq!(st.load_queued("j9").unwrap().id, "j9");
    }

    #[test]
    fn from_dict_validation() {
        let missing = |v: Value| {
            PrintJob::from_dict(v.as_object().unwrap())
                .unwrap_err()
                .message
        };
        assert_eq!(missing(json!({})), "job missing id");
        assert_eq!(missing(json!({"id": 1})), "job missing cups_name");
        assert_eq!(
            missing(json!({"id": 1, "cups_name": "P"})),
            "job missing content_type"
        );
        assert_eq!(
            missing(json!({"id": 1, "cups_name": "P", "type": "pdf_uri", "content": ""})),
            "job missing content"
        );
    }

    #[test]
    fn corrupt_queue_file_is_reported() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        fs::write(st.queue_path("bad"), "{nope").unwrap();
        let results = test_pipeline().drain(&st);
        assert_eq!(
            results,
            vec![("bad".to_string(), "error:corrupt_queue".to_string())]
        );
    }

    #[test]
    fn drain_recovers_queued_jobs() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        st.write_queue(&png_job("q1")).unwrap();
        st.write_queue(&png_job("q2")).unwrap();
        let printed: Events = Arc::default();
        let pr = printed.clone();
        let p = Pipeline {
            lp: Arc::new(move |_, path, _| {
                pr.lock()
                    .unwrap()
                    .push(path.file_name().unwrap().to_string_lossy().into());
                Ok(None)
            }),
            ..test_pipeline()
        };
        let results = p.drain(&st);
        assert_eq!(results.len(), 2);
        assert!(results
            .iter()
            .all(|(_, r)| r == "delivered" || r == "printed"));
        assert_eq!(printed.lock().unwrap().len(), 2);
        assert!(st.list_queued_ids().is_empty());
        assert!(st.is_processed("q1") && st.is_processed("q2"));
    }

    #[test]
    fn drain_skips_already_processed() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let j = png_job("done-already");
        st.write_queue(&j).unwrap();
        st.mark_processed(&j.id).unwrap();
        let p = Pipeline {
            lp: Arc::new(|_, _, _| panic!("lp must not run")),
            ..test_pipeline()
        };
        assert_eq!(
            p.drain(&st),
            vec![("done-already".to_string(), "printed".to_string())]
        );
        assert!(!st.has_queue_file(&j.id));
    }

    fn no_fetch() -> FetchFn {
        Arc::new(|_| Err("unexpected fetch".into()))
    }

    #[test]
    fn pdf_base64() {
        let pdf = b"%PDF-1.1\n1 0 obj<<>>endobj\ntrailer<<>>\n%%EOF\n";
        let j = job("pdf1", "P", "pdf_base64", b64(pdf));
        let td = tempfile::tempdir().unwrap();
        let (path, is_temp) = materialize_content(&j, Some(td.path()), &no_fetch()).unwrap();
        assert!(is_temp);
        assert_eq!(fs::read(&path).unwrap(), pdf);
    }

    #[test]
    fn base64_data_url_and_whitespace() {
        let pdf = b"%PDF-1.4 hello";
        let encoded = b64(pdf);
        let wrapped = format!(
            "data:application/pdf;base64,{}\n{}",
            &encoded[..8],
            &encoded[8..]
        );
        let j = job("pdf2", "P", "pdf_base64", wrapped);
        let td = tempfile::tempdir().unwrap();
        let (path, _) = materialize_content(&j, Some(td.path()), &no_fetch()).unwrap();
        assert_eq!(fs::read(&path).unwrap(), pdf);
    }

    #[test]
    fn png_uri_fetch() {
        let png = b"\x89PNG\r\n\x1a\nfake".to_vec();
        let j = job(
            "uri1",
            "P",
            "png_uri",
            "https://example.test/label.png".into(),
        );
        let td = tempfile::tempdir().unwrap();
        let data = png.clone();
        let fetch: FetchFn = Arc::new(move |u| {
            assert_eq!(u, "https://example.test/label.png");
            Ok(data.clone())
        });
        let (path, is_temp) = materialize_content(&j, Some(td.path()), &fetch).unwrap();
        assert!(is_temp);
        assert_eq!(fs::read(&path).unwrap(), png);
    }

    #[test]
    fn local_path() {
        let td = tempfile::tempdir().unwrap();
        let f = td.path().join("x.jpg");
        fs::write(&f, b"jpeg-bytes").unwrap();
        let j = job_from_local_file(&f, "Q1", None, None, 1, false).unwrap();
        let (path, is_temp) = materialize_content(&j, None, &no_fetch()).unwrap();
        assert!(!is_temp);
        assert_eq!(path, fs::canonicalize(&f).unwrap());
    }

    #[test]
    fn raw_base64_writes_zpl_without_pdf_sniff() {
        let zpl = b"^XA^FO50,50^FDHello^FS^XZ";
        let j = job("raw1", "P", "raw_base64", b64(zpl));
        let td = tempfile::tempdir().unwrap();
        let (path, is_temp) = materialize_content(&j, Some(td.path()), &no_fetch()).unwrap();
        assert!(is_temp);
        assert_eq!(path.extension().unwrap(), "zpl");
        assert_eq!(fs::read(&path).unwrap(), zpl);
    }

    #[test]
    fn raw_uri_writes_raw_suffix_for_non_zpl() {
        let payload = b"\x1b@EPL2-not-zpl".to_vec();
        let j = job(
            "raw2",
            "P",
            "raw_uri",
            "https://example.test/label.bin".into(),
        );
        let td = tempfile::tempdir().unwrap();
        let data = payload.clone();
        let fetch: FetchFn = Arc::new(move |_| Ok(data.clone()));
        let (path, _) = materialize_content(&j, Some(td.path()), &fetch).unwrap();
        assert_eq!(path.extension().unwrap(), "raw");
        assert_eq!(fs::read(&path).unwrap(), payload);
    }

    #[test]
    fn raw_does_not_sniff_as_pdf() {
        let weird = b"%PDF-lookalike-but-raw";
        let j = job("raw-pdfish", "P", "raw_base64", b64(weird));
        let td = tempfile::tempdir().unwrap();
        let (path, _) = materialize_content(&j, Some(td.path()), &no_fetch()).unwrap();
        assert_eq!(path.extension().unwrap(), "raw");
        assert_eq!(fs::read(&path).unwrap(), weird);
    }

    #[test]
    fn sniff_overrides_wrong_pdf_label_for_png() {
        let j = job("mislabel", "P", "pdf_base64", PNG_1X1_B64.into());
        let td = tempfile::tempdir().unwrap();
        let (path, _) = materialize_content(&j, Some(td.path()), &no_fetch()).unwrap();
        assert_eq!(path.extension().unwrap(), "png");
    }

    #[test]
    fn unsupported_content_type() {
        let j = job("u", "P", "docx_uri", "x".into());
        assert_eq!(
            materialize_content(&j, None, &no_fetch()).unwrap_err().code,
            "unsupported_content"
        );
    }

    #[test]
    fn process_job_passes_raw_to_lp() {
        let zpl = b"^XA^FO50,50^A0N,30,30^FDTest^FS^XZ";
        let j = job("raw-lp", "Zebra_Raw", "raw_base64", b64(zpl));
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let (tx, rx) = mpsc::channel();
        let p = Pipeline {
            lp: Arc::new(move |cups, path, args| {
                tx.send((
                    cups.to_string(),
                    path.to_path_buf(),
                    args.raw,
                    fs::read(path).unwrap(),
                ))
                .unwrap();
                Ok(Some("Zebra_Raw-9".into()))
            }),
            wait_cups_job: Arc::new(|id, _| {
                assert_eq!(id, "Zebra_Raw-9");
                CupsOutcome::Printed
            }),
            wait_cups: WaitCups::Sync,
            ..test_pipeline()
        };
        assert_eq!(p.process(&j, &st).unwrap(), JobOutcome::Printed);
        let (cups, path, raw, bytes) = rx.recv().unwrap();
        assert!(raw);
        assert_eq!(cups, "Zebra_Raw");
        assert_eq!(bytes, zpl);
        assert_eq!(path.extension().unwrap(), "zpl");
    }

    #[test]
    fn cups_error_in_sync_mode_fails_job() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let p = Pipeline {
            lp: Arc::new(|_, _, _| Ok(Some("Q-1".into()))),
            wait_cups_job: Arc::new(|_, _| CupsOutcome::Error),
            wait_cups: WaitCups::Sync,
            ..test_pipeline()
        };
        let err = p.process(&png_job("e1"), &st).unwrap_err();
        assert_eq!(err.code, "cups_job_failed");
        assert!(st.has_queue_file("e1"));
    }

    #[test]
    fn lp_argv_includes_o_raw() {
        let args = lp_args(
            "Zebra",
            Path::new("/tmp/label.zpl"),
            &LpArgs {
                title: Some("t"),
                copies: 1,
                raw: true,
            },
        );
        assert_eq!(
            args,
            ["-d", "Zebra", "-t", "t", "-o", "raw", "/tmp/label.zpl"]
        );
        let args = lp_args(
            "Q",
            Path::new("/f.pdf"),
            &LpArgs {
                title: None,
                copies: 3,
                raw: false,
            },
        );
        assert_eq!(args, ["-d", "Q", "-n", "3", "/f.pdf"]);
        assert_eq!(
            parse_lp_request_id("request id is Zebra-1 (1 file(s))\n").as_deref(),
            Some("Zebra-1")
        );
        assert_eq!(parse_lp_request_id("nothing"), None);
    }

    #[test]
    fn local_path_options_raw() {
        let td = tempfile::tempdir().unwrap();
        let f = td.path().join("x.zpl");
        fs::write(&f, b"^XA^XZ").unwrap();
        let j = job_from_local_file(&f, "Q", None, None, 1, true).unwrap();
        assert!(is_raw_job(&j));
        let st = store(td.path());
        let flags: Arc<Mutex<Vec<bool>>> = Arc::default();
        let fl = flags.clone();
        let p = Pipeline {
            lp: Arc::new(move |_, _, a| {
                fl.lock().unwrap().push(a.raw);
                Ok(None)
            }),
            ..test_pipeline()
        };
        p.process(&j, &st).unwrap();
        assert_eq!(*flags.lock().unwrap(), vec![true]);
        // local_path source file is never deleted.
        assert!(f.is_file());
    }

    #[test]
    fn wait_cups_off_skips_poll() {
        let td = tempfile::tempdir().unwrap();
        let f = td.path().join("x.zpl");
        fs::write(&f, b"^XA^XZ").unwrap();
        let j = job_from_local_file(&f, "Q", None, None, 1, true).unwrap();
        let st = store(td.path());
        let p = Pipeline {
            lp: Arc::new(|_, _, _| Ok(Some("Q-99".into()))),
            wait_cups: WaitCups::Off,
            ..test_pipeline()
        };
        assert_eq!(p.process(&j, &st).unwrap(), JobOutcome::Delivered);
    }

    #[test]
    fn wait_cups_async_returns_before_cups_finishes() {
        let td = tempfile::tempdir().unwrap();
        let f = td.path().join("x.zpl");
        fs::write(&f, b"^XA^XZ").unwrap();
        let j = job_from_local_file(&f, "Q", None, None, 1, true).unwrap();
        let st = store(td.path());
        let events: Events = Arc::default();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let p = Pipeline {
            lp: Arc::new(|_, _, _| Ok(Some("Q-99".into()))),
            wait_cups_job: Arc::new(move |_, _| {
                started_tx.send(()).unwrap();
                let _ = release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2));
                CupsOutcome::Printed
            }),
            report_state: recording_state(&events),
            wait_cups: WaitCups::Async,
            ..test_pipeline()
        };
        assert_eq!(p.process(&j, &st).unwrap(), JobOutcome::Delivered);
        assert!(events.lock().unwrap().contains(&"state:delivered".into()));
        assert!(!events.lock().unwrap().contains(&"state:printed".into()));
        assert!(st.is_processed(&j.id));
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !events.lock().unwrap().contains(&"state:printed".into()) && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(events.lock().unwrap().contains(&"state:printed".into()));
    }

    #[test]
    fn double_receive_prints_once() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let p = Pipeline {
            lp: Arc::new(move |_, _, _| {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(None)
            }),
            ..test_pipeline()
        };
        let j = png_job("once");
        p.process(&j, &st).unwrap();
        p.process(&j, &st).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn png_to_zebra_queue_converts_to_zpl() {
        let td = tempfile::tempdir().unwrap();
        let png = crate::zpl::tests::tiny_png(td.path());
        let st = store(&td.path().join("state"));
        let (tx, rx) = mpsc::channel();
        let mut j = job(
            "j1",
            "Zebra_ZD220-203dpi_ZPL",
            "local_path",
            png.display().to_string(),
        );
        j.title = None;
        let p = Pipeline {
            lp: Arc::new(move |q, path, args| {
                tx.send((q.to_string(), fs::read_to_string(path).unwrap(), args.raw))
                    .unwrap();
                Ok(Some("Zebra_ZD220-1".into()))
            }),
            supports_raw: Arc::new(|_| Ok(true)),
            wait_cups_job: Arc::new(|_, _| CupsOutcome::Printed),
            wait_cups: WaitCups::Sync,
            ..test_pipeline()
        };
        assert_eq!(p.process(&j, &st).unwrap(), JobOutcome::Printed);
        let (_, text, raw) = rx.recv().unwrap();
        assert!(text.contains("^GFA,"));
        assert!(raw);
        // Original local file untouched.
        assert!(png.is_file());
    }

    #[test]
    fn completed_outcome_markers() {
        assert_eq!(
            completed_outcome("Q-1 user 1024 ... aborted", "Q-1"),
            CupsOutcome::Error
        );
        assert_eq!(
            completed_outcome("Q-1 user 1024 completed", "Q-1"),
            CupsOutcome::Printed
        );
        assert_eq!(completed_outcome("", "Q-1"), CupsOutcome::Printed);
    }
}
