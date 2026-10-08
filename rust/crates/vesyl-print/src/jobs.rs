//! Local print jobs: durable queue, content fetch, CUPS submit + completion.
//!
//! Ordering (at-least-once, crash-safe):
//!
//! 1. If processed/<job_id> exists → already finished (idempotent), skip print
//! 2. If no queue file → write + fsync full job JSON to queue/<job_id>.json
//! 3. Only then call ack callback (when cloud supports it)
//! 4. Record the attempt in the queue file, then materialize content →
//!    lp -d <cups_name> (`-o raw` for raw_*/ZPL)
//! 5. When lp accepts the job, record that in the queue file (its CUPS
//!    request id), then report **delivered**
//! 6. Optionally poll CUPS → report **printed** or **error**
//!    (`WaitCups::Async` hands the job to the pipeline's shared
//!    [`CupsWatcher`] so the next job can be `lp`'d immediately; CUPS FIFO
//!    keeps page order on one queue)
//! 7. On success path: write processed/<job_id>, delete queue file
//!
//! A failed job is reported **error**. When a retry cannot help (see
//! [`JobError::is_permanent`]) its queue file moves to `queue/failed/`, out of
//! [`JobStore::list_queued_ids`]; other failures keep it for the next drain.
//! A retired file goes once the job finishes after all (a redelivery), or
//! when [`JobStore::prune_processed`] finds it past the retention.
//!
//! On agent start: [`Pipeline::drain`] recovers queue/*.json left from crashes.
//!
//! What steps 4 and 5 record (under `_agent` in the queue file, beside the
//! cloud's payload) is what the next run goes by. A job CUPS already has is
//! never given to `lp` again, which would print a second label: it is
//! finished from what CUPS says ([`Pipeline::resume`]). A job the agent died
//! in [`MAX_ATTEMPTS`] times while converting or submitting it (out of
//! memory, say) is retired as `crash_loop` instead of killing every start.
//! Once [`Pipeline::stop`] is set the drain takes no more jobs, none goes to
//! `lp`, and a CUPS wait ends early, each leaving its record for the next
//! start.
//!
//! Job ids become file names, so only [`valid_job_id`] ids are accepted.
//!
//! `raw_uri` / `raw_base64` write a temp `.zpl`/`.raw` file (no PDF/PNG
//! magic sniff) and submit with `lp -o raw`. PDF/PNG/JPEG sent to a raw Zebra
//! queue are converted to ZPL `^GFA` graphics (see [`crate::zpl`]).

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::ffi::{CString, OsStr};
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use base64::Engine as _;
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::WaitCups;
use crate::net;
use crate::printers::{run_cups, CmdOutput};
use crate::util::{py_int, py_str, truthy, utc_now_iso, write_durable};
use crate::{zpl, BoxError, JsonObject};

const LOG: &str = "vesyl-print.jobs";

const RAW_TYPES: &[&str] = &["raw_uri", "raw_base64"];

/// Largest content body `http_get` accepts.
const MAX_CONTENT_BYTES: u64 = 256 * 1024 * 1024;

/// Longest accepted job id (server ids are 36-character UUIDs).
const MAX_JOB_ID_LEN: usize = 128;

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

/// True when `id` is safe to use as a file name: an ASCII letter or digit,
/// then letters, digits, `.`, `_`, `:` or `-`, at most 128 characters. That
/// rules out `.`, `..`, `/`, `\` and NUL, so no job id (from the cloud, a
/// cancel message or a queue file) can reach outside queue/ or processed/.
pub fn valid_job_id(id: &str) -> bool {
    let b = id.as_bytes();
    b.first().is_some_and(u8::is_ascii_alphanumeric)
        && b.len() <= MAX_JOB_ID_LEN
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b':' | b'-'))
}

/// An untrusted id, quoted and shortened for log lines.
pub(crate) fn shown_id(id: &str) -> String {
    format!("{:?}", id.chars().take(64).collect::<String>())
}

/// A queue file stem we may join onto queue/ (anything `read_dir` returns
/// is one path component; this also guards the public helpers).
fn plain_file_stem(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

fn invalid_id_error() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "invalid job id")
}

/// Mutex lock that survives a panicked holder (state here stays consistent).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The message of a caught panic (`panic!` payloads are `&str` or `String`).
pub(crate) fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

/// Run `f`; a panic (e.g. the OS refusing a thread deep inside a library)
/// comes back as `Err` with its message.
pub(crate) fn catch_panic<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(f)).map_err(|p| panic_message(&*p))
}

/// Print / queue failure with a short machine-friendly code.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct JobError {
    pub message: String,
    pub code: String,
}

/// Codes of failures a retry cannot fix. Their queue files are retired to
/// `queue/failed/` instead of being re-run on every agent start.
const PERMANENT_CODES: &[&str] = &[
    // Payload / queue file unusable.
    "invalid_job",
    "corrupt_queue",
    "unsupported_content",
    // Content missing, undecodable, or refused by its host (HTTP 4xx).
    "content_missing",
    "content_bad",
    "content_rejected",
    // Image / PDF → ZPL conversion.
    "image_bad",
    "pdf_render",
    "pdf_too_many_pages",
    "zpl_error",
    // `lp`: the CUPS queue does not exist.
    "unknown_queue",
    // CUPS canceled or aborted the job: it is final, and printing it again
    // on the next start would surprise whoever canceled it.
    "cups_job_failed",
    // Image / PDF → ZPL: a PDF page, or the label graphic the options ask
    // for, too large to rasterize within the conversion's memory limits.
    "pdf_page_too_large",
    "label_too_large",
];

/// Code of a job the agent died in [`MAX_ATTEMPTS`] times while converting
/// or submitting it: permanent, as one more try would only kill it again.
pub const CRASH_LOOP: &str = "crash_loop";

impl JobError {
    pub fn new(message: impl Into<String>, code: &str) -> Self {
        JobError {
            message: message.into(),
            code: code.into(),
        }
    }

    /// True when retrying cannot succeed (bad payload or content, unknown
    /// CUPS queue, a CUPS job that was canceled or aborted, a job that
    /// keeps killing the agent). Transient failures (network, CUPS down,
    /// local I/O) return false.
    pub fn is_permanent(&self) -> bool {
        PERMANENT_CODES.contains(&self.code.as_str()) || self.code == CRASH_LOOP
    }
}

impl From<zpl::ZplError> for JobError {
    fn from(e: zpl::ZplError) -> Self {
        JobError::new(e.message, e.code)
    }
}

/// HTTP error status from a content host. [`materialize_content`] maps a
/// permanent one (see [`HttpStatusError::is_permanent`]) to `content_rejected`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("HTTP {status} fetching content")]
pub struct HttpStatusError {
    pub status: u16,
}

impl HttpStatusError {
    /// A 4xx other than 408 (timeout) and 429 (rate limit): an expired or
    /// missing presigned URL will not come back.
    pub fn is_permanent(&self) -> bool {
        (400..500).contains(&self.status) && !matches!(self.status, 408 | 429)
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
        let id = py_str(
            first("id", "job_id").ok_or_else(|| JobError::new("job missing id", "invalid_job"))?,
        );
        // The id names queue/<id>.json and processed/<id>: reject it before
        // any file I/O or ack.
        if !valid_job_id(&id) {
            return Err(JobError::new(
                format!("invalid job id {}", shown_id(&id)),
                "invalid_job",
            ));
        }
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
            id,
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

    /// The job without its content or original payload: all a status report
    /// needs, and cheap to hold while CUPS works through a long backlog.
    fn without_content(&self) -> PrintJob {
        PrintJob {
            id: self.id.clone(),
            cups_name: self.cups_name.clone(),
            content_type: self.content_type.clone(),
            content: String::new(),
            title: self.title.clone(),
            printer_id: self.printer_id.clone(),
            options: self.options.clone(),
            raw: JsonObject::new(),
        }
    }
}

/// Checks for jobs delivered by the cloud (REST pull or ActionCable push),
/// before anything is queued: `local_path` content would let the control plane
/// print any file the agent can read (credentials.json included), so only the
/// local CLI print-test ([`job_from_local_file`]) may use it.
pub fn check_remote_job(job: &PrintJob) -> Result<(), JobError> {
    if job.content_type == "local_path" {
        return Err(JobError::new(
            "content_type local_path is only accepted from the local CLI",
            "unsupported_content",
        ));
    }
    Ok(())
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
    /// Cut short because the agent is stopping ([`Pipeline::stop`]): the job
    /// keeps its queue record, and the next start prints it, or (once `lp`
    /// has it) finishes following it in CUPS.
    Interrupted,
}

impl JobOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            JobOutcome::Printed => "printed",
            JobOutcome::Delivered => "delivered",
            JobOutcome::Interrupted => "interrupted",
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
    /// Hard timeout, CUPS queries failed repeatedly, or the wait was cut
    /// short because the agent is stopping ([`WaitCtx::stop`]).
    Unknown,
}

/// Where CUPS has a job now ([`Pipeline::cups_lookup`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CupsJobState {
    /// Pending, held or printing (`lpstat -W not-completed` lists it).
    Active,
    /// Completed (`lpstat -W completed`, with no cancel/abort reason).
    Printed,
    /// Canceled or aborted.
    Failed,
    /// In neither list: CUPS no longer knows the job.
    Forgotten,
}

/// What a CUPS wait ([`WaitFn`]) gets besides the request id.
pub struct WaitCtx {
    /// Called on each poll that finds the job still active
    /// ([`Pipeline::on_wait_tick`]).
    pub tick: Option<TickFn>,
    /// Once set, the wait gives up and returns [`CupsOutcome::Unknown`].
    pub stop: Arc<AtomicBool>,
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
pub type WaitFn = Arc<dyn Fn(&str, &WaitCtx) -> CupsOutcome + Send + Sync>;
/// Where CUPS has the job with this request key, or `Err` when CUPS could
/// not be asked. See [`lookup_cups_job`].
pub type CupsLookupFn = Arc<dyn Fn(&str) -> Result<CupsJobState, String> + Send + Sync>;
pub type RawProbeFn = Arc<dyn Fn(&str) -> Result<bool, BoxError> + Send + Sync>;
/// One CUPS round for a set of request keys (`Printer-N`): the final outcome
/// of every key that has left the not-completed list (keys still active are
/// absent), or `Err` when CUPS could not be queried. See [`poll_cups_jobs`].
pub type CupsPollFn =
    Arc<dyn Fn(&[String]) -> Result<HashMap<String, CupsOutcome>, String> + Send + Sync>;
/// Starts a named background thread. Injectable so tests can make the OS
/// "refuse" a thread; production uses [`spawn_thread`].
pub type SpawnFn = Arc<dyn Fn(&str, Box<dyn FnOnce() + Send>) -> io::Result<()> + Send + Sync>;

/// [`SpawnFn`] backed by `thread::Builder`: a refused thread is an `Err`, never
/// a panic.
pub fn spawn_thread(name: &str, body: Box<dyn FnOnce() + Send>) -> io::Result<()> {
    thread::Builder::new()
        .name(name.to_string())
        .spawn(body)
        .map(|_| ())
}

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

/// `lp`'s complaint when the destination queue does not exist
/// ("lp: Error - The printer or class does not exist.").
const UNKNOWN_QUEUE_MARKERS: &[&str] = &[
    "printer or class does not exist",
    "printer or class was not found",
    "printer or class not found",
];

/// JobError for a failed `lp` run: `unknown_queue` (permanent) when CUPS has
/// no such destination, else `lp_error` (retryable).
fn lp_failure(text: &str) -> JobError {
    let lower = text.to_lowercase();
    let code = if UNKNOWN_QUEUE_MARKERS.iter().any(|m| lower.contains(m)) {
        "unknown_queue"
    } else {
        "lp_error"
    };
    JobError::new(if text.is_empty() { "lp failed" } else { text }, code)
}

/// CUPS canceled or aborted request `cid` (`cups_job_failed`, permanent).
fn cups_job_failed(cid: &str) -> JobError {
    JobError::new(format!("CUPS job {cid} failed"), "cups_job_failed")
}

/// Submit a file to CUPS via `lp`.
///
/// When `raw` is set, passes `-o raw` so CUPS does not filter/transform
/// the payload (required for ZPL/EPL on raw thermal queues).
pub fn default_lp(cups_name: &str, path: &Path, args: &LpArgs) -> Result<Option<String>, JobError> {
    run_lp(&["lp"], cups_name, path, args)
}

/// [`default_lp`] with `lp` run as `cmd` (its program and leading
/// arguments). It runs untranslated ([`run_cups`]): the request id and the
/// unknown-queue markers are read in English, and in German `lp` says
/// "Anfrage-ID ist …", so no job got its CUPS id.
fn run_lp(
    cmd: &[&str],
    cups_name: &str,
    path: &Path,
    args: &LpArgs,
) -> Result<Option<String>, JobError> {
    let (program, lead) = cmd.split_first().expect("lp command");
    let argv = lp_args(cups_name, path, args);
    let argv: Vec<&str> = lead
        .iter()
        .copied()
        .chain(argv.iter().map(String::as_str))
        .collect();
    let out = run_cups(program, &argv, Duration::from_secs(60))
        .map_err(|e| JobError::new(format!("lp failed: {e}"), "lp_error"))?;
    if !out.success {
        let err = if !out.stderr.trim().is_empty() {
            &out.stderr
        } else {
            &out.stdout
        };
        return Err(lp_failure(err.trim()));
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
/// Consecutive failed CUPS queries before a job is left `delivered`.
const MAX_CUPS_QUERY_FAILURES: u32 = 5;
/// Most jobs one [`CupsWatcher`] follows; past that the oldest is left delivered.
const MAX_WATCHED_JOBS: usize = 1000;

/// Runs `lpstat` with the given arguments.
type Lpstat = dyn Fn(&[&str]) -> io::Result<CmdOutput>;

/// `lpstat`, untranslated ([`run_cups`]): CUPS translates the labels of its
/// output into the locale's language ("Alerts:" is "Alarme:" in German),
/// and [`completed_outcome`] reads the untranslated ones.
fn run_lpstat(args: &[&str]) -> io::Result<CmdOutput> {
    run_cups("lpstat", args, Duration::from_secs(15))
}

/// stdout of a successful `lpstat` run. A run that fails to start, times out
/// or exits non-zero ("lpstat: Scheduler is not running.") is an `Err`, so
/// error text is never read as an (empty) job listing.
fn lpstat_listing(lpstat: &Lpstat, args: &[&str]) -> Result<String, String> {
    match lpstat(args) {
        Ok(out) if out.success => Ok(out.stdout),
        Ok(out) => {
            let why = if out.stderr.trim().is_empty() {
                out.stdout
            } else {
                out.stderr
            };
            Err(format!(
                "lpstat {} failed: {}",
                args.join(" "),
                why.trim().lines().next().unwrap_or("non-zero exit")
            ))
        }
        Err(e) => Err(format!("lpstat {}: {e}", args.join(" "))),
    }
}

/// Request ids listed by `lpstat -W …`: the first token of each unindented
/// line (long-format detail lines are indented).
fn listed_job_ids(listing: &str) -> HashSet<&str> {
    listing
        .lines()
        .filter(|l| !l.starts_with(char::is_whitespace))
        .filter_map(|l| l.split_whitespace().next())
        .collect()
}

/// Outcome of `job_key` from `lpstat -W completed -l`. Only that job's own
/// block is read (its header line, first token exactly `job_key`, and the
/// indented lines under it), so a canceled or aborted neighbour can never
/// mark a printed job failed. And in that block only the `Alerts:` line,
/// the job's state reasons (`job-canceled-by-user`, `aborted-by-system`, …):
/// the header's user name and the `Status:` line (a free-text printer
/// message) may say "canceled" about anything. No failure reason, or no
/// block for the job → `Printed`.
fn completed_outcome(done_out: &str, job_key: &str) -> CupsOutcome {
    completed_block_outcome(done_out, job_key).unwrap_or(CupsOutcome::Printed)
}

/// [`completed_outcome`], or `None` when the listing has no block for
/// `job_key`.
fn completed_block_outcome(done_out: &str, job_key: &str) -> Option<CupsOutcome> {
    let mut in_block = false;
    for line in done_out.lines() {
        if !line.starts_with(char::is_whitespace) {
            if in_block {
                break; // next job's block
            }
            in_block = line.split_whitespace().next() == Some(job_key);
            continue;
        }
        if !in_block {
            continue;
        }
        let Some(reasons) = line.trim_start().strip_prefix("Alerts:") else {
            continue;
        };
        let failed = reasons.split_whitespace().map(str::to_lowercase).any(|r| {
            ["canceled", "cancelled", "aborted"]
                .iter()
                .any(|k| r.contains(k))
        });
        if failed {
            return Some(CupsOutcome::Error);
        }
    }
    // Still set when the job's block was found (the loop stops in it).
    in_block.then_some(CupsOutcome::Printed)
}

/// Where CUPS has the job `job_key` ([`CupsJobState`]): `lpstat -W
/// not-completed`, then, when it is not listed there, `lpstat -W completed
/// -l`.
fn lookup_cups_job(lpstat: &Lpstat, job_key: &str) -> Result<CupsJobState, String> {
    let active = lpstat_listing(lpstat, &["-W", "not-completed"])?;
    if listed_job_ids(&active).contains(job_key) {
        return Ok(CupsJobState::Active);
    }
    let done = lpstat_listing(lpstat, &["-W", "completed", "-l"])?;
    Ok(match completed_block_outcome(&done, job_key) {
        Some(CupsOutcome::Error) => CupsJobState::Failed,
        Some(_) => CupsJobState::Printed,
        None => CupsJobState::Forgotten,
    })
}

/// One CUPS round for `keys`: `lpstat -W not-completed`, then (only if some
/// key has left it) one `lpstat -W completed -l` for all of them.
fn query_cups(lpstat: &Lpstat, keys: &[String]) -> Result<HashMap<String, CupsOutcome>, String> {
    let active = lpstat_listing(lpstat, &["-W", "not-completed"])?;
    let active = listed_job_ids(&active);
    let gone: Vec<&String> = keys
        .iter()
        .filter(|k| !active.contains(k.as_str()))
        .collect();
    if gone.is_empty() {
        return Ok(HashMap::new());
    }
    // A failed completed query is retried like any other query failure: the
    // job left the active list, but we don't know how yet.
    let done = lpstat_listing(lpstat, &["-W", "completed", "-l"])?;
    Ok(gone
        .into_iter()
        .map(|k| (k.clone(), completed_outcome(&done, k)))
        .collect())
}

/// Default [`CupsPollFn`]: one [`query_cups`] round against the real `lpstat`.
pub fn poll_cups_jobs(keys: &[String]) -> Result<HashMap<String, CupsOutcome>, String> {
    query_cups(&run_lpstat, keys)
}

/// Poll CUPS until the job leaves the active (not-completed) queue, or
/// `ctx.stop` is set.
pub fn wait_cups_job(
    request_id: &str,
    timeout: Duration,
    poll: Duration,
    ctx: &WaitCtx,
) -> CupsOutcome {
    wait_cups_job_with(request_id, timeout, poll, ctx, &run_lpstat)
}

/// Sleep `pause`, or less once `stop` is set.
fn nap(pause: Duration, stop: &AtomicBool) {
    let until = Instant::now() + pause;
    while !stop.load(Ordering::SeqCst) {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        thread::sleep(left.min(Duration::from_millis(100)));
    }
}

fn wait_cups_job_with(
    request_id: &str,
    timeout: Duration,
    poll: Duration,
    ctx: &WaitCtx,
    lpstat: &Lpstat,
) -> CupsOutcome {
    let Some(job_key) = request_id.split_whitespace().next() else {
        return CupsOutcome::Unknown;
    };
    let keys = [job_key.to_string()];
    let started = Instant::now();
    let deadline = started + timeout.max(Duration::from_secs(30));
    let mut failures = 0;
    let mut last_log: Option<Instant> = None;

    while Instant::now() < deadline {
        // A stop (SIGTERM) must not wait out a printer that is out of paper.
        if ctx.stop.load(Ordering::SeqCst) {
            log::info!(target: LOG, "stopping — no longer waiting on CUPS job {job_key}");
            return CupsOutcome::Unknown;
        }
        // A panic (a thread `lpstat` needs is refused) is a failed query.
        let polled = catch_panic(|| query_cups(lpstat, &keys))
            .unwrap_or_else(|msg| Err(format!("CUPS query panicked: {msg}")));
        match polled {
            Ok(done) => {
                failures = 0;
                if let Some(outcome) = done.get(job_key) {
                    return *outcome;
                }
            }
            Err(e) => {
                failures += 1;
                log::debug!(target: LOG, "CUPS query for {job_key} failed: {e}");
                if failures >= MAX_CUPS_QUERY_FAILURES {
                    log::warn!(target: LOG, "CUPS lpstat failed {failures} times for {job_key} — leaving delivered");
                    return CupsOutcome::Unknown;
                }
                nap(poll.max(Duration::from_millis(500)), &ctx.stop);
                continue;
            }
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
        if let Some(tick) = &ctx.tick {
            if let Err(msg) = catch_panic(|| tick()) {
                log::warn!(target: LOG, "CUPS wait tick for {job_key} panicked: {msg}");
            }
        }
        nap(poll.max(Duration::from_millis(250)), &ctx.stop);
    }
    log::warn!(
        target: LOG,
        "CUPS job {job_key} still active after {:.0}s — leaving delivered",
        timeout.as_secs_f64()
    );
    CupsOutcome::Unknown
}

/// A job handed to the [`CupsWatcher`] after `lp` accepted it.
struct WatchedJob {
    /// The job minus its content (only its identity is reported).
    job: PrintJob,
    cups_id: String,
    /// First token of `cups_id`, as `lpstat` lists it.
    key: String,
    report: StateFn,
    since: Instant,
}

#[derive(Default)]
struct WatchList {
    jobs: Vec<WatchedJob>,
    /// A watcher thread is alive; it exits once `jobs` is empty.
    running: bool,
}

/// Follows every `WaitCups::Async` job after `lp` on **one** background thread:
/// each poll is a single `lpstat -W not-completed` (plus one
/// `lpstat -W completed -l` when something finished) however many jobs are
/// outstanding, so a printer that is out of paper for hours costs one query
/// every couple of seconds instead of a thread and a query per label.
///
/// The thread starts on demand and exits when nothing is left to follow. Each
/// job is reported printed / error through the hook it was registered with,
/// or left `delivered` after `max_wait` or repeated CUPS query failures.
pub struct CupsWatcher {
    poll: CupsPollFn,
    interval: Duration,
    max_wait: Duration,
    spawn: SpawnFn,
    list: Mutex<WatchList>,
}

impl Default for CupsWatcher {
    /// Real `lpstat`, polled every 2 s, each job followed for up to 24 h.
    fn default() -> Self {
        CupsWatcher::new(
            Arc::new(poll_cups_jobs),
            Duration::from_secs(2),
            DEFAULT_CUPS_WAIT,
        )
    }
}

impl CupsWatcher {
    pub fn new(poll: CupsPollFn, interval: Duration, max_wait: Duration) -> Self {
        CupsWatcher {
            poll,
            interval,
            max_wait,
            spawn: Arc::new(spawn_thread),
            list: Mutex::default(),
        }
    }

    /// Replace how the watcher thread is started (tests).
    pub fn with_spawn(mut self, spawn: SpawnFn) -> Self {
        self.spawn = spawn;
        self
    }

    /// Jobs currently being followed.
    pub fn pending(&self) -> usize {
        lock(&self.list).jobs.len()
    }

    /// Follow `cups_id` until CUPS finishes it, then report `printed` or
    /// `error` through `report`. Returns false — the job stays `delivered`,
    /// logged — when the watcher thread could not be started.
    pub fn watch(self: &Arc<Self>, job: &PrintJob, cups_id: &str, report: StateFn) -> bool {
        let Some(key) = cups_id.split_whitespace().next().map(String::from) else {
            return false;
        };
        let entry = WatchedJob {
            job: job.without_content(),
            cups_id: cups_id.to_string(),
            key,
            report,
            since: Instant::now(),
        };
        let start_thread = {
            let mut list = lock(&self.list);
            if list.jobs.len() >= MAX_WATCHED_JOBS {
                let oldest = list.jobs.remove(0);
                log::warn!(
                    target: LOG,
                    "following {MAX_WATCHED_JOBS} CUPS jobs — job {} ({}) left delivered",
                    oldest.job.id,
                    oldest.cups_id
                );
            }
            list.jobs.push(entry);
            !std::mem::replace(&mut list.running, true)
        };
        if !start_thread {
            return true;
        }
        let this = Arc::clone(self);
        match (self.spawn)("vesyl-print-cups-wait", Box::new(move || this.run())) {
            Ok(()) => true,
            Err(e) => {
                let dropped = {
                    let mut list = lock(&self.list);
                    list.running = false;
                    std::mem::take(&mut list.jobs)
                };
                for w in dropped {
                    log::error!(
                        target: LOG,
                        "CUPS watcher failed to start ({e}) — job {} left delivered",
                        w.job.id
                    );
                }
                false
            }
        }
    }

    fn run(self: Arc<Self>) {
        // Should this thread die anyway, let the next job start a new one.
        struct Exit<'a>(&'a CupsWatcher);
        impl Drop for Exit<'_> {
            fn drop(&mut self) {
                if thread::panicking() {
                    lock(&self.0.list).running = false;
                }
            }
        }
        let _exit = Exit(&self);

        let mut failures = 0u32;
        let mut last_log = Instant::now();
        loop {
            let keys: Vec<String> = {
                let mut list = lock(&self.list);
                if list.jobs.is_empty() {
                    list.running = false;
                    return;
                }
                let mut keys: Vec<String> = list.jobs.iter().map(|w| w.key.clone()).collect();
                keys.sort();
                keys.dedup();
                keys
            };
            let polled = catch_panic(|| (self.poll)(&keys))
                .unwrap_or_else(|msg| Err(format!("CUPS query panicked: {msg}")));
            for (w, outcome) in self.settle(&keys, polled, &mut failures) {
                report_cups_outcome(w, outcome);
            }
            if last_log.elapsed() >= CUPS_WAIT_LOG_EVERY {
                let list = lock(&self.list);
                if let Some(oldest) = list.jobs.iter().min_by_key(|w| w.since) {
                    log::info!(
                        target: LOG,
                        "{} CUPS job(s) still active; oldest {} after {:.0}s (waiting for printer)",
                        list.jobs.len(),
                        oldest.key,
                        oldest.since.elapsed().as_secs_f64()
                    );
                }
                last_log = Instant::now();
            }
            thread::sleep(self.interval);
        }
    }

    /// Apply one poll result. Returns (and removes) the jobs that are done:
    /// finished in CUPS, past `max_wait`, or given up after repeated query
    /// failures (`Unknown`). Jobs added during the poll are kept for the next.
    fn settle(
        &self,
        polled_keys: &[String],
        polled: Result<HashMap<String, CupsOutcome>, String>,
        failures: &mut u32,
    ) -> Vec<(WatchedJob, CupsOutcome)> {
        let give_up = match &polled {
            Ok(_) => {
                *failures = 0;
                false
            }
            Err(e) => {
                *failures += 1;
                log::debug!(target: LOG, "CUPS query failed: {e}");
                *failures >= MAX_CUPS_QUERY_FAILURES
            }
        };
        if give_up {
            log::warn!(
                target: LOG,
                "CUPS lpstat failed {failures} times — leaving {} job(s) delivered",
                polled_keys.len()
            );
            *failures = 0;
        }
        let mut list = lock(&self.list);
        let mut done = Vec::new();
        for w in std::mem::take(&mut list.jobs) {
            let outcome = match &polled {
                Ok(finished) => finished.get(&w.key).copied(),
                Err(_) if give_up && polled_keys.contains(&w.key) => Some(CupsOutcome::Unknown),
                Err(_) => None,
            };
            match outcome {
                Some(o) => done.push((w, o)),
                None if w.since.elapsed() >= self.max_wait => done.push((w, CupsOutcome::Unknown)),
                None => list.jobs.push(w),
            }
        }
        done
    }
}

/// Send one job status through `report`. A failure, or a panic (the status
/// call needing a thread the OS refuses), is logged and goes no further: a
/// status update never fails a job, least of all after `lp` accepted it.
fn send_state(report: &StateFn, job: &PrintJob, state: JobState, detail: Option<&str>) {
    match catch_panic(|| report(job, state, detail)) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => log::debug!(target: LOG, "report_state {} failed: {e}", state.as_str()),
        Err(msg) => log::error!(
            target: LOG,
            "report_state {} panicked for job {}: {msg}",
            state.as_str(),
            job.id
        ),
    }
}

fn report_cups_outcome(w: WatchedJob, outcome: CupsOutcome) {
    let send = |state: JobState, detail: &str| send_state(&w.report, &w.job, state, Some(detail));
    match outcome {
        CupsOutcome::Printed => send(JobState::Printed, &w.cups_id),
        CupsOutcome::Error => send(JobState::Error, &format!("CUPS job {} failed", w.cups_id)),
        CupsOutcome::Unknown => log::info!(
            target: LOG,
            "job {} CUPS tracking timed out for {} — left delivered",
            w.job.id,
            w.cups_id
        ),
    }
}

/// The key of a queue record under which the agent keeps its own notes on
/// the job, beside the cloud's payload ([`LocalState`]). An older agent,
/// reading the record after a rollback, ignores it as it ignores any key it
/// does not know (Python's `PrintJob.from_dict` as well as ours keep the
/// whole payload in `raw` and read only theirs).
const LOCAL_KEY: &str = "_agent";

/// Runs of conversion and `lp` for one job that the agent died in (out of
/// memory, an allocation abort no `catch_unwind` stops) before the job is
/// retired as [`CRASH_LOOP`]: each restart (systemd's, 5 s later) would run
/// it again, and kill the agent again, before it took any other job.
pub const MAX_ATTEMPTS: u32 = 3;

/// What a queue record says about earlier runs of its job ([`LOCAL_KEY`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct LocalState {
    /// Runs of conversion and `lp` that never returned: the agent died in
    /// them. Written before each run, and taken back once it returns.
    attempts: u32,
    /// Set once `lp` has accepted the job.
    submitted: Option<Submission>,
}

/// `lp` accepted the job: CUPS has it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Submission {
    /// The CUPS request (`Zebra-42`), when `lp` named one.
    cups_job_id: Option<String>,
    submitted_at: String,
}

impl LocalState {
    /// From a record's [`LOCAL_KEY`] value; anything unreadable counts as
    /// no notes at all.
    fn from_value(v: Option<&Value>) -> LocalState {
        let Some(Value::Object(o)) = v else {
            return LocalState::default();
        };
        let text = |k: &str| {
            o.get(k)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
        };
        LocalState {
            attempts: o
                .get("attempts")
                .and_then(py_int)
                .map_or(0, |n| n.clamp(0, i64::from(u32::MAX)) as u32),
            submitted: text("submitted_at").map(|submitted_at| Submission {
                cups_job_id: text("cups_job_id"),
                submitted_at,
            }),
        }
    }

    fn to_value(&self) -> Value {
        let mut v = json!({ "attempts": self.attempts });
        if let Some(s) = &self.submitted {
            v["cups_job_id"] = json!(s.cups_job_id);
            v["submitted_at"] = json!(s.submitted_at);
        }
        v
    }
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

    /// Create queue/ and processed/. One that root (an operator's CLI run)
    /// has to create goes to the owner of the closest directory above it, as
    /// the files root writes there do, so the non-root agent can still use it.
    pub fn ensure(&self) -> std::io::Result<()> {
        crate::util::create_dir_all_owned(&self.queue_dir)?;
        crate::util::create_dir_all_owned(&self.processed_dir)
    }

    pub fn queue_path(&self, job_id: &str) -> PathBuf {
        self.queue_dir.join(format!("{job_id}.json"))
    }

    pub fn processed_path(&self, job_id: &str) -> PathBuf {
        self.processed_dir.join(job_id)
    }

    /// Where permanently failed queue files go (`queue/failed/`). Not a
    /// `*.json` file in queue/, so it is never listed, drained or counted
    /// (old Python slots glob `queue/*.json` the same way).
    pub fn failed_dir(&self) -> PathBuf {
        self.queue_dir.join("failed")
    }

    pub fn is_processed(&self, job_id: &str) -> bool {
        valid_job_id(job_id) && self.processed_path(job_id).is_file()
    }

    pub fn has_queue_file(&self, job_id: &str) -> bool {
        valid_job_id(job_id) && self.queue_path(job_id).is_file()
    }

    /// Write job JSON with fsync (file + dir). Idempotent if file already exists.
    pub fn write_queue(&self, job: &PrintJob) -> std::io::Result<PathBuf> {
        if !valid_job_id(&job.id) {
            return Err(invalid_id_error());
        }
        self.ensure()?;
        let path = self.queue_path(&job.id);
        if path.is_file() {
            return Ok(path);
        }
        // A new record has no history here, whatever its payload carries.
        let mut data = job.to_dict();
        data.remove(LOCAL_KEY);
        let mut raw = serde_json::to_string_pretty(&data).map_err(std::io::Error::other)?;
        raw.push('\n');
        write_durable(&path, raw.as_bytes(), 0o600, true)?;
        Ok(path)
    }

    /// What queue/<job_id>.json records about earlier runs of the job:
    /// nothing for a record without notes, or one that cannot be read. Only
    /// the notes are kept from the file, not its content.
    fn local_state(&self, job_id: &str) -> LocalState {
        #[derive(Deserialize)]
        struct Notes {
            // LOCAL_KEY (an attribute takes a literal only).
            #[serde(rename = "_agent")]
            notes: Option<Value>,
        }
        if !valid_job_id(job_id) {
            return LocalState::default();
        }
        fs::read(self.queue_path(job_id))
            .ok()
            .and_then(|raw| serde_json::from_slice::<Notes>(&raw).ok())
            .map(|n| LocalState::from_value(n.notes.as_ref()))
            .unwrap_or_default()
    }

    /// Rewrite queue/<job.id>.json as `job` with the notes `local`, as
    /// durably as [`JobStore::write_queue`] (a new file, fsynced, renamed
    /// over the old one, directory fsynced).
    fn write_local(&self, job: &PrintJob, local: &LocalState) -> std::io::Result<()> {
        if !valid_job_id(&job.id) {
            return Err(invalid_id_error());
        }
        let mut data = job.to_dict();
        data.insert(LOCAL_KEY.into(), local.to_value());
        let mut raw = serde_json::to_string_pretty(&data).map_err(std::io::Error::other)?;
        raw.push('\n');
        write_durable(&self.queue_path(&job.id), raw.as_bytes(), 0o600, true)
    }

    /// Write the processed/<job_id> marker (a timestamp). It goes through
    /// [`write_durable`]: the service user owns processed/, and a symlink it
    /// put at that name is replaced by the rename, never written or chmodded
    /// through, when root (an operator's print-test) writes the marker; root
    /// also hands the marker to the directory's owner.
    pub fn mark_processed(&self, job_id: &str) -> std::io::Result<()> {
        if !valid_job_id(job_id) {
            return Err(invalid_id_error());
        }
        self.ensure()?;
        let stamp = utc_now_iso() + "\n";
        write_durable(&self.processed_path(job_id), stamp.as_bytes(), 0o644, false)
    }

    pub fn delete_queue(&self, job_id: &str) {
        if valid_job_id(job_id) {
            let _ = fs::remove_file(self.queue_path(job_id));
        }
    }

    /// Remove `queue/failed/<job_id>.json`, left by an earlier permanent
    /// failure, once the job has finished after all (say a redelivery
    /// printed it to a queue that was fixed meanwhile).
    pub fn delete_failed(&self, job_id: &str) {
        if !valid_job_id(job_id) {
            return;
        }
        if let Ok(dir) = NoFollowDir::open(&self.failed_dir()) {
            let _ = dir.remove(format!("{job_id}.json").as_ref());
        }
    }

    /// Move `queue/<name>.json` to `queue/failed/<name>.json` (atomic rename,
    /// both directories fsynced) and set its mtime to now, so
    /// [`JobStore::prune_failed`] keeps it for the full retention after the
    /// failure. `name` is a queue file stem, normally the job id. Returns the
    /// new path, or `None` when there was no queue file.
    ///
    /// The service user owns queue/, so when root runs this (an operator's
    /// print-test) it could put a symlink where failed/ is, beforehand or
    /// right after root makes it. failed/ is created with
    /// [`crate::util::create_dir_all_owned`], which hands a new one to
    /// queue/'s owner through a descriptor, never by path; then it is opened
    /// without following a symlink and the file is moved relative to that
    /// descriptor. A link there fails the call: nothing is chowned in, or
    /// moved into, its target, and the queue file stays where it was.
    pub fn retire_queue(&self, name: &str) -> std::io::Result<Option<PathBuf>> {
        self.retire_queue_with(name, &|_| {})
    }

    /// [`JobStore::retire_queue`]. `after_mkdir` runs once failed/ exists,
    /// so tests can swap it for a symlink the way the service user could.
    fn retire_queue_with(
        &self,
        name: &str,
        after_mkdir: &dyn Fn(&Path),
    ) -> std::io::Result<Option<PathBuf>> {
        if !plain_file_stem(name) {
            return Err(invalid_id_error());
        }
        let file = format!("{name}.json");
        let src = self.queue_dir.join(&file);
        if !src.is_file() {
            return Ok(None);
        }
        let dir = self.failed_dir();
        crate::util::create_dir_all_owned(&dir)?;
        after_mkdir(&dir);
        let failed = NoFollowDir::open(&dir)?;
        failed.rename_into(&src, &file)?;
        if let Err(e) = failed.touch(&file) {
            log::debug!(target: LOG, "could not restart the retention of failed/{file}: {e}");
        }
        if let Ok(f) = File::open(&self.queue_dir) {
            let _ = f.sync_all();
        }
        let _ = failed.0.sync_all();
        Ok(Some(dir.join(file)))
    }

    pub fn load_queued(&self, job_id: &str) -> Result<PrintJob, JobError> {
        let path = self.queue_dir.join(format!("{job_id}.json"));
        let corrupt = |detail: String| {
            JobError::new(
                format!("corrupt queue file {}{detail}", path.display()),
                "corrupt_queue",
            )
        };
        if !plain_file_stem(job_id) {
            return Err(corrupt(": bad file name".into()));
        }
        let raw = fs::read_to_string(&path).map_err(|e| corrupt(format!(": {e}")))?;
        let job = match serde_json::from_str::<Value>(&raw) {
            Ok(Value::Object(data)) => PrintJob::from_dict(&data)?,
            Ok(_) => return Err(corrupt(String::new())),
            Err(e) => return Err(corrupt(format!(": {e}"))),
        };
        // Processing finishes by deleting queue/<job.id>.json, so a file under
        // any other name could never leave the queue.
        if job.id != job_id {
            return Err(corrupt(format!(
                ": holds job {} (file name does not match)",
                shown_id(&job.id)
            )));
        }
        Ok(job)
    }

    /// Stems of the `*.json` files directly in queue/ (sorted). Dot-files,
    /// directories (`failed/`) and temp files are not listed.
    pub fn list_queued_ids(&self) -> Vec<String> {
        let _ = self.ensure();
        let mut ids: Vec<String> = fs::read_dir(&self.queue_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_file())
            .filter_map(|e| {
                e.file_name()
                    .to_str()
                    .filter(|n| !n.starts_with('.'))
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

    /// Delete processed markers older than `max_age` (by mtime) and return how
    /// many went. A marker whose queue file still exists is kept: the startup
    /// drain relies on it to skip re-printing a job that finished just before
    /// a crash. Per-file errors are ignored, and a symlink at processed/ is
    /// not followed (see [`prune_files`]).
    ///
    /// The same pass prunes `queue/failed/` with the same retention (see
    /// [`JobStore::prune_failed`]); those are logged here, not counted.
    pub fn prune_processed(&self, max_age: Duration) -> usize {
        let failed = self.prune_failed(max_age);
        if failed > 0 {
            log::info!(
                target: LOG,
                "pruned {failed} file(s) from {} older than {} day(s)",
                self.failed_dir().display(),
                max_age.as_secs() / (24 * 60 * 60)
            );
        }
        prune_files(&self.processed_dir, max_age, |name| {
            let queued = name
                .to_str()
                .is_some_and(|n| self.queue_dir.join(format!("{n}.json")).exists());
            !queued
        })
    }

    /// Delete `queue/failed/*.json` older than `max_age` (by mtime, which
    /// [`JobStore::retire_queue`] sets when it retires a file) and return how
    /// many went. Every permanently failed job keeps its whole payload there,
    /// inline base64 content included, so without this the directory only
    /// grows. Per-file errors are ignored, and a symlink at failed/ is not
    /// followed (see [`prune_files`]).
    pub fn prune_failed(&self, max_age: Duration) -> usize {
        prune_files(&self.failed_dir(), max_age, |name| {
            name.as_bytes().ends_with(b".json")
        })
    }
}

/// Delete the regular files directly in `dir` that were last modified at
/// least `max_age` ago and whose names `prunable` accepts; return how many
/// went. Per-file errors are ignored.
///
/// The service user owns the state directory and queue/, so it can put a
/// symlink where processed/ or queue/failed/ is (to /etc, say) for an agent
/// started as root to prune through. So `dir` is opened without following
/// one (a link there makes this a no-op), and each file is checked and
/// unlinked relative to that descriptor. The names come from listing `dir`
/// by path: a link swapped in after the open only changes which names are
/// tried, never where they are checked or removed.
fn prune_files(dir: &Path, max_age: Duration, prunable: impl Fn(&OsStr) -> bool) -> usize {
    prune_files_with(dir, max_age, prunable, &|_| {})
}

/// [`prune_files`]. `after_open` runs once `dir` is open, so tests can swap
/// it for a symlink the way the service user could.
fn prune_files_with(
    dir: &Path,
    max_age: Duration,
    prunable: impl Fn(&OsStr) -> bool,
    after_open: &dyn Fn(&Path),
) -> usize {
    let Ok(opened) = NoFollowDir::open(dir) else {
        return 0;
    };
    after_open(dir);
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if opened
            .metadata(&name)
            .is_ok_and(|m| expired_file(&m, now, max_age))
            && prunable(&name)
            && opened.remove(&name).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

/// True for a regular file (`meta` from lstat: a symlink is not one) last
/// modified at least `max_age` before `now`.
fn expired_file(meta: &fs::Metadata, now: SystemTime, max_age: Duration) -> bool {
    meta.is_file()
        && meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age >= max_age)
}

/// A directory the service user owns (processed/, queue/failed/), opened
/// without following a symlink at its name: that user could put one there
/// while root (an operator's print-test, or an agent started as root) works
/// in it. Names in it are resolved relative to this descriptor, never
/// through the path again.
struct NoFollowDir(File);

impl NoFollowDir {
    /// Open the directory; a symlink there fails (ENOTDIR or ELOOP).
    fn open(path: &Path) -> io::Result<Self> {
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)
            .map(NoFollowDir)
    }

    /// rename(2) `src` to `name` in this directory.
    fn rename_into(&self, src: &Path, name: &str) -> io::Result<()> {
        let from = CString::new(src.as_os_str().as_bytes())?;
        let to = CString::new(name)?;
        // SAFETY: the descriptor is open and both strings are NUL-terminated.
        let rc = unsafe {
            libc::renameat(
                libc::AT_FDCWD,
                from.as_ptr(),
                self.0.as_raw_fd(),
                to.as_ptr(),
            )
        };
        os_result(rc)
    }

    /// Set `name`'s times to now (a symlink's own, never its target's).
    fn touch(&self, name: &str) -> io::Result<()> {
        let name = CString::new(name)?;
        // SAFETY: as above; a null `times` means "now" for both.
        let rc = unsafe {
            libc::utimensat(
                self.0.as_raw_fd(),
                name.as_ptr(),
                std::ptr::null(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        os_result(rc)
    }

    /// lstat(2) `name` in this directory: a symlink is described, never
    /// followed. Linux's O_PATH only locates the file without opening it (no
    /// read permission needed, a FIFO does not block), and with O_NOFOLLOW
    /// it locates a symlink itself.
    fn metadata(&self, name: &OsStr) -> io::Result<fs::Metadata> {
        let name = CString::new(name.as_bytes())?;
        let flags = libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: as above; without O_CREAT, openat reads no mode argument.
        let fd = unsafe { libc::openat(self.0.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` was just opened here and nothing else owns it.
        unsafe { File::from_raw_fd(fd) }.metadata()
    }

    /// unlink(2) `name` in this directory.
    fn remove(&self, name: &OsStr) -> io::Result<()> {
        let name = CString::new(name.as_bytes())?;
        // SAFETY: as above.
        let rc = unsafe { libc::unlinkat(self.0.as_raw_fd(), name.as_ptr(), 0) };
        os_result(rc)
    }
}

/// A libc return code as an `io::Result` (errno on failure).
fn os_result(rc: libc::c_int) -> io::Result<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
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
        let data = fetch_url(&job.content).map_err(|e| {
            // An expired / missing URL (4xx) won't recover; anything else might.
            let code = match e.downcast_ref::<HttpStatusError>() {
                Some(h) if h.is_permanent() => "content_rejected",
                _ => "content_fetch",
            };
            JobError::new(format!("fetch failed: {e}"), code)
        })?;
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

pub(crate) fn expand_user(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return Path::new(&home).join(rest);
        }
    }
    PathBuf::from(p)
}

/// Default content fetcher for `*_uri` jobs.
///
/// Timeouts like Python's urllib `timeout=60` ([`net::Timeouts::CONTENT`]):
/// a slow download that keeps making progress is not cut off at 60 s, and one
/// that goes silent fails after 60 s without data. Proxies follow urllib,
/// chosen again on every redirect hop. An HTTP error status comes back as
/// [`HttpStatusError`].
pub fn http_get(url: &str) -> Result<Vec<u8>, BoxError> {
    http_get_with(url, net::Timeouts::CONTENT)
}

fn http_get_with(url: &str, timeouts: net::Timeouts) -> Result<Vec<u8>, BoxError> {
    let mut resp = net::agent(url, timeouts, net::Redirects::Follow)
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
        return Err(Box::new(HttpStatusError { status }));
    }
    Ok(resp
        .body_mut()
        .with_config()
        .limit(MAX_CONTENT_BYTES)
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
    /// Invoked periodically while a `WaitCups::Sync` job waits on CUPS
    /// completion (e.g. paper-out recovery), when the agent loop is blocked,
    /// so the agent can keep heartbeating and reporting printer inventory.
    pub on_wait_tick: Option<TickFn>,
    pub wait_cups: WaitCups,
    /// Blocking CUPS wait for `WaitCups::Sync`.
    pub wait_cups_job: WaitFn,
    /// Follows `WaitCups::Async` jobs; shared by every clone of the pipeline.
    pub cups_watcher: Arc<CupsWatcher>,
    /// Where CUPS has a job `lp` accepted before the agent stopped or died
    /// ([`Pipeline::resume`]).
    pub cups_lookup: CupsLookupFn,
    pub supports_raw: RawProbeFn,
    pub work_dir: Option<PathBuf>,
    /// Set when the agent is stopping (the agent's stop flag; never, by
    /// default): the drain takes no further job, no job goes to `lp`, and a
    /// CUPS wait ends early. Each such job keeps its queue record for the
    /// next start ([`JobOutcome::Interrupted`]).
    pub stop: Arc<AtomicBool>,
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
            wait_cups_job: Arc::new(|id, ctx| {
                wait_cups_job(id, DEFAULT_CUPS_WAIT, Duration::from_secs(2), ctx)
            }),
            cups_watcher: Arc::new(CupsWatcher::default()),
            cups_lookup: Arc::new(|key| lookup_cups_job(&run_lpstat, key)),
            supports_raw: Arc::new(|q| Ok(crate::printers::queue_supports_raw(q, None))),
            work_dir: None,
            stop: Arc::default(),
        }
    }
}

/// Move a queue file that can never succeed out of queue/ (into
/// `queue/failed/`, kept for diagnosis) so it is not re-run on every start.
fn retire_queue_file(store: &JobStore, name: &str, err: &JobError) {
    match store.retire_queue(name) {
        Ok(Some(path)) => log::warn!(
            target: LOG,
            "job {name} failed permanently ({}) — queue file moved to {}",
            err.code,
            path.display()
        ),
        Ok(None) => {}
        Err(e) => {
            log::warn!(target: LOG, "could not move queue file of job {name} to failed/: {e}")
        }
    }
}

impl Pipeline {
    fn report(&self, job: &PrintJob, state: JobState, detail: Option<&str>) {
        send_state(&self.report_state, job, state, detail);
    }

    fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Report `e` as the job's error; a permanent one also retires its queue
    /// file to `queue/failed/`.
    fn failed(&self, job: &PrintJob, store: &JobStore, e: JobError) -> JobError {
        self.report(job, JobState::Error, Some(&e.message));
        log::error!(target: LOG, "job {} error: {}", job.id, e.message);
        if e.is_permanent() {
            retire_queue_file(store, &job.id, &e);
        }
        e
    }

    /// Run the full durable pipeline for one job.
    ///
    /// Returns `Printed`, `Delivered` (CUPS not tracked or tracked in the
    /// background), `Interrupted` (the agent is stopping), or a `JobError`
    /// after reporting `error` (a permanent one also moves the queue file to
    /// `queue/failed/`).
    ///
    /// A panic in a step (e.g. the OS refusing a thread that `lp` or a cloud
    /// call needs) is handled like Python's catch-all: the ack and status
    /// reports are best-effort as ever, a panic while materializing or
    /// printing is a retryable `job_error` (reported, temp files removed,
    /// queue file kept), and one while waiting on CUPS after `lp` accepted
    /// the job leaves it delivered.
    pub fn process(&self, job: &PrintJob, store: &JobStore) -> Result<JobOutcome, JobError> {
        let job_id = job.id.as_str();
        if !valid_job_id(job_id) {
            log::error!(target: LOG, "refusing job with invalid id {}", shown_id(job_id));
            return Err(JobError::new("invalid job id", "invalid_job"));
        }
        let io_err = |e: std::io::Error| JobError::new(e.to_string(), "job_error");
        store.ensure().map_err(io_err)?;

        // 1. Already finished — idempotent success (drop any leftover queue
        // file, and a failed/ copy from before it finished)
        if store.is_processed(job_id) {
            log::info!(target: LOG, "job {job_id} already processed — skip");
            store.delete_queue(job_id);
            store.delete_failed(job_id);
            self.report(job, JobState::Printed, Some("already_processed"));
            return Ok(JobOutcome::Printed);
        }

        // 2. Durable receive before any ack / print
        store.write_queue(job).map_err(io_err)?;

        // What an earlier run of this job left in its record.
        let local = store.local_state(job_id);
        if let Some(sub) = &local.submitted {
            // CUPS has it already: never `lp` it again.
            return self
                .resume(job, store, sub)
                .map_err(|e| self.failed(job, store, e));
        }
        if local.attempts >= MAX_ATTEMPTS {
            let e = JobError::new(
                format!(
                    "printing this job ended the agent {} times (out of memory?) — not tried again",
                    local.attempts
                ),
                CRASH_LOOP,
            );
            return Err(self.failed(job, store, e));
        }
        if self.stopping() {
            log::info!(target: LOG, "stopping — job {job_id} stays queued for the next start");
            return Ok(JobOutcome::Interrupted);
        }

        // 3. Ack only after disk durability. Non-fatal: cloud can redeliver.
        // An ack that panics did not happen either; the job still prints.
        match catch_panic(|| (self.ack)(job)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => log::warn!(target: LOG, "ack failed for job {job_id}: {e}"),
            Err(msg) => log::warn!(target: LOG, "ack failed for job {job_id}: panicked: {msg}"),
        }

        // 4–6. Materialize + submit + optional CUPS completion wait. The
        // attempt is on disk before the steps that could end the agent
        // (conversion, lp), so a job that keeps doing so is caught.
        self.report(job, JobState::Printing, None);
        let attempt = LocalState {
            attempts: local.attempts + 1,
            submitted: None,
        };
        if let Err(e) = store.write_local(job, &attempt) {
            let e = JobError::new(format!("could not record the attempt: {e}"), "job_error");
            return Err(self.failed(job, store, e));
        }
        let mut temp: Option<PathBuf> = None;
        let result = catch_panic(|| self.submit(job, store, &attempt, &mut temp))
            .unwrap_or_else(|msg| Err(JobError::new(format!("job panicked: {msg}"), "job_error")));

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

        // The agent lived through this attempt: take it back, unless the
        // record is gone (done, or retired) or CUPS has the job now.
        let kept = match &result {
            Ok(outcome) => *outcome == JobOutcome::Interrupted,
            Err(e) => !e.is_permanent(),
        };
        if kept {
            self.end_attempt(job, store, &local);
        }
        result.map_err(|e| self.failed(job, store, e))
    }

    /// Put back `before`, the record's notes from before an attempt that
    /// returned, unless `lp` took the job meanwhile (the notes say so).
    fn end_attempt(&self, job: &PrintJob, store: &JobStore, before: &LocalState) {
        if !store.has_queue_file(&job.id) || store.local_state(&job.id).submitted.is_some() {
            return;
        }
        if let Err(e) = store.write_local(job, before) {
            log::warn!(target: LOG, "job {}: could not take back the attempt: {e}", job.id);
        }
    }

    /// Steps 4–7; `temp` receives any temp file the caller must delete, and
    /// `attempt` is the record's notes for this run.
    fn submit(
        &self,
        job: &PrintJob,
        store: &JobStore,
        attempt: &LocalState,
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

        // A stop does not wait for one more label: the next start prints it.
        if self.stopping() {
            log::info!(target: LOG, "stopping — job {job_id} not sent to CUPS; it stays queued for the next start");
            return Ok(JobOutcome::Interrupted);
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
        // On disk before anything else: should the agent stop or die before
        // the job is marked processed, the next start must not print it again.
        let submitted = LocalState {
            submitted: Some(Submission {
                cups_job_id: cups_job.clone(),
                submitted_at: utc_now_iso(),
            }),
            ..attempt.clone()
        };
        if let Err(e) = store.write_local(job, &submitted) {
            log::warn!(target: LOG, "job {job_id}: could not record that CUPS has it ({e}) — a restart before it is marked processed prints it again");
        }
        self.report(job, JobState::Delivered, cups_job.as_deref());

        match self.follow(job, cups_job.as_deref())? {
            // CUPS has it, and its record says so: the next start follows it.
            JobOutcome::Interrupted => Ok(JobOutcome::Interrupted),
            outcome => self.finish(job, store, outcome),
        }
    }

    /// What follows `lp`: wait for CUPS to finish the job (`Sync`), hand it
    /// to the watcher (`Async`), or neither (`Off`). `Interrupted` when a
    /// stop cut the wait short.
    fn follow(&self, job: &PrintJob, cups_job: Option<&str>) -> Result<JobOutcome, JobError> {
        let job_id = job.id.as_str();
        let mut outcome = JobOutcome::Delivered;
        match (cups_job, self.wait_cups) {
            (Some(cid), WaitCups::Sync) => {
                let ctx = WaitCtx {
                    tick: self.on_wait_tick.clone(),
                    stop: self.stop.clone(),
                };
                // CUPS has the job: a panic while waiting leaves it delivered
                // (a job error would keep the queue file and print it again).
                match catch_panic(|| (self.wait_cups_job)(cid, &ctx)) {
                    Ok(CupsOutcome::Printed) => {
                        self.report(job, JobState::Printed, Some(cid));
                        outcome = JobOutcome::Printed;
                    }
                    Ok(CupsOutcome::Error) => return Err(cups_job_failed(cid)),
                    Ok(CupsOutcome::Unknown) if self.stopping() => {
                        log::info!(target: LOG, "stopping — job {job_id} is with CUPS ({cid}); the next start follows it");
                        outcome = JobOutcome::Interrupted;
                    }
                    Ok(CupsOutcome::Unknown) => {
                        log::info!(target: LOG, "job {job_id} CUPS tracking timed out for {cid} — left delivered");
                    }
                    Err(msg) => {
                        log::error!(target: LOG, "job {job_id} CUPS wait for {cid} panicked: {msg} — left delivered");
                    }
                }
            }
            (Some(cid), WaitCups::Async) => {
                // Reports printed/error later without blocking the next lp.
                self.cups_watcher.watch(job, cid, self.report_state.clone());
            }
            (Some(_), WaitCups::Off) => {}
            (None, _) => {
                log::info!(target: LOG, "job {job_id} no CUPS request id — left delivered")
            }
        }
        Ok(outcome)
    }

    /// Step 7: the processed marker, then the queue file goes.
    fn finish(
        &self,
        job: &PrintJob,
        store: &JobStore,
        outcome: JobOutcome,
    ) -> Result<JobOutcome, JobError> {
        let job_id = job.id.as_str();
        store
            .mark_processed(job_id)
            .map_err(|e| JobError::new(e.to_string(), "job_error"))?;
        store.delete_queue(job_id);
        // An earlier delivery may have failed permanently (say before its
        // queue was fixed); that copy describes a failure that is now moot.
        store.delete_failed(job_id);
        log::info!(target: LOG, "job {job_id} {} → {}", outcome.as_str(), job.cups_name);
        Ok(outcome)
    }

    /// Finish a job whose record says `lp` accepted it (`sub`): the agent
    /// stopped or died before marking it processed. Giving it to `lp` again
    /// would print a second label, so what is left is what follows `lp`,
    /// from where CUPS has the job now: still printing → report delivered
    /// and follow it as usual; finished → report printed, or the error;
    /// with `WaitCups::Off` or no request id → report delivered.
    ///
    /// A job CUPS no longer knows (its history purged, CUPS reset or
    /// reinstalled) is finished as delivered, without printing: `lp` took
    /// it, and whether it printed is lost with CUPS's record. Printing it
    /// again risks the very duplicate label this avoids, and reporting it
    /// printed or failed would claim what no one can tell; delivered is what
    /// the agent reports whenever it loses track of a job in CUPS. When CUPS
    /// cannot be asked (down), it is followed like a job just handed to
    /// `lp`, which gives up on CUPS the same way.
    fn resume(
        &self,
        job: &PrintJob,
        store: &JobStore,
        sub: &Submission,
    ) -> Result<JobOutcome, JobError> {
        let job_id = job.id.as_str();
        let cid = sub.cups_job_id.as_deref();
        log::info!(
            target: LOG,
            "job {job_id} went to CUPS ({}) at {} — finishing it without printing it again",
            cid.unwrap_or("no request id"),
            sub.submitted_at
        );
        let Some(cid) = cid.filter(|_| self.wait_cups != WaitCups::Off) else {
            self.report(job, JobState::Delivered, cid);
            return self.finish(job, store, JobOutcome::Delivered);
        };
        let key = cid.split_whitespace().next().unwrap_or(cid);
        let state = catch_panic(|| (self.cups_lookup)(key))
            .unwrap_or_else(|msg| Err(format!("CUPS query panicked: {msg}")));
        let outcome = match state {
            Ok(CupsJobState::Printed) => {
                self.report(job, JobState::Printed, Some(cid));
                JobOutcome::Printed
            }
            Ok(CupsJobState::Failed) => return Err(cups_job_failed(cid)),
            Ok(CupsJobState::Forgotten) => {
                log::warn!(target: LOG, "job {job_id}: CUPS no longer knows {cid} — left delivered");
                self.report(job, JobState::Delivered, Some(cid));
                JobOutcome::Delivered
            }
            Ok(CupsJobState::Active) | Err(_) => {
                if let Err(e) = &state {
                    log::warn!(target: LOG, "job {job_id}: {e} — following {cid} as if just sent");
                }
                self.report(job, JobState::Delivered, Some(cid));
                match self.follow(job, Some(cid))? {
                    JobOutcome::Interrupted => return Ok(JobOutcome::Interrupted),
                    outcome => outcome,
                }
            }
        };
        self.finish(job, store, outcome)
    }

    /// Process every queue/*.json (crash recovery). Returns `[(job_id, result)]`
    /// where result is `printed`, `delivered`, `interrupted` or
    /// `error:<code>`. Once [`Pipeline::stop`] is set it takes no further
    /// job: the rest stay queued for the next start.
    ///
    /// Files that cannot be loaded (corrupt, invalid, misnamed) can never
    /// succeed and are moved to `queue/failed/`; so are jobs that fail
    /// permanently (see [`JobError::is_permanent`]).
    pub fn drain(&self, store: &JobStore) -> Vec<(String, String)> {
        let _ = store.ensure();
        let mut results = Vec::new();
        for job_id in store.list_queued_ids() {
            if self.stopping() {
                log::info!(target: LOG, "stopping — the queued jobs from {job_id} on wait for the next start");
                break;
            }
            let job = match store.load_queued(&job_id) {
                Ok(j) => j,
                Err(e) => {
                    log::error!(target: LOG, "skip corrupt queue {job_id}: {}", e.message);
                    retire_queue_file(store, &job_id, &e);
                    results.push((job_id, format!("error:{}", e.code)));
                    continue;
                }
            };
            // process() turns a panicking step into a job error; this is the
            // backstop, so one job can never stop the drain.
            let r = match catch_panic(|| self.process(&job, store)) {
                Ok(Ok(o)) => o.as_str().to_string(),
                Ok(Err(e)) => format!("error:{}", e.code),
                Err(msg) => {
                    log::error!(target: LOG, "job {job_id} panicked: {msg} — left queued");
                    "error:job_panic".to_string()
                }
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
    if let Some(id) = job_id.filter(|id| !valid_job_id(id)) {
        return Err(JobError::new(
            format!("invalid job id {}", shown_id(id)),
            "invalid_job",
        ));
    }
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
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;

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

    /// Watcher with a fake CUPS query and fast polling.
    fn watcher(poll: CupsPollFn, max_wait: Duration) -> Arc<CupsWatcher> {
        Arc::new(CupsWatcher::new(poll, Duration::from_millis(10), max_wait))
    }

    /// Pipeline that never touches real CUPS.
    fn test_pipeline() -> Pipeline {
        Pipeline {
            lp: Arc::new(|_, _, _| Ok(None)),
            supports_raw: Arc::new(|_| Ok(false)),
            wait_cups_job: Arc::new(|_, _| panic!("wait_cups_job should not run")),
            cups_watcher: watcher(
                Arc::new(|_| Err("no CUPS in tests".into())),
                Duration::from_secs(5),
            ),
            fetch_url: Arc::new(|_| Err("no network in tests".into())),
            cups_lookup: Arc::new(|_| panic!("cups_lookup should not run")),
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

    /// Records `<job id>:<state>` for every report.
    fn per_job_state(events: &Events) -> StateFn {
        let ev = events.clone();
        Arc::new(move |j, st, _| {
            ev.lock().unwrap().push(format!("{}:{}", j.id, st.as_str()));
            Ok(())
        })
    }

    fn wait_until(what: &str, cond: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(5));
        }
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
        assert!(!err.is_permanent());
        assert!(st.has_queue_file("job-1"));
        assert!(!st.is_processed("job-1"));
        assert!(!st.failed_dir().exists());
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
    fn job_id_rules() {
        let long_ok = "x".repeat(128);
        for ok in [
            "45fb5ef3-9d43-4b83-a1b9-8d8541b4dece",
            "job-uuid-1",
            "j9",
            "7",
            "Zebra_1-42",
            "a.b:c",
            long_ok.as_str(),
        ] {
            assert!(valid_job_id(ok), "{ok}");
        }
        let too_long = "x".repeat(129);
        for bad in [
            "",
            ".",
            "..",
            "../x",
            "a/b",
            "a\\b",
            "a\0b",
            "-x",
            ".hidden",
            "a b",
            "é",
            "../../../../etc/vesyl-print/credentials",
            too_long.as_str(),
        ] {
            assert!(!valid_job_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn from_dict_rejects_path_like_ids() {
        for id in ["../../etc/victim", "c/d", "..", " x"] {
            let payload = json!({"id": id, "cups_name": "P", "content_type": "png_base64", "content": "AA=="});
            let err = PrintJob::from_dict(payload.as_object().unwrap()).unwrap_err();
            assert_eq!(err.code, "invalid_job", "{id:?}");
        }
    }

    /// Ids that could reach outside queue/ never cause file I/O or an ack.
    #[test]
    fn invalid_ids_never_touch_the_filesystem() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        // queue/../victim.json is td/victim.json.
        let victim = td.path().join("victim.json");
        fs::write(&victim, "{}").unwrap();
        st.delete_queue("../victim");
        assert!(victim.is_file());
        assert!(!st.has_queue_file("../victim"));
        assert!(!st.is_processed("../victim.json"));
        assert!(st.mark_processed("../stray").is_err());
        assert!(!td.path().join("stray").exists());
        assert!(st.write_queue(&png_job("c/d")).is_err());
        assert!(!st.queue_dir.join("c").exists());

        let acked = Arc::new(AtomicUsize::new(0));
        let a = acked.clone();
        let p = Pipeline {
            ack: Arc::new(move |_| {
                a.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
            lp: Arc::new(|_, _, _| panic!("must not print")),
            ..test_pipeline()
        };
        for id in ["c/d", "../../etc/victim"] {
            assert_eq!(
                p.process(&png_job(id), &st).unwrap_err().code,
                "invalid_job"
            );
        }
        assert_eq!(acked.load(Ordering::SeqCst), 0);
        assert!(st.list_queued_ids().is_empty());

        let f = td.path().join("x.zpl");
        fs::write(&f, b"^XA^XZ").unwrap();
        assert_eq!(
            job_from_local_file(&f, "Q", Some("../x"), None, 1, true)
                .unwrap_err()
                .code,
            "invalid_job"
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
        // Retired, so it is neither re-run nor counted as pending work.
        assert!(st.failed_dir().join("bad.json").is_file());
        assert!(!st.has_pending_work());
        assert!(test_pipeline().drain(&st).is_empty());
    }

    /// Unloadable files can never succeed: all of them leave queue/.
    #[test]
    fn drain_retires_files_that_cannot_load() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let q = &st.queue_dir;
        fs::write(q.join("array.json"), "[1, 2]").unwrap();
        fs::write(q.join("noid.json"), r#"{"cups_name": "P"}"#).unwrap();
        fs::write(
            q.join("x y.json"),
            r#"{"id": "x y", "cups_name": "P", "content_type": "png_base64", "content": "AA=="}"#,
        )
        .unwrap();
        // Holds job b2 under a1's name: deleting queue/b2.json could never clear it.
        fs::write(
            q.join("a1.json"),
            serde_json::to_string(&png_job("b2").to_dict()).unwrap(),
        )
        .unwrap();
        fs::write(q.join(".hidden.json"), "{}").unwrap();
        let p = Pipeline {
            lp: Arc::new(|_, _, _| panic!("must not print")),
            ..test_pipeline()
        };
        let results = p.drain(&st);
        assert_eq!(
            results,
            vec![
                ("a1".to_string(), "error:corrupt_queue".to_string()),
                ("array".to_string(), "error:corrupt_queue".to_string()),
                ("noid".to_string(), "error:invalid_job".to_string()),
                ("x y".to_string(), "error:invalid_job".to_string()),
            ]
        );
        assert!(st.list_queued_ids().is_empty());
        for name in ["a1", "array", "noid", "x y"] {
            assert!(
                st.failed_dir().join(format!("{name}.json")).is_file(),
                "{name}"
            );
        }
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

    /// A step that panics (the OS refusing one of `lp`'s helper threads) is a
    /// retryable job error: reported, temp content removed, queue file kept,
    /// and the drain carries on.
    #[test]
    fn drain_survives_a_panicking_job() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        st.write_queue(&png_job("q1")).unwrap();
        st.write_queue(&png_job("q2")).unwrap();
        let events: Events = Arc::default();
        let ev = events.clone();
        let content: Arc<Mutex<Option<PathBuf>>> = Arc::default();
        let c = content.clone();
        let p = Pipeline {
            lp: Arc::new(move |_, path, _| {
                if path.file_stem().is_some_and(|s| s == "q1") {
                    *c.lock().unwrap() = Some(path.to_path_buf());
                    panic!("failed to spawn thread");
                }
                Ok(None)
            }),
            report_state: Arc::new(move |j, state, detail| {
                ev.lock().unwrap().push(format!(
                    "{}:{}:{}",
                    j.id,
                    state.as_str(),
                    detail.unwrap_or("")
                ));
                Ok(())
            }),
            ..test_pipeline()
        };
        assert_eq!(
            p.drain(&st),
            vec![
                ("q1".to_string(), "error:job_error".to_string()),
                ("q2".to_string(), "delivered".to_string()),
            ]
        );
        let q1: Vec<String> = events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.starts_with("q1:"))
            .cloned()
            .collect();
        assert_eq!(
            q1,
            [
                "q1:printing:",
                "q1:error:job panicked: failed to spawn thread"
            ]
        );
        // The temp file and the vesyl-print-* dir it was written to are gone.
        let content = content.lock().unwrap().clone().expect("lp saw q1");
        let dir = content.parent().unwrap();
        assert!(dir
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("vesyl-print-")));
        assert!(!content.exists() && !dir.exists(), "{}", dir.display());
        // Outcome unknown: q1 stays queued (not retired) for the next start.
        assert_eq!(st.list_queued_ids(), ["q1"]);
        assert!(!st.failed_dir().exists());
        assert!(!st.is_processed("q1"));
    }

    /// An ack or status report that panics (its REST call refused a thread)
    /// is a failed ack or report like any other: the job still prints, once.
    #[test]
    fn panicking_ack_and_reports_do_not_fail_the_job() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let printed = Arc::new(AtomicUsize::new(0));
        let pr = printed.clone();
        let p = Pipeline {
            ack: Arc::new(|_| panic!("failed to spawn thread")),
            report_state: Arc::new(|_, _, _| panic!("failed to spawn thread")),
            lp: Arc::new(move |_, _, _| {
                pr.fetch_add(1, Ordering::SeqCst);
                Ok(Some("Q-1".into()))
            }),
            wait_cups_job: Arc::new(|_, _| CupsOutcome::Printed),
            wait_cups: WaitCups::Sync,
            ..test_pipeline()
        };
        assert_eq!(p.process(&png_job("r1"), &st).unwrap(), JobOutcome::Printed);
        assert!(st.is_processed("r1") && !st.has_queue_file("r1"));
        // A redelivery is answered from the marker; its report panics too.
        assert_eq!(p.process(&png_job("r1"), &st).unwrap(), JobOutcome::Printed);
        assert_eq!(printed.load(Ordering::SeqCst), 1);
        // A failed print still fails with its own error.
        let offline = Pipeline {
            lp: Arc::new(|_, _, _| Err(JobError::new("printer offline", "lp_error"))),
            ..p.clone()
        };
        let err = offline.process(&png_job("r2"), &st).unwrap_err();
        assert_eq!(err.code, "lp_error");
        assert!(st.has_queue_file("r2"));
    }

    /// Once `lp` accepted the job, a panic while waiting on CUPS leaves it
    /// delivered: an error (queue file kept) would print the label again on
    /// the next start.
    #[test]
    fn panic_while_waiting_on_cups_leaves_the_job_delivered() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let events: Events = Arc::default();
        let p = Pipeline {
            lp: Arc::new(|_, _, _| Ok(Some("Q-5".into()))),
            wait_cups_job: Arc::new(|_, _| panic!("failed to spawn thread")),
            wait_cups: WaitCups::Sync,
            report_state: recording_state(&events),
            ..test_pipeline()
        };
        assert_eq!(
            p.process(&png_job("w1"), &st).unwrap(),
            JobOutcome::Delivered
        );
        assert!(st.is_processed("w1") && !st.has_queue_file("w1"));
        assert_eq!(
            *events.lock().unwrap(),
            ["state:printing", "state:delivered"]
        );
    }

    /// Device d2d071: a print-test job whose file the agent can't read used
    /// to stay in queue/ forever, keeping `has_pending_work()` (and with it
    /// the OTA deferral) true on every start.
    #[test]
    fn job_that_can_never_print_leaves_the_queue() {
        let td = tempfile::tempdir().unwrap();
        let st = store(&td.path().join("state"));
        let j = job(
            "45fb5ef3-9d43-4b83-a1b9-8d8541b4dece",
            "Zebra ZD220-203dpi ZPL",
            "local_path",
            td.path().join("root/label-1bit.png").display().to_string(),
        );
        st.write_queue(&j).unwrap();
        let events: Events = Arc::default();
        let p = Pipeline {
            report_state: recording_state(&events),
            lp: Arc::new(|_, _, _| panic!("must not print")),
            ..test_pipeline()
        };
        assert_eq!(
            p.drain(&st),
            vec![(j.id.clone(), "error:content_missing".to_string())]
        );
        assert_eq!(events.lock().unwrap().last().unwrap(), "state:error");
        assert!(!st.has_pending_work());
        assert!(st.failed_dir().join(format!("{}.json", j.id)).is_file());
        // It never printed: no processed marker (a redelivery may still print).
        assert!(!st.is_processed(&j.id));
        assert!(p.drain(&st).is_empty());
    }

    #[test]
    fn permanent_errors_retire_and_transient_errors_keep() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let cases: Vec<(&str, JobError, bool)> = vec![
            (
                "p1",
                lp_failure("lp: Error - The printer or class does not exist."),
                true,
            ),
            ("p2", JobError::new("bad", "image_bad"), true),
            ("p3", JobError::new("bad", "pdf_render"), true),
            ("p4", JobError::new("51 pages", "pdf_too_many_pages"), true),
            (
                "t1",
                lp_failure("lp: Unable to connect to server: Connection refused"),
                false,
            ),
            ("t2", JobError::new("disk", "job_error"), false),
        ];
        for (id, err, permanent) in cases {
            assert_eq!(err.is_permanent(), permanent, "{id}");
            let p = Pipeline {
                lp: Arc::new(move |_, _, _| Err(err.clone())),
                ..test_pipeline()
            };
            p.process(&png_job(id), &st).unwrap_err();
            assert_eq!(st.has_queue_file(id), !permanent, "{id}");
            assert_eq!(
                st.failed_dir().join(format!("{id}.json")).is_file(),
                permanent
            );
        }
        assert_eq!(st.list_queued_ids(), ["t1", "t2"]);
    }

    #[test]
    fn lp_unknown_destination_is_classified() {
        for text in [
            "lp: Error - The printer or class does not exist.",
            "lp: The printer or class does not exist.",
            "lp: error - The printer or class was not found.",
        ] {
            let e = lp_failure(text);
            assert_eq!(
                (e.code.as_str(), e.message.as_str()),
                ("unknown_queue", text)
            );
        }
        assert_eq!(
            lp_failure("lp: Destination \"Q\" is not accepting jobs.").code,
            "lp_error"
        );
        assert_eq!(lp_failure("").message, "lp failed");
    }

    #[test]
    fn content_fetch_4xx_is_permanent_but_timeouts_and_5xx_retry() {
        let td = tempfile::tempdir().unwrap();
        let j = job("u1", "P", "pdf_uri", "https://example.test/a.pdf".into());
        for (status, code) in [
            (400, "content_rejected"),
            (403, "content_rejected"),
            (404, "content_rejected"),
            (410, "content_rejected"),
            (408, "content_fetch"),
            (429, "content_fetch"),
            (500, "content_fetch"),
            (503, "content_fetch"),
        ] {
            let fetch: FetchFn =
                Arc::new(move |_| Err(Box::new(HttpStatusError { status }) as BoxError));
            let err = materialize_content(&j, Some(td.path()), &fetch).unwrap_err();
            assert_eq!(err.code, code, "{status}");
            assert_eq!(
                err.message,
                format!("fetch failed: HTTP {status} fetching content")
            );
            assert_eq!(err.is_permanent(), code == "content_rejected");
        }
        let fetch: FetchFn = Arc::new(|_| Err("network error fetching content: refused".into()));
        let err = materialize_content(&j, Some(td.path()), &fetch).unwrap_err();
        assert_eq!(err.code, "content_fetch");
    }

    #[test]
    fn local_path_content_is_cli_only() {
        let payload = json!({"id": "x1", "cups_name": "P", "content_type": "local_path",
                             "content": "/etc/vesyl-print/credentials.json"});
        let cloud = PrintJob::from_dict(payload.as_object().unwrap()).unwrap();
        let err = check_remote_job(&cloud).unwrap_err();
        assert_eq!(err.code, "unsupported_content");
        assert!(err.is_permanent());
        assert!(check_remote_job(&png_job("p1")).is_ok());
    }

    #[test]
    fn prune_processed_drops_only_old_markers() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        for id in ["old1", "old-queued", "new1"] {
            st.mark_processed(id).unwrap();
        }
        st.write_queue(&png_job("old-queued")).unwrap();
        let month_ago = SystemTime::now() - Duration::from_secs(31 * 24 * 60 * 60);
        for id in ["old1", "old-queued"] {
            File::options()
                .write(true)
                .open(st.processed_path(id))
                .unwrap()
                .set_modified(month_ago)
                .unwrap();
        }
        let thirty_days = Duration::from_secs(30 * 24 * 60 * 60);
        assert_eq!(st.prune_processed(thirty_days), 1);
        assert!(!st.is_processed("old1"));
        // Kept: the startup drain needs it to skip re-printing the queued copy.
        assert!(st.is_processed("old-queued"));
        assert!(st.is_processed("new1"));
        assert_eq!(st.prune_processed(thirty_days), 0);
    }

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    /// Move `path`'s mtime `age` into the past.
    fn backdate(path: &Path, age: Duration) {
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::now() - age)
            .unwrap();
    }

    fn is_root() -> bool {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }

    /// N07: a symlink planted at processed/<id> (the service user owns
    /// processed/), here while the job is with CUPS as during a root
    /// print-test, is replaced by the marker, never written or chmodded
    /// through.
    #[test]
    fn marker_replaces_a_planted_symlink() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let elsewhere = tempfile::tempdir().unwrap();
        let victim = elsewhere.path().join("victim");
        fs::write(&victim, "keep\n").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o600)).unwrap();
        let (link, target) = (st.processed_path("p-1"), victim.clone());
        let p = Pipeline {
            lp: Arc::new(move |_, _, _| {
                std::os::unix::fs::symlink(&target, &link).unwrap();
                Ok(None)
            }),
            ..test_pipeline()
        };
        assert_eq!(
            p.process(&png_job("p-1"), &st).unwrap(),
            JobOutcome::Delivered
        );
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep\n");
        assert_eq!(fs::metadata(&victim).unwrap().mode() & 0o777, 0o600);
        let marker = st.processed_path("p-1");
        assert!(fs::symlink_metadata(&marker).unwrap().is_file());
        assert_eq!(fs::metadata(&marker).unwrap().mode() & 0o777, 0o644);
        assert!(fs::read_to_string(&marker).unwrap().ends_with("+00:00\n"));
        assert!(st.is_processed("p-1"));
        let names: Vec<_> = fs::read_dir(&st.processed_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["p-1"], "no temp file left behind");
    }

    /// N07 as root (an operator's print-test): the victim is a root-owned
    /// file, and the marker goes to the owner of processed/.
    ///
    /// Needs root: `sudo cargo test`, or unprivileged with
    /// `unshare --map-root-user --map-auto <test binary> --include-ignored root_`.
    #[test]
    #[ignore = "needs root (or a user namespace) to chown"]
    fn root_marker_replaces_a_planted_symlink() {
        if !is_root() {
            return;
        }
        let td = tempfile::tempdir().unwrap();
        std::os::unix::fs::chown(td.path(), Some(1000), Some(1000)).unwrap();
        // Directories root creates go to the service user too.
        let st = store(td.path());
        for d in [&st.queue_dir, &st.processed_dir] {
            assert_eq!(fs::metadata(d).unwrap().uid(), 1000, "{}", d.display());
        }
        let elsewhere = tempfile::tempdir().unwrap();
        let victim = elsewhere.path().join("shadow");
        fs::write(&victim, "root:secret-hash\n").unwrap();
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o600)).unwrap();
        let (link, target) = (st.processed_path("p-1"), victim.clone());
        let p = Pipeline {
            lp: Arc::new(move |_, _, _| {
                std::os::unix::fs::symlink(&target, &link).unwrap();
                std::os::unix::fs::lchown(&link, Some(1000), Some(1000)).unwrap();
                Ok(None)
            }),
            ..test_pipeline()
        };
        assert_eq!(
            p.process(&png_job("p-1"), &st).unwrap(),
            JobOutcome::Delivered
        );
        assert_eq!(fs::read_to_string(&victim).unwrap(), "root:secret-hash\n");
        let v = fs::metadata(&victim).unwrap();
        assert_eq!((v.uid(), v.mode() & 0o777), (0, 0o600));
        let m = fs::symlink_metadata(st.processed_path("p-1")).unwrap();
        assert!(m.is_file());
        assert_eq!((m.uid(), m.gid(), m.mode() & 0o777), (1000, 1000, 0o644));
    }

    /// N08: queue/failed/ is pruned in the same pass as the markers, with
    /// the same retention. Only `*.json` files go.
    #[test]
    fn prune_processed_also_prunes_old_failed_files() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        for id in ["f-old", "f-new"] {
            st.write_queue(&png_job(id)).unwrap();
            st.retire_queue(id).unwrap().unwrap();
        }
        let failed = st.failed_dir();
        fs::write(failed.join("notes.txt"), "kept").unwrap();
        fs::create_dir(failed.join("dir.json")).unwrap();
        st.mark_processed("m-old").unwrap();
        for p in [
            failed.join("f-old.json"),
            failed.join("notes.txt"),
            st.processed_path("m-old"),
        ] {
            backdate(&p, 31 * DAY);
        }
        let retention = 30 * DAY;
        // Counts the markers only (the agent logs it as such).
        assert_eq!(st.prune_processed(retention), 1);
        assert!(!st.is_processed("m-old"));
        assert!(!failed.join("f-old.json").exists());
        assert!(failed.join("f-new.json").is_file());
        assert!(failed.join("notes.txt").is_file());
        assert!(failed.join("dir.json").is_dir());

        backdate(&failed.join("f-new.json"), 31 * DAY);
        assert_eq!(st.prune_failed(retention), 1);
        assert!(!failed.join("f-new.json").exists());
        assert_eq!(st.prune_failed(retention), 0);
        // No failed/ at all is fine.
        let fresh = store(&td.path().join("fresh"));
        assert_eq!(fresh.prune_failed(retention), 0);
    }

    /// A job can sit in queue/ for weeks (retried on each start) before it
    /// fails for good; its failed/ copy still gets the whole retention.
    #[test]
    fn retiring_restarts_the_retention_clock() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        st.write_queue(&png_job("slow")).unwrap();
        backdate(&st.queue_path("slow"), 40 * DAY);
        let path = st.retire_queue("slow").unwrap().unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default();
        assert!(age < DAY, "{age:?}");
        assert_eq!(st.prune_failed(30 * DAY), 0);
        assert!(path.is_file());
    }

    /// N08: a job retired after a permanent failure that prints later (a
    /// redelivery once its queue is fixed) leaves no failed/ copy behind;
    /// nor does one that turns out to be processed already.
    #[test]
    fn finishing_a_job_drops_its_failed_copy() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let j = png_job("r-1");
        let no_queue = Pipeline {
            lp: Arc::new(|_, _, _| {
                Err(lp_failure(
                    "lp: Error - The printer or class does not exist.",
                ))
            }),
            ..test_pipeline()
        };
        assert_eq!(no_queue.process(&j, &st).unwrap_err().code, "unknown_queue");
        no_queue.process(&png_job("other"), &st).unwrap_err();
        let copy = st.failed_dir().join("r-1.json");
        assert!(copy.is_file());

        assert_eq!(
            test_pipeline().process(&j, &st).unwrap(),
            JobOutcome::Delivered
        );
        assert!(st.is_processed("r-1") && !st.has_queue_file("r-1"));
        assert!(!copy.exists());
        // Another job's copy stays.
        assert!(st.failed_dir().join("other.json").is_file());

        fs::write(&copy, "{}").unwrap();
        assert_eq!(
            test_pipeline().process(&j, &st).unwrap(),
            JobOutcome::Printed
        );
        assert!(!copy.exists());

        // Ids that are not file names are ignored: queue/x.json is not failed/../x.
        fs::write(st.queue_dir.join("x.json"), "{}").unwrap();
        st.delete_failed("../x");
        assert!(st.queue_dir.join("x.json").is_file());
    }

    /// Swaps failed/, just made, for a symlink to `target`, as the service
    /// user (which owns queue/) could while root works there.
    fn swap_failed_for(target: &Path) -> impl Fn(&Path) + '_ {
        move |made: &Path| {
            fs::rename(made, made.with_extension("moved")).unwrap();
            std::os::unix::fs::symlink(target, made).unwrap();
        }
    }

    /// The open met a symlink and refused it: ENOTDIR on Linux, where
    /// O_DIRECTORY is checked before O_NOFOLLOW's ELOOP.
    fn refused_link(e: &io::Error) -> bool {
        matches!(e.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP))
    }

    /// N09: retiring never goes through a symlink at queue/failed, whether
    /// swapped in right after failed/ was made or planted beforehand: nothing
    /// lands in (or is removed from) the link's target, and the queue file
    /// stays put.
    #[test]
    fn retire_never_follows_a_symlink_at_failed() {
        let td = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let st = store(td.path());
        st.write_queue(&png_job("s-1")).unwrap();
        let moved_into_target = || fs::read_dir(elsewhere.path()).unwrap().count() > 0;
        let result = st.retire_queue_with("s-1", &swap_failed_for(elsewhere.path()));
        assert!(!moved_into_target(), "moved into the link's target");
        assert!(refused_link(&result.unwrap_err()));
        assert!(st.has_queue_file("s-1"));

        // The link is still there for the next call.
        let result = st.retire_queue("s-1");
        assert!(!moved_into_target(), "moved into the link's target");
        assert!(refused_link(&result.unwrap_err()));
        assert!(st.has_queue_file("s-1"));
        fs::write(elsewhere.path().join("s-1.json"), "{}").unwrap();
        st.delete_failed("s-1");
        assert!(elsewhere.path().join("s-1.json").is_file());

        // Without the link, retiring works again.
        fs::remove_file(st.failed_dir()).unwrap();
        assert!(st.retire_queue("s-1").unwrap().unwrap().is_file());
        assert!(!st.has_queue_file("s-1"));
    }

    /// N09 as root (an operator's print-test that fails for good): failed/
    /// goes to the owner of queue/ without a chown by path, so a link
    /// swapped in for it never gets its target handed to the service user.
    ///
    /// Needs root: `sudo cargo test`, or unprivileged with
    /// `unshare --map-root-user --map-auto <test binary> --include-ignored root_`.
    #[test]
    #[ignore = "needs root (or a user namespace) to chown"]
    fn root_retire_hands_failed_to_the_queue_owner_never_a_link_target() {
        if !is_root() {
            return;
        }
        let owner = |p: &Path| {
            let m = fs::symlink_metadata(p).unwrap();
            (m.uid(), m.gid())
        };
        let td = tempfile::tempdir().unwrap();
        std::os::unix::fs::chown(td.path(), Some(1000), Some(1000)).unwrap();
        let st = store(td.path());
        assert_eq!(owner(&st.queue_dir), (1000, 1000));
        st.write_queue(&png_job("t-1")).unwrap();
        let moved = st.retire_queue("t-1").unwrap().unwrap();
        // The agent can retire into, and prune, what root made.
        assert_eq!(owner(&st.failed_dir()), (1000, 1000));
        assert_eq!(owner(&moved), (1000, 1000));

        let rooted = tempfile::tempdir().unwrap();
        let before = owner(rooted.path());
        assert_eq!(before.0, 0);
        fs::remove_dir_all(st.failed_dir()).unwrap();
        st.write_queue(&png_job("t-2")).unwrap();
        let result = st.retire_queue_with("t-2", &swap_failed_for(rooted.path()));
        assert_eq!(owner(rooted.path()), before, "link target chowned");
        assert_eq!(fs::read_dir(rooted.path()).unwrap().count(), 0);
        assert!(refused_link(&result.unwrap_err()));
        assert!(st.has_queue_file("t-2"));
        // Planted beforehand: the same.
        let result = st.retire_queue("t-2");
        assert_eq!(owner(rooted.path()), before, "link target chowned");
        assert_eq!(fs::read_dir(rooted.path()).unwrap().count(), 0);
        assert!(refused_link(&result.unwrap_err()));
    }

    /// A temp directory holding a file per name, each 31 days old.
    fn month_old_files(names: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for name in names {
            let path = dir.path().join(name);
            fs::write(&path, "keep").unwrap();
            backdate(&path, 31 * DAY);
        }
        dir
    }

    fn all_files_in(dir: &Path, names: &[&str]) -> bool {
        names.iter().all(|n| dir.join(n).is_file())
    }

    /// The service user owns queue/ and the state directory, so it can plant
    /// queue/failed or processed as a symlink (to /etc/vesyl-print, say)
    /// before an agent run as root prunes. Neither prune follows it: nothing
    /// in the link's target goes, however old.
    #[test]
    fn prune_never_follows_a_symlink_at_failed_or_processed() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let retention = 30 * DAY;

        let names = ["credentials.json", "config.json"];
        let target = month_old_files(&names);
        std::os::unix::fs::symlink(target.path(), st.failed_dir()).unwrap();
        assert_eq!(st.prune_failed(retention), 0);
        assert_eq!(st.prune_processed(retention), 0);
        assert!(all_files_in(target.path(), &names), "went via failed/");

        let names = ["passwd", "shadow"];
        let target = month_old_files(&names);
        fs::remove_dir(&st.processed_dir).unwrap();
        std::os::unix::fs::symlink(target.path(), &st.processed_dir).unwrap();
        assert_eq!(st.prune_processed(retention), 0);
        assert!(all_files_in(target.path(), &names), "went via processed/");
    }

    /// failed/ swapped for a link after the prune opened it: the link's
    /// target only supplies the names tried. Each is checked, and removed,
    /// in the directory that was opened; nothing in the target goes.
    #[test]
    fn prune_checks_and_removes_in_the_directory_it_opened() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let failed = st.failed_dir();
        fs::create_dir(&failed).unwrap();
        for (name, age) in [
            ("stale.json", 31 * DAY),
            ("fresh.json", DAY),
            ("unlisted.json", 31 * DAY),
        ] {
            fs::write(failed.join(name), "{}").unwrap();
            backdate(&failed.join(name), age);
        }
        let in_target = ["stale.json", "fresh.json", "elsewhere.json"];
        let target = month_old_files(&in_target);
        let swap = swap_failed_for(target.path());
        assert_eq!(prune_files_with(&failed, 30 * DAY, |_| true, &swap), 1);
        assert!(all_files_in(target.path(), &in_target), "target pruned");
        let opened = failed.with_extension("moved");
        assert!(!opened.join("stale.json").exists());
        // Fresh where it was checked; the other was not listed, so it waits
        // for the next pass.
        assert!(all_files_in(&opened, &["fresh.json", "unlisted.json"]));
    }

    /// The same as root, the case that matters: an operator starts the
    /// agent as root over links the service user planted to a root-owned
    /// directory.
    ///
    /// Needs root: `sudo cargo test`, or unprivileged with
    /// `unshare --map-root-user --map-auto <test binary> --include-ignored root_`.
    #[test]
    #[ignore = "needs root (or a user namespace) to chown"]
    fn root_prune_never_follows_a_planted_symlink() {
        if !is_root() {
            return;
        }
        let td = tempfile::tempdir().unwrap();
        std::os::unix::fs::chown(td.path(), Some(1000), Some(1000)).unwrap();
        let st = store(td.path());
        let names = ["credentials.json", "config.json", "shadow"];
        let rooted = month_old_files(&names);
        assert_eq!(fs::metadata(rooted.path()).unwrap().uid(), 0);
        fs::remove_dir(&st.processed_dir).unwrap();
        for link in [st.failed_dir(), st.processed_dir.clone()] {
            std::os::unix::fs::symlink(rooted.path(), &link).unwrap();
            std::os::unix::fs::lchown(&link, Some(1000), Some(1000)).unwrap();
        }
        assert_eq!(st.prune_failed(30 * DAY), 0);
        assert_eq!(st.prune_processed(30 * DAY), 0);
        assert!(all_files_in(rooted.path(), &names));
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

    /// Serve one HTTP response: `head` right away, then `chunks` of the body
    /// `gap` apart. Returns the URL.
    fn trickle_server(head: String, chunks: Vec<Vec<u8>>, gap: Duration) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/label.pdf", listener.local_addr().unwrap());
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap() > 0 && line != "\r\n" {
                line.clear();
            }
            stream.write_all(head.as_bytes()).unwrap();
            for chunk in chunks {
                thread::sleep(gap);
                if stream.write_all(&chunk).is_err() {
                    return;
                }
            }
        });
        url
    }

    /// urllib's `timeout=60` bounds each read, not the transfer: a download
    /// that keeps making progress must finish even when it takes longer than
    /// the connect/response timeouts (the old 60 s end-to-end deadline
    /// failed large labels on slow links).
    #[test]
    fn slow_download_is_not_cut_off_by_a_total_deadline() {
        let chunks: Vec<Vec<u8>> = (0..8u8).map(|i| vec![i; 1024]).collect();
        let head =
            "HTTP/1.1 200 OK\r\nContent-Length: 8192\r\nConnection: close\r\n\r\n".to_string();
        let url = trickle_server(head, chunks, Duration::from_millis(200));
        let short = net::Timeouts {
            connect: Duration::from_secs(1),
            response: Duration::from_secs(1),
            idle: Duration::from_secs(1),
            body: Duration::from_secs(30),
        };
        let started = Instant::now();
        let data = http_get_with(&url, short).unwrap();
        assert!(started.elapsed() > Duration::from_millis(1500));
        assert_eq!(data.len(), 8192);
        assert_eq!(&data[7 * 1024..], &[7u8; 1024][..]);
    }

    /// N15: a content host whose connection goes silent after the headers
    /// fails after the per-read timeout (Python's 60 s), not the 15-minute
    /// body budget, so the agent loop gets back to printing and heartbeating.
    #[test]
    fn stalled_download_fails_after_the_idle_timeout() {
        let head = "HTTP/1.1 200 OK\r\nContent-Length: 8192\r\n\r\n".to_string();
        // The body would only start 10 s after the headers.
        let url = trickle_server(head, vec![vec![1u8; 1024]], Duration::from_secs(10));
        let short = net::Timeouts {
            idle: Duration::from_secs(1),
            ..net::Timeouts::CONTENT
        };
        let started = Instant::now();
        let err = http_get_with(&url, short).unwrap_err();
        let took = started.elapsed();
        assert!(err.to_string().contains("timeout"), "{err}");
        assert!(took >= Duration::from_millis(900), "{took:?}");
        assert!(took < Duration::from_secs(5), "{took:?}");
        assert_eq!(net::Timeouts::CONTENT.idle, Duration::from_secs(60));
    }

    #[test]
    fn http_error_status_is_typed() {
        let head = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let url = trickle_server(head.into(), Vec::new(), Duration::ZERO);
        let err = http_get(&url).unwrap_err();
        assert_eq!(
            err.downcast_ref::<HttpStatusError>(),
            Some(&HttpStatusError { status: 404 })
        );
        assert_eq!(err.to_string(), "HTTP 404 fetching content");
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

    /// CUPS canceled/aborted the job: final, so it is retired rather than
    /// printed again on the next start.
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
        assert!(!st.has_queue_file("e1"));
        assert!(st.failed_dir().join("e1.json").is_file());
        assert!(!st.is_processed("e1"));
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
        assert_eq!(p.cups_watcher.pending(), 0);
    }

    #[test]
    fn wait_cups_async_returns_before_cups_finishes() {
        let td = tempfile::tempdir().unwrap();
        let f = td.path().join("x.zpl");
        fs::write(&f, b"^XA^XZ").unwrap();
        let j = job_from_local_file(&f, "Q", None, None, 1, true).unwrap();
        let st = store(td.path());
        let events: Events = Arc::default();
        let finished = Arc::new(AtomicBool::new(false));
        let fin = finished.clone();
        let p = Pipeline {
            lp: Arc::new(|_, _, _| Ok(Some("Q-99".into()))),
            cups_watcher: watcher(
                Arc::new(move |keys| {
                    assert_eq!(keys, ["Q-99"]);
                    Ok(if fin.load(Ordering::SeqCst) {
                        HashMap::from([("Q-99".to_string(), CupsOutcome::Printed)])
                    } else {
                        HashMap::new()
                    })
                }),
                Duration::from_secs(5),
            ),
            report_state: recording_state(&events),
            wait_cups: WaitCups::Async,
            ..test_pipeline()
        };
        assert_eq!(p.process(&j, &st).unwrap(), JobOutcome::Delivered);
        assert!(events.lock().unwrap().contains(&"state:delivered".into()));
        assert!(!events.lock().unwrap().contains(&"state:printed".into()));
        assert!(st.is_processed(&j.id));
        assert_eq!(p.cups_watcher.pending(), 1);
        finished.store(true, Ordering::SeqCst);
        wait_until("printed report", || {
            events.lock().unwrap().contains(&"state:printed".into())
        });
        wait_until("watcher idle", || p.cups_watcher.pending() == 0);
    }

    /// Many outstanding async jobs (printer out of paper): one watcher thread,
    /// one CUPS query per poll for all of them, no wait tick, no per-job thread.
    #[test]
    fn async_jobs_share_one_watcher_thread_and_query() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let polls: Arc<Mutex<Vec<(thread::ThreadId, usize)>>> = Arc::default();
        let refilled = Arc::new(AtomicBool::new(false));
        let (pl, rf) = (polls.clone(), refilled.clone());
        let events: Events = Arc::default();
        let next_id = Arc::new(AtomicUsize::new(0));
        let ticks = Arc::new(AtomicUsize::new(0));
        let t = ticks.clone();
        let p = Pipeline {
            lp: Arc::new(move |_, _, _| {
                Ok(Some(format!(
                    "Zebra-{}",
                    next_id.fetch_add(1, Ordering::SeqCst)
                )))
            }),
            cups_watcher: watcher(
                Arc::new(move |keys| {
                    pl.lock()
                        .unwrap()
                        .push((thread::current().id(), keys.len()));
                    Ok(if rf.load(Ordering::SeqCst) {
                        keys.iter()
                            .map(|k| (k.clone(), CupsOutcome::Printed))
                            .collect()
                    } else {
                        HashMap::new()
                    })
                }),
                Duration::from_secs(30),
            ),
            on_wait_tick: Some(Arc::new(move || {
                t.fetch_add(1, Ordering::SeqCst);
            })),
            report_state: per_job_state(&events),
            wait_cups: WaitCups::Async,
            ..test_pipeline()
        };
        for i in 0..20 {
            // Each job gets its own pipeline clone, as in the agent.
            let pipeline = p.clone();
            let id = format!("a{i}");
            assert_eq!(
                pipeline.process(&png_job(&id), &st).unwrap(),
                JobOutcome::Delivered
            );
        }
        assert_eq!(p.cups_watcher.pending(), 20);
        wait_until("a poll covering all jobs", || {
            polls.lock().unwrap().iter().any(|(_, n)| *n == 20)
        });
        refilled.store(true, Ordering::SeqCst);
        wait_until("all printed", || {
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|e| e.ends_with(":printed"))
                .count()
                == 20
        });
        wait_until("watcher idle", || p.cups_watcher.pending() == 0);
        let threads: HashSet<thread::ThreadId> =
            polls.lock().unwrap().iter().map(|(t, _)| *t).collect();
        assert_eq!(threads.len(), 1, "one watcher thread for all jobs");
        assert_ne!(threads.into_iter().next(), Some(thread::current().id()));
        assert_eq!(ticks.load(Ordering::SeqCst), 0, "async waits never tick");
        for i in 0..20 {
            let id = format!("a{i}");
            let ev = events.lock().unwrap();
            let mine: Vec<&String> = ev
                .iter()
                .filter(|e| e.starts_with(&format!("{id}:")))
                .collect();
            assert_eq!(
                mine,
                [
                    &format!("{id}:printing"),
                    &format!("{id}:delivered"),
                    &format!("{id}:printed")
                ]
            );
        }
    }

    #[test]
    fn watcher_reports_errors_and_survives_a_panicking_hook() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let events: Events = Arc::default();
        let ev = events.clone();
        let p = Pipeline {
            lp: Arc::new(|_, path, _| {
                let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
                Ok(Some(format!("Q-{stem}")))
            }),
            cups_watcher: watcher(
                Arc::new(|keys| {
                    Ok(keys
                        .iter()
                        .map(|k| {
                            let outcome = if k == "Q-bad" {
                                CupsOutcome::Error
                            } else {
                                CupsOutcome::Printed
                            };
                            (k.clone(), outcome)
                        })
                        .collect())
                }),
                Duration::from_secs(30),
            ),
            report_state: Arc::new(move |j, state, detail| {
                if j.id == "boom" && state == JobState::Printed {
                    panic!("report hook panicked");
                }
                // The watcher holds the job without its (possibly huge) content.
                if state == JobState::Printed || state == JobState::Error {
                    assert!(j.content.is_empty() && j.raw.is_empty());
                }
                ev.lock().unwrap().push(format!(
                    "{}:{}:{}",
                    j.id,
                    state.as_str(),
                    detail.unwrap_or("")
                ));
                Ok(())
            }),
            wait_cups: WaitCups::Async,
            ..test_pipeline()
        };
        for id in ["boom", "bad", "good"] {
            p.process(&png_job(id), &st).unwrap();
        }
        wait_until("watcher idle", || p.cups_watcher.pending() == 0);
        wait_until("good printed", || {
            events
                .lock()
                .unwrap()
                .contains(&"good:printed:Q-good".to_string())
        });
        assert!(events
            .lock()
            .unwrap()
            .contains(&"bad:error:CUPS job Q-bad failed".to_string()));
    }

    #[test]
    fn watcher_leaves_jobs_delivered_after_max_wait_or_query_failures() {
        // Still printing (paper out) past the per-job ceiling.
        let paper_out: CupsPollFn = Arc::new(|_| Ok(HashMap::new()));
        // CUPS down: repeated query failures.
        let cups_down: CupsPollFn = Arc::new(|_| Err("lpstat: Scheduler is not running.".into()));
        for (poll, max_wait) in [
            (paper_out, Duration::from_millis(100)),
            (cups_down, Duration::from_secs(30)),
        ] {
            let td = tempfile::tempdir().unwrap();
            let st = store(td.path());
            let events: Events = Arc::default();
            let p = Pipeline {
                lp: Arc::new(|_, _, _| Ok(Some("Q-1".into()))),
                cups_watcher: watcher(poll, max_wait),
                report_state: recording_state(&events),
                wait_cups: WaitCups::Async,
                ..test_pipeline()
            };
            assert_eq!(
                p.process(&png_job("j1"), &st).unwrap(),
                JobOutcome::Delivered
            );
            wait_until("watcher gives up", || p.cups_watcher.pending() == 0);
            thread::sleep(Duration::from_millis(50));
            assert_eq!(
                *events.lock().unwrap(),
                ["state:printing", "state:delivered"]
            );
        }
    }

    /// The OS refusing a thread must not panic the agent: the job stays
    /// `delivered` (reported) and the pipeline carries on.
    #[test]
    fn watcher_spawn_failure_leaves_job_delivered() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let events: Events = Arc::default();
        let refused = CupsWatcher::new(
            Arc::new(|_| panic!("no thread, no poll")),
            Duration::from_millis(10),
            Duration::from_secs(5),
        )
        .with_spawn(Arc::new(|_, _| {
            Err(io::Error::from_raw_os_error(libc::EAGAIN))
        }));
        let p = Pipeline {
            lp: Arc::new(|_, _, _| Ok(Some("Q-7".into()))),
            cups_watcher: Arc::new(refused),
            report_state: recording_state(&events),
            wait_cups: WaitCups::Async,
            ..test_pipeline()
        };
        for id in ["s1", "s2"] {
            assert_eq!(p.process(&png_job(id), &st).unwrap(), JobOutcome::Delivered);
            assert!(st.is_processed(id));
        }
        assert_eq!(p.cups_watcher.pending(), 0);
        assert!(!events.lock().unwrap().contains(&"state:printed".into()));
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

    /// The agent's notes in a job's queue record (`_agent`).
    fn notes(st: &JobStore, id: &str) -> Value {
        let raw = fs::read_to_string(st.queue_path(id)).unwrap();
        serde_json::from_str::<Value>(&raw).unwrap()[LOCAL_KEY].clone()
    }

    /// `lpstat` with `key` still pending (paper out), calling `on_poll`
    /// before it answers each not-completed query.
    fn printer_out_of_paper(
        key: &'static str,
        on_poll: impl Fn() + Send + Sync + 'static,
    ) -> WaitFn {
        let on_poll = Arc::new(on_poll);
        Arc::new(move |id, ctx| {
            let on_poll = on_poll.clone();
            let lpstat = move |args: &[&str]| {
                assert_eq!(args[1], "not-completed", "it never completes");
                on_poll();
                ok(&format!("{key}  ben  1024  date\n"))
            };
            wait_cups_job_with(
                id,
                Duration::from_secs(60),
                Duration::from_secs(5),
                ctx,
                &lpstat,
            )
        })
    }

    /// J3: a stop during the CUPS wait (out of paper) used to be ignored for
    /// up to 24 h; systemd then SIGKILLs the agent, leaving the queue file
    /// as if `lp` had never run, and the next start printed the label again.
    /// The wait ends at once now, and the record says CUPS has the job.
    #[test]
    fn a_stop_during_the_cups_wait_leaves_the_job_with_cups_on_record() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let events: Events = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let p = Pipeline {
            lp: Arc::new(|_, _, _| Ok(Some("Zebra-42".into()))),
            wait_cups: WaitCups::Sync,
            // systemctl stop, while the printer is out of paper.
            wait_cups_job: printer_out_of_paper("Zebra-42", move || {
                s.store(true, Ordering::SeqCst)
            }),
            report_state: recording_state(&events),
            stop,
            ..test_pipeline()
        };
        let j = png_job("w-1");
        let started = Instant::now();
        assert_eq!(p.process(&j, &st).unwrap(), JobOutcome::Interrupted);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(
            *events.lock().unwrap(),
            ["state:printing", "state:delivered"]
        );
        assert!(!st.is_processed("w-1"));
        let n = notes(&st, "w-1");
        assert_eq!(n["cups_job_id"], "Zebra-42");
        assert!(n["submitted_at"]
            .as_str()
            .is_some_and(|t| t.ends_with("+00:00")));
        assert_eq!(n["attempts"], 1);
        // The payload is as it was: an older agent reads the record as ever.
        let mut back = st.load_queued("w-1").unwrap().raw;
        assert!(back.remove(LOCAL_KEY).is_some());
        assert_eq!(back, j.to_dict());
    }

    /// J3: the next start never gives a job CUPS already has to `lp`. It
    /// finishes it from what CUPS says now, and reports what the pipeline
    /// would have.
    #[test]
    fn a_job_cups_already_has_is_finished_without_printing_it_again() {
        type Case = (
            &'static str,
            Option<&'static str>,
            WaitCups,
            Result<CupsJobState, String>,
        );
        let cases: Vec<(Case, &str, Vec<&str>)> = vec![
            (
                (
                    "printed",
                    Some("Zebra-42"),
                    WaitCups::Sync,
                    Ok(CupsJobState::Printed),
                ),
                "printed",
                vec!["printed:Zebra-42"],
            ),
            (
                (
                    "failed",
                    Some("Zebra-42"),
                    WaitCups::Async,
                    Ok(CupsJobState::Failed),
                ),
                "error:cups_job_failed",
                vec!["error:CUPS job Zebra-42 failed"],
            ),
            // Its history purged, or CUPS reset: delivered, never printed again.
            (
                (
                    "forgotten",
                    Some("Zebra-42"),
                    WaitCups::Sync,
                    Ok(CupsJobState::Forgotten),
                ),
                "delivered",
                vec!["delivered:Zebra-42"],
            ),
            // Still printing: followed as right after lp (here the wait).
            (
                (
                    "active",
                    Some("Zebra-42"),
                    WaitCups::Sync,
                    Ok(CupsJobState::Active),
                ),
                "printed",
                vec!["delivered:Zebra-42", "printed:Zebra-42"],
            ),
            // CUPS down: the same.
            (
                (
                    "cups-down",
                    Some("Zebra-42"),
                    WaitCups::Sync,
                    Err("lpstat: Scheduler is not running.".into()),
                ),
                "printed",
                vec!["delivered:Zebra-42", "printed:Zebra-42"],
            ),
            // Not followed in CUPS, or no request id: delivered, CUPS unasked.
            (
                (
                    "off",
                    Some("Zebra-42"),
                    WaitCups::Off,
                    Err("not asked".into()),
                ),
                "delivered",
                vec!["delivered:Zebra-42"],
            ),
            (
                ("no-id", None, WaitCups::Sync, Err("not asked".into())),
                "delivered",
                vec!["delivered:"],
            ),
        ];
        for ((id, cid, mode, answer), result, reports) in cases {
            let td = tempfile::tempdir().unwrap();
            let st = store(td.path());
            let j = png_job(id);
            st.write_queue(&j).unwrap();
            let sub = Submission {
                cups_job_id: cid.map(String::from),
                submitted_at: "2026-10-08T12:00:00+00:00".into(),
            };
            st.write_local(
                &j,
                &LocalState {
                    attempts: 1,
                    submitted: Some(sub),
                },
            )
            .unwrap();
            let events: Events = Arc::default();
            let ev = events.clone();
            let asked = Arc::new(AtomicUsize::new(0));
            let a = asked.clone();
            let p = Pipeline {
                lp: Arc::new(|_, _, _| panic!("must not print again")),
                ack: Arc::new(|_| panic!("no second ack")),
                wait_cups: mode,
                wait_cups_job: Arc::new(|cid, _| {
                    assert_eq!(cid, "Zebra-42");
                    CupsOutcome::Printed
                }),
                cups_lookup: Arc::new(move |key| {
                    assert_eq!(key, "Zebra-42");
                    a.fetch_add(1, Ordering::SeqCst);
                    answer.clone()
                }),
                report_state: Arc::new(move |_, state, detail| {
                    ev.lock()
                        .unwrap()
                        .push(format!("{}:{}", state.as_str(), detail.unwrap_or("")));
                    Ok(())
                }),
                ..test_pipeline()
            };
            assert_eq!(p.drain(&st), [(id.to_string(), result.to_string())], "{id}");
            assert_eq!(*events.lock().unwrap(), reports, "{id}");
            let looked_up = cid.is_some() && mode != WaitCups::Off;
            assert_eq!(asked.load(Ordering::SeqCst), usize::from(looked_up), "{id}");
            assert!(!st.has_queue_file(id), "{id}");
            let failed = result.starts_with("error:");
            assert_eq!(st.is_processed(id), !failed, "{id}");
            assert_eq!(
                st.failed_dir().join(format!("{id}.json")).is_file(),
                failed,
                "{id}"
            );
        }
    }

    /// J3: a stop before `lp` (here during the content fetch) sends nothing
    /// to CUPS: the record stays as it was, and the next start prints the
    /// job once. Once stopping, the drain takes no further job either.
    #[test]
    fn a_stop_before_lp_leaves_the_job_queued_and_it_prints_once() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        for id in ["s-1", "s-2"] {
            st.write_queue(&job(
                id,
                "P",
                "png_uri",
                "https://example.test/l.png".into(),
            ))
            .unwrap();
        }
        let printed: Events = Arc::default();
        let pipeline = |stop: Arc<AtomicBool>| {
            let (pr, s) = (printed.clone(), stop.clone());
            Pipeline {
                fetch_url: Arc::new(move |_| {
                    // systemctl stop, while the content downloads.
                    s.store(true, Ordering::SeqCst);
                    Ok(b"\x89PNG\r\n\x1a\nfake".to_vec())
                }),
                lp: Arc::new(move |_, path, _| {
                    pr.lock()
                        .unwrap()
                        .push(path.file_stem().unwrap().to_string_lossy().into());
                    Ok(None)
                }),
                stop,
                ..test_pipeline()
            }
        };
        let first = pipeline(Arc::default());
        assert_eq!(
            first.drain(&st),
            [("s-1".to_string(), "interrupted".to_string())]
        );
        assert!(printed.lock().unwrap().is_empty());
        assert_eq!(st.list_queued_ids(), ["s-1", "s-2"]);
        // The attempt that returned is taken back; nothing went to CUPS.
        assert_eq!(notes(&st, "s-1"), json!({"attempts": 0}));

        // The next start: each prints once.
        let next = Pipeline {
            fetch_url: Arc::new(|_| Ok(b"\x89PNG\r\n\x1a\nfake".to_vec())),
            ..pipeline(Arc::default())
        };
        let results = next.drain(&st);
        assert!(results.iter().all(|(_, r)| r == "delivered"), "{results:?}");
        assert_eq!(*printed.lock().unwrap(), ["s-1", "s-2"]);
        assert!(st.list_queued_ids().is_empty());
    }

    /// J3: a CUPS wait ends promptly once the agent is stopping, however
    /// long its poll interval (here 5 s).
    #[test]
    fn the_cups_wait_ends_promptly_when_stopping() {
        let ctx = wait_ctx(None);
        let stop = ctx.stop.clone();
        let lpstat = move |_: &[&str]| {
            stop.store(true, Ordering::SeqCst);
            ok("Zebra-43 ben 1 date\n")
        };
        let started = Instant::now();
        let outcome = wait_cups_job_with(
            "Zebra-43",
            Duration::from_secs(60),
            Duration::from_secs(5),
            &ctx,
            &lpstat,
        );
        assert_eq!(outcome, CupsOutcome::Unknown);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    /// A queue record for `id` as a run the agent died in leaves it after
    /// `attempts` such runs.
    fn died_in(st: &JobStore, id: &str, attempts: u32) -> PrintJob {
        let j = png_job(id);
        st.write_queue(&j).unwrap();
        let local = LocalState {
            attempts,
            submitted: None,
        };
        st.write_local(&j, &local).unwrap();
        j
    }

    /// J5: the attempt is on disk before the steps that can kill the agent
    /// (conversion, lp), and taken back once a run returns: a job the agent
    /// died in MAX_ATTEMPTS times is retired as crash_loop and reported
    /// failed, instead of killing every start (RestartSec=5) again.
    #[test]
    fn a_job_that_keeps_killing_the_agent_is_retired() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        died_in(&st, "poison", 2);
        let s = st.clone();
        let p = Pipeline {
            lp: Arc::new(move |_, _, _| {
                assert_eq!(notes(&s, "poison")["attempts"], 3, "recorded before lp");
                Err(JobError::new("printer offline", "lp_error"))
            }),
            ..test_pipeline()
        };
        assert_eq!(
            p.drain(&st),
            [("poison".to_string(), "error:lp_error".to_string())]
        );
        // It returned: two deaths on record, as before.
        assert_eq!(notes(&st, "poison"), json!({"attempts": 2}));

        // The third death (what one leaves on disk), and the next start.
        died_in(&st, "poison", MAX_ATTEMPTS);
        let events: Events = Arc::default();
        let ev = events.clone();
        let p = Pipeline {
            lp: Arc::new(|_, _, _| panic!("must not run again")),
            ack: Arc::new(|_| panic!("no ack either")),
            report_state: Arc::new(move |_, state, detail| {
                ev.lock()
                    .unwrap()
                    .push(format!("{}:{}", state.as_str(), detail.unwrap_or("")));
                Ok(())
            }),
            ..test_pipeline()
        };
        assert_eq!(
            p.drain(&st),
            [("poison".to_string(), format!("error:{CRASH_LOOP}"))]
        );
        assert_eq!(
            *events.lock().unwrap(),
            ["error:printing this job ended the agent 3 times (out of memory?) — not tried again"]
        );
        assert!(st.failed_dir().join("poison.json").is_file());
        assert!(!st.has_pending_work());
        assert!(!st.is_processed("poison"));
        assert!(JobError::new("x", CRASH_LOOP).is_permanent());

        // One death short of the limit, the job still runs, and prints.
        let st = store(&td.path().join("other"));
        died_in(&st, "survivor", MAX_ATTEMPTS - 1);
        assert_eq!(
            test_pipeline().drain(&st),
            [("survivor".to_string(), "delivered".to_string())]
        );
        assert!(st.is_processed("survivor"));
    }

    /// J5: a run that returns, failed or not, does not count: a job that
    /// fails again and again (printer offline) is never taken for one that
    /// kills the agent.
    #[test]
    fn retried_failures_never_count_as_crashes() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        st.write_queue(&png_job("offline")).unwrap();
        let offline = Pipeline {
            lp: Arc::new(|_, _, _| Err(JobError::new("printer offline", "lp_error"))),
            ..test_pipeline()
        };
        let panicking = Pipeline {
            lp: Arc::new(|_, _, _| panic!("failed to spawn thread")),
            ..test_pipeline()
        };
        for p in [&offline, &panicking, &offline, &offline, &panicking] {
            let results = p.drain(&st);
            assert_eq!(results.len(), 1);
            assert_ne!(results[0].1, format!("error:{CRASH_LOOP}"));
            assert_eq!(notes(&st, "offline"), json!({"attempts": 0}));
        }
        assert_eq!(
            test_pipeline().drain(&st),
            [("offline".to_string(), "delivered".to_string())]
        );
    }

    /// A cloud payload never brings notes of its own: they would make the
    /// agent skip the job (CUPS has it) or retire it unprinted.
    #[test]
    fn a_payload_cannot_bring_notes_of_its_own() {
        let td = tempfile::tempdir().unwrap();
        let st = store(td.path());
        let payload = json!({"id": "n-1", "cups_name": "P", "content_type": "png_base64",
            "content": PNG_1X1_B64, LOCAL_KEY: {"attempts": 9,
            "cups_job_id": "P-1", "submitted_at": "2026-10-08T12:00:00+00:00"}});
        let j = PrintJob::from_dict(payload.as_object().unwrap()).unwrap();
        let printed = Arc::new(AtomicUsize::new(0));
        let pr = printed.clone();
        let p = Pipeline {
            lp: Arc::new(move |_, _, _| {
                pr.fetch_add(1, Ordering::SeqCst);
                Ok(None)
            }),
            ..test_pipeline()
        };
        assert_eq!(p.process(&j, &st).unwrap(), JobOutcome::Delivered);
        assert_eq!(printed.load(Ordering::SeqCst), 1);
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
        let block = |alerts: &str| {
            format!("Q-1  ben  1024  date\n\tStatus: \n\tAlerts: {alerts}\n\tqueued for Q\n")
        };
        for failed in [
            "job-canceled-by-user",
            "job-canceled-by-operator",
            "job-canceled-at-device",
            "job-cancelled-by-user",
            "aborted-by-system",
            "printer-stopped job-aborted-by-system",
        ] {
            let outcome = completed_outcome(&block(failed), "Q-1");
            assert_eq!(outcome, CupsOutcome::Error, "{failed}");
        }
        for done in [
            "job-completed-successfully",
            "job-completed-with-warnings",
            "",
        ] {
            let outcome = completed_outcome(&block(done), "Q-1");
            assert_eq!(outcome, CupsOutcome::Printed, "{done}");
        }
        // A header, whatever it says, holds no failure reason.
        assert_eq!(
            completed_outcome("Q-1 user 1024 ... aborted", "Q-1"),
            CupsOutcome::Printed
        );
        assert_eq!(completed_outcome("", "Q-1"), CupsOutcome::Printed);
    }

    /// A fake `lp` in `dir`, run as `sh <script>`: it answers like CUPS on a
    /// German node (`german` on stderr, exit 1, when `fail`), unless both
    /// LC_ALL and LC_MESSAGES name the C locale.
    fn german_lp(dir: &Path, english: &str, german: &str, fail: bool) -> String {
        let script = dir.join("lp");
        let (to, exit) = if fail { (" >&2", 1) } else { ("", 0) };
        fs::write(
            &script,
            format!(
                "case \"${{LC_ALL:-}}\" in C|C.*) all=C ;; *) all= ;; esac\n\
                 case \"${{LC_MESSAGES:-}}\" in C|C.*) msgs=C ;; *) msgs= ;; esac\n\
                 if [ \"$all$msgs\" = CC ]; then echo '{english}'{to}\n\
                 else echo '{german}'{to}; fi\nexit {exit}\n"
            ),
        )
        .unwrap();
        script.display().to_string()
    }

    /// J6: CUPS translates `lp`'s answer ("Anfrage-ID ist Zebra-42" in
    /// German), so on a German node no job got its CUPS request id (none
    /// was followed to printed) and an unknown queue was a retryable lp
    /// error. lp runs untranslated, as lpstat does ("Alerts:" is "Alarme:").
    #[test]
    fn lp_runs_untranslated() {
        let td = tempfile::tempdir().unwrap();
        let file = td.path().join("label.zpl");
        fs::write(&file, "^XA^XZ").unwrap();
        let args = LpArgs {
            title: Some("Ship label"),
            copies: 1,
            raw: true,
        };
        let lp = german_lp(
            td.path(),
            "request id is Zebra-42 (1 file(s))",
            "Anfrage-ID ist Zebra-42 (1 Datei(en))",
            false,
        );
        assert_eq!(
            run_lp(&["sh", &lp], "Zebra", &file, &args)
                .unwrap()
                .as_deref(),
            Some("Zebra-42")
        );
        let lp = german_lp(
            td.path(),
            "lp: Error - The printer or class does not exist.",
            "lp: Fehler - Der Drucker oder die Klasse existiert nicht.",
            true,
        );
        let err = run_lp(&["sh", &lp], "Gone", &file, &args).unwrap_err();
        assert_eq!(err.code, "unknown_queue", "{}", err.message);
        assert!(err.is_permanent());
    }

    /// `lpstat -W completed -l`, newest first (real CUPS layout).
    const COMPLETED: &str = "\
Zebra-43                ben            1024   Wed 08 Oct 2026 01:03:00 AM CDT
\tStatus:
\tAlerts: job-completed-successfully
\tqueued for Zebra
Zebra-42                ben            1024   Wed 08 Oct 2026 01:02:00 AM CDT
\tStatus:
\tAlerts: job-canceled-by-user
\tqueued for Zebra
Zebra-41                ben            1024   Wed 08 Oct 2026 01:01:00 AM CDT
\tStatus:
\tAlerts: job-completed-successfully
\tqueued for Zebra
Zebra-40                ben            1024   Wed 08 Oct 2026 01:00:00 AM CDT
\tStatus:
\tAlerts: job-aborted-by-system
\tqueued for Zebra
";

    /// A neighbour that was canceled or aborted must not mark a printed job failed.
    #[test]
    fn completed_outcome_reads_only_the_jobs_own_block() {
        assert_eq!(
            completed_outcome(COMPLETED, "Zebra-43"),
            CupsOutcome::Printed
        );
        assert_eq!(completed_outcome(COMPLETED, "Zebra-42"), CupsOutcome::Error);
        assert_eq!(
            completed_outcome(COMPLETED, "Zebra-41"),
            CupsOutcome::Printed
        );
        assert_eq!(completed_outcome(COMPLETED, "Zebra-40"), CupsOutcome::Error);
        // A prefix of other ids is not a match.
        assert_eq!(
            completed_outcome(COMPLETED, "Zebra-4"),
            CupsOutcome::Printed
        );
        // The queue name itself is not a marker.
        let named = "Canceled_Returns-7  ben  1024  date\n\tAlerts: job-completed-successfully\n\tqueued for Canceled_Returns\n";
        assert_eq!(
            completed_outcome(named, "Canceled_Returns-7"),
            CupsOutcome::Printed
        );
    }

    /// Only the job's Alerts line (its state reasons) can mark it failed: a
    /// user name, or a printer message, saying "canceled" does not.
    #[test]
    fn completed_outcome_reads_only_the_alerts_line() {
        let done = "\
Zebra-51                cancelled-orders 1024   Wed 08 Oct 2026 01:06:00 AM CDT
\tStatus: Ready; the previous job was aborted at the printer
\tAlerts: job-completed-successfully
\tqueued for Zebra
Zebra-50                canceled       1024   Wed 08 Oct 2026 01:05:00 AM CDT
\tStatus: Job canceled by user.
\tAlerts: job-completed-successfully
\tqueued for Zebra
Zebra-49                ben            1024   Wed 08 Oct 2026 01:04:00 AM CDT
\tStatus: Ready
\tAlerts: job-canceled-by-user
\tqueued for Zebra
Zebra-48                ben            1024   Wed 08 Oct 2026 01:03:00 AM CDT
\tAlerts: processing-to-stop-point aborted-by-system
\tqueued for Zebra
";
        assert_eq!(completed_outcome(done, "Zebra-51"), CupsOutcome::Printed);
        assert_eq!(completed_outcome(done, "Zebra-50"), CupsOutcome::Printed);
        assert_eq!(completed_outcome(done, "Zebra-49"), CupsOutcome::Error);
        assert_eq!(completed_outcome(done, "Zebra-48"), CupsOutcome::Error);
    }

    #[test]
    fn active_ids_match_whole_tokens() {
        let listing = "Zebra-40                ben   1024   date\nZebra-41                ben   1024   date\n";
        let ids = listed_job_ids(listing);
        assert!(ids.contains("Zebra-40") && ids.contains("Zebra-41"));
        assert!(!ids.contains("Zebra-4"));
        assert!(listed_job_ids("").is_empty());
    }

    type LpstatCalls = Arc<Mutex<Vec<String>>>;

    /// A wait context that is never stopped.
    fn wait_ctx(tick: Option<TickFn>) -> WaitCtx {
        WaitCtx {
            tick,
            stop: Arc::default(),
        }
    }

    fn ok(stdout: &str) -> io::Result<CmdOutput> {
        Ok(CmdOutput {
            success: true,
            stdout: stdout.into(),
            stderr: String::new(),
        })
    }

    fn scheduler_down() -> io::Result<CmdOutput> {
        Ok(CmdOutput {
            success: false,
            stdout: String::new(),
            stderr: "lpstat: Scheduler is not running.\n".into(),
        })
    }

    /// A failed `lpstat` (cupsd down) is a query failure, never "the job left
    /// the queue": after 5 failures the job stays delivered (Unknown), not printed.
    #[test]
    fn lpstat_failure_is_not_completion() {
        let calls: LpstatCalls = Arc::default();
        let c = calls.clone();
        let lpstat = move |args: &[&str]| {
            c.lock().unwrap().push(args.join(" "));
            scheduler_down()
        };
        let outcome = wait_cups_job_with(
            "Zebra_1-42 (1 file(s))",
            Duration::from_secs(60),
            Duration::from_millis(10),
            &wait_ctx(None),
            &lpstat,
        );
        assert_eq!(outcome, CupsOutcome::Unknown);
        assert_eq!(*calls.lock().unwrap(), vec!["-W not-completed"; 5]);
        // The same answer through the watcher's batch query.
        assert!(query_cups(&|_: &[&str]| scheduler_down(), &["Zebra_1-42".into()]).is_err());
    }

    #[test]
    fn failed_completed_query_is_retried() {
        let calls: LpstatCalls = Arc::default();
        let c = calls.clone();
        let lpstat = move |args: &[&str]| {
            let mut calls = c.lock().unwrap();
            calls.push(args.join(" "));
            match (args[1], calls.len()) {
                ("not-completed", _) => ok(""),
                ("completed", 2) => scheduler_down(),
                _ => ok(COMPLETED),
            }
        };
        let outcome = wait_cups_job_with(
            "Zebra-43",
            Duration::from_secs(60),
            Duration::from_millis(10),
            &wait_ctx(None),
            &lpstat,
        );
        assert_eq!(outcome, CupsOutcome::Printed);
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "-W not-completed",
                "-W completed -l",
                "-W not-completed",
                "-W completed -l"
            ]
        );
    }

    #[test]
    fn sync_wait_ticks_while_active_then_reports() {
        let polls = Arc::new(AtomicUsize::new(0));
        let p = polls.clone();
        let lpstat = move |args: &[&str]| {
            if args[1] == "not-completed" {
                let n = p.fetch_add(1, Ordering::SeqCst);
                // Neighbour Zebra-430 stays active; ours is listed for two polls.
                return ok(if n < 2 {
                    "Zebra-430 ben 1 date\nZebra-43 ben 1 date\n"
                } else {
                    "Zebra-430 ben 1 date\n"
                });
            }
            ok(COMPLETED)
        };
        let ticks = Arc::new(AtomicUsize::new(0));
        let t = ticks.clone();
        let tick: TickFn = Arc::new(move || {
            t.fetch_add(1, Ordering::SeqCst);
        });
        let outcome = wait_cups_job_with(
            "Zebra-43",
            Duration::from_secs(60),
            Duration::from_millis(10),
            &wait_ctx(Some(tick)),
            &lpstat,
        );
        assert_eq!(outcome, CupsOutcome::Printed);
        assert_eq!(ticks.load(Ordering::SeqCst), 2);
    }

    /// In the synchronous wait a panicking `lpstat` run is one failed query
    /// and a panicking tick is skipped: the wait goes on to the outcome.
    #[test]
    fn sync_wait_survives_panicking_lpstat_and_tick() {
        let polls = Arc::new(AtomicUsize::new(0));
        let p = polls.clone();
        let lpstat = move |args: &[&str]| -> io::Result<CmdOutput> {
            if args[1] != "not-completed" {
                return ok(COMPLETED);
            }
            match p.fetch_add(1, Ordering::SeqCst) {
                0 => panic!("failed to spawn thread"),
                1 => ok("Zebra-43 ben 1 date\n"),
                _ => ok(""),
            }
        };
        let tick: TickFn = Arc::new(|| panic!("wait tick panicked"));
        let outcome = wait_cups_job_with(
            "Zebra-43",
            Duration::from_secs(60),
            Duration::from_millis(10),
            &wait_ctx(Some(tick)),
            &lpstat,
        );
        assert_eq!(outcome, CupsOutcome::Printed);
        assert_eq!(polls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn batch_query_resolves_only_finished_jobs() {
        let keys: Vec<String> = ["Zebra-41", "Zebra-42", "Zebra-44"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let lpstat = |args: &[&str]| {
            ok(if args[1] == "not-completed" {
                "Zebra-44  ben  1024  date\n"
            } else {
                COMPLETED
            })
        };
        let done = query_cups(&lpstat, &keys).unwrap();
        assert_eq!(
            done,
            HashMap::from([
                ("Zebra-41".to_string(), CupsOutcome::Printed),
                ("Zebra-42".to_string(), CupsOutcome::Error),
            ])
        );
        // Nothing finished: the completed list is not queried at all.
        let only_active = |args: &[&str]| {
            assert_eq!(args[1], "not-completed");
            ok("Zebra-44  ben  1024  date\n")
        };
        assert!(query_cups(&only_active, &keys[2..]).unwrap().is_empty());
    }

    /// J3: a job `lp` already took is finished from where `lpstat` finds it
    /// ([`Pipeline::resume`]), never printed again. Still not completed:
    /// active, and the history is not read. In the history: printed, or
    /// failed when canceled or aborted. In neither listing: forgotten, which
    /// leaves it delivered (read as printed, it would claim a label no one
    /// saw). CUPS down at either query: an error, never a state.
    #[test]
    fn lookup_cups_job_reads_where_cups_has_the_job() {
        let calls: LpstatCalls = Arc::default();
        let c = calls.clone();
        let lpstat = move |args: &[&str]| {
            c.lock().unwrap().push(args.join(" "));
            ok(if args[1] == "not-completed" {
                "Zebra-44                ben            1024   Wed 08 Oct 2026 01:04:00 AM CDT\n"
            } else {
                COMPLETED
            })
        };
        let lookup = |key: &str| {
            calls.lock().unwrap().clear();
            let state = lookup_cups_job(&lpstat, key);
            (state, calls.lock().unwrap().clone())
        };

        let (state, asked) = lookup("Zebra-44");
        assert_eq!(state, Ok(CupsJobState::Active));
        assert_eq!(asked, ["-W not-completed"]);
        for (key, expected, why) in [
            ("Zebra-43", CupsJobState::Printed, "completed successfully"),
            ("Zebra-42", CupsJobState::Failed, "canceled by user"),
            ("Zebra-40", CupsJobState::Failed, "aborted by system"),
            ("Zebra-39", CupsJobState::Forgotten, "not in the history"),
            ("Zebra-4", CupsJobState::Forgotten, "prefix of listed ids"),
        ] {
            let (state, asked) = lookup(key);
            assert_eq!(state, Ok(expected), "{key}: {why}");
            assert_eq!(asked, ["-W not-completed", "-W completed -l"], "{key}");
        }

        let down = lookup_cups_job(&|_: &[&str]| scheduler_down(), "Zebra-43").unwrap_err();
        assert!(
            down.contains("not-completed failed: lpstat: Scheduler is not running"),
            "{down}"
        );
        let history_down = |args: &[&str]| {
            if args[1] == "not-completed" {
                ok("")
            } else {
                scheduler_down()
            }
        };
        let down = lookup_cups_job(&history_down, "Zebra-43").unwrap_err();
        assert!(
            down.contains("completed -l failed: lpstat: Scheduler is not running"),
            "{down}"
        );
    }
}
