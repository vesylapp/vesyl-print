//! App OTA: download, verify, atomic install, rollback.
//!
//! Production layout:
//!
//! ```text
//! /opt/vesyl-print/
//!   current -> releases/0.4.0
//!   releases/0.3.0/
//!   releases/0.4.0/
//!   releases/0.5.0.staging/  # an install, put together before it is swapped in
//!   update/                  # downloads
//! ```
//!
//! Lab/dev without root uses `{state_dir}/app/` the same way.
//!
//! Control plane: heartbeat JSON may include `desired_agent_version` and an
//! optional `update_url` (manifest). Artifacts are HTTPS tarballs verified by
//! SHA-256 + Ed25519 signature over the canonical manifest (signature field
//! excluded) — byte-compatible with `scripts/build-release.sh`.
//!
//! A slot is runnable ([`slot_is_runnable`]) when it holds an executable
//! `vesyl-print`, the binary the units exec (the agent and CLI; the LCD
//! display is still Python and ships in the same slot). Installs and
//! rollbacks activate no other slot, and the apply-update helper refuses one.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{Read, Seek, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use base64::Engine as _;
use chrono::{DateTime, SecondsFormat, Utc};
use ed25519_dalek::pkcs8::DecodePublicKey;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::{agent_version, Config, ENV_INSTALL_ROOT};
use crate::net::{self, Redirects, Timeouts};
use crate::util::{euid, fd_path, opt_str, py_int, py_str, truthy, write_durable};
use crate::JsonObject;

const LOG: &str = "vesyl-print.update";

/// `last_error` prefix written when the post-update health gate fails.
const HEALTH_FAILED: &str = "health failed";

/// `last_error` prefix written when the gate's deadline passed while the
/// agent it replaces was still running: the restart into the new version
/// never happened, so that version never ran and is not held (see
/// [`failed_health_gate`]).
const RESTART_MISSED: &str = "restart never happened";

/// `last_error` prefix written when `current` was switched away from the
/// gate's version by hand while the gate was open: `update rollback`, or an
/// `update apply` without --restart, which may well be a newer version.
const CURRENT_CHANGED: &str = "current changed";

/// `last_error` prefix `update rollback` writes (see [`record_manual_rollback`]).
const MANUAL_ROLLBACK: &str = "manual rollback";

/// [`UpdateError`] code of an install the agent's stop cut short before
/// anything was activated: not a failure, and tried again on the next start.
const STOPPED: &str = "stopped";

/// [`UpdateError`] code of an install this machine could not carry out (a
/// full or read-only disk, a permission), whatever the release: retried.
const INSTALL_FAILED: &str = "install_failed";

/// [`UpdateError`] code of a release that is not the version asked for: its
/// manifest names another, or its archive does.
const VERSION_MISMATCH: &str = "version_mismatch";

/// [`UpdateError`] code of a reinstall of the slot `current` points at on a
/// filesystem that cannot swap two directories in one step.
const NO_EXCHANGE: &str = "no_exchange";

/// After a failure that may pass (see [`fails_for_good`]), the next attempt
/// at the same version waits this long, doubled after each failure in a
/// row up to [`RETRY_MAX_SECONDS`].
const RETRY_FIRST_SECONDS: i64 = 60;
const RETRY_MAX_SECONDS: i64 = 3600;

/// Public key shipped with this build (rotated by shipping a new release).
const BUNDLED_PUBLIC_KEY_PEM: &str = include_str!("../../../../keys/update_public.pem");

/// After activate + restart, wait this long for whoami (or local checks if
/// unpaired) before auto-rolling back to the previous slot.
pub const DEFAULT_HEALTH_GATE_SECONDS: i64 = 120;

// Status lifecycle:
//   idle → downloading → installing → pending_health → idle
//                                      ↘ failed | rolled_back
pub const STATUS_IDLE: &str = "idle";
pub const STATUS_CHECKING: &str = "checking";
pub const STATUS_DOWNLOADING: &str = "downloading";
pub const STATUS_INSTALLING: &str = "installing";
pub const STATUS_PENDING_HEALTH: &str = "pending_health";
pub const STATUS_FAILED: &str = "failed";
pub const STATUS_ROLLED_BACK: &str = "rolled_back";

/// While in these states, do not pull/process new print jobs (OTA in progress).
const JOB_PAUSE_STATUSES: &[&str] = &[STATUS_DOWNLOADING, STATUS_INSTALLING, STATUS_PENDING_HEALTH];

/// What the units exec from a slot (`<slot>/vesyl-print agent`), and what
/// `scripts/apply-update` requires before it activates one.
const SLOT_BINARY: &str = "vesyl-print";

/// An install puts the release together in `<slot>` + this, beside the
/// slot (see [`Staged`]).
const STAGING_SUFFIX: &str = ".staging";

/// Installed root-owned helper (NOPASSWD sudoers on appliances).
const APPLY_HELPER: &str = "/usr/local/lib/vesyl-print/apply-update";

fn version_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // [0-9], not \d: the regex crate's \d is any Unicode digit (it took
    // `١.٢.٣`), the scripts' [0-9] only ASCII ones (in the C locale).
    RE.get_or_init(|| Regex::new(r"^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.]+)?$").expect("regex"))
}

/// A release version: the pattern `scripts/apply-update`, build-release.sh
/// and setup.sh check too, digits ASCII only, with a last dot-component of
/// `staging` refused, as they refuse it. `<version>.staging` is the
/// directory an install leaves beside its slot if it dies midway, so such a
/// name never counts as a release ([`list_releases`], `current`), and no
/// manifest can name a slot that is another version's staging dir.
pub fn is_version(s: &str) -> bool {
    version_re().is_match(s) && !s.ends_with(STAGING_SUFFIX)
}

/// `v` as a release version is written: trimmed, without a leading `v`
/// (`v0.5.0` is 0.5.0, as in the release tags). Check it with [`is_version`].
pub fn normalize_version(v: &str) -> &str {
    let v = v.trim();
    v.strip_prefix('v').unwrap_or(v)
}

/// True when `dir` is a slot the units can start and `scripts/apply-update`
/// will activate: a real directory (not a symlink) holding `vesyl-print`, a
/// regular file with an execute bit. As with the helper's `[[ -f && -x ]]`,
/// a symlink there is judged by what it points at.
pub fn slot_is_runnable(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir())
        && fs::metadata(dir.join(SLOT_BINARY))
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct UpdateError {
    pub message: String,
    pub code: &'static str,
}

impl UpdateError {
    pub fn new(message: impl Into<String>, code: &'static str) -> Self {
        UpdateError {
            message: message.into(),
            code,
        }
    }
}

fn io_err(code: &'static str) -> impl Fn(std::io::Error) -> UpdateError {
    move |e| UpdateError::new(e.to_string(), code)
}

/// [`STOPPED`] once the agent is stopping. An OTA checks this between its
/// steps: a `systemctl stop` must neither wait for the rest of a download
/// nor be undone by the restart at its end.
fn unless_stopping(stop: &AtomicBool, when: &str) -> Result<(), UpdateError> {
    if stop.load(Ordering::SeqCst) {
        return Err(UpdateError::new(
            format!("update stopped {when}: the agent is stopping"),
            STOPPED,
        ));
    }
    Ok(())
}

/// True for an [`UpdateError`] code that another try at the same release
/// would only repeat: what was fetched is not a release this node can
/// install (its manifest, signature, checksum, archive or version), or this
/// agent is too old for it. Anything else (the network, an HTTP error, the
/// disk, the helper) may pass, and is retried after a while.
fn fails_for_good(code: &str) -> bool {
    matches!(
        code,
        "bad_manifest"
            | "bad_signature"
            | "bad_checksum"
            | "bad_archive"
            | "too_old"
            | VERSION_MISMATCH
            | NO_EXCHANGE
    )
}

/// Seconds to wait before the next attempt after `attempts` failures in a
/// row that may pass: [`RETRY_FIRST_SECONDS`], doubling, at most
/// [`RETRY_MAX_SECONDS`].
fn retry_delay_seconds(attempts: i64) -> i64 {
    let doublings = (attempts.max(1) - 1).min(16) as u32;
    (RETRY_FIRST_SECONDS << doublings).min(RETRY_MAX_SECONDS)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateStatus {
    pub status: String,
    pub current_version: String,
    pub target_version: Option<String>,
    pub last_error: Option<String>,
    pub last_checked_at: Option<String>,
    pub channel: Option<String>,
    /// Health gate: slot we left so we can auto-rollback if the new agent is unhealthy.
    pub previous_version: Option<String>,
    pub health_deadline_at: Option<String>,
    pub health_attempts: i64,
    /// When the open health gate was armed (RFC 3339, to the microsecond).
    /// The agent the activation replaces started before this; one started
    /// after it is judged by the gate (see `replaced_by_gated_activation`).
    /// Written to `update_status.json` only while set.
    pub armed_at: Option<String>,
    /// The [`UpdateError`] code of the failed install of `target_version`
    /// that `last_error` describes: whether it is held or retried (see
    /// [`fails_for_good`]). Written only while set.
    pub last_error_code: Option<String>,
    /// Failed installs of `target_version` in a row. Written only while
    /// not 0.
    pub attempts: i64,
    /// No new attempt at `target_version` before this (RFC 3339): the
    /// backoff after a failure that may pass. Written only while set.
    pub retry_at: Option<String>,
}

impl Default for UpdateStatus {
    fn default() -> Self {
        UpdateStatus {
            status: STATUS_IDLE.into(),
            current_version: String::new(),
            target_version: None,
            last_error: None,
            last_checked_at: None,
            channel: None,
            previous_version: None,
            health_deadline_at: None,
            health_attempts: 0,
            armed_at: None,
            last_error_code: None,
            attempts: 0,
            retry_at: None,
        }
    }
}

impl UpdateStatus {
    pub fn with_status(status: &str) -> Self {
        UpdateStatus {
            status: status.into(),
            ..Default::default()
        }
    }

    pub fn to_dict(&self) -> JsonObject {
        let v = serde_json::json!({
            "status": self.status,
            "current_version": self.current_version,
            "target_version": self.target_version,
            "last_error": self.last_error,
            "last_checked_at": self.last_checked_at,
            "channel": self.channel,
            "previous_version": self.previous_version,
            "health_deadline_at": self.health_deadline_at,
            "health_attempts": self.health_attempts,
        });
        let mut d = v.as_object().cloned().expect("object");
        // Fields added since: only while set, so the file stays as it was
        // for every status that does not need them.
        for (key, value) in [
            ("armed_at", &self.armed_at),
            ("last_error_code", &self.last_error_code),
            ("retry_at", &self.retry_at),
        ] {
            if let Some(v) = value {
                d.insert(key.into(), v.clone().into());
            }
        }
        if self.attempts != 0 {
            d.insert("attempts".into(), self.attempts.into());
        }
        d
    }

    fn is(&self, status: &str) -> bool {
        self.status == status
    }

    /// Forget the failed attempts at `target_version` (after a success, or
    /// when there is another version to try).
    fn clear_failures(&mut self) {
        self.last_error_code = None;
        self.attempts = 0;
        self.retry_at = None;
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReleaseManifest {
    pub version: String,
    pub channel: String,
    pub artifact_url: String,
    pub artifact_sha256: String,
    pub min_agent_version: Option<String>,
    /// Base64 Ed25519 over canonical JSON (no signature).
    pub signature: Option<String>,
    pub released_at: Option<String>,
    pub changelog: Option<String>,
    pub raw: JsonObject,
}

impl ReleaseManifest {
    pub fn from_dict(data: &JsonObject) -> Result<Self, UpdateError> {
        let s = |k: &str| data.get(k).filter(|v| truthy(v)).map(py_str);
        let version = s("version").unwrap_or_default().trim().to_string();
        if version.is_empty() || !is_version(&version) {
            return Err(UpdateError::new(
                format!("invalid version in manifest: {version:?}"),
                "bad_manifest",
            ));
        }
        let url = s("artifact_url")
            .or_else(|| s("url"))
            .ok_or_else(|| UpdateError::new("manifest missing artifact_url", "bad_manifest"))?;
        let sha = s("artifact_sha256")
            .or_else(|| s("sha256"))
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        if sha.len() != 64 || !sha.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(UpdateError::new(
                "manifest missing or invalid artifact_sha256",
                "bad_manifest",
            ));
        }
        Ok(ReleaseManifest {
            version,
            channel: s("channel").unwrap_or_else(|| "stable".into()),
            artifact_url: url,
            artifact_sha256: sha,
            min_agent_version: s("min_agent_version"),
            signature: s("signature"),
            released_at: s("released_at"),
            changelog: s("changelog"),
            raw: data.clone(),
        })
    }

    /// Stable JSON for signing: all fields except signature, sorted keys.
    ///
    /// Must stay byte-identical to what `scripts/build-release.sh` signs,
    /// `jq -S -c -a` of the manifest without `signature` and nulls (see
    /// `canonical_json`); `tests/build_release.rs` checks the two agree.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let body: JsonObject = if !self.raw.is_empty() {
            self.raw
                .iter()
                .filter(|(k, v)| k.as_str() != "signature" && !v.is_null())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        } else {
            let mut b = JsonObject::new();
            b.insert("version".into(), self.version.clone().into());
            b.insert("channel".into(), self.channel.clone().into());
            b.insert("artifact_url".into(), self.artifact_url.clone().into());
            b.insert(
                "artifact_sha256".into(),
                self.artifact_sha256.clone().into(),
            );
            for (k, v) in [
                ("min_agent_version", &self.min_agent_version),
                ("released_at", &self.released_at),
                ("changelog", &self.changelog),
            ] {
                if let Some(v) = v.as_ref().filter(|v| !v.is_empty()) {
                    b.insert(k.into(), v.clone().into());
                }
            }
            b
        };
        let mut out = String::new();
        canonical_json(&Value::Object(body), &mut out);
        out.into_bytes()
    }
}

/// Serialize as `jq -S -c -a` does: keys sorted by code point (serde_json's
/// Map is a BTreeMap), no whitespace, `\"` `\\` `\n` `\r` `\t` `\b` `\f`,
/// and every other character outside printable ASCII as `\uXXXX` (a UTF-16
/// surrogate pair above U+FFFF). Manifests signed before build-release.sh
/// used jq got the same bytes from Python's `json.dumps(…, sort_keys=True,
/// separators=(",", ":"))`.
fn canonical_json(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => {
            out.push('"');
            for ch in s.chars() {
                match ch {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    '\u{08}' => out.push_str("\\b"),
                    '\u{0c}' => out.push_str("\\f"),
                    c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                        let mut buf = [0u16; 2];
                        for unit in c.encode_utf16(&mut buf) {
                            out.push_str(&format!("\\u{unit:04x}"));
                        }
                    }
                    c => out.push(c),
                }
            }
            out.push('"');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical_json(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (k, val)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical_json(&Value::String(k.clone()), out);
                out.push(':');
                canonical_json(val, out);
            }
            out.push('}');
        }
    }
}

/// Version of the running agent.
pub fn package_version() -> &'static str {
    agent_version()
}

pub fn parse_version(v: &str) -> Vec<i64> {
    let core = v.split('-').next().unwrap_or("");
    let core = core.split('+').next().unwrap_or("");
    core.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

/// Ordering of the numbers of two versions only (missing parts = 0), any
/// suffix ignored: what `build-release.sh`'s `version_core_ge` compares, so
/// the `min_agent_version` floor means the same to the build that checks it
/// and to the agent that applies it (a 0.4.0-rc.1 agent meets a 0.4.0
/// floor, as the build that let it be numbered so assumed).
fn version_number_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut ta, mut tb) = (parse_version(a), parse_version(b));
    let n = ta.len().max(tb.len());
    ta.resize(n, 0);
    tb.resize(n, 0);
    ta.cmp(&tb)
}

/// What follows the numbers of `v` (`-rc.1` in `0.9.1-rc.1`, `+b7` in
/// `0.9.1+b7`, `.lab` in `0.5.0.lab`), or "" when it has nothing else.
fn version_suffix(v: &str) -> &str {
    let bytes = v.as_bytes();
    // End of the last number of the leading `N(.N)*`.
    let (mut end, mut i) = (0, 0);
    loop {
        let start = i;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            break;
        }
        end = i;
        if bytes.get(i) != Some(&b'.') {
            break;
        }
        i += 1;
    }
    &v[end..]
}

/// Semver's order of two pre-release strings (after the `-`, before any
/// `+`): identifier by identifier, numbers numerically and below words, and
/// fewer identifiers first when all before are equal.
fn prerelease_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let ident = |s: &str| -> Result<u64, String> { s.parse::<u64>().map_err(|_| s.to_string()) };
    let ta = a.split('.').map(ident);
    let tb = b.split('.').map(ident);
    // Ok (a number) sorts below Err (a word), as semver has it.
    ta.cmp(tb)
}

/// Ordering of two semver-ish version strings: numeric first (missing parts
/// = 0, so 0.3 is 0.3.0). The same numbers with a suffix then sort as
/// semver has them: a pre-release (`-rc.1`) before the release
/// (0.9.1-rc.1 < 0.9.1), pre-releases by their identifiers; any other
/// suffix (`+b7`, `.lab`) after the bare release, by its text. So two
/// different releases never compare equal: a device running 0.9.1-rc.1 is
/// not taken to run 0.9.1 already.
pub fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let by_numbers = version_number_cmp(a, b);
    if by_numbers.is_ne() {
        return by_numbers;
    }
    let (sa, sb) = (version_suffix(a), version_suffix(b));
    fn pre(s: &str) -> Option<&str> {
        s.strip_prefix('-')
            .map(|p| p.split('+').next().unwrap_or(""))
    }
    match (pre(sa), pre(sb)) {
        (Some(pa), Some(pb)) => prerelease_cmp(pa, pb).then_with(|| sa.cmp(sb)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => sa.cmp(sb),
    }
}

fn same_version(a: &str, b: &str) -> bool {
    version_cmp(a, b).is_eq()
}

fn dir_writable(path: &str) -> bool {
    let Ok(c) = std::ffi::CString::new(path) else {
        return false;
    };
    // SAFETY: c is a valid NUL-terminated string.
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

/// Prefer /opt/vesyl-print; else state_dir/app for lab installs.
pub fn resolve_install_root(cfg: &Config) -> PathBuf {
    if let Some(env) = std::env::var_os(ENV_INSTALL_ROOT).filter(|v| !v.is_empty()) {
        return PathBuf::from(env);
    }
    let opt = Path::new("/opt/vesyl-print");
    if opt.is_dir() || dir_writable("/opt") {
        return opt.to_path_buf();
    }
    cfg.state_dir.join("app")
}

pub fn current_release_dir(install_root: &Path) -> Option<PathBuf> {
    let cur = install_root.join("current");
    if cur.is_symlink() || cur.is_dir() {
        return fs::canonicalize(cur).ok();
    }
    None
}

fn dir_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn current_release_version(install_root: &Path) -> Option<String> {
    current_release_dir(install_root)
        .map(|c| dir_name(&c))
        .filter(|n| is_version(n))
}

pub fn list_releases(install_root: &Path) -> Vec<String> {
    let mut vers: Vec<String> = fs::read_dir(install_root.join("releases"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| is_version(n))
        .collect();
    vers.sort_by(|a, b| version_cmp(a, b));
    vers
}

pub fn health_gate_seconds(cfg: &Config) -> i64 {
    cfg.update_health_gate_seconds.max(15)
}

/// True while OTA is downloading, installing, or waiting on health gate.
pub fn should_pause_jobs(status: Option<&UpdateStatus>) -> bool {
    status.is_some_and(|s| JOB_PAUSE_STATUSES.contains(&s.status.as_str()))
}

/// Read `update_status.json` and apply [`should_pause_jobs`].
pub fn should_pause_jobs_from_path(path: &Path) -> bool {
    should_pause_jobs(read_update_status(path).as_ref())
}

// --- crypto ----------------------------------------------------------------

/// Explicit PEM text, else key file, else the key bundled into this build.
pub fn load_public_key_pem(
    path: Option<&Path>,
    pem_text: Option<&str>,
) -> Result<String, UpdateError> {
    if let Some(t) = pem_text.filter(|t| !t.trim().is_empty()) {
        return Ok(t.to_string());
    }
    if let Some(p) = path.filter(|p| p.is_file()) {
        let raw = fs::read(p).map_err(|e| {
            UpdateError::new(
                format!("cannot read update public key {}: {e}", p.display()),
                "bad_public_key",
            )
        })?;
        return String::from_utf8(raw).map_err(|_| {
            UpdateError::new(
                format!(
                    "update public key {} is not a PEM file (DER or corrupt?)",
                    p.display()
                ),
                "bad_public_key",
            )
        });
    }
    if !BUNDLED_PUBLIC_KEY_PEM.trim().is_empty() {
        return Ok(BUNDLED_PUBLIC_KEY_PEM.to_string());
    }
    Err(UpdateError::new(
        "no update public key configured (keys/update_public.pem or config)",
        "no_public_key",
    ))
}

/// Verify an Ed25519 signature (base64) with an SPKI PEM public key.
pub fn verify_ed25519(
    public_key_pem: &str,
    message: &[u8],
    signature_b64: &str,
) -> Result<(), UpdateError> {
    let key = VerifyingKey::from_public_key_pem(public_key_pem.trim()).map_err(|e| {
        UpdateError::new(format!("invalid update public key: {e}"), "bad_public_key")
    })?;
    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(signature_b64.trim())
        .map_err(|_| UpdateError::new("invalid signature encoding", "bad_signature"))?;
    let sig = Signature::from_slice(&sig_bytes)
        .map_err(|_| UpdateError::new("invalid signature encoding", "bad_signature"))?;
    key.verify(message, &sig)
        .map_err(|_| UpdateError::new("manifest signature verification failed", "bad_signature"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    sha256_of(&File::open(path)?)
}

/// SHA-256 of `f` from where it is read next to its end: all of a file just
/// opened.
fn sha256_of(mut f: &File) -> std::io::Result<String> {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

pub fn verify_manifest(
    manifest: &ReleaseManifest,
    public_key_pem: Option<&str>,
    require_signature: bool,
) -> Result<(), UpdateError> {
    if !require_signature {
        return Ok(());
    }
    let sig = manifest
        .signature
        .as_deref()
        .ok_or_else(|| UpdateError::new("manifest missing signature", "bad_signature"))?;
    let pem = match public_key_pem {
        Some(p) => p.to_string(),
        None => load_public_key_pem(None, None)?,
    };
    verify_ed25519(&pem, &manifest.canonical_bytes(), sig)
}

/// The key every apply path verifies manifests with, per config.
///
/// `Ok(None)` only when `update_require_signature` is off. When signatures
/// are required, a configured key that cannot be read is an error: a broken
/// key file must fail the update, never quietly turn verification off.
pub fn manifest_public_key(cfg: &Config) -> Result<Option<String>, UpdateError> {
    if !cfg.update_require_signature {
        return Ok(None);
    }
    let key_path =
        (!cfg.update_public_key_path.is_empty()).then(|| Path::new(&cfg.update_public_key_path));
    load_public_key_pem(key_path, None).map(Some)
}

// --- download / install ----------------------------------------------------

/// Most characters of a URL that an error or a log line shows: the server
/// or the manifest picks how long it is.
const SHOWN_URL_CHARS: usize = 200;

/// `url` as an error or a log line shows it (both reach `update_status.json`,
/// the heartbeat or the journal): [`net::redact_url`]'s form, without the
/// userinfo, query or fragment a presigned URL carries its signature in,
/// cut to [`SHOWN_URL_CHARS`] as cloud.rs cuts a redirect target.
pub(crate) fn shown_url(url: &str) -> String {
    net::redact_url(url).chars().take(SHOWN_URL_CHARS).collect()
}

/// `e`, from a request ureq could not make, as an error says it: in ureq's
/// words, except for a redirect target it could not follow (a space or a
/// byte a URL may not hold, `..` above the root). ureq quotes that target
/// whole (`location header is malformed: <target>`), and the server picks
/// it, so it is named as [`shown_url`] shows it.
fn network_error(e: &ureq::Error) -> String {
    const MALFORMED: &str = "location header is malformed: ";
    if let ureq::Error::Protocol(protocol) = e {
        let text = protocol.to_string();
        if let Some(target) = text.strip_prefix(MALFORMED) {
            return format!("network error: protocol: {MALFORMED}{}", shown_url(target));
        }
    }
    format!("network error: {e}")
}

/// Open a URL for streaming. `file://` is supported for lab installs/tests.
///
/// HTTP goes through [`net::agent`]: urllib's timeouts (every read waits at
/// most `timeouts.idle`, so a dead connection fails while a slow but steady
/// download is not cut off at a fixed deadline), urllib's proxy rules chosen
/// again on every redirect hop, and no transparent decompression, so the
/// SHA-256 always covers the bytes the server sent. A URL that does not
/// parse is refused here, named as [`shown_url`] shows it: ureq's own error
/// would quote it whole, as it quotes a redirect target ([`network_error`]).
fn open_url(
    url: &str,
    timeouts: Timeouts,
    what: &str,
) -> Result<Box<dyn Read + Send>, UpdateError> {
    let parsed = url::Url::parse(url).map_err(|e| {
        UpdateError::new(
            format!("network error: invalid URL {:?}: {e}", shown_url(url)),
            "download_failed",
        )
    })?;
    if let ("file", Ok(path)) = (parsed.scheme(), parsed.to_file_path()) {
        return File::open(path)
            .map(|f| Box::new(f) as Box<dyn Read + Send>)
            .map_err(|e| UpdateError::new(format!("network error: {e}"), "download_failed"));
    }
    let resp = net::agent(url, timeouts, Redirects::Follow)
        .get(url)
        .header("Accept", "*/*")
        // What urllib sends: a CDN must not compress the tarball on the fly.
        .header("Accept-Encoding", "identity")
        .call()
        .map_err(|e| UpdateError::new(network_error(&e), "download_failed"))?;
    let status = resp.status().as_u16();
    // urllib raises for anything it could not turn into a 2xx (incl. a 3xx
    // without a usable Location).
    if !(200..300).contains(&status) {
        return Err(UpdateError::new(
            format!("HTTP {status} {what}"),
            "download_failed",
        ));
    }
    Ok(Box::new(resp.into_body().into_reader()))
}

/// GET `url` (the release manifest) into memory ([`Timeouts::MANIFEST`]).
pub fn http_get_bytes(url: &str) -> Result<Vec<u8>, UpdateError> {
    let mut out = Vec::new();
    open_url(
        url,
        Timeouts::MANIFEST,
        &format!("fetching {}", shown_url(url)),
    )?
    .read_to_end(&mut out)
    .map_err(|e| UpdateError::new(format!("network error: {e}"), "download_failed"))?;
    Ok(out)
}

/// Stream `url` to `dest` via `dest.part`, verifying SHA-256. Once `stop`
/// is set it gives up after the read in progress ([`STOPPED`]); like any
/// failure, that removes the `.part` file. The directory is opened first,
/// as [`crate::util::open_dir_owned`] opens it, and the download made and
/// renamed relative to it (see [`download_into`]).
pub fn http_download_to_file(
    url: &str,
    dest: &Path,
    expected_sha256: &str,
    stop: &AtomicBool,
) -> Result<(), UpdateError> {
    let name = dest.file_name().ok_or_else(|| {
        UpdateError::new(
            format!("{}: not a file name", dest.display()),
            "download_failed",
        )
    })?;
    let at = match dest.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let dir = crate::util::open_dir_owned(at).map_err(io_err("download_failed"))?;
    download_into(&dir, at, name, url, expected_sha256, stop).map(drop)
}

/// [`http_download_to_file`] into `name` in the open directory `dir` (at
/// `at`, for messages): `<name>.part` is made, renamed onto `name` and, on
/// a failure, removed relative to `dir`, so a name on the way swapped
/// meanwhile cannot take the download anywhere else (as root, `update/` is
/// the service user's to rename). Returns the artifact, open for reading
/// and writing.
fn download_into(
    dir: &File,
    at: &Path,
    name: &OsStr,
    url: &str,
    expected_sha256: &str,
    stop: &AtomicBool,
) -> Result<File, UpdateError> {
    let mut part = name.to_owned();
    part.push(".part");
    let result = (|| {
        let mut reader = open_url(url, Timeouts::ARTIFACT, "downloading artifact")?;
        let mut out = fresh_part_file(dir, at, &part).map_err(io_err("download_failed"))?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            unless_stopping(stop, "while downloading")?;
            let n = reader
                .read(&mut buf)
                .map_err(|e| UpdateError::new(format!("network error: {e}"), "download_failed"))?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])
                .map_err(io_err("download_failed"))?;
            h.update(&buf[..n]);
        }
        out.sync_all().map_err(io_err("download_failed"))?;
        let digest = hex(&h.finalize());
        if digest != expected_sha256.to_lowercase() {
            return Err(UpdateError::new(
                format!("artifact sha256 mismatch (got {}…)", &digest[..12]),
                "bad_checksum",
            ));
        }
        crate::util::rename_at(dir, &part, dir, name).map_err(io_err("download_failed"))?;
        Ok(out)
    })();
    if result.is_err() {
        let _ = crate::util::unlink_at(dir, &part);
    }
    result
}

/// A new, empty download file `name` in the open directory `dir` (at `at`),
/// mode 0644: O_EXCL and O_NOFOLLOW ([`crate::util::create_file_at`]). One
/// an earlier download left is unlinked, never reopened: a crashed `sudo
/// vesyl-print update apply` leaves it root's, which the agent could not
/// open for writing again (it owns `update/`, so it can unlink it), and
/// opening a symlink planted there would write through it. Root hands the
/// new file to the directory's owner.
fn fresh_part_file(dir: &File, at: &Path, name: &OsStr) -> std::io::Result<File> {
    match crate::util::unlink_at(dir, name) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let file = crate::util::create_file_at(dir, name, 0o644)?;
    crate::util::hand_new_file_to_dir_owner(&file, dir, at);
    Ok(file)
}

pub fn fetch_manifest(url: &str) -> Result<ReleaseManifest, UpdateError> {
    let raw = http_get_bytes(url)?;
    match serde_json::from_slice::<Value>(&raw) {
        Ok(Value::Object(data)) => ReleaseManifest::from_dict(&data),
        Ok(_) => Err(UpdateError::new(
            "manifest must be a JSON object",
            "bad_manifest",
        )),
        Err(_) => Err(UpdateError::new(
            "manifest is not valid JSON",
            "bad_manifest",
        )),
    }
}

fn releases_url(base: &str, tag: &str, name: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.contains("github.com") && base.contains("/releases/download") {
        format!("{base}/{tag}/{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// `{base}/vX.Y.Z/vesyl-print-X.Y.Z.manifest.json` on GitHub Releases, or
/// `{base}/vesyl-print-X.Y.Z.manifest.json` on a flat CDN.
pub fn default_manifest_url(releases_base_url: &str, version: &str) -> String {
    let ver = version.trim_start_matches('v');
    releases_url(
        releases_base_url,
        &format!("v{ver}"),
        &format!("vesyl-print-{ver}.manifest.json"),
    )
}

pub fn default_artifact_url(releases_base_url: &str, version: &str, arch: &str) -> String {
    let ver = version.trim_start_matches('v');
    releases_url(
        releases_base_url,
        &format!("v{ver}"),
        &format!("vesyl-print-{ver}-{arch}.tar.gz"),
    )
}

fn unsafe_archive_path(p: &Path) -> bool {
    p.is_absolute()
        || p.to_string_lossy().starts_with('/')
        || p.components().any(|c| matches!(c, Component::ParentDir))
}

/// Remove `dir` — a staging dir an install left behind, the slot an install
/// swapped out, or one it replaces without a swap — so the name is free
/// again. One the agent cannot delete, because root unpacked it (an `update
/// apply` run as root before slots were handed to the install owner, or one
/// that died midway), is renamed aside to `.<name>.stale-<n>` in the same
/// directory instead. That needs write access to that directory only, and
/// the service user owns `releases/`. [`clean_stale`] deletes such leftovers
/// once an install can. A missing `dir` is fine.
pub fn clear_release_dir(dir: &Path) -> Result<(), UpdateError> {
    // lstat: a symlink in its place is removed itself, never followed.
    let Ok(meta) = fs::symlink_metadata(dir) else {
        return Ok(());
    };
    let removed = if meta.is_dir() {
        fs::remove_dir_all(dir)
    } else {
        fs::remove_file(dir)
    };
    let Err(e) = removed else {
        return Ok(());
    };
    match set_aside(dir) {
        Ok(aside) => {
            log::warn!(
                target: LOG,
                "cannot remove {} ({e}); moved it aside to {}",
                dir.display(),
                aside.display()
            );
            Ok(())
        }
        Err(e2) => Err(UpdateError::new(
            format!(
                "cannot remove {} ({e}) or move it aside ({e2})",
                dir.display()
            ),
            INSTALL_FAILED,
        )),
    }
}

/// Rename `path` to the first free `.<name>.stale-<n>` beside it. A
/// directory renamed within its parent needs no write access to itself.
fn set_aside(path: &Path) -> std::io::Result<PathBuf> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    for n in 1..=100 {
        let aside = parent.join(format!(".{name}.stale-{n}"));
        if matches!(fs::symlink_metadata(&aside), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
        {
            fs::rename(path, &aside)?;
            return Ok(aside);
        }
    }
    Err(std::io::Error::other("too many stale copies beside it"))
}

/// A name [`set_aside`] gives: `.<name>.stale-<n>`. Never a version, so
/// [`list_releases`] and the apply-update helper ignore it.
fn is_stale_name(name: &str) -> bool {
    name.starts_with('.')
        && name
            .rsplit_once(".stale-")
            .is_some_and(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Best effort: delete what [`clear_release_dir`] moved aside in `dir`. A
/// tree the agent cannot delete stays until an install that can (an
/// `update apply` as root) comes along.
fn clean_stale(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if !is_stale_name(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let path = entry.path();
        // file_type does not follow a symlink: a link is removed itself.
        let removed = match entry.file_type() {
            Ok(t) if t.is_dir() => fs::remove_dir_all(&path),
            _ => fs::remove_file(&path),
        };
        match removed {
            Ok(()) => log::info!(target: LOG, "removed stale {}", path.display()),
            Err(e) => log::debug!(target: LOG, "cannot remove stale {} yet: {e}", path.display()),
        }
    }
}

/// Extract tarball into `dest_dir` (must not already exist). Rejects absolute
/// paths, `..`, and links that point outside the archive.
pub fn extract_tarball(tarball: &Path, dest_dir: &Path) -> Result<(), UpdateError> {
    if dest_dir.exists() {
        return Err(UpdateError::new(
            format!("release dir already exists: {}", dest_dir.display()),
            "exists",
        ));
    }
    let tarball = File::open(tarball).map_err(extract_failed)?;
    Staged::unpack(&tarball, dest_dir)?.put_in_place(dest_dir, false)
}

/// An unpack that failed with `e`. A system call that failed (a full disk,
/// say) says nothing about the archive: such a failure is retried, a bad
/// archive is not.
fn extract_failed(e: std::io::Error) -> UpdateError {
    let code = if from_the_os(&e) {
        INSTALL_FAILED
    } else {
        "bad_archive"
    };
    UpdateError::new(format!("extract failed: {e}"), code)
}

/// True when root runs this: an operator's `sudo vesyl-print update apply`,
/// in a tree the service user owns.
fn running_as_root() -> bool {
    euid() == 0
}

/// True when `e`, or an error it wraps, came from a system call: a full or
/// read-only disk, a missing permission. That is this machine's state,
/// which can change, never the archive's content.
fn from_the_os(e: &std::io::Error) -> bool {
    let mut next: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = next {
        if err
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.raw_os_error().is_some())
        {
            return true;
        }
        next = err.source();
    }
    false
}

#[cfg(test)]
thread_local! {
    /// While a test sets this, [`exchange`] fails on its thread as it does
    /// on a filesystem without RENAME_EXCHANGE (see `tests::without_exchange`).
    static EXCHANGE_UNSUPPORTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Swap the entries at `a` and `b` in one step: renameat2(2) with
/// RENAME_EXCHANGE (Linux 3.15; not every filesystem has it).
fn exchange(a: &Path, b: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    #[cfg(test)]
    if EXCHANGE_UNSUPPORTED.with(|u| u.get()) {
        return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
    }
    let a = std::ffi::CString::new(a.as_os_str().as_bytes())?;
    let b = std::ffi::CString::new(b.as_os_str().as_bytes())?;
    // SAFETY: both are valid NUL-terminated paths, resolved from the
    // current directory as rename(2) resolves them.
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            a.as_ptr(),
            libc::AT_FDCWD,
            b.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// True when `e` says the kernel or the filesystem cannot [`exchange`].
fn exchange_unsupported(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP)
    )
}

/// Flush the filesystem that holds `dir` (syncfs(2)), so that a release
/// just unpacked there is on the disk before it is put in place: after a
/// power loss, a slot must not turn out to hold files the disk never got.
fn sync_filesystem(dir: &Path) {
    use std::os::fd::AsRawFd;
    if let Ok(d) = File::open(dir) {
        // SAFETY: `d` stays open for the duration of the call.
        unsafe { libc::syncfs(d.as_raw_fd()) };
    }
}

/// A release put together beside its slot, in `<slot>.staging`, and not yet
/// in place: an install checks it there, before it touches the slot itself.
#[derive(Debug)]
struct Staged {
    /// `<slot>.staging`: the release (the archive's single top-level
    /// directory, else all the archive holds). A sibling of the slot, so
    /// the two trade places without either moving to another directory,
    /// which would need write access to the directory moved (to update its
    /// `..`), and root may own the slot it replaces.
    dir: PathBuf,
    /// The name of the archive's single top-level directory, if it has one.
    top: Option<String>,
}

impl Staged {
    /// Unpack `tarball` for the slot `dest_dir`: into `.<slot>.unpack`
    /// beside it, from where the release moves to `<slot>.staging`. Rejects
    /// absolute paths, `..`, and links that point outside the archive.
    ///
    /// The agent owns `releases/` and unpacks as tar does, by path. Root (an
    /// operator's `update apply`) unpacks where the service user may rename
    /// anything at any moment, so it unpacks through descriptors, into a
    /// directory only root can enter, and the release stays root's until it
    /// is in place (see [`Staged::unpack_as_root`]).
    fn unpack(tarball: &File, dest_dir: &Path) -> Result<Staged, UpdateError> {
        Staged::unpack_as(tarball, dest_dir, running_as_root())
    }

    /// [`Staged::unpack`], the way root unpacks when `as_root` (tests take
    /// that way unprivileged too).
    fn unpack_as(tarball: &File, dest_dir: &Path, as_root: bool) -> Result<Staged, UpdateError> {
        let parent = match dest_dir.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        let releases = crate::util::open_dir_owned(parent).map_err(io_err(INSTALL_FAILED))?;
        clean_stale(parent);
        let slot = dest_dir.file_name().unwrap_or_default();
        let mut staging = slot.to_owned();
        staging.push(STAGING_SUFFIX);
        let staging = parent.join(staging);
        // Hidden: never a version, whatever the archive holds.
        let mut unpacked = OsString::from(".");
        unpacked.push(slot);
        unpacked.push(".unpack");
        let unpacked = parent.join(unpacked);
        // Left by an install that died midway, perhaps one run as root.
        clear_release_dir(&staging)?;
        clear_release_dir(&unpacked)?;

        let open = || -> Result<tar::Archive<flate2::read::GzDecoder<File>>, UpdateError> {
            // From its start each time: a clone shares the file offset.
            let mut f = tarball.try_clone().map_err(extract_failed)?;
            f.rewind().map_err(extract_failed)?;
            Ok(tar::Archive::new(flate2::read::GzDecoder::new(f)))
        };
        // Pass 1: validate every member before writing anything.
        for entry in open()?.entries().map_err(extract_failed)? {
            check_member(&entry.map_err(extract_failed)?)?;
        }
        // Pass 2: unpack.
        let mut archive = open()?;
        archive.set_preserve_permissions(false);
        let top = if as_root {
            Staged::unpack_as_root(&mut archive, &releases, &unpacked, &staging)
        } else {
            Staged::unpack_by_path(&mut archive, &unpacked, &staging)
        };
        // Empty now, or holding what could not be moved (or unpacked).
        let _ = fs::remove_dir_all(&unpacked);
        Ok(Staged {
            dir: staging,
            top: top?.map(|t| t.to_string_lossy().into_owned()),
        })
    }

    /// The agent's pass 2: `archive` into `unpacked` (made as
    /// `create_dir_all` makes it) by path, then the release (see
    /// [`single_top_dir`]) moved to `staging`. Returns the archive's single
    /// top-level directory, if it has one.
    fn unpack_by_path(
        archive: &mut tar::Archive<impl Read>,
        unpacked: &Path,
        staging: &Path,
    ) -> Result<Option<OsString>, UpdateError> {
        fs::create_dir_all(unpacked).map_err(io_err(INSTALL_FAILED))?;
        archive.unpack(unpacked).map_err(extract_failed)?;
        let top = single_top_dir(unpacked)?;
        let tree = match &top {
            Some(top) => unpacked.join(top),
            None => unpacked.to_path_buf(),
        };
        fs::rename(&tree, staging).map_err(io_err(INSTALL_FAILED))?;
        Ok(top)
    }

    /// Root's pass 2. `archive` goes into `<unpacked>/tree`, where
    /// `unpacked`, in the open `releases`, is root's own and 0700, and
    /// through the tree's descriptor ([`unpack_through`]): a name the
    /// service user swaps in `releases/` (it owns it) takes no write
    /// elsewhere. Nothing in the tree is that user's to change before the
    /// release is in place, after which it is handed over
    /// ([`Staged::put_in_place`]): before the tree leaves `unpacked`, group
    /// and others lose write on all of it, whatever mode the archive or the
    /// umask gave ([`crate::util::clear_group_other_write`]; tar gives a
    /// directory only a member's path implies the umask's). The release
    /// then moves to `staging`, relative to the directories opened. Returns
    /// the archive's single top-level directory, if it has one.
    fn unpack_as_root(
        archive: &mut tar::Archive<impl Read>,
        releases: &File,
        unpacked: &Path,
        staging: &Path,
    ) -> Result<Option<OsString>, UpdateError> {
        const TREE: &str = "tree";
        let failed = io_err(INSTALL_FAILED);
        let name = |p: &Path| p.file_name().unwrap_or_default().to_owned();
        crate::util::create_dir_at(releases, &name(unpacked), 0o700).map_err(&failed)?;
        #[cfg(test)]
        run_hook(&AFTER_UNPACK_DIR_MADE);
        let private = crate::util::open_dir_at(releases, &name(unpacked)).map_err(&failed)?;
        // The directory just made, unless another user put one of theirs,
        // or one open to them, in its place since.
        let mine = private
            .metadata()
            .is_ok_and(|m| m.uid() == euid() && m.mode() & 0o077 == 0);
        if !mine {
            return Err(UpdateError::new(
                format!("{} is not the directory just made", unpacked.display()),
                INSTALL_FAILED,
            ));
        }
        crate::util::create_dir_at(&private, OsStr::new(TREE), 0o755).map_err(&failed)?;
        let tree = crate::util::open_dir_at(&private, OsStr::new(TREE)).map_err(&failed)?;
        unpack_through(archive, &tree)?;
        crate::util::clear_group_other_write(&tree).map_err(&failed)?;
        let top = single_top_dir(&fd_path(&tree))?;
        let (from, from_name) = match &top {
            Some(top) => (&tree, top.as_os_str()),
            None => (&private, OsStr::new(TREE)),
        };
        crate::util::rename_at(from, from_name, releases, &name(staging)).map_err(&failed)?;
        Ok(top)
    }

    /// Refuse a release whose archive names a version other than `version`
    /// in its top-level directory, as `scripts/build-release.sh` packs it
    /// (`vesyl-print-<version>/`): a manifest that points at the artifact of
    /// another release. Any other layout says nothing about the version.
    fn check_version(&self, version: &str) -> Result<(), UpdateError> {
        let packed = self
            .top
            .as_deref()
            .and_then(|t| t.strip_prefix("vesyl-print-"))
            .filter(|v| is_version(v));
        match packed {
            Some(packed) if packed != version => Err(UpdateError::new(
                format!("archive holds vesyl-print-{packed}, not version {version}"),
                VERSION_MISMATCH,
            )),
            _ => Ok(()),
        }
    }

    /// Put the release in place at `dest_dir`. A slot already there trades
    /// places with it in one step ([`exchange`]) and is removed after, so
    /// `dest_dir` always holds one whole release: a reinstall of the
    /// version `current` points at never leaves `current` dangling, nor
    /// does a crash midway. Where the filesystem cannot swap, a slot that
    /// is not `active` is cleared first; the `active` one is left as it is,
    /// and the install refused.
    ///
    /// Then an operator running `update apply` as root must not leave a
    /// root-owned slot the non-root agent can never replace or remove, so
    /// the release, `VERSION` and all, goes to the owner of `releases/`:
    /// only now, so that nothing in it was the service user's to change
    /// while root still worked on it.
    fn put_in_place(self, dest_dir: &Path, active: bool) -> Result<(), UpdateError> {
        let placed = if fs::symlink_metadata(dest_dir).is_err() {
            fs::rename(&self.dir, dest_dir).map_err(io_err(INSTALL_FAILED))
        } else {
            match exchange(&self.dir, dest_dir) {
                Ok(()) => Ok(()),
                Err(e) if active => Err(UpdateError::new(
                    format!(
                        "cannot swap the new release in for {} in one step ({e}); \
                         `current` points at it, so it was left as it is",
                        dest_dir.display()
                    ),
                    if exchange_unsupported(&e) {
                        NO_EXCHANGE
                    } else {
                        INSTALL_FAILED
                    },
                )),
                Err(e) => {
                    log::info!(
                        target: LOG,
                        "cannot swap {} in one step ({e}): replacing it",
                        dest_dir.display()
                    );
                    clear_release_dir(dest_dir).and_then(|()| {
                        fs::rename(&self.dir, dest_dir).map_err(io_err(INSTALL_FAILED))
                    })
                }
            }
        };
        // What is left here: the slot that was swapped out, or the release
        // when it could not be put in place.
        if let Err(e) = clear_release_dir(&self.dir) {
            log::warn!(target: LOG, "{}", e.message);
        }
        placed?;
        crate::util::hand_tree_to_parent_owner(dest_dir).map_err(io_err(INSTALL_FAILED))
    }

    fn discard(self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Refuse an archive member with an unsafe path, or a link to one.
fn check_member(entry: &tar::Entry<impl Read>) -> Result<(), UpdateError> {
    let path = entry.path().map_err(extract_failed)?;
    let link = entry.link_name().map_err(extract_failed)?;
    if unsafe_archive_path(&path) || link.as_deref().is_some_and(unsafe_archive_path) {
        return Err(UpdateError::new(
            format!("refusing unsafe path in archive: {}", path.display()),
            "bad_archive",
        ));
    }
    Ok(())
}

/// What the archive unpacked in `dir` holds, if that is a single directory
/// (`vesyl-print-<version>/`, as `scripts/build-release.sh` packs it): the
/// release is then that directory, else all of `dir`. Never a symlink:
/// moved to where the slot goes, its target would resolve elsewhere.
fn single_top_dir(dir: &Path) -> Result<Option<OsString>, UpdateError> {
    let children: Vec<fs::DirEntry> = fs::read_dir(dir)
        .map_err(extract_failed)?
        .flatten()
        .collect();
    Ok(match children.as_slice() {
        [only] if only.file_type().is_ok_and(|t| t.is_dir()) => Some(only.file_name()),
        _ => None,
    })
}

/// What a test runs at a step of root's unpack, on its own thread
/// ([`run_hook`]).
#[cfg(test)]
type TestHook = RefCell<Option<Box<dyn Fn()>>>;

#[cfg(test)]
thread_local! {
    /// While a test sets this, [`unpack_through`] on its thread calls it
    /// after each member it unpacks: the service user may rename anything
    /// in `releases/` while root unpacks there (see
    /// `tests::unpacking_as_root_takes_no_write_elsewhere`).
    static AFTER_EACH_MEMBER: TestHook = const { RefCell::new(None) };
    /// While a test sets this, [`Staged::unpack_as_root`] on its thread
    /// calls it right after it makes `.<slot>.unpack`, before it opens it:
    /// the service user may put a directory of its own at that name
    /// meanwhile (see `tests::unpacking_as_root_refuses_a_dir_put_in_its_place`).
    static AFTER_UNPACK_DIR_MADE: TestHook = const { RefCell::new(None) };
}

/// Call `hook`, if a test on this thread set it.
#[cfg(test)]
fn run_hook(hook: &'static std::thread::LocalKey<TestHook>) {
    hook.with(|hook| {
        if let Some(hook) = hook.borrow().as_ref() {
            hook();
        }
    });
}

/// Unpack `archive` into the open directory `dir`, member by member as
/// `tar::Archive::unpack` does it (the archive's directories last, deepest
/// first), only through [`fd_path`]: no name on the way is looked up again,
/// so one swapped meanwhile takes no write elsewhere (tar's own `unpack`
/// turns that path back into the name first). A hard link is refused, as
/// tar finds its target by name; so is a member [`check_member`] refuses,
/// in case the archive changed since it was checked.
fn unpack_through(archive: &mut tar::Archive<impl Read>, dir: &File) -> Result<(), UpdateError> {
    let at = fd_path(dir);
    let mut directories = Vec::new();
    for entry in archive.entries().map_err(extract_failed)? {
        let mut entry = entry.map_err(extract_failed)?;
        check_member(&entry)?;
        let kind = entry.header().entry_type();
        if kind.is_hard_link() {
            return Err(UpdateError::new(
                format!(
                    "refusing hard link in archive: {}",
                    String::from_utf8_lossy(&entry.path_bytes())
                ),
                "bad_archive",
            ));
        }
        if kind.is_dir() {
            directories.push(entry);
        } else {
            entry.unpack_in(&at).map_err(extract_failed)?;
        }
        #[cfg(test)]
        run_hook(&AFTER_EACH_MEMBER);
    }
    directories.sort_by(|a, b| b.path_bytes().cmp(&a.path_bytes()));
    for mut directory in directories {
        directory.unpack_in(&at).map_err(extract_failed)?;
    }
    Ok(())
}

/// A `VERSION` symlink from the archive is replaced, never written through.
/// Written by root (the CLI), it ends up the slot owner's like the rest of
/// the slot: kept by [`write_durable`] in a slot already in place, handed
/// over with it by an install, which writes it before then.
pub fn write_version_file(release_dir: &Path, version: &str) -> std::io::Result<()> {
    write_durable(
        &release_dir.join("VERSION"),
        format!("{version}\n").as_bytes(),
        0o644,
        false,
    )
}

fn absolute(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    }
}

/// `os.path.relpath(target, start)` for absolute, already-normalized paths.
fn relpath(target: &Path, start: &Path) -> PathBuf {
    let (target, start) = (absolute(target), absolute(start));
    let t: Vec<_> = target.components().collect();
    let s: Vec<_> = start.components().collect();
    let common = t.iter().zip(&s).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in common..s.len() {
        out.push("..");
    }
    for c in &t[common..] {
        out.push(c.as_os_str());
    }
    out
}

/// Point `link_path` at `target` (relative) via atomic rename.
pub fn atomic_symlink(target: &Path, link_path: &Path) -> std::io::Result<()> {
    let parent = link_path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let rel = relpath(target, parent);
    let tmp = PathBuf::from(format!("{}.new", link_path.display()));
    if tmp.is_symlink() || tmp.exists() {
        fs::remove_file(&tmp)?;
    }
    std::os::unix::fs::symlink(rel, &tmp)?;
    fs::rename(&tmp, link_path)
}

pub fn flip_current(install_root: &Path, version: &str) -> Result<PathBuf, UpdateError> {
    let release_dir = install_root.join("releases").join(version);
    if !release_dir.is_dir() {
        return Err(UpdateError::new(
            format!("release not installed: {version}"),
            "missing_release",
        ));
    }
    atomic_symlink(&release_dir, &install_root.join("current"))
        .map_err(io_err("activate_failed"))?;
    Ok(release_dir)
}

/// What runs the helper, to activate or restart: `sudo -n` (NOPASSWD on
/// appliances). Unit tests run a stand-in helper script with `sh` instead,
/// so no test ever runs sudo.
#[cfg(not(test))]
const HELPER_RUNNER: &[&str] = &["sudo", "-n"];
#[cfg(test)]
const HELPER_RUNNER: &[&str] = &["sh"];

/// A stand-in for the apply-update helper (run with `sh`, see
/// `HELPER_RUNNER`): appends its arguments to `<dir>/helper.calls`, then
/// exits 0, or refuses with exit 1 when `refuse`.
#[cfg(test)]
pub(crate) fn fake_helper(dir: &Path, refuse: bool) -> PathBuf {
    let helper = dir.join("apply-update");
    let calls = dir.join("helper.calls");
    let exit = if refuse {
        "echo 'apply-update: refused' >&2; exit 1"
    } else {
        "exit 0"
    };
    fs::write(
        &helper,
        format!("echo \"$*\" >> '{}'\n{exit}\n", calls.display()),
    )
    .unwrap();
    helper
}

/// `sudo -n <helper> activate <release_dir> <current>`.
fn helper_activate(helper: &Path, release_dir: &Path, current: &Path) -> Result<(), UpdateError> {
    let h = helper.display().to_string();
    let r = release_dir.display().to_string();
    let c = current.display().to_string();
    let (runner, runner_args) = HELPER_RUNNER.split_first().expect("runner");
    let args: Vec<&str> = runner_args
        .iter()
        .copied()
        .chain([h.as_str(), "activate", r.as_str(), c.as_str()])
        .collect();
    let out =
        crate::printers::run_with_timeout(runner, &args, Duration::from_secs(60)).map_err(|e| {
            UpdateError::new(
                format!("apply-update activate failed: {e}"),
                "activate_failed",
            )
        })?;
    if !out.success {
        return Err(UpdateError::new(
            format!("apply-update activate failed: {}", out.stderr.trim()),
            "activate_failed",
        ));
    }
    Ok(())
}

/// Point `current` at `releases/<version>`. Where the apply-update helper is
/// installed (appliances), only through it: it checks the slot again as
/// root, and when it refuses, nothing is flipped here instead. Without one
/// (lab installs run by the service user, tests), [`flip_current`] does it.
fn activate(
    install_root: &Path,
    version: &str,
    apply_helper: Option<&Path>,
) -> Result<PathBuf, UpdateError> {
    match apply_helper.filter(|h| h.is_file()) {
        Some(helper) => {
            let release_dir = install_root.join("releases").join(version);
            helper_activate(helper, &release_dir, &install_root.join("current"))?;
            Ok(release_dir)
        }
        None => flip_current(install_root, version),
    }
}

/// Activate the newest release other than the one `current` points at, or
/// `to_version`. Only a runnable slot ([`slot_is_runnable`]) is a candidate:
/// others are passed over when choosing, and an explicit `to_version` that
/// cannot run is refused, as the helper would refuse it.
pub fn rollback(
    install_root: &Path,
    to_version: Option<&str>,
    apply_helper: Option<&Path>,
) -> Result<String, UpdateError> {
    let releases = list_releases(install_root);
    if releases.is_empty() {
        return Err(UpdateError::new(
            "no releases to roll back to",
            "no_rollback",
        ));
    }
    let slot = |v: &str| install_root.join("releases").join(v);
    let cur_ver = current_release_dir(install_root).map(|c| dir_name(&c));
    let target = match to_version {
        Some(v) => {
            if !releases.iter().any(|r| r == v) {
                return Err(UpdateError::new(
                    format!("unknown release {v}"),
                    "missing_release",
                ));
            }
            if !slot_is_runnable(&slot(v)) {
                return Err(UpdateError::new(
                    format!("release {v} cannot run: no executable {SLOT_BINARY} in its slot"),
                    "not_runnable",
                ));
            }
            v.to_string()
        }
        None => {
            let (runnable, broken): (Vec<&String>, Vec<&String>) = releases
                .iter()
                .filter(|v| Some(v.as_str()) != cur_ver.as_deref())
                .partition(|v| slot_is_runnable(&slot(v)));
            let broken = broken
                .iter()
                .map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            if !broken.is_empty() {
                log::warn!(target: LOG, "passing over releases without an executable {SLOT_BINARY}: {broken}");
            }
            let Some(newest) = runnable.last() else {
                let why = if broken.is_empty() {
                    String::new()
                } else {
                    format!(": no executable {SLOT_BINARY} in {broken}")
                };
                return Err(UpdateError::new(
                    format!("no previous release for rollback{why}"),
                    "no_rollback",
                ));
            };
            newest.to_string()
        }
    };
    activate(install_root, &target, apply_helper)?;
    log::info!(target: LOG, "rolled back to {target}");
    Ok(target)
}

/// argv for restarting both services (helper preferred, else systemctl).
/// The helper runs as `sudo -n <helper> restart` (see `HELPER_RUNNER`).
pub fn restart_commands(helper: Option<&Path>) -> Vec<Vec<String>> {
    match helper.filter(|h| h.is_file()) {
        Some(h) => vec![HELPER_RUNNER
            .iter()
            .map(|a| a.to_string())
            .chain([h.display().to_string(), "restart".into()])
            .collect()],
        None => ["vesyl-print-agent", "vesyl-print-display"]
            .iter()
            .map(|u| {
                vec![
                    "systemctl".into(),
                    "restart".into(),
                    "--no-block".into(),
                    u.to_string(),
                ]
            })
            .collect(),
    }
}

#[cfg(test)]
thread_local! {
    /// While a test sets this, restarts asked for on its thread are counted
    /// here instead of run (see [`restarts_during`]).
    static RESTARTS_SEEN: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// Count the restarts `f` asks for (on this thread) instead of running them.
#[cfg(test)]
pub(crate) fn restarts_during<T>(f: impl FnOnce() -> T) -> (T, usize) {
    RESTARTS_SEEN.with(|n| n.set(Some(0)));
    let out = f();
    (out, RESTARTS_SEEN.with(|n| n.take()).unwrap_or(0))
}

#[cfg(test)]
thread_local! {
    /// The lines [`logs_during`] collects on this thread while it runs.
    static LOGGED: RefCell<Option<Vec<(log::Level, String)>>> = const { RefCell::new(None) };
}

/// The test binary's logger: it keeps this crate's lines for the thread
/// that collects them ([`logs_during`]), and drops everything else.
#[cfg(test)]
struct ThreadLogs;

#[cfg(test)]
impl log::Log for ThreadLogs {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.target().starts_with("vesyl-print") && LOGGED.with(|l| l.borrow().is_some())
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            let line = (record.level(), record.args().to_string());
            LOGGED.with(|l| {
                if let Some(lines) = l.borrow_mut().as_mut() {
                    lines.push(line);
                }
            });
        }
    }

    fn flush(&self) {}
}

/// What `f` logs (on this thread) for the journal, as `(level, line)`, down
/// to debug. The first call installs [`ThreadLogs`] for the test binary.
#[cfg(test)]
pub(crate) fn logs_during<T>(f: impl FnOnce() -> T) -> (T, Vec<(log::Level, String)>) {
    static INSTALL: std::sync::Once = std::sync::Once::new();
    INSTALL.call_once(|| {
        log::set_boxed_logger(Box::new(ThreadLogs)).expect("no other logger in the lib tests");
        log::set_max_level(log::LevelFilter::Debug);
    });
    LOGGED.with(|l| *l.borrow_mut() = Some(Vec::new()));
    let out = f();
    (
        out,
        LOGGED.with(|l| l.borrow_mut().take()).unwrap_or_default(),
    )
}

/// Restart display + agent. Best-effort and non-blocking: when the agent
/// restarts *itself*, systemd SIGTERMs this process while the helper is still
/// running, so launch it in its own process group and never wait on it.
/// Like every other command the agent runs, it starts with no signal
/// blocked (see [`crate::printers::unblocked_signals`]).
pub fn restart_services(helper: Option<&Path>) {
    use std::os::unix::process::CommandExt;
    #[cfg(test)]
    if RESTARTS_SEEN
        .with(|n| n.get().map(|seen| n.set(Some(seen + 1))))
        .is_some()
    {
        return;
    }
    for argv in restart_commands(helper) {
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let spawned = crate::printers::unblocked_signals(&mut cmd).spawn();
        match spawned {
            // Reap in the background so it doesn't linger as a zombie.
            Ok(mut child) => {
                std::thread::spawn(move || child.wait());
            }
            Err(e) => log::warn!(target: LOG, "restart {:?} failed: {e}", argv.last()),
        }
    }
}

/// After `version` was activated (an install, or a gate's rollback): restart
/// the services into it as `env.restart` says, unless `stop` is set. That
/// restart would replace the `systemctl stop` in progress and bring the
/// agent back; instead its next start runs `version`. The display keeps
/// the code it runs until it restarts, too.
fn restart_unless_stopping(env: &UpdateEnv, stop: &AtomicBool, version: &str) {
    if stop.load(Ordering::SeqCst) {
        log::info!(target: LOG, "stopping — not restarting into {version}: the next start runs it");
    } else if env.restart {
        restart_services(env.apply_helper.as_deref());
    }
}

/// Paths/facts about the running process that the OTA flow depends on.
/// Tests construct this directly instead of patching globals.
#[derive(Debug, Clone)]
pub struct UpdateEnv {
    pub install_root: PathBuf,
    pub apply_helper: Option<PathBuf>,
    /// Version of the running binary ([`package_version`]).
    pub running_version: String,
    /// True when this process was started from `install_root/current`.
    pub running_from_slot: bool,
    /// Restart services after activate / rollback.
    pub restart: bool,
}

impl UpdateEnv {
    pub fn detect(cfg: &Config) -> Self {
        let install_root = resolve_install_root(cfg);
        let running_from_slot = match (
            std::env::current_exe().and_then(fs::canonicalize),
            fs::canonicalize(install_root.join("current")),
        ) {
            (Ok(exe), Ok(cur)) => exe.starts_with(cur),
            _ => false,
        };
        let helper = Path::new(APPLY_HELPER);
        UpdateEnv {
            install_root,
            apply_helper: helper.is_file().then(|| helper.to_path_buf()),
            running_version: package_version().to_string(),
            running_from_slot,
            restart: true,
        }
    }
}

/// Full path: verify manifest → download → extract → flip current.
///
/// Does **not** run the post-update health gate or restart services. Once
/// `stop` is set it gives up ([`STOPPED`]) at the next step that comes
/// before the activation: a read of the download, the extract, putting the
/// slot in place. The downloaded artifact is removed however it ends.
///
/// `update/` is opened once ([`crate::util::open_dir_owned`]); the
/// artifact is downloaded and removed relative to it, and unpacked from the
/// file the download wrote: as root, nothing swapped in `update/` since
/// (the service user owns it) can redirect any of it.
pub fn apply_release(
    manifest: &ReleaseManifest,
    env: &UpdateEnv,
    public_key_pem: Option<&str>,
    require_signature: bool,
    stop: &AtomicBool,
) -> Result<PathBuf, UpdateError> {
    check_manifest(manifest, env, public_key_pem, require_signature)?;

    let update_dir = env.install_root.join("update");
    let dir = crate::util::open_dir_owned(&update_dir).map_err(io_err("download_failed"))?;
    let name = OsString::from(format!("vesyl-print-{}.tar.gz", manifest.version));

    log::info!(target: LOG, "downloading {}", shown_url(&manifest.artifact_url));
    let tarball = download_into(
        &dir,
        &update_dir,
        &name,
        &manifest.artifact_url,
        &manifest.artifact_sha256,
        stop,
    )?;

    let installed = install_release(manifest, env, &tarball, stop);
    let _ = crate::util::unlink_at(&dir, &name);
    installed
}

/// [`apply_release`] for an artifact already on disk (`update apply
/// --file`): the same checks, install and activation, with `tarball`
/// checked against the manifest's SHA-256 instead of downloaded. It is
/// opened once: what is checked is what is unpacked.
pub fn apply_local_release(
    manifest: &ReleaseManifest,
    env: &UpdateEnv,
    tarball: &Path,
    public_key_pem: Option<&str>,
    require_signature: bool,
) -> Result<PathBuf, UpdateError> {
    check_manifest(manifest, env, public_key_pem, require_signature)?;
    let cannot_read = |e: std::io::Error| {
        UpdateError::new(
            format!("cannot read {}: {e}", tarball.display()),
            "bad_archive",
        )
    };
    let file = File::open(tarball).map_err(cannot_read)?;
    let sha = sha256_of(&file).map_err(cannot_read)?;
    if sha != manifest.artifact_sha256 {
        return Err(UpdateError::new(
            format!(
                "sha256 mismatch: file={sha} manifest={}",
                manifest.artifact_sha256
            ),
            "bad_checksum",
        ));
    }
    // The CLI has no stop to honor: a signal ends it outright.
    install_release(manifest, env, &file, &AtomicBool::new(false))
}

/// What every install checks before it touches anything: that this agent
/// is new enough for the release (`min_agent_version`), and the signature.
fn check_manifest(
    manifest: &ReleaseManifest,
    env: &UpdateEnv,
    public_key_pem: Option<&str>,
    require_signature: bool,
) -> Result<(), UpdateError> {
    if let Some(min) = &manifest.min_agent_version {
        if version_number_cmp(&env.running_version, min).is_lt() {
            return Err(UpdateError::new(
                format!("current {} < min_agent_version {min}", env.running_version),
                "too_old",
            ));
        }
    }
    verify_manifest(manifest, public_key_pem, require_signature)
}

#[cfg(test)]
thread_local! {
    /// While a test sets this, [`install_release`] on its thread sets its
    /// `stop` once the release is unpacked: the agent is stopped while the
    /// release is checked (see `tests::stopping_once_unpacked`).
    static STOP_ONCE_UNPACKED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Install the verified `tarball` (open) as the slot for `manifest.version`,
/// then [`activate`] it. The archive is unpacked into a staging dir beside
/// the slot, where it must be the release the manifest names and hold a
/// runnable slot ([`slot_is_runnable`]); its `VERSION` is written there.
/// Only then does it replace the slot, in one step ([`Staged::put_in_place`]):
/// a bad archive, or a `stop` before that, leaves an installed slot of the
/// same version as it was, `current` included.
fn install_release(
    manifest: &ReleaseManifest,
    env: &UpdateEnv,
    tarball: &File,
    stop: &AtomicBool,
) -> Result<PathBuf, UpdateError> {
    let root = &env.install_root;
    let release_dir = root.join("releases").join(&manifest.version);
    unless_stopping(stop, "before the extract")?;
    log::info!(target: LOG, "extracting beside {}", release_dir.display());
    let staged = Staged::unpack(tarball, &release_dir)?;
    #[cfg(test)]
    if STOP_ONCE_UNPACKED.with(|s| s.get()) {
        stop.store(true, Ordering::SeqCst);
    }
    let checked = (|| {
        staged.check_version(&manifest.version)?;
        if !slot_is_runnable(&staged.dir) {
            return Err(UpdateError::new(
                "archive missing an executable vesyl-print binary",
                "bad_archive",
            ));
        }
        write_version_file(&staged.dir, &manifest.version).map_err(io_err(INSTALL_FAILED))?;
        unless_stopping(stop, "before the activation")
    })();
    if let Err(e) = checked {
        staged.discard();
        return Err(e);
    }
    // A slot of this version from an earlier install is swapped out, the
    // one `current` points at too (a reinstall, or a repair). If the agent
    // cannot delete it after (root unpacked it), it is moved aside: failing
    // the install would download the artifact again on every attempt.
    let active = current_release_dir(root)
        .is_some_and(|cur| fs::canonicalize(&release_dir).is_ok_and(|dir| dir == cur));
    sync_filesystem(&staged.dir);
    staged.put_in_place(&release_dir, active)?;

    activate(root, &manifest.version, env.apply_helper.as_deref())?;
    log::info!(target: LOG, "activated version {}", manifest.version);
    Ok(release_dir)
}

pub fn utc_now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, false)
}

pub fn utc_now_plus(seconds: i64) -> String {
    (Utc::now() + chrono::Duration::seconds(seconds)).to_rfc3339_opts(SecondsFormat::Secs, false)
}

fn parse_utc(iso: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(iso)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// When this process started, on the wall clock: now minus the process's
/// age, which the kernel keeps on the boot clock (`starttime` in
/// /proc/self/stat), so a clock step since the start does not move it. The
/// start is rounded down to a clock tick (10 ms): early, never late. `None`
/// if /proc cannot tell.
fn process_started_at() -> Option<DateTime<Utc>> {
    // Wall clock first: reading the boot clock after it only adds age.
    let now = Utc::now();
    let mut boot = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `boot` is a valid timespec for clock_gettime to fill.
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut boot) } != 0 {
        return None;
    }
    let stat = fs::read_to_string("/proc/self/stat").ok()?;
    // Field 2 (comm) is parenthesized and may hold spaces or parentheses;
    // field 22 (starttime, clock ticks after boot) is the 20th after it.
    let ticks: i128 = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()?;
    // SAFETY: sysconf has no preconditions.
    let hz = i128::from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) });
    if hz <= 0 {
        return None;
    }
    let boot_ns = i128::from(boot.tv_sec) * 1_000_000_000 + i128::from(boot.tv_nsec);
    let age_ns = (boot_ns - ticks * 1_000_000_000 / hz).max(0);
    Some(now - chrono::Duration::nanoseconds(i64::try_from(age_ns).ok()?))
}

/// Record that activate succeeded; health gate must pass before idle.
pub fn mark_pending_health(
    st: &mut UpdateStatus,
    target_version: &str,
    previous_version: Option<String>,
    gate_seconds: i64,
    channel: Option<String>,
) {
    st.status = STATUS_PENDING_HEALTH.into();
    st.current_version = target_version.into();
    st.target_version = Some(target_version.into());
    st.previous_version = previous_version;
    st.health_deadline_at = Some(utc_now_plus(gate_seconds.max(15)));
    st.health_attempts = 0;
    st.last_error = None;
    st.clear_failures();
    st.last_checked_at = Some(utc_now());
    // Compared with process start times, so to the microsecond: cut to the
    // second, a gate armed just after a process started could look older.
    st.armed_at = Some(Utc::now().to_rfc3339_opts(SecondsFormat::Micros, false));
    if channel.is_some() {
        st.channel = channel;
    }
}

/// A release slot: its directory and the version it holds.
struct Slot {
    /// Directory name under `releases/`.
    name: String,
    /// Its `VERSION` file, else its name.
    version: String,
}

impl Slot {
    fn at(dir: &Path) -> Slot {
        let name = dir_name(dir);
        let version = fs::read_to_string(dir.join("VERSION"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| name.clone());
        Slot { name, version }
    }

    /// `None` when `current` is missing or broken.
    fn current(install_root: &Path) -> Option<Slot> {
        current_release_dir(install_root).map(|d| Slot::at(&d))
    }

    fn is(&self, version: &str) -> bool {
        same_version(&self.version, version) || self.name == version
    }
}

/// Fast checks on the active release dir (no network).
pub fn local_slot_healthy(env: &UpdateEnv, expected_version: Option<&str>) -> Result<(), String> {
    let cur = current_release_dir(&env.install_root).ok_or("current symlink missing or broken")?;
    // Checked when `current` was pointed here (the helper, else
    // `slot_is_runnable`); here, that the units can still exec it.
    if !slot_is_runnable(&cur) {
        return Err("current slot has no executable vesyl-print binary".into());
    }
    if let Some(expected) = expected_version.filter(|e| !e.is_empty()) {
        let slot = Slot::at(&cur);
        if !slot.is(expected) {
            return Err(format!(
                "slot version {:?} != expected {expected:?}",
                slot.version
            ));
        }
        // After a real service restart, code is loaded from current — require match.
        if env.running_from_slot && !same_version(&env.running_version, expected) {
            return Err(format!(
                "running version {:?} != expected {expected:?}",
                env.running_version
            ));
        }
    }
    Ok(())
}

/// ISO-8601 strings in the same UTC shape compare lexicographically.
fn deadline_passed(deadline_iso: Option<&str>, now_iso: &str) -> bool {
    deadline_iso.is_some_and(|d| !d.is_empty() && now_iso >= d)
}

/// True when `st` records a gate that judged its version and failed it
/// with nothing to roll back to (`health failed: …`, see
/// [`close_failed_gate`]).
fn failed_by_its_gate(st: &UpdateStatus) -> bool {
    st.is(STATUS_FAILED)
        && st
            .last_error
            .as_deref()
            .is_some_and(|e| e.starts_with(HEALTH_FAILED))
}

/// If OTA activated successfully but status was marked failed (e.g. SIGTERM
/// during self-restart), promote back to `pending_health` so the gate runs.
///
/// Never a gate that has judged its version already and failed it with
/// nothing to roll back to (`health failed: …`): promoted, it would arm
/// again on every cycle, pausing jobs each time for the same verdict. Such
/// a failure clears once its version runs healthy (see
/// `clear_failed_gate_once_healthy`).
pub fn recover_false_update_failure(
    mut st: UpdateStatus,
    cfg: &Config,
    env: &UpdateEnv,
) -> UpdateStatus {
    let Some(target) = gate_to_reopen(&st, env) else {
        return st;
    };
    log::info!(target: LOG, "recovering sticky failed update status for {target} (slot healthy) → pending_health");
    let (prev, channel) = (st.previous_version.clone(), st.channel.clone());
    mark_pending_health(&mut st, &target, prev, health_gate_seconds(cfg), channel);
    st
}

/// The version whose health gate [`recover_false_update_failure`] reopens
/// for `st`: a `failed` status of the version this process runs from a
/// healthy slot, unless that gate failed it already. Logs nothing.
fn gate_to_reopen(st: &UpdateStatus, env: &UpdateEnv) -> Option<String> {
    if !st.is(STATUS_FAILED) || failed_by_its_gate(st) {
        return None;
    }
    let target = st.target_version.as_deref().unwrap_or_default().trim();
    (!target.is_empty()
        && same_version(&env.running_version, target)
        && local_slot_healthy(env, Some(target)).is_ok())
    .then(|| target.to_string())
}

/// [`should_pause_jobs`] for `status` as the next heartbeat will find it: a
/// `failed` one that [`recover_false_update_failure`] turns back into a
/// health gate (an install cut off after its flip) pauses jobs already. For
/// a decision taken before that heartbeat (the agent's startup drain): it
/// logs nothing, as that heartbeat logs the promotion.
pub fn should_pause_jobs_once_recovered(status: Option<&UpdateStatus>, env: &UpdateEnv) -> bool {
    should_pause_jobs(status) || status.is_some_and(|st| gate_to_reopen(st, env).is_some())
}

/// Outcome of the whoami call used by the health gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhoamiResult {
    Ok,
    /// API answered 401 — the code path works, so it counts as healthy.
    Unauthorized,
    Error,
    /// Unpaired: local checks only.
    Skipped,
}

impl WhoamiResult {
    pub fn as_str(self) -> &'static str {
        match self {
            WhoamiResult::Ok => "ok",
            WhoamiResult::Unauthorized => "unauthorized",
            WhoamiResult::Error => "error",
            WhoamiResult::Skipped => "skipped",
        }
    }
}

/// True when this process is the agent that the gated activation replaces,
/// which must leave the gate to its successor: it was started from a
/// release slot before the gate was armed, it does not run the gate's
/// version, and `current` still points at that version. Activations arm the
/// gate before the services restart (a heartbeat OTA in this very process,
/// or `update apply … --restart` from the CLI), and `systemctl restart
/// --no-block` lets this process finish its current cycle first. Its start
/// time is what sets it apart from a process started after the activation
/// (a mislabeled build in the new slot, which the gate must judge): the
/// slot the gate rolls back to need not be the one it runs from (another
/// version staged without a restart).
fn replaced_by_gated_activation(
    st: &UpdateStatus,
    env: &UpdateEnv,
    expected: &str,
    started_at: Option<DateTime<Utc>>,
) -> bool {
    env.running_from_slot
        && !same_version(&env.running_version, expected)
        && Slot::current(&env.install_root).is_some_and(|s| s.is(expected))
        && started_before_gate(st, env, started_at)
}

/// True when this process started before the gate in `st` was armed.
/// Without both times (a gate armed by a build that did not record
/// `armed_at`), the agent being replaced is recognized as it was before: it
/// runs the version the gate rolls back to.
fn started_before_gate(
    st: &UpdateStatus,
    env: &UpdateEnv,
    started_at: Option<DateTime<Utc>>,
) -> bool {
    match (st.armed_at.as_deref().and_then(parse_utc), started_at) {
        (Some(armed), Some(started)) => started < armed,
        _ => st
            .previous_version
            .as_deref()
            .is_some_and(|p| same_version(p, &env.running_version)),
    }
}

/// Post-update health gate.
///
/// Declares success only after local slot checks pass and (when paired) whoami
/// reaches the API. On hard failure or deadline expiry: auto-rollback to
/// `previous_version` when available, set `rolled_back`, and restart services.
///
/// The agent being replaced leaves the gate alone until the deadline: it can
/// never pass the running-version check, so judging the new slot would roll
/// back an activation whose restart is already on its way. A deadline that
/// passes while it still runs means the restart never came: it rolls back,
/// without holding a version that never ran (see [`failed_health_gate`]).
///
/// A gate whose `current` was switched to another version by hand (`update
/// rollback`, or `update apply` without --restart) is closed as
/// `rolled_back` at once, with nothing flipped or restarted again.
///
/// Once `stop` is set, a rollback still flips `current`, but the services
/// are not restarted: that restart would replace the `systemctl stop` in
/// progress. The next start runs the slot rolled back to.
///
/// A gate that failed with nothing to roll back to is not judged again,
/// but cleared once its version runs healthy (see
/// `clear_failed_gate_once_healthy`).
pub fn process_pending_health(
    st: UpdateStatus,
    cfg: &Config,
    env: &UpdateEnv,
    whoami: WhoamiResult,
    whoami_error: Option<&str>,
    now_iso: Option<&str>,
    stop: &AtomicBool,
) -> UpdateStatus {
    judge_pending_health(
        st,
        cfg,
        env,
        whoami,
        whoami_error,
        now_iso,
        process_started_at(),
        stop,
    )
}

/// [`process_pending_health`] for a process started at `started_at`.
#[allow(clippy::too_many_arguments)] // process_pending_health's, and the start time tests pick
fn judge_pending_health(
    st: UpdateStatus,
    cfg: &Config,
    env: &UpdateEnv,
    whoami: WhoamiResult,
    whoami_error: Option<&str>,
    now_iso: Option<&str>,
    started_at: Option<DateTime<Utc>>,
    stop: &AtomicBool,
) -> UpdateStatus {
    let report = Report::start("health gate");
    let st = if st.is(STATUS_FAILED) {
        recover_false_update_failure(st, cfg, env)
    } else {
        st
    };
    let now = now_iso.map(String::from).unwrap_or_else(utc_now);
    let mut st = clear_failed_gate_once_healthy(st, env, whoami, &now);
    if !st.is(STATUS_PENDING_HEALTH) {
        return st;
    }

    let expected = st
        .target_version
        .clone()
        .unwrap_or_else(|| st.current_version.clone());

    // `current` was switched off the gate's version by hand while the gate
    // was open: `update rollback`, the documented way out of a bad slot (or
    // another `update apply` without --restart). Either way the gate's
    // version is out, so the gate closes as rolled back, holding the version
    // like any other; it has nothing left to flip or restart. A missing
    // `current` is no such choice: the gate judges it below and rolls back
    // to repair it.
    if let Some(cur) = Slot::current(&env.install_root).filter(|s| !s.is(&expected)) {
        log::warn!(
            target: LOG,
            "pending_health for {expected}: current was switched to {} — closing the gate as rolled back",
            cur.version
        );
        st.status = STATUS_ROLLED_BACK.into();
        st.current_version = env.running_version.clone();
        st.target_version = Some(expected);
        st.previous_version = None;
        st.health_deadline_at = None;
        st.armed_at = None;
        st.last_checked_at = Some(now);
        st.last_error = Some(format!(
            "{CURRENT_CHANGED} to {} during the health gate",
            cur.version
        ));
        return st;
    }

    let replaced = replaced_by_gated_activation(&st, env, &expected, started_at);
    if replaced && !deadline_passed(st.health_deadline_at.as_deref(), &now) {
        report.log(format!(
            "pending_health for {expected}: waiting for the restart (running {})",
            env.running_version
        ));
        return st;
    }

    st.last_checked_at = Some(now.clone());
    st.health_attempts += 1;

    if replaced {
        // Still the agent being replaced at the deadline: the restart into
        // `expected` never happened (the restart helper failed, say). Roll
        // back to the slot the activation replaced: usually the one this
        // process runs from, but one staged after it started, without a
        // restart, if there was one. `expected` never ran: this says nothing
        // about it, and an agent started since may retry it. `armed_at`
        // stays to tell that one from this process, which does not (see
        // `restart_missed_here`).
        let armed_at = st.armed_at.clone();
        let why = format!("{RESTART_MISSED} (still running {})", env.running_version);
        let mut st = close_failed_gate(st, env, &expected, &why, stop);
        st.armed_at = armed_at;
        return st;
    }

    let local = local_slot_healthy(env, Some(&expected));
    let cloud_ok = whoami != WhoamiResult::Error;

    if local.is_ok() && cloud_ok {
        log::info!(
            target: LOG,
            "post-update health ok version={expected} attempts={} whoami={}",
            st.health_attempts,
            whoami.as_str()
        );
        st.status = STATUS_IDLE.into();
        st.current_version = env.running_version.clone();
        st.previous_version = None;
        st.health_deadline_at = None;
        st.armed_at = None;
        st.last_error = None;
        return st;
    }

    let mut reasons: Vec<String> = Vec::new();
    if let Err(e) = &local {
        reasons.push(e.clone());
    }
    if whoami == WhoamiResult::Error {
        reasons.push(whoami_error.unwrap_or("whoami failed").to_string());
    }
    let reason = if reasons.is_empty() {
        "health check failed".to_string()
    } else {
        reasons.join("; ")
    };
    st.last_error = Some(reason.clone());

    let past_deadline = deadline_passed(st.health_deadline_at.as_deref(), &now);
    // Local slot broken (wrong version / no vesyl-print binary) → fail fast.
    let hard_fail = local.is_err();
    if !past_deadline && !hard_fail {
        log::warn!(
            target: LOG,
            "post-update health not ready yet ({reason}); will retry until {}",
            st.health_deadline_at.as_deref().unwrap_or("?")
        );
        return st;
    }

    // Deadline or hard local failure → rollback if we can.
    close_failed_gate(
        st,
        env,
        &expected,
        &format!("{HEALTH_FAILED}: {reason}"),
        stop,
    )
}

/// Close a gate for `expected` that did not pass: roll back to
/// `previous_version` and restart the services (unless `stop` is set), else
/// mark it failed. `why` leads `last_error`, which [`failed_health_gate`]
/// reads.
fn close_failed_gate(
    mut st: UpdateStatus,
    env: &UpdateEnv,
    expected: &str,
    why: &str,
    stop: &AtomicBool,
) -> UpdateStatus {
    st.armed_at = None;
    if let Some(prev) = st
        .previous_version
        .clone()
        .filter(|p| p.as_str() != expected)
    {
        log::error!(target: LOG, "{why} — rolling back to {prev}");
        return match rollback(&env.install_root, Some(&prev), env.apply_helper.as_deref()) {
            Ok(rolled) => {
                st.status = STATUS_ROLLED_BACK.into();
                st.current_version = rolled.clone();
                st.target_version = Some(expected.into());
                st.previous_version = None;
                st.health_deadline_at = None;
                st.last_error = Some(format!("{why}; rolled back to {rolled}"));
                restart_unless_stopping(env, stop, &rolled);
                st
            }
            Err(e) => {
                log::error!(target: LOG, "auto-rollback failed: {}", e.message);
                st.status = STATUS_FAILED.into();
                st.last_error = Some(format!("{why}; rollback error: {}", e.message));
                st
            }
        };
    }

    st.status = STATUS_FAILED.into();
    st.health_deadline_at = None;
    st.last_error = Some(format!("{why} (no previous slot to roll back to)"));
    log::error!(target: LOG, "{}", st.last_error.as_deref().unwrap_or_default());
    st
}

/// A gate that failed its version with nothing to roll back to (`health
/// failed: …`) is never armed again ([`recover_false_update_failure`]), but
/// once that version runs healthy here after all (its slot checks out and
/// whoami does not fail: the network is back, say), its failure is over:
/// `idle` at once, as a gate that passes leaves it, never through
/// `pending_health`, so jobs do not pause for it. While whoami fails it
/// stays failed. Any other status is returned as it is.
fn clear_failed_gate_once_healthy(
    mut st: UpdateStatus,
    env: &UpdateEnv,
    whoami: WhoamiResult,
    now: &str,
) -> UpdateStatus {
    let target = st
        .target_version
        .clone()
        .unwrap_or_default()
        .trim()
        .to_string();
    if !failed_by_its_gate(&st)
        || whoami == WhoamiResult::Error
        || target.is_empty()
        || !same_version(&env.running_version, &target)
        || local_slot_healthy(env, Some(&target)).is_err()
    {
        return st;
    }
    log::info!(
        target: LOG,
        "{target} runs healthy after its failed health gate ({}) — clearing the failure",
        st.last_error.as_deref().unwrap_or_default()
    );
    st.status = STATUS_IDLE.into();
    st.current_version = env.running_version.clone();
    st.previous_version = None;
    st.health_deadline_at = None;
    st.armed_at = None;
    st.last_error = None;
    st.last_checked_at = Some(now.into());
    st
}

/// Channel recorded in `update_status.json` (local-only; wms-api sends none).
fn status_channel(cfg: &Config) -> String {
    if cfg.update_channel.is_empty() {
        "stable".to_string()
    } else {
        cfg.update_channel.clone()
    }
}

/// The slot to roll back to if the activation about to happen fails its
/// health gate: the version `current` points at now, else the running version
/// (a lab install whose slot is not version-named). Call before activating.
pub fn slot_before_activation(env: &UpdateEnv) -> Option<String> {
    current_release_version(&env.install_root)
        .or_else(|| Some(env.running_version.clone()).filter(|v| !v.is_empty()))
}

/// Arm the post-update health gate for `version` after an activation outside
/// the heartbeat path (`update apply --file/--manifest-url --restart`): the
/// restarted agent must reach the API, or it rolls back to `previous`.
/// Writes `status_path` (call before restarting) and returns the status.
pub fn arm_health_gate(
    cfg: &Config,
    status_path: &Path,
    version: &str,
    previous: Option<String>,
) -> std::io::Result<UpdateStatus> {
    let mut st = read_update_status(status_path).unwrap_or_default();
    let previous = previous.filter(|p| !same_version(p, version));
    mark_pending_health(
        &mut st,
        version,
        previous,
        health_gate_seconds(cfg),
        Some(status_channel(cfg)),
    );
    write_update_status(status_path, &st)?;
    Ok(st)
}

/// Record that `update rollback` switched `current` from `left` to `to`:
/// `rolled_back`, with `left` as the target, so the agent does not install
/// `left` again while the server still asks for it (see
/// [`failed_health_gate`]), only once it asks for another version or an
/// operator applies one. A health gate still open is closed with it.
/// Writes `status_path` and returns the status.
pub fn record_manual_rollback(
    status_path: &Path,
    left: &str,
    to: &str,
) -> std::io::Result<UpdateStatus> {
    let mut st = read_update_status(status_path).unwrap_or_default();
    st.status = STATUS_ROLLED_BACK.into();
    st.current_version = to.into();
    st.target_version = Some(left.into());
    st.previous_version = None;
    st.health_deadline_at = None;
    st.health_attempts = 0;
    st.armed_at = None;
    st.last_error = Some(format!("{MANUAL_ROLLBACK} from {left} to {to}"));
    st.clear_failures();
    st.last_checked_at = Some(utc_now());
    write_update_status(status_path, &st)?;
    Ok(st)
}

/// True for the record [`record_manual_rollback`] writes.
fn manual_rollback(st: &UpdateStatus) -> bool {
    st.is(STATUS_ROLLED_BACK)
        && st
            .last_error
            .as_deref()
            .is_some_and(|e| e.starts_with(MANUAL_ROLLBACK))
}

/// True when `st` records a rollback that restarted the services, or asked
/// for their restart: one a health gate made by rolling back to its
/// previous slot. A manual `update rollback` (it restarts only with
/// `--restart`, and then itself) and a gate closed because `current` was
/// switched by hand restart nothing, though they also move `current` and
/// leave `rolled_back`: an agent that sees `current` move under it must not
/// wait for a restart after them.
pub fn rollback_restarted_services(st: &UpdateStatus) -> bool {
    st.is(STATUS_ROLLED_BACK)
        && !manual_rollback(st)
        && !st
            .last_error
            .as_deref()
            .is_some_and(|e| e.starts_with(CURRENT_CHANGED))
}

/// The version `st` holds back on a node running `running`: its target when
/// the agent would not install that again for the same desired version (it
/// failed its health gate here, a rollback left it, or it cannot be
/// installed here: see [`failed_health_gate`], [`failed_for_good`]), and is
/// not the version running. An `update apply` that installs nothing must
/// not write over such a record: the agent would install the version again.
pub fn held_version<'a>(st: &'a UpdateStatus, running: &str) -> Option<&'a str> {
    st.target_version
        .as_deref()
        .filter(|t| !same_version(t, running))
        .filter(|_| failed_health_gate(st) || failed_for_good(st))
}

/// True when this status records that its target failed the health gate on
/// this node: rolled back (by the gate, or by hand, while the gate was open
/// or after), or failed with no way to roll back. A rollback because the
/// restart into the target never happened is not one: that version never
/// ran.
fn failed_health_gate(st: &UpdateStatus) -> bool {
    let err = st.last_error.as_deref().unwrap_or_default();
    (st.is(STATUS_ROLLED_BACK) && !err.starts_with(RESTART_MISSED)) || failed_by_its_gate(st)
}

/// True when this status records an install of its target that failed in a
/// way another try would only repeat ([`fails_for_good`]): a release this
/// node cannot install.
fn failed_for_good(st: &UpdateStatus) -> bool {
    st.is(STATUS_FAILED) && st.last_error_code.as_deref().is_some_and(fails_for_good)
}

/// True while the backoff after a failed install of the status's target
/// lasts (`retry_at`). A `retry_at` further off than the longest backoff
/// came from a clock that was ahead: it has passed.
fn retry_pending(st: &UpdateStatus, now: DateTime<Utc>) -> bool {
    st.is(STATUS_FAILED)
        && st
            .retry_at
            .as_deref()
            .and_then(parse_utc)
            .is_some_and(|at| at > now && at - now <= chrono::Duration::seconds(RETRY_MAX_SECONDS))
}

/// True when `st` records a restart into its target that never came while
/// this process (started at `started_at()`) ran: it started before that
/// gate was armed. Its restart evidently fails, so a retry from it would
/// only download again and pause jobs for a whole gate before rolling back
/// once more. A process started since retries; so does any process when
/// either time is unknown.
fn restart_missed_here(
    st: &UpdateStatus,
    started_at: impl FnOnce() -> Option<DateTime<Utc>>,
) -> bool {
    st.last_error
        .as_deref()
        .is_some_and(|e| e.starts_with(RESTART_MISSED))
        && match (st.armed_at.as_deref().and_then(parse_utc), started_at()) {
            (Some(armed), Some(started)) => started < armed,
            _ => false,
        }
}

thread_local! {
    /// What this thread's heartbeats last logged about an update they left
    /// alone, by where it was logged (see [`Report`]). Per thread: the
    /// agent's heartbeats all run on its main loop, and tests each run on a
    /// thread of their own.
    static REPORTED: RefCell<BTreeMap<&'static str, String>> =
        const { RefCell::new(BTreeMap::new()) };
}

/// What a heartbeat logs about an update it leaves alone: deferred, held,
/// backing off, waiting for a restart, or available with auto-update off.
/// The same line comes every heartbeat for as long as nothing changes, so
/// it goes to the journal at info only when it is news (the first one this
/// process logs there, or one other than what the heartbeat before
/// logged), at debug while it repeats.
struct Report {
    at: &'static str,
    /// What the heartbeat before logged at `at`, if it logged anything.
    before: Option<String>,
}

impl Report {
    /// A heartbeat's step at `at` begins. What the one before logged there
    /// is forgotten now, not when this one logs: after a heartbeat that
    /// logs nothing there, the next line is news again.
    fn start(at: &'static str) -> Report {
        let before = REPORTED.with(|reported| reported.borrow_mut().remove(at));
        Report { at, before }
    }

    fn log(&self, line: String) {
        let level = if self.before.as_deref() == Some(line.as_str()) {
            log::Level::Debug
        } else {
            log::Level::Info
        };
        log::log!(target: LOG, level, "{line}");
        REPORTED.with(|reported| reported.borrow_mut().insert(self.at, line));
    }
}

/// The status to start a heartbeat update from: `status` as the caller read
/// it earlier in its cycle, unless `status_path` now holds a `pending_health`
/// or a manual rollback it does not have. That is a gate armed meanwhile by
/// another process (`update apply … --restart`), or an `update rollback`
/// away from a version; the caller writes the result back, so its stale
/// copy would disarm the gate the restarted agent needs, or install the
/// version rolled back from again.
fn current_status(status: Option<UpdateStatus>, status_path: Option<&Path>) -> UpdateStatus {
    let Some(written) = status_path
        .and_then(read_update_status)
        .filter(|on_disk| on_disk.is(STATUS_PENDING_HEALTH) || manual_rollback(on_disk))
    else {
        return status.unwrap_or_default();
    };
    if status.as_ref() != Some(&written) {
        log::info!(
            target: LOG,
            "update status changed on disk: {} for {} (was {})",
            written.status,
            written.target_version.as_deref().unwrap_or("?"),
            status.as_ref().map_or("none", |s| s.status.as_str())
        );
    }
    written
}

/// Refuse a manifest for a version other than `desired`, the one asked for:
/// installed, it would leave the node off `desired`, to fetch it again on
/// every heartbeat (and `update apply --version` would install what was not
/// asked for, whatever its source). Only a leading `v` may differ.
pub fn check_manifest_version(
    manifest: &ReleaseManifest,
    desired: &str,
) -> Result<(), UpdateError> {
    if normalize_version(&manifest.version) == normalize_version(desired) {
        return Ok(());
    }
    Err(UpdateError::new(
        format!(
            "manifest is for version {}, not {desired}",
            manifest.version
        ),
        VERSION_MISMATCH,
    ))
}

/// Inspect a heartbeat response and optionally apply an update.
///
/// After a successful activate, status becomes `pending_health` (not idle);
/// the new process must call [`process_pending_health`] after restart. When
/// `jobs_busy`, download/install is deferred so slots never flip mid-print.
/// A version that already failed here for good (its health gate, a manual
/// rollback away from it, a release this node cannot install) is not
/// re-applied for the same desired version, nor is one the services never
/// restarted into by the process that stayed (see `restart_missed_here`).
/// After any other failure the same version is tried again only once its
/// backoff has passed (`retry_at`). With `status_path`, a gate armed (or a
/// rollback recorded) there since `status` was read wins over `status` (see
/// `current_status`), and so does one written there while this update ran
/// (see [`written_meanwhile`]): the caller writes the result back.
///
/// Once `stop` is set, an update not yet activated gives up and is tried
/// again on the next start; one activated already stays `pending_health`
/// without the restart, and the next start runs the new slot and its gate.
///
/// Why an update is left alone is logged at info only when that is news,
/// at debug while it repeats from one heartbeat to the next (see
/// [`Report`]).
pub fn maybe_update_from_heartbeat(
    hb: &JsonObject,
    cfg: &Config,
    env: &UpdateEnv,
    status: Option<UpdateStatus>,
    status_path: Option<&Path>,
    jobs_busy: bool,
    stop: &AtomicBool,
) -> UpdateStatus {
    let Some(path) = status_path else {
        return update_from_heartbeat(
            hb,
            cfg,
            env,
            status,
            None,
            jobs_busy,
            stop,
            &RefCell::default(),
        );
    };
    let before = read_update_status(path);
    let written = RefCell::default();
    let st = update_from_heartbeat(hb, cfg, env, status, Some(path), jobs_busy, stop, &written);
    written_meanwhile(path, written.into_inner().or(before)).unwrap_or(st)
}

/// The record at `path` if another process wrote a `pending_health` or a
/// manual rollback there since it held `ours` (what this heartbeat last
/// wrote there, else what it found): `update apply … --restart` arming its
/// gate while this agent downloaded, then stopping it with the restart, or
/// an `update rollback`. Written over by the stale status this heartbeat
/// returns, the restarted agent would run the new slot with no gate (and
/// nothing to roll a bad one back), or install the version rolled back from
/// again.
fn written_meanwhile(path: &Path, ours: Option<UpdateStatus>) -> Option<UpdateStatus> {
    let now = read_update_status(path)
        .filter(|on_disk| on_disk.is(STATUS_PENDING_HEALTH) || manual_rollback(on_disk))?;
    if Some(&now) == ours.as_ref() {
        return None;
    }
    log::warn!(
        target: LOG,
        "update status written meanwhile by another process: {} for {} — keeping it",
        now.status,
        now.target_version.as_deref().unwrap_or("?")
    );
    Some(now)
}

/// [`maybe_update_from_heartbeat`], noting in `written` the record it last
/// wrote to `status_path` (as read back).
#[allow(clippy::too_many_arguments)] // maybe_update_from_heartbeat's, and what it wrote
fn update_from_heartbeat(
    hb: &JsonObject,
    cfg: &Config,
    env: &UpdateEnv,
    status: Option<UpdateStatus>,
    status_path: Option<&Path>,
    jobs_busy: bool,
    stop: &AtomicBool,
    written: &RefCell<Option<UpdateStatus>>,
) -> UpdateStatus {
    let report = Report::start("heartbeat");
    let mut st = current_status(status, status_path);
    st.current_version = env.running_version.clone();
    st.last_checked_at = Some(utc_now());

    // Never start another OTA while health gate is open.
    if st.is(STATUS_PENDING_HEALTH) {
        report.log(format!(
            "update deferred: pending_health for {}",
            st.target_version.as_deref().unwrap_or("?")
        ));
        return st;
    }

    // As release tags write it, `v1.2.3` is 1.2.3: compared as given, it
    // would never equal the version installed, and be installed again.
    let desired = hb
        .get("desired_agent_version")
        .filter(|v| truthy(v))
        .or_else(|| hb.get("desired_version").filter(|v| truthy(v)))
        .map(|v| normalize_version(&py_str(v)).to_string())
        .filter(|v| !v.is_empty());
    // Channel is local-only (not sent by wms-api); kept for status display.
    st.channel = Some(status_channel(cfg));
    let sticky = |st: &UpdateStatus| st.is(STATUS_FAILED) || st.is(STATUS_ROLLED_BACK);

    let Some(desired) = desired else {
        if !sticky(&st) {
            st.status = STATUS_IDLE.into();
        }
        st.target_version = None;
        return st;
    };

    // The version this status is about, before it becomes `desired`.
    let prev_target = st.target_version.replace(desired.clone());
    if same_version(&desired, &st.current_version) {
        // A rollback is over once the server asks for the version running
        // and `current` is that version too: nothing is held any more.
        // Kept, it showed "Rolled back" on the LCD (and reported it) while
        // the server's own version ran, for good: say after `update
        // rollback` from 0.9.0 to 0.8.0 and a reinstall of 0.9.0 by
        // `update apply --file` without --restart, while the server asks
        // for 0.9.0. The version running alone is not enough: the process
        // a rollback leaves running until its restart (`update rollback`
        // without --restart, a gate's restart that is late or never comes)
        // runs the version rolled back from, and cleared by it the hold
        // would be gone when the slot rolled back to starts, which would
        // install that version again. A failure stays: its gate clears it
        // once the version runs healthy (`clear_failed_gate_once_healthy`).
        let current_is_desired =
            current_release_version(&env.install_root).is_some_and(|c| same_version(&c, &desired));
        if st.is(STATUS_ROLLED_BACK) && current_is_desired {
            log::info!(
                target: LOG,
                "{desired} runs and is the desired version: the rollback ({}) is over",
                st.last_error.as_deref().unwrap_or("?")
            );
            st.last_error = None;
            st.status = STATUS_IDLE.into();
        } else if !sticky(&st) {
            st.status = STATUS_IDLE.into();
        }
        return st;
    }

    // Re-applying a version that failed its health gate here would loop
    // download → activate → restart → gate → rollback for as long as the
    // server asks for it, and one that cannot be installed (a Python-era
    // release without the binary, a bad signature) would be downloaded
    // again on every heartbeat. Hold until the desired version changes; a
    // manual `vesyl-print update apply` (which starts from a fresh status)
    // still works.
    let held = failed_health_gate(&st) || failed_for_good(&st);
    let missed_here = restart_missed_here(&st, process_started_at);
    let backing_off = retry_pending(&st, Utc::now());
    let same_target = prev_target
        .as_deref()
        .is_some_and(|t| same_version(t, &desired));
    if held && same_target {
        report.log(format!(
            "not re-applying {desired} ({} on this node: {}); \
             waiting for a different desired version or a manual update",
            st.status,
            st.last_error.as_deref().unwrap_or("?")
        ));
        return st;
    }
    if missed_here && same_target {
        report.log(format!(
            "not re-applying {desired} from this process: the restart into it never came; \
             the agent tries again once restarted"
        ));
        return st;
    }
    // A failure that may pass (the network, the disk) is retried, but not
    // on every heartbeat: each try may download the whole artifact again.
    if backing_off && same_target {
        report.log(format!(
            "not retrying {desired} before {} ({} failed attempt(s), last: {})",
            st.retry_at.as_deref().unwrap_or("?"),
            st.attempts,
            st.last_error.as_deref().unwrap_or("?")
        ));
        return st;
    }
    // A heartbeat that defers `desired` keeps a status that blocks a retry
    // about the version it blocks. Recording `desired` in it would block
    // `desired`, a version this node has not tried, once updates resume.
    let defer = |mut st: UpdateStatus| {
        if held || missed_here || backing_off {
            st.target_version = prev_target.clone();
        }
        st
    };
    // Failed installs of `desired` in a row so far: the backoff grows with
    // them. Another version starts again from none.
    let attempts = if same_target && st.is(STATUS_FAILED) {
        st.attempts
    } else {
        0
    };

    if !cfg.auto_update_enabled {
        if !sticky(&st) {
            st.status = STATUS_IDLE.into();
        }
        report.log(format!(
            "update available: {} → {desired} (auto_update disabled)",
            st.current_version
        ));
        return defer(st);
    }

    // Do not begin download/install while a job is mid-pipeline.
    if jobs_busy {
        if st.is(STATUS_DOWNLOADING) || st.is(STATUS_INSTALLING) {
            // Only this function writes these, and it always moves on before
            // returning, so here they are left over from a process killed
            // mid-update. Keeping them would keep jobs paused, and the held
            // jobs keep jobs_busy true: nothing would print again. `failed`
            // releases the pause; the next idle heartbeat downloads afresh.
            st.last_error = Some(format!(
                "update interrupted while {} (agent restarted mid-update)",
                st.status
            ));
            st.status = STATUS_FAILED.into();
        } else if !sticky(&st) {
            st.status = STATUS_IDLE.into();
        }
        report.log(format!(
            "update deferred: jobs in flight ({} → {desired})",
            st.current_version
        ));
        return defer(st);
    }

    let update_url = hb
        .get("update_url")
        .filter(|v| truthy(v))
        .or_else(|| hb.get("manifest_url").filter(|v| truthy(v)))
        .map(py_str);
    let manifest_url = match update_url {
        Some(u) => u,
        None if !cfg.releases_base_url.is_empty() => {
            default_manifest_url(&cfg.releases_base_url, &desired)
        }
        None => {
            // A setting to fix, checked again on each heartbeat (nothing is
            // fetched): no failed attempt, neither held nor waited on.
            st.status = STATUS_FAILED.into();
            st.last_error =
                Some("desired version set but no update_url or releases_base_url".into());
            st.clear_failures();
            log::warn!(target: LOG, "{}", st.last_error.as_deref().unwrap_or_default());
            return st;
        }
    };

    let root = &env.install_root;
    let previous = slot_before_activation(env);

    let persist = |st: &UpdateStatus| {
        if let Some(p) = status_path {
            match write_update_status(p, st) {
                Ok(()) => *written.borrow_mut() = read_update_status(p),
                Err(e) => log::warn!(target: LOG, "write update status: {e}"),
            }
        }
    };

    let result = (|| -> Result<(), UpdateError> {
        st.status = STATUS_DOWNLOADING.into();
        // A new attempt: what the last one left is no longer the news.
        st.last_error = None;
        st.clear_failures();
        st.attempts = attempts;
        // The slot to roll back to, on disk before anything can flip
        // `current`: an install cut off after the flip (power loss) comes
        // back as a gate (see `recover_false_update_failure`), and that
        // gate must still be able to roll back.
        st.previous_version = previous.clone().filter(|p| !same_version(p, &desired));
        // Persist early so the LCD can show "Updating…" during the download.
        persist(&st);
        // With signatures required, an unreadable key fails here (closed).
        let pem = manifest_public_key(cfg)?;
        log::info!(target: LOG, "applying update {desired} from {}", shown_url(&manifest_url));
        let manifest = fetch_manifest(&manifest_url)?;
        check_manifest_version(&manifest, &desired)?;
        let prev = previous
            .clone()
            .filter(|p| !same_version(p, &manifest.version));
        st.status = STATUS_INSTALLING.into();
        st.previous_version = prev.clone();
        // Persist installing so a crash mid-apply is visible.
        persist(&st);

        apply_release(
            &manifest,
            env,
            pem.as_deref(),
            cfg.update_require_signature,
            stop,
        )?;
        let channel = st.channel.clone();
        mark_pending_health(
            &mut st,
            &manifest.version,
            prev,
            health_gate_seconds(cfg),
            channel,
        );
        persist(&st);
        log::info!(
            target: LOG,
            "activated {} — pending_health until whoami (deadline {})",
            manifest.version,
            st.health_deadline_at.as_deref().unwrap_or("?")
        );
        // Activate already succeeded. Restart may SIGTERM this process; never
        // overwrite pending_health with failed because of that.
        restart_unless_stopping(env, stop, &manifest.version);
        Ok(())
    })();

    if let Err(e) = result {
        // If activate already flipped current, keep pending_health (not failed).
        if st.is(STATUS_PENDING_HEALTH) {
            log::warn!(target: LOG, "update error after activate (keeping pending_health): {}", e.message);
            return st;
        }
        // Activate may have flipped the slot before the error surfaced.
        let target = st.target_version.clone();
        if let (Some(t), Some(cur)) = (target, current_release_version(root)) {
            if same_version(&cur, &t) && e.code == "activate_failed" {
                log::warn!(target: LOG, "error after activate of {t} (keeping pending_health): {}", e.message);
                // Roll back to the slot we left, not a stale previous_version.
                let prev = previous.clone().filter(|p| !same_version(p, &t));
                let channel = st.channel.clone();
                mark_pending_health(&mut st, &t, prev, health_gate_seconds(cfg), channel);
                persist(&st);
                // As after a clean activation: the gate is for the new
                // version, which runs only once the services restart. This
                // process would otherwise wait out the gate, then roll back.
                restart_unless_stopping(env, stop, &t);
                return st;
            }
        }
        if e.code == STOPPED {
            // Nothing was activated: neither a failure nor a hold, and no
            // gate to roll back to. The next start tries again, afresh.
            log::info!(target: LOG, "update to {desired} stopped before activation: tried again on the next start");
            st.status = STATUS_IDLE.into();
            st.previous_version = None;
            st.last_error = Some(e.message);
            st.clear_failures();
            return st;
        }
        st.status = STATUS_FAILED.into();
        st.last_error = Some(e.message.clone());
        st.last_error_code = Some(e.code.into());
        st.attempts = attempts + 1;
        if fails_for_good(e.code) {
            log::error!(target: LOG, "update failed: {} — not retried until the desired version changes", e.message);
        } else {
            let wait = retry_delay_seconds(st.attempts);
            st.retry_at = Some(utc_now_plus(wait));
            log::error!(target: LOG, "update failed: {} — retrying in {wait}s", e.message);
        }
    }
    st
}

/// The CLI often runs as root (`update apply … --restart` arms the gate here)
/// while the agent runs as the service user that owns the state dir:
/// [`write_durable`] leaves the file that user's, not root's.
pub fn write_update_status(path: &Path, status: &UpdateStatus) -> std::io::Result<()> {
    let mut raw = serde_json::to_string_pretty(&status.to_dict()).map_err(std::io::Error::other)?;
    raw.push('\n');
    write_durable(path, raw.as_bytes(), 0o644, false)
}

pub fn read_update_status(path: &Path) -> Option<UpdateStatus> {
    if !path.is_file() {
        return None;
    }
    let Value::Object(data) =
        serde_json::from_str::<Value>(&fs::read_to_string(path).ok()?).ok()?
    else {
        return None;
    };
    let s = |k: &str| opt_str(data.get(k));
    Some(UpdateStatus {
        status: data
            .get("status")
            .filter(|v| truthy(v))
            .map(py_str)
            .unwrap_or_else(|| STATUS_IDLE.into()),
        current_version: data
            .get("current_version")
            .filter(|v| truthy(v))
            .map(py_str)
            .unwrap_or_else(|| package_version().into()),
        target_version: s("target_version"),
        last_error: s("last_error"),
        last_checked_at: s("last_checked_at"),
        channel: s("channel"),
        previous_version: s("previous_version"),
        health_deadline_at: s("health_deadline_at"),
        health_attempts: data
            .get("health_attempts")
            .filter(|v| truthy(v))
            .and_then(py_int)
            .unwrap_or(0),
        armed_at: s("armed_at"),
        last_error_code: s("last_error_code"),
        attempts: data
            .get("attempts")
            .filter(|v| truthy(v))
            .and_then(py_int)
            .unwrap_or(0),
        retry_at: s("retry_at"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::pkcs8::EncodePublicKey;
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::json;

    fn obj(v: Value) -> JsonObject {
        v.as_object().unwrap().clone()
    }

    /// The stop of an agent that is not stopping.
    static NO_STOP: AtomicBool = AtomicBool::new(false);

    /// Tiny fake release dir tarred as `vesyl-print-<ver>/…`.
    fn build_release(root: &Path, version: &str) -> PathBuf {
        let src = root.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("vesyl-print"), b"\x7fELF fake agent").unwrap();
        crate::util::set_mode(&src.join("vesyl-print"), 0o755).unwrap();
        // The LCD display is still Python and ships in the same slot.
        fs::write(src.join("main.py"), "# fake display\n").unwrap();
        fs::write(src.join("VERSION"), format!("{version}\n")).unwrap();
        let tarball = root.join(format!("vesyl-print-{version}.tar.gz"));
        let gz = flate2::write::GzEncoder::new(
            File::create(&tarball).unwrap(),
            flate2::Compression::default(),
        );
        let mut tar = tar::Builder::new(gz);
        tar.append_dir_all(format!("vesyl-print-{version}"), &src)
            .unwrap();
        tar.into_inner().unwrap().finish().unwrap();
        tarball
    }

    /// A release tarball in `dir` holding only `files` (path, executable)
    /// under `vesyl-print-<version>/`.
    fn tarball_with(dir: &Path, version: &str, files: &[(&str, bool)]) -> PathBuf {
        let tarball = dir.join(format!("vesyl-print-{version}-custom.tar.gz"));
        evil_tarball(&tarball, |tar| {
            for (path, exec) in files {
                let mut h = tar::Header::new_gnu();
                h.set_size(4);
                h.set_mode(if *exec { 0o755 } else { 0o644 });
                tar.append_data(
                    &mut h,
                    format!("vesyl-print-{version}/{path}"),
                    &b"\x7fELF"[..],
                )
                .unwrap();
            }
        });
        tarball
    }

    /// Unsigned manifest for the local `tarball`.
    fn manifest_for(tarball: &Path, version: &str) -> ReleaseManifest {
        ReleaseManifest::from_dict(&obj(json!({
            "version": version,
            "artifact_url": url::Url::from_file_path(tarball).unwrap().to_string(),
            "artifact_sha256": sha256_file(tarball).unwrap(),
        })))
        .unwrap()
    }

    fn cfg(td: &Path) -> Config {
        Config {
            api_base_url: "https://example.test".into(),
            state_dir: td.join("state"),
            config_dir: td.join("cfg"),
            ..Config::default()
        }
    }

    fn env(root: &Path) -> UpdateEnv {
        UpdateEnv {
            install_root: root.to_path_buf(),
            apply_helper: None,
            running_version: "0.4.0".into(),
            running_from_slot: false,
            restart: false,
        }
    }

    fn two_slots(td: &Path) -> PathBuf {
        let root = td.join("opt");
        for ver in ["0.3.0", "0.4.0"] {
            let tarball = build_release(&td.join(ver), ver);
            let dir = root.join("releases").join(ver);
            extract_tarball(&tarball, &dir).unwrap();
            write_version_file(&dir, ver).unwrap();
        }
        flip_current(&root, "0.4.0").unwrap();
        root
    }

    fn current_name(root: &Path) -> String {
        dir_name(&fs::canonicalize(root.join("current")).unwrap())
    }

    fn pending(deadline: String) -> UpdateStatus {
        UpdateStatus {
            status: STATUS_PENDING_HEALTH.into(),
            current_version: "0.4.0".into(),
            target_version: Some("0.4.0".into()),
            previous_version: Some("0.3.0".into()),
            health_deadline_at: Some(deadline),
            ..Default::default()
        }
    }

    #[test]
    fn versions() {
        use std::cmp::Ordering::*;
        assert_eq!(version_cmp("0.3.0", "0.3.0"), Equal);
        assert_eq!(version_cmp("0.3.0", "0.4.0"), Less);
        assert_eq!(version_cmp("1.0.0", "0.9.9"), Greater);
        assert_eq!(version_cmp("0.3", "0.3.0"), Equal);
        // A suffix makes another release, ordered as semver orders it (it
        // was ignored: a device on 0.9.1-rc.1 stayed there when the server
        // asked for 0.9.1, which it took for the version running).
        assert_eq!(version_cmp("0.3.17-rc.1", "0.3.17"), Less);
        assert_eq!(version_cmp("0.3.17", "0.3.17-rc.1"), Greater);
        assert_eq!(version_cmp("0.3.17-rc.1", "0.3.16"), Greater);
        assert_eq!(version_cmp("0.3.17-rc.1", "0.3.17-rc.1"), Equal);
        assert_eq!(version_cmp("0.3.17-rc.1", "0.3.17-rc.2"), Less);
        assert_eq!(version_cmp("0.3.17-rc.2", "0.3.17-rc.10"), Less);
        assert_eq!(version_cmp("0.3.17-alpha", "0.3.17-alpha.1"), Less);
        assert_eq!(version_cmp("0.3.17-1", "0.3.17-alpha"), Less);
        assert_eq!(version_cmp("0.3.17-beta", "0.3.17-alpha.9"), Greater);
        assert_eq!(version_cmp("0.5.0.lab", "0.5.0"), Greater);
        assert_eq!(version_cmp("0.5.0+b7", "0.5.0"), Greater);
        assert_eq!(version_cmp("0.5.0.lab", "0.5.1"), Less);
        for (a, b) in [
            ("0.9.1-rc.1", "0.9.1"),
            ("0.5.0.lab", "0.5.0"),
            ("0.5.0+b7", "0.5.0"),
        ] {
            assert!(!same_version(a, b), "{a} {b}");
        }
        assert!(same_version("0.3", "0.3.0"));
        assert_eq!(version_suffix("0.9.1-rc.1"), "-rc.1");
        assert_eq!(version_suffix("0.5.0.lab"), ".lab");
        assert_eq!(version_suffix("0.5.0+b7"), "+b7");
        assert_eq!(version_suffix("0.5.0"), "");
    }

    /// Releases sort with a pre-release below its release: a rollback with
    /// no version picks the newest of the rest.
    #[test]
    fn releases_sort_pre_releases_before_their_release() {
        let td = tempfile::tempdir().unwrap();
        for v in ["0.9.1", "0.9.1-rc.1", "0.9.0", "0.9.1-rc.2"] {
            fs::create_dir_all(td.path().join("releases").join(v)).unwrap();
        }
        assert_eq!(
            list_releases(td.path()),
            ["0.9.0", "0.9.1-rc.1", "0.9.1-rc.2", "0.9.1"]
        );
    }

    #[test]
    fn github_urls() {
        let base = "https://github.com/vesylapp/vesyl-print/releases/download";
        assert_eq!(
            default_manifest_url(base, "0.4.0"),
            format!("{base}/v0.4.0/vesyl-print-0.4.0.manifest.json")
        );
        assert_eq!(
            default_artifact_url(base, "0.4.0", "linux-aarch64"),
            format!("{base}/v0.4.0/vesyl-print-0.4.0-linux-aarch64.tar.gz")
        );
        assert_eq!(
            default_manifest_url("https://cdn.example/print/", "v1.2.3"),
            "https://cdn.example/print/vesyl-print-1.2.3.manifest.json"
        );
    }

    #[test]
    fn manifest_parse_and_canonical() {
        let m = ReleaseManifest::from_dict(&obj(json!({
            "version": "0.4.0",
            "channel": "stable",
            "artifact_url": "https://example/a.tar.gz",
            "artifact_sha256": "a".repeat(64),
            "signature": "ignored-in-canonical",
            "changelog": "Fix — “quotes”",
            "min_agent_version": null,
        })))
        .unwrap();
        let raw = String::from_utf8(m.canonical_bytes()).unwrap();
        assert!(!raw.contains("signature"));
        assert!(!raw.contains("min_agent_version"));
        // Exactly what `jq -S -c -a` (build-release.sh) signs.
        assert_eq!(
            raw,
            format!(
                r#"{{"artifact_sha256":"{}","artifact_url":"https://example/a.tar.gz","changelog":"Fix \u2014 \u201cquotes\u201d","channel":"stable","version":"0.4.0"}}"#,
                "a".repeat(64)
            )
        );
    }

    #[test]
    fn bad_sha_rejected() {
        let err = ReleaseManifest::from_dict(&obj(json!({
            "version": "0.4.0", "artifact_url": "https://x", "artifact_sha256": "deadbeef"
        })))
        .unwrap_err();
        assert_eq!(err.code, "bad_manifest");
    }

    #[test]
    fn signature_ok_and_bad() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let pem = key
            .verifying_key()
            .to_public_key_pem(ed25519_dalek::pkcs8::spki::der::pem::LineEnding::LF)
            .unwrap();
        let mut m = ReleaseManifest::from_dict(&obj(json!({
            "version": "0.4.0", "channel": "stable",
            "artifact_url": "https://example/a.tar.gz", "artifact_sha256": "b".repeat(64),
        })))
        .unwrap();
        let sig = base64::engine::general_purpose::STANDARD
            .encode(key.sign(&m.canonical_bytes()).to_bytes());
        m.signature = Some(sig.clone());
        m.raw.insert("signature".into(), sig.into());
        verify_manifest(&m, Some(&pem), true).unwrap();

        m.signature = Some(base64::engine::general_purpose::STANDARD.encode([0u8; 64]));
        assert_eq!(
            verify_manifest(&m, Some(&pem), true).unwrap_err().code,
            "bad_signature"
        );
        m.signature = None;
        assert_eq!(
            verify_manifest(&m, Some(&pem), true).unwrap_err().code,
            "bad_signature"
        );
        verify_manifest(&m, Some(&pem), false).unwrap();
    }

    #[test]
    fn bundled_key_parses() {
        VerifyingKey::from_public_key_pem(load_public_key_pem(None, None).unwrap().trim()).unwrap();
    }

    #[test]
    fn extract_flip_rollback() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("opt");
        let r1 = root.join("releases/0.4.0");
        extract_tarball(&build_release(td.path(), "0.4.0"), &r1).unwrap();
        assert!(r1.join("vesyl-print").is_file());
        write_version_file(&r1, "0.4.0").unwrap();
        flip_current(&root, "0.4.0").unwrap();
        assert_eq!(current_name(&root), "0.4.0");
        // Link is relative so the tree can move.
        assert_eq!(
            fs::read_link(root.join("current")).unwrap(),
            Path::new("releases/0.4.0")
        );

        let r2 = root.join("releases/0.4.1");
        extract_tarball(&build_release(&td.path().join("b"), "0.4.1"), &r2).unwrap();
        flip_current(&root, "0.4.1").unwrap();
        assert_eq!(current_name(&root), "0.4.1");
        assert_eq!(list_releases(&root), vec!["0.4.0", "0.4.1"]);

        assert_eq!(rollback(&root, None, None).unwrap(), "0.4.0");
        assert_eq!(current_name(&root), "0.4.0");
        assert_eq!(
            rollback(&root, Some("9.9.9"), None).unwrap_err().code,
            "missing_release"
        );
    }

    fn evil_tarball(
        path: &Path,
        build: impl FnOnce(&mut tar::Builder<flate2::write::GzEncoder<File>>),
    ) {
        let gz = flate2::write::GzEncoder::new(
            File::create(path).unwrap(),
            flate2::Compression::default(),
        );
        let mut tar = tar::Builder::new(gz);
        build(&mut tar);
        tar.into_inner().unwrap().finish().unwrap();
    }

    #[test]
    fn path_escape_rejected() {
        let td = tempfile::tempdir().unwrap();
        let evil = td.path().join("evil.tar.gz");
        evil_tarball(&evil, |tar| {
            // Builder::append_data refuses "..", so write the raw header name.
            let mut h = tar::Header::new_gnu();
            h.as_old_mut().name[..12].copy_from_slice(b"../escape.py");
            h.set_size(1);
            h.set_mode(0o644);
            h.set_cksum();
            tar.append(&h, &b"x"[..]).unwrap();
        });
        let err = extract_tarball(&evil, &td.path().join("out")).unwrap_err();
        assert_eq!(err.code, "bad_archive");
        assert!(!td.path().join("escape.py").exists());
        assert!(!td.path().join("out").exists());
    }

    #[test]
    fn escaping_symlink_rejected() {
        let td = tempfile::tempdir().unwrap();
        let evil = td.path().join("link.tar.gz");
        evil_tarball(&evil, |tar| {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_size(0);
            h.set_mode(0o777);
            tar.append_link(&mut h, "app/passwd", "/etc/passwd")
                .unwrap();
        });
        assert_eq!(
            extract_tarball(&evil, &td.path().join("out"))
                .unwrap_err()
                .code,
            "bad_archive"
        );
    }

    #[test]
    fn idle_when_no_desired() {
        let td = tempfile::tempdir().unwrap();
        let st = maybe_update_from_heartbeat(
            &obj(json!({"ok": true})),
            &cfg(td.path()),
            &env(td.path()),
            None,
            None,
            false,
            &NO_STOP,
        );
        assert_eq!(st.status, STATUS_IDLE);
        assert!(st.target_version.is_none());
    }

    #[test]
    fn idle_when_already_current() {
        let td = tempfile::tempdir().unwrap();
        let st = maybe_update_from_heartbeat(
            &obj(json!({"desired_agent_version": "0.4.0"})),
            &cfg(td.path()),
            &env(td.path()),
            None,
            None,
            false,
            &NO_STOP,
        );
        assert_eq!(st.status, STATUS_IDLE);
        assert_eq!(st.target_version.as_deref(), Some("0.4.0"));
    }

    #[test]
    fn skips_apply_when_auto_disabled() {
        let td = tempfile::tempdir().unwrap();
        let c = Config {
            auto_update_enabled: false,
            ..cfg(td.path())
        };
        let st = maybe_update_from_heartbeat(
            &obj(json!({"desired_agent_version": "9.9.9"})),
            &c,
            &env(td.path()),
            None,
            None,
            false,
            &NO_STOP,
        );
        assert_eq!(st.target_version.as_deref(), Some("9.9.9"));
        assert_eq!(st.status, STATUS_IDLE);
    }

    #[test]
    fn defers_when_jobs_busy() {
        let td = tempfile::tempdir().unwrap();
        // An unreachable manifest URL proves nothing was fetched.
        let hb = obj(
            json!({"desired_agent_version": "9.9.9", "update_url": "http://127.0.0.1:9/m.json"}),
        );
        let st = maybe_update_from_heartbeat(
            &hb,
            &cfg(td.path()),
            &env(td.path()),
            None,
            None,
            true,
            &NO_STOP,
        );
        assert_eq!(st.status, STATUS_IDLE);
        assert_eq!(st.target_version.as_deref(), Some("9.9.9"));
    }

    #[test]
    fn defers_while_pending_health() {
        let td = tempfile::tempdir().unwrap();
        let st = maybe_update_from_heartbeat(
            &obj(json!({"desired_agent_version": "9.9.9"})),
            &cfg(td.path()),
            &env(td.path()),
            Some(pending(utc_now_plus(60))),
            None,
            false,
            &NO_STOP,
        );
        assert_eq!(st.status, STATUS_PENDING_HEALTH);
        assert_eq!(st.target_version.as_deref(), Some("0.4.0"));
    }

    #[test]
    fn apply_release_with_local_artifact() {
        let td = tempfile::tempdir().unwrap();
        let tarball = build_release(td.path(), "0.5.0");
        let sha = sha256_file(&tarball).unwrap();
        let root = td.path().join("install");
        let m = ReleaseManifest::from_dict(&obj(json!({
            "version": "0.5.0", "channel": "stable",
            "artifact_url": url::Url::from_file_path(&tarball).unwrap().to_string(),
            "artifact_sha256": sha,
        })))
        .unwrap();
        apply_release(&m, &env(&root), None, false, &NO_STOP).unwrap();
        assert_eq!(current_name(&root), "0.5.0");
        assert!(root.join("releases/0.5.0/vesyl-print").is_file());
        assert!(!root.join("update/vesyl-print-0.5.0.tar.gz").exists());
    }

    #[test]
    fn heartbeat_driven_update_end_to_end() {
        let td = tempfile::tempdir().unwrap();
        let tarball = build_release(td.path(), "0.5.0");
        let manifest = td.path().join("m.json");
        fs::write(
            &manifest,
            json!({
                "version": "0.5.0",
                "artifact_url": url::Url::from_file_path(&tarball).unwrap().to_string(),
                "artifact_sha256": sha256_file(&tarball).unwrap(),
            })
            .to_string(),
        )
        .unwrap();
        let root = two_slots(td.path());
        let status_path = td.path().join("update_status.json");
        let c = Config {
            update_require_signature: false,
            ..cfg(td.path())
        };
        let hb = obj(json!({
            "desired_agent_version": "0.5.0",
            "update_url": url::Url::from_file_path(&manifest).unwrap().to_string(),
        }));
        let st = maybe_update_from_heartbeat(
            &hb,
            &c,
            &env(&root),
            None,
            Some(&status_path),
            false,
            &NO_STOP,
        );
        assert_eq!(st.status, STATUS_PENDING_HEALTH, "{:?}", st.last_error);
        assert_eq!(st.previous_version.as_deref(), Some("0.4.0"));
        assert_eq!(current_name(&root), "0.5.0");
        assert_eq!(
            read_update_status(&status_path).unwrap().status,
            STATUS_PENDING_HEALTH
        );
    }

    #[test]
    fn signature_required_when_key_available() {
        let td = tempfile::tempdir().unwrap();
        let tarball = build_release(td.path(), "0.5.0");
        let manifest = td.path().join("m.json");
        fs::write(
            &manifest,
            json!({
                "version": "0.5.0",
                "artifact_url": url::Url::from_file_path(&tarball).unwrap().to_string(),
                "artifact_sha256": sha256_file(&tarball).unwrap(),
            })
            .to_string(),
        )
        .unwrap();
        let root = two_slots(td.path());
        let hb = obj(json!({
            "desired_agent_version": "0.5.0",
            "update_url": url::Url::from_file_path(&manifest).unwrap().to_string(),
        }));
        // Default config requires signatures and the bundled key exists → unsigned manifest fails.
        let st = maybe_update_from_heartbeat(
            &hb,
            &cfg(td.path()),
            &env(&root),
            None,
            None,
            false,
            &NO_STOP,
        );
        assert_eq!(st.status, STATUS_FAILED);
        assert_eq!(st.last_error.as_deref(), Some("manifest missing signature"));
        assert_eq!(current_name(&root), "0.4.0");
    }

    #[test]
    fn download_checksum() {
        let td = tempfile::tempdir().unwrap();
        let src = td.path().join("a.bin");
        fs::write(&src, b"hello-ota").unwrap();
        let sha = hex(&Sha256::digest(b"hello-ota"));
        let url = url::Url::from_file_path(&src).unwrap().to_string();
        let dest = td.path().join("out.bin");
        http_download_to_file(&url, &dest, &sha, &NO_STOP).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"hello-ota");
        let err =
            http_download_to_file(&url, &td.path().join("bad.bin"), &"0".repeat(64), &NO_STOP)
                .unwrap_err();
        assert_eq!(err.code, "bad_checksum");
        assert!(!td.path().join("bad.bin.part").exists());
    }

    /// A `.part` file an earlier download left is replaced, never reopened:
    /// one this user cannot write (root's, from a crashed `sudo vesyl-print
    /// update apply`; read-only here) failed every later download, and a
    /// symlink planted there was written through.
    #[test]
    fn download_replaces_a_leftover_part_file() {
        let td = tempfile::tempdir().unwrap();
        let src = td.path().join("a.bin");
        fs::write(&src, b"hello-ota").unwrap();
        let url = url::Url::from_file_path(&src).unwrap().to_string();
        let sha = hex(&Sha256::digest(b"hello-ota"));
        let update = td.path().join("update");
        fs::create_dir(&update).unwrap();
        let dest = update.join("a.tar.gz");
        let part = update.join("a.tar.gz.part");

        fs::write(&part, b"stale").unwrap();
        crate::util::set_mode(&part, 0o444).unwrap();
        http_download_to_file(&url, &dest, &sha, &NO_STOP).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"hello-ota");
        assert!(fs::symlink_metadata(&part).is_err());

        let victim = td.path().join("victim");
        fs::write(&victim, b"keep").unwrap();
        std::os::unix::fs::symlink(&victim, &part).unwrap();
        http_download_to_file(&url, &dest, &sha, &NO_STOP).unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"keep");
        assert_eq!(fs::read(&dest).unwrap(), b"hello-ota");
        assert!(fs::symlink_metadata(&part).is_err());
    }

    /// Root downloading into the service user's `update/` (a `sudo vesyl-print
    /// update apply` whose install then fails, say) leaves the artifact to
    /// that user, past a root-owned `.part` a crashed run left. Needs root
    /// (or a user namespace).
    #[test]
    #[ignore = "needs root (or a user namespace) to chown"]
    fn root_download_leaves_the_artifact_to_the_update_dir_owner() {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        let td = tempfile::tempdir().unwrap();
        let src = td.path().join("a.bin");
        fs::write(&src, b"hello-ota").unwrap();
        let url = url::Url::from_file_path(&src).unwrap().to_string();
        let update = td.path().join("update");
        fs::create_dir(&update).unwrap();
        std::os::unix::fs::chown(&update, Some(1000), Some(1000)).unwrap();
        fs::write(update.join("a.tar.gz.part"), b"stale").unwrap();
        let dest = update.join("a.tar.gz");
        http_download_to_file(&url, &dest, &hex(&Sha256::digest(b"hello-ota")), &NO_STOP).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"hello-ota");
        let meta = fs::symlink_metadata(&dest).unwrap();
        assert_eq!((meta.uid(), meta.gid()), (1000, 1000));
    }

    #[test]
    fn health_success_whoami_ok() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let out = process_pending_health(
            pending(utc_now_plus(60)),
            &cfg(td.path()),
            &env(&root),
            WhoamiResult::Ok,
            None,
            None,
            &NO_STOP,
        );
        assert_eq!(out.status, STATUS_IDLE);
        assert!(out.previous_version.is_none());
        assert!(out.health_deadline_at.is_none());
        assert_eq!(current_name(&root), "0.4.0");
    }

    #[test]
    fn health_retries_before_deadline() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let out = process_pending_health(
            pending(utc_now_plus(120)),
            &cfg(td.path()),
            &env(&root),
            WhoamiResult::Error,
            Some("connection refused"),
            None,
            &NO_STOP,
        );
        assert_eq!(out.status, STATUS_PENDING_HEALTH);
        assert_eq!(out.health_attempts, 1);
        assert_eq!(out.last_error.as_deref(), Some("connection refused"));
        assert_eq!(current_name(&root), "0.4.0");
    }

    #[test]
    fn health_rollback_after_deadline() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let out = process_pending_health(
            pending("2000-01-01T00:00:00+00:00".into()),
            &cfg(td.path()),
            &env(&root),
            WhoamiResult::Error,
            Some("timeout"),
            None,
            &NO_STOP,
        );
        assert_eq!(out.status, STATUS_ROLLED_BACK);
        assert_eq!(current_name(&root), "0.3.0");
        assert!(out.last_error.unwrap().contains("rolled back"));
    }

    #[test]
    fn hard_local_fail_rolls_back_immediately() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let cur = fs::canonicalize(root.join("current")).unwrap();
        fs::remove_file(cur.join("vesyl-print")).unwrap();
        let out = process_pending_health(
            pending(utc_now_plus(120)),
            &cfg(td.path()),
            &env(&root),
            WhoamiResult::Ok,
            None,
            None,
            &NO_STOP,
        );
        assert_eq!(out.status, STATUS_ROLLED_BACK);
        assert_eq!(current_name(&root), "0.3.0");
    }

    #[test]
    fn slot_needs_the_rust_binary() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        assert!(local_slot_healthy(&env(&root), Some("0.4.0")).is_ok());
        // The units exec <slot>/vesyl-print: one that lost its execute bit
        // cannot start the agent, ...
        let cur = fs::canonicalize(root.join("current")).unwrap();
        crate::util::set_mode(&cur.join("vesyl-print"), 0o644).unwrap();
        let err = local_slot_healthy(&env(&root), Some("0.4.0")).unwrap_err();
        assert_eq!(err, "current slot has no executable vesyl-print binary");
        crate::util::set_mode(&cur.join("vesyl-print"), 0o755).unwrap();
        // ... nor can the binary only under bin/.
        fs::create_dir(cur.join("bin")).unwrap();
        fs::rename(cur.join("vesyl-print"), cur.join("bin/vesyl-print")).unwrap();
        let err = local_slot_healthy(&env(&root), Some("0.4.0")).unwrap_err();
        assert!(err.contains("vesyl-print binary"), "{err}");
        // A Python-era slot (agent.py / main.py only) is no longer runnable.
        fs::remove_dir_all(cur.join("bin")).unwrap();
        fs::write(cur.join("agent.py"), "# old python agent\n").unwrap();
        let err = local_slot_healthy(&env(&root), Some("0.4.0")).unwrap_err();
        assert!(err.contains("vesyl-print binary"), "{err}");
    }

    #[test]
    fn archive_without_the_binary_is_rejected() {
        let td = tempfile::tempdir().unwrap();
        let src = td.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("agent.py"), "# python only\n").unwrap();
        let tarball = td.path().join("py.tar.gz");
        let gz = flate2::write::GzEncoder::new(
            File::create(&tarball).unwrap(),
            flate2::Compression::default(),
        );
        let mut tar = tar::Builder::new(gz);
        tar.append_dir_all("vesyl-print-0.9.0", &src).unwrap();
        tar.into_inner().unwrap().finish().unwrap();
        let m = ReleaseManifest::from_dict(&obj(json!({
            "version": "0.9.0",
            "artifact_url": url::Url::from_file_path(&tarball).unwrap().to_string(),
            "artifact_sha256": sha256_file(&tarball).unwrap(),
        })))
        .unwrap();
        let root = td.path().join("install");
        let err = apply_release(&m, &env(&root), None, false, &NO_STOP).unwrap_err();
        assert_eq!(err.code, "bad_archive");
        assert!(!root.join("releases/0.9.0").exists());
    }

    // --- runnable slots, rollback, the install path, staging (B1-B4) ----------

    /// A slot under `root/releases` holding `files` (path, executable).
    fn slot_with(root: &Path, version: &str, files: &[(&str, bool)]) -> PathBuf {
        let slot = root.join("releases").join(version);
        fs::create_dir_all(&slot).unwrap();
        for (path, exec) in files {
            let file = slot.join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(&file, b"\x7fELF").unwrap();
            crate::util::set_mode(&file, if *exec { 0o755 } else { 0o644 }).unwrap();
        }
        slot
    }

    /// What the units exec and the helper checks (`[[ -f && -x ]]` on
    /// `<slot>/vesyl-print`, in a slot that is not a symlink), and nothing
    /// else, makes a slot runnable.
    #[test]
    fn a_runnable_slot_has_an_executable_binary_at_its_root() {
        use std::os::unix::fs::symlink;
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        assert!(slot_is_runnable(&slot_with(
            root,
            "1.0.0",
            &[("vesyl-print", true)]
        )));
        for (version, files) in [
            ("1.0.1", &[("vesyl-print", false)][..]),
            ("1.0.2", &[("bin/vesyl-print", true)]),
            ("1.0.3", &[("vesyl-print/vesyl-print", true)]),
            ("1.0.4", &[("main.py", false)]),
        ] {
            assert!(
                !slot_is_runnable(&slot_with(root, version, files)),
                "{version}"
            );
        }
        assert!(!slot_is_runnable(&root.join("releases/9.9.9")));
        // A symlinked binary counts by what it points at...
        let linked = slot_with(root, "1.1.0", &[("bin/vesyl-print", true)]);
        symlink("bin/vesyl-print", linked.join("vesyl-print")).unwrap();
        assert!(slot_is_runnable(&linked));
        let dangling = slot_with(root, "1.1.1", &[]);
        symlink("bin/vesyl-print", dangling.join("vesyl-print")).unwrap();
        assert!(!slot_is_runnable(&dangling));
        // ... but a symlinked slot is refused, as the helper refuses it.
        symlink("1.0.0", root.join("releases/1.2.0")).unwrap();
        assert!(!slot_is_runnable(&root.join("releases/1.2.0")));
    }

    /// An archive whose binary is not executable, or is only under bin/,
    /// is refused before activation, however it arrives, and leaves nothing.
    #[test]
    fn archive_needs_an_executable_binary_at_its_root() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        for (version, files) in [
            ("0.9.0", &[("vesyl-print", false)][..]),
            ("0.9.1", &[("bin/vesyl-print", true)]),
        ] {
            let tarball = tarball_with(td.path(), version, files);
            let m = manifest_for(&tarball, version);
            for err in [
                apply_release(&m, &env(&root), None, false, &NO_STOP).unwrap_err(),
                apply_local_release(&m, &env(&root), &tarball, None, false).unwrap_err(),
            ] {
                assert_eq!(err.code, "bad_archive", "{version}: {err}");
                assert_eq!(
                    err.message, "archive missing an executable vesyl-print binary",
                    "{version}"
                );
            }
            assert_eq!(current_name(&root), "0.4.0", "{version}");
        }
        let left: Vec<String> = fs::read_dir(root.join("releases"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left.len(), 2, "{left:?}");
        assert_eq!(list_releases(&root), ["0.3.0", "0.4.0"]);
    }

    /// The archive is checked in its staging dir, before the slot it would
    /// replace is touched: a bad archive of an installed version leaves that
    /// slot as it was, and writes nothing outside the staging dir.
    #[test]
    fn bad_archive_leaves_the_installed_slot_alone() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let slot = root.join("releases/0.4.0");
        let binary = fs::read(slot.join("vesyl-print")).unwrap();
        let bad = tarball_with(td.path(), "0.4.0", &[("README", false)]);
        let err = apply_release(
            &manifest_for(&bad, "0.4.0"),
            &env(&root),
            None,
            false,
            &NO_STOP,
        )
        .unwrap_err();
        assert_eq!(err.code, "bad_archive");
        assert_eq!(fs::read(slot.join("vesyl-print")).unwrap(), binary);
        assert!(slot_is_runnable(&slot));
        assert!(local_slot_healthy(&env(&root), Some("0.4.0")).is_ok());

        // Its one entry a link to "." (inside the archive): moved to where
        // the slot goes, it would be `releases/` itself, and VERSION would
        // be written there.
        let linked = td.path().join("linked.tar.gz");
        evil_tarball(&linked, |tar| {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Symlink);
            h.set_size(0);
            h.set_mode(0o777);
            tar.append_link(&mut h, "vesyl-print-0.4.0", ".").unwrap();
        });
        let m = manifest_for(&linked, "0.4.0");
        let err = apply_local_release(&m, &env(&root), &linked, None, false).unwrap_err();
        assert_eq!(err.code, "bad_archive");
        assert!(!root.join("releases/VERSION").exists());
        assert_eq!(fs::read(slot.join("vesyl-print")).unwrap(), binary);
        let mut left: Vec<String> = fs::read_dir(root.join("releases"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["0.3.0", "0.4.0"]);
    }

    /// `update apply --file` checks what an online apply checks: a release
    /// this agent is too old for is refused, and a wrong checksum.
    #[test]
    fn local_release_checks_the_manifest_and_the_artifact() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let tarball = build_release(&td.path().join("0.5.0"), "0.5.0");
        let mut m = manifest_for(&tarball, "0.5.0");
        m.min_agent_version = Some("0.4.1".into());
        let err = apply_local_release(&m, &env(&root), &tarball, None, false).unwrap_err();
        assert_eq!(
            (err.code, err.message.as_str()),
            ("too_old", "current 0.4.0 < min_agent_version 0.4.1")
        );
        // The floor compares numbers only, as build-release.sh does: a
        // pre-release of 0.4.0 meets a 0.4.0 floor.
        m.min_agent_version = Some("0.4.0".into());
        let rc = UpdateEnv {
            running_version: "0.4.0-rc.1".into(),
            ..env(&root)
        };
        assert!(check_manifest(&m, &rc, None, false).is_ok());
        let other = td.path().join("other.tar.gz");
        fs::write(&other, b"not the artifact").unwrap();
        let err = apply_local_release(&m, &env(&root), &other, None, false).unwrap_err();
        assert_eq!(err.code, "bad_checksum");
        assert!(err.message.starts_with("sha256 mismatch: file="), "{err}");
        assert!(!root.join("releases/0.5.0").exists());

        let dir = apply_local_release(&m, &env(&root), &tarball, None, false).unwrap();
        assert_eq!(dir, root.join("releases/0.5.0"));
        assert_eq!(current_name(&root), "0.5.0");
        assert_eq!(fs::read_to_string(dir.join("VERSION")).unwrap(), "0.5.0\n");
        // Nothing downloaded, and the operator's tarball is left alone.
        assert!(!root.join("update").exists());
        assert!(tarball.is_file());
    }

    /// The slot's `VERSION`, which the LCD reads as the version it runs, is
    /// the manifest's: every install writes it, whether the archive ships a
    /// stale one or none.
    #[test]
    fn install_writes_the_manifest_version() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        // A 0.5.0 archive whose VERSION still says 0.4.9, and one without.
        let stale = td.path().join("stale.tar.gz");
        evil_tarball(&stale, |tar| {
            for (path, mode, data) in [
                ("vesyl-print", 0o755, &b"\x7fELF"[..]),
                ("VERSION", 0o644, b"0.4.9\n"),
            ] {
                let mut h = tar::Header::new_gnu();
                h.set_size(data.len() as u64);
                h.set_mode(mode);
                tar.append_data(&mut h, format!("vesyl-print-0.5.0/{path}"), data)
                    .unwrap();
            }
        });
        let bare = tarball_with(td.path(), "0.5.1", &[("vesyl-print", true)]);
        for (tarball, version) in [(stale, "0.5.0"), (bare, "0.5.1")] {
            let m = manifest_for(&tarball, version);
            let online = apply_release(&m, &env(&root), None, false, &NO_STOP).unwrap();
            let written = fs::read_to_string(online.join("VERSION")).ok();
            assert_eq!(written, Some(format!("{version}\n")), "apply_release");
            let local = apply_local_release(&m, &env(&root), &tarball, None, false).unwrap();
            let written = fs::read_to_string(local.join("VERSION")).ok();
            assert_eq!(written, Some(format!("{version}\n")), "apply_local_release");
            assert_eq!(current_name(&root), version);
        }
    }

    /// Rollback activates only a slot that can run: others are passed over
    /// when choosing, an explicit one is refused, and with none left the
    /// error says what was passed over.
    #[test]
    fn rollback_only_activates_runnable_slots() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        // Newer than 0.3.0, so before they were picked first.
        slot_with(&root, "0.3.5", &[("vesyl-print", false)]);
        slot_with(&root, "0.3.6", &[("bin/vesyl-print", true)]);
        assert_eq!(rollback(&root, None, None).unwrap(), "0.3.0");
        assert_eq!(current_name(&root), "0.3.0");
        for v in ["0.3.5", "0.3.6"] {
            let err = rollback(&root, Some(v), None).unwrap_err();
            assert_eq!(err.code, "not_runnable", "{v}");
            assert_eq!(
                err.message,
                format!("release {v} cannot run: no executable vesyl-print in its slot")
            );
            assert_eq!(current_name(&root), "0.3.0");
        }
        // From 0.3.0, 0.4.0 is the one other slot that runs.
        assert_eq!(rollback(&root, None, None).unwrap(), "0.4.0");
        fs::remove_dir_all(root.join("releases/0.3.0")).unwrap();
        let err = rollback(&root, None, None).unwrap_err();
        assert_eq!(err.code, "no_rollback");
        assert_eq!(
            err.message,
            "no previous release for rollback: no executable vesyl-print in 0.3.5, 0.3.6"
        );
        assert_eq!(current_name(&root), "0.4.0");
    }

    /// Where the apply-update helper is installed, it alone activates: when
    /// it refuses, nothing is flipped in-process instead, past the checks it
    /// makes as root. That holds for rollbacks and installs alike.
    #[test]
    fn the_helper_alone_activates_when_installed() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let line = |v: &str| {
            format!(
                "activate {} {}\n",
                root.join("releases").join(v).display(),
                root.join("current").display()
            )
        };
        let refusing = td.path().join("refusing");
        fs::create_dir(&refusing).unwrap();
        let helper = fake_helper(&refusing, true);
        for to in [None, Some("0.3.0")] {
            let err = rollback(&root, to, Some(&helper)).unwrap_err();
            assert_eq!(err.code, "activate_failed", "{to:?}");
            assert_eq!(
                err.message,
                "apply-update activate failed: apply-update: refused"
            );
            assert_eq!(current_name(&root), "0.4.0", "{to:?}");
        }
        let tarball = build_release(&td.path().join("0.5.0"), "0.5.0");
        let m = manifest_for(&tarball, "0.5.0");
        let refused = UpdateEnv {
            apply_helper: Some(helper),
            ..env(&root)
        };
        for err in [
            apply_release(&m, &refused, None, false, &NO_STOP).unwrap_err(),
            apply_local_release(&m, &refused, &tarball, None, false).unwrap_err(),
        ] {
            assert_eq!(err.code, "activate_failed");
            assert_eq!(current_name(&root), "0.4.0");
        }
        assert_eq!(
            fs::read_to_string(refusing.join("helper.calls")).unwrap(),
            [line("0.3.0"), line("0.3.0"), line("0.5.0"), line("0.5.0")].concat()
        );

        // A helper that accepts: its word is the activation (this stand-in
        // flips nothing, so `current` stays where it was).
        let accepting = td.path().join("accepting");
        fs::create_dir(&accepting).unwrap();
        let helped = UpdateEnv {
            apply_helper: Some(fake_helper(&accepting, false)),
            ..env(&root)
        };
        assert_eq!(
            rollback(&root, None, helped.apply_helper.as_deref()).unwrap(),
            "0.5.0"
        );
        apply_local_release(&m, &helped, &tarball, None, false).unwrap();
        assert_eq!(current_name(&root), "0.4.0");
        assert_eq!(
            fs::read_to_string(accepting.join("helper.calls")).unwrap(),
            [line("0.5.0"), line("0.5.0")].concat()
        );
    }

    /// The health gate never rolls back to a slot that cannot run: it
    /// fails instead, leaving `current` and the services as they are.
    #[test]
    fn gate_never_rolls_back_to_a_slot_that_cannot_run() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        crate::util::set_mode(&root.join("releases/0.3.0/vesyl-print"), 0o644).unwrap();
        let agent = UpdateEnv {
            restart: true,
            ..env(&root)
        };
        let (out, restarts) = restarts_during(|| {
            process_pending_health(
                pending("2000-01-01T00:00:00+00:00".into()),
                &cfg(td.path()),
                &agent,
                WhoamiResult::Error,
                Some("timeout"),
                None,
                &NO_STOP,
            )
        });
        assert_eq!(out.status, STATUS_FAILED);
        assert_eq!(
            out.last_error.as_deref(),
            Some(
                "health failed: timeout; rollback error: \
                 release 0.3.0 cannot run: no executable vesyl-print in its slot"
            )
        );
        assert!(failed_health_gate(&out));
        assert_eq!(current_name(&root), "0.4.0");
        assert_eq!(restarts, 0);
    }

    /// `<version>.staging` is where an extract unpacks. One a crash left is
    /// never a release (listed, rolled back to, `current`'s version), and no
    /// manifest names a slot that is another version's staging dir.
    #[test]
    fn staging_dirs_are_never_releases() {
        for v in ["0.5.0.staging", "0.5.0-rc.1.staging"] {
            assert!(!is_version(v), "{v}");
            let err = ReleaseManifest::from_dict(&obj(json!({
                "version": v, "artifact_url": "https://x/a.tar.gz", "artifact_sha256": "a".repeat(64),
            })))
            .unwrap_err();
            assert_eq!(err.code, "bad_manifest", "{v}");
        }
        for v in [
            "0.5.0",
            "0.5.0-rc.1",
            "1.0.0.2",
            "0.5.0-staging",
            "0.5.0.staging2",
        ] {
            assert!(is_version(v), "{v}");
        }

        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        // A flat archive's, cut off before its rename: it holds the binary.
        let staging = slot_with(&root, "0.5.0.staging", &[("vesyl-print", true)]);
        assert!(slot_is_runnable(&staging));
        assert_eq!(list_releases(&root), ["0.3.0", "0.4.0"]);
        assert_eq!(rollback(&root, None, None).unwrap(), "0.3.0");
        assert_eq!(rollback(&root, None, None).unwrap(), "0.4.0");
        assert_eq!(
            rollback(&root, Some("0.5.0.staging"), None)
                .unwrap_err()
                .code,
            "missing_release"
        );
        flip_current(&root, "0.5.0.staging").unwrap();
        assert_eq!(current_release_version(&root), None);
    }

    #[test]
    fn running_version_mismatch_is_unhealthy_when_in_slot() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let e = UpdateEnv {
            running_from_slot: true,
            running_version: "0.3.0".into(),
            ..env(&root)
        };
        assert!(local_slot_healthy(&e, Some("0.4.0"))
            .unwrap_err()
            .contains("running version"));
    }

    #[test]
    fn unpaired_skipped_whoami_succeeds_local() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let out = process_pending_health(
            pending(utc_now_plus(60)),
            &cfg(td.path()),
            &env(&root),
            WhoamiResult::Skipped,
            None,
            None,
            &NO_STOP,
        );
        assert_eq!(out.status, STATUS_IDLE);
    }

    #[test]
    fn mark_pending_health_fields() {
        let mut st = UpdateStatus::default();
        mark_pending_health(
            &mut st,
            "0.5.0",
            Some("0.4.0".into()),
            90,
            Some("stable".into()),
        );
        assert_eq!(st.status, STATUS_PENDING_HEALTH);
        assert_eq!(st.previous_version.as_deref(), Some("0.4.0"));
        assert_eq!(st.target_version.as_deref(), Some("0.5.0"));
        assert!(st.health_deadline_at.is_some());
        assert_eq!(st.channel.as_deref(), Some("stable"));
    }

    #[test]
    fn recover_false_failed_then_health_ok() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let st = UpdateStatus {
            status: STATUS_FAILED.into(),
            last_error: Some("died with <Signals.SIGTERM: 15>".into()),
            health_deadline_at: None,
            ..pending(String::new())
        };
        let out = process_pending_health(
            st,
            &cfg(td.path()),
            &env(&root),
            WhoamiResult::Ok,
            None,
            None,
            &NO_STOP,
        );
        assert_eq!(out.status, STATUS_IDLE);
        assert!(out.last_error.is_none());
        assert_eq!(current_name(&root), "0.4.0");
    }

    #[test]
    fn restart_commands_prefer_helper() {
        let td = tempfile::tempdir().unwrap();
        let helper = td.path().join("apply-update");
        fs::write(&helper, "#!/bin/sh\n").unwrap();
        let cmds = restart_commands(Some(&helper));
        assert_eq!(cmds.len(), 1);
        // `sudo -n`, except in tests (see HELPER_RUNNER).
        let (runner, rest) = cmds[0].split_at(HELPER_RUNNER.len());
        assert_eq!(runner, HELPER_RUNNER);
        assert_eq!(rest, [helper.display().to_string(), "restart".into()]);
        let fallback = restart_commands(None);
        assert_eq!(
            fallback[0],
            ["systemctl", "restart", "--no-block", "vesyl-print-agent"]
        );
    }

    /// The agent blocks SIGINT and SIGTERM in every thread, and std passes
    /// the spawning thread's mask on to a child. The restart helper must
    /// still start with nothing blocked, as every other command the agent
    /// runs does, or it would ignore the SIGTERM a stop sends the unit.
    #[test]
    fn restart_helper_starts_with_no_signal_blocked() {
        let td = tempfile::tempdir().unwrap();
        // Reports its own mask: `sh` reads /proc/self/status itself.
        let report = td.path().join("sigblk");
        let helper = td.path().join("apply-update");
        fs::write(
            &helper,
            format!(
                "while read -r key value; do\n\
                 \x20 if [ \"$key\" = SigBlk: ]; then echo \"$value\" > '{0}.tmp' && mv '{0}.tmp' '{0}'; fi\n\
                 done < /proc/self/status\n",
                report.display()
            ),
        )
        .unwrap();
        let stop_bits = (1u64 << (libc::SIGINT - 1)) | (1 << (libc::SIGTERM - 1));
        // On a thread of its own: the blocked signals stay with it.
        let blocking = helper.clone();
        std::thread::spawn(move || {
            let mut set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
            // SAFETY: the set is initialized before it is changed or used.
            let rc = unsafe {
                libc::sigemptyset(set.as_mut_ptr());
                libc::sigaddset(set.as_mut_ptr(), libc::SIGINT);
                libc::sigaddset(set.as_mut_ptr(), libc::SIGTERM);
                libc::pthread_sigmask(libc::SIG_BLOCK, set.as_ptr(), std::ptr::null_mut())
            };
            assert_eq!(rc, 0);
            restart_services(Some(&blocking));
        })
        .join()
        .unwrap();
        // The helper runs detached: wait for its report.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !report.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "the restart helper never ran"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let mask = u64::from_str_radix(fs::read_to_string(&report).unwrap().trim(), 16).unwrap();
        assert_eq!(
            mask & stop_bits,
            0,
            "the restart helper started with {mask:#x} blocked"
        );
    }

    #[test]
    fn job_pause_statuses() {
        assert!(!should_pause_jobs(None));
        assert!(!should_pause_jobs(Some(&UpdateStatus::with_status(
            STATUS_IDLE
        ))));
        assert!(!should_pause_jobs(Some(&UpdateStatus::with_status(
            STATUS_FAILED
        ))));
        for s in [STATUS_DOWNLOADING, STATUS_INSTALLING, STATUS_PENDING_HEALTH] {
            assert!(
                should_pause_jobs(Some(&UpdateStatus::with_status(s))),
                "{s}"
            );
        }
    }

    #[test]
    fn job_pause_from_path() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("update_status.json");
        assert!(!should_pause_jobs_from_path(&path));
        let st = UpdateStatus {
            status: STATUS_INSTALLING.into(),
            current_version: "0.3.0".into(),
            target_version: Some("0.4.0".into()),
            ..Default::default()
        };
        write_update_status(&path, &st).unwrap();
        assert!(should_pause_jobs_from_path(&path));
        assert_eq!(read_update_status(&path).unwrap(), st);
    }

    // --- OTA decisions (C11 / C12 / C15) --------------------------------------

    /// Unsigned `file://` manifest (and its tarball) for `version`.
    fn local_manifest(td: &Path, version: &str) -> String {
        let tarball = build_release(&td.join(format!("rel-{version}")), version);
        let manifest = td.join(format!("m-{version}.json"));
        fs::write(
            &manifest,
            json!({
                "version": version,
                "artifact_url": url::Url::from_file_path(&tarball).unwrap().to_string(),
                "artifact_sha256": sha256_file(&tarball).unwrap(),
            })
            .to_string(),
        )
        .unwrap();
        url::Url::from_file_path(&manifest).unwrap().to_string()
    }

    fn desire(td: &Path, version: &str) -> JsonObject {
        obj(json!({
            "desired_agent_version": version,
            "update_url": local_manifest(td, version),
        }))
    }

    fn unsigned_ok(td: &Path) -> Config {
        Config {
            update_require_signature: false,
            ..cfg(td)
        }
    }

    #[test]
    fn rolled_back_version_is_not_reapplied() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let c = unsigned_ok(td.path());
        let rolled = UpdateStatus {
            status: STATUS_ROLLED_BACK.into(),
            current_version: "0.4.0".into(),
            target_version: Some("0.5.0".into()),
            last_error: Some("health failed: timeout; rolled back to 0.4.0".into()),
            ..Default::default()
        };
        let hb = desire(td.path(), "0.5.0");
        for _ in 0..2 {
            let st = maybe_update_from_heartbeat(
                &hb,
                &c,
                &env(&root),
                Some(rolled.clone()),
                None,
                false,
                &NO_STOP,
            );
            assert_eq!(st.status, STATUS_ROLLED_BACK);
            assert_eq!(st.target_version.as_deref(), Some("0.5.0"));
            assert_eq!(st.last_error, rolled.last_error);
            assert_eq!(current_name(&root), "0.4.0");
            assert!(!root.join("releases/0.5.0").exists(), "re-downloaded 0.5.0");
        }

        // A different desired version is applied as usual.
        let st = maybe_update_from_heartbeat(
            &desire(td.path(), "0.5.1"),
            &c,
            &env(&root),
            Some(rolled),
            None,
            false,
            &NO_STOP,
        );
        assert_eq!(st.status, STATUS_PENDING_HEALTH, "{:?}", st.last_error);
        assert_eq!(st.previous_version.as_deref(), Some("0.4.0"));
        assert_eq!(current_name(&root), "0.5.1");
    }

    #[test]
    fn gate_rollback_is_not_followed_by_a_reinstall() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let c = unsigned_ok(td.path());
        // 0.4.0 never reached the API: the gate rolls back to 0.3.0 ...
        let out = process_pending_health(
            pending("2000-01-01T00:00:00+00:00".into()),
            &c,
            &env(&root),
            WhoamiResult::Error,
            Some("timeout"),
            None,
            &NO_STOP,
        );
        assert_eq!(out.status, STATUS_ROLLED_BACK);
        assert_eq!(current_name(&root), "0.3.0");
        // ... and the restarted 0.3.0 agent keeps hearing desired 0.4.0.
        let old = UpdateEnv {
            running_version: "0.3.0".into(),
            ..env(&root)
        };
        let st = maybe_update_from_heartbeat(
            &desire(td.path(), "0.4.0"),
            &c,
            &old,
            Some(out),
            None,
            false,
            &NO_STOP,
        );
        assert_eq!(st.status, STATUS_ROLLED_BACK);
        assert_eq!(current_name(&root), "0.3.0");
    }

    #[test]
    fn only_health_gate_failures_block_a_retry() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let c = unsigned_ok(td.path());
        let hb = desire(td.path(), "0.5.0");
        let failed = |err: &str| UpdateStatus {
            status: STATUS_FAILED.into(),
            current_version: "0.4.0".into(),
            target_version: Some("0.5.0".into()),
            last_error: Some(err.into()),
            ..Default::default()
        };
        // Gate failed with nothing to roll back to: hold.
        let held = failed("health failed: timeout (no previous slot to roll back to)");
        let st =
            maybe_update_from_heartbeat(&hb, &c, &env(&root), Some(held), None, false, &NO_STOP);
        assert_eq!(st.status, STATUS_FAILED);
        assert_eq!(current_name(&root), "0.4.0");
        // A download error is transient: retried.
        let st = maybe_update_from_heartbeat(
            &hb,
            &c,
            &env(&root),
            Some(failed("network error: Connection refused")),
            None,
            false,
            &NO_STOP,
        );
        assert_eq!(st.status, STATUS_PENDING_HEALTH, "{:?}", st.last_error);
        assert_eq!(current_name(&root), "0.5.0");
    }

    #[test]
    fn stale_download_status_is_released_while_jobs_busy() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let c = unsigned_ok(td.path());
        let hb = desire(td.path(), "0.5.0");
        let path = td.path().join("update_status.json");
        for stale in [STATUS_DOWNLOADING, STATUS_INSTALLING] {
            // Left behind by a process killed mid-update.
            let st = UpdateStatus {
                status: stale.into(),
                current_version: "0.4.0".into(),
                target_version: Some("0.5.0".into()),
                ..Default::default()
            };
            write_update_status(&path, &st).unwrap();
            assert!(should_pause_jobs_from_path(&path));
            // Held push jobs make jobs_busy true; that must not keep the pause.
            let st = maybe_update_from_heartbeat(
                &hb,
                &c,
                &env(&root),
                read_update_status(&path),
                Some(&path),
                true,
                &NO_STOP,
            );
            write_update_status(&path, &st).unwrap();
            assert_eq!(st.status, STATUS_FAILED, "{stale}");
            assert_eq!(
                st.last_error.as_deref(),
                Some(
                    format!("update interrupted while {stale} (agent restarted mid-update)")
                        .as_str()
                )
            );
            assert!(!should_pause_jobs_from_path(&path), "{stale}");
            assert_eq!(current_name(&root), "0.4.0");
        }
        // Once the held jobs have printed, the next heartbeat downloads afresh.
        let st = maybe_update_from_heartbeat(
            &hb,
            &c,
            &env(&root),
            read_update_status(&path),
            Some(&path),
            false,
            &NO_STOP,
        );
        assert_eq!(st.status, STATUS_PENDING_HEALTH, "{:?}", st.last_error);
        assert_eq!(current_name(&root), "0.5.0");
    }

    #[test]
    fn unreadable_configured_key_fails_closed() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let der = td.path().join("update_public.der");
        fs::write(
            &der,
            [0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0xff],
        )
        .unwrap();
        let c = Config {
            update_public_key_path: der.display().to_string(),
            ..cfg(td.path())
        };
        assert!(c.update_require_signature);
        let hb = desire(td.path(), "0.5.0");
        let st = maybe_update_from_heartbeat(&hb, &c, &env(&root), None, None, false, &NO_STOP);
        assert_eq!(st.status, STATUS_FAILED);
        assert!(
            st.last_error
                .as_deref()
                .unwrap()
                .contains("is not a PEM file"),
            "{:?}",
            st.last_error
        );
        assert_eq!(current_name(&root), "0.4.0");

        // Only signatures disabled in config skip verification (and the key).
        let lab = Config {
            update_require_signature: false,
            ..c
        };
        let st = maybe_update_from_heartbeat(&hb, &lab, &env(&root), None, None, false, &NO_STOP);
        assert_eq!(st.status, STATUS_PENDING_HEALTH, "{:?}", st.last_error);
    }

    #[test]
    fn manifest_key_follows_config() {
        let td = tempfile::tempdir().unwrap();
        let on = cfg(td.path());
        assert_eq!(
            manifest_public_key(&on).unwrap().as_deref(),
            Some(BUNDLED_PUBLIC_KEY_PEM)
        );
        let off = Config {
            update_require_signature: false,
            update_public_key_path: "/nonexistent/key.pem".into(),
            ..cfg(td.path())
        };
        assert_eq!(manifest_public_key(&off).unwrap(), None);
        // A configured path that does not exist falls back to the bundled
        // key: verification still happens.
        let missing = Config {
            update_public_key_path: td.path().join("nope.pem").display().to_string(),
            ..cfg(td.path())
        };
        assert_eq!(
            manifest_public_key(&missing).unwrap().as_deref(),
            Some(BUNDLED_PUBLIC_KEY_PEM)
        );
        // One that exists but cannot be read is an error.
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            let locked = td.path().join("locked.pem");
            fs::write(&locked, BUNDLED_PUBLIC_KEY_PEM).unwrap();
            crate::util::set_mode(&locked, 0o000).unwrap();
            let err = manifest_public_key(&Config {
                update_public_key_path: locked.display().to_string(),
                ..cfg(td.path())
            })
            .unwrap_err();
            assert_eq!(err.code, "bad_public_key");
            assert!(
                err.message.starts_with("cannot read update public key"),
                "{err}"
            );
        }
    }

    #[test]
    fn configured_key_verifies_heartbeat_update() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let key = SigningKey::from_bytes(&[9u8; 32]);
        let pem_path = td.path().join("lab_public.pem");
        fs::write(
            &pem_path,
            key.verifying_key()
                .to_public_key_pem(ed25519_dalek::pkcs8::spki::der::pem::LineEnding::LF)
                .unwrap(),
        )
        .unwrap();
        let c = Config {
            update_public_key_path: pem_path.display().to_string(),
            ..cfg(td.path())
        };
        // Unsigned → refused.
        let hb = desire(td.path(), "0.5.0");
        let st = maybe_update_from_heartbeat(&hb, &c, &env(&root), None, None, false, &NO_STOP);
        assert_eq!(st.last_error.as_deref(), Some("manifest missing signature"));
        // Signed with the configured key → installed.
        let manifest_path = url::Url::parse(hb["update_url"].as_str().unwrap())
            .unwrap()
            .to_file_path()
            .unwrap();
        let mut m = ReleaseManifest::from_dict(
            serde_json::from_str::<Value>(&fs::read_to_string(&manifest_path).unwrap())
                .unwrap()
                .as_object()
                .unwrap(),
        )
        .unwrap();
        let sig = base64::engine::general_purpose::STANDARD
            .encode(key.sign(&m.canonical_bytes()).to_bytes());
        m.raw.insert("signature".into(), sig.into());
        fs::write(&manifest_path, Value::Object(m.raw).to_string()).unwrap();
        let st = maybe_update_from_heartbeat(&hb, &c, &env(&root), None, None, false, &NO_STOP);
        assert_eq!(st.status, STATUS_PENDING_HEALTH, "{:?}", st.last_error);
        assert_eq!(current_name(&root), "0.5.0");
    }

    #[test]
    fn arm_health_gate_writes_pending_health() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("update_status.json");
        let st = arm_health_gate(&cfg(td.path()), &path, "0.5.0", Some("0.4.0".into())).unwrap();
        assert_eq!(read_update_status(&path).unwrap(), st);
        assert_eq!(st.status, STATUS_PENDING_HEALTH);
        assert_eq!(st.target_version.as_deref(), Some("0.5.0"));
        assert_eq!(st.previous_version.as_deref(), Some("0.4.0"));
        assert_eq!(st.channel.as_deref(), Some("stable"));
        assert!(st.health_deadline_at.is_some());
        // Reinstalling the same version leaves nothing to roll back to.
        let st = arm_health_gate(&cfg(td.path()), &path, "0.5.0", Some("0.5.0".into())).unwrap();
        assert!(st.previous_version.is_none());
    }

    /// `update apply … --restart` run as root arms the gate in the service
    /// user's state dir: the status file is that user's, even replacing a
    /// root-owned one an older root run left. Needs root (or a user
    /// namespace).
    #[test]
    #[ignore = "needs root (or a user namespace) to chown"]
    fn root_update_status_goes_to_the_state_dir_owner() {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        let td = tempfile::tempdir().unwrap();
        std::os::unix::fs::chown(td.path(), Some(1000), Some(1000)).unwrap();
        let path = td.path().join("update_status.json");
        fs::write(&path, "{}").unwrap();
        arm_health_gate(&cfg(td.path()), &path, "0.5.0", Some("0.4.0".into())).unwrap();
        let meta = fs::symlink_metadata(&path).unwrap();
        assert_eq!(
            (meta.uid(), meta.gid(), meta.mode() & 0o777),
            (1000, 1000, 0o644)
        );
    }

    #[test]
    fn slot_before_activation_prefers_current() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        assert_eq!(
            slot_before_activation(&env(&root)).as_deref(),
            Some("0.4.0")
        );
        let lab = UpdateEnv {
            running_version: "0.3.9".into(),
            ..env(&td.path().join("empty"))
        };
        assert_eq!(slot_before_activation(&lab).as_deref(), Some("0.3.9"));
    }

    /// An agent at `version`, started from its slot.
    fn slot_agent(root: &Path, version: &str) -> UpdateEnv {
        UpdateEnv {
            running_version: version.into(),
            running_from_slot: true,
            ..env(root)
        }
    }

    /// A process start time `secs` from now.
    fn started(secs: i64) -> Option<DateTime<Utc>> {
        Some(Utc::now() + chrono::Duration::seconds(secs))
    }

    /// The gate `update apply … --restart` arms for 0.4.0 over 0.3.0.
    fn armed_gate(td: &Path, c: &Config) -> UpdateStatus {
        let path = td.join("update_status.json");
        arm_health_gate(c, &path, "0.4.0", Some("0.3.0".into())).unwrap()
    }

    #[test]
    fn process_start_time_is_in_the_recent_past() {
        let started = process_started_at().expect("/proc/self/stat");
        let now = Utc::now();
        assert!(started <= now, "{started} > {now}");
        assert!(now - started < chrono::Duration::hours(1), "{started}");
    }

    #[test]
    fn armed_at_round_trips_and_is_written_only_while_set() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("update_status.json");
        let st = armed_gate(td.path(), &cfg(td.path()));
        let armed = parse_utc(st.armed_at.as_deref().unwrap()).unwrap();
        assert!(armed <= Utc::now());
        assert_eq!(read_update_status(&path).unwrap(), st);
        write_update_status(&path, &UpdateStatus::default()).unwrap();
        assert!(!fs::read_to_string(&path).unwrap().contains("armed_at"));
    }

    /// The 0.3.0 agent still runs from its slot after 0.4.0 was activated and
    /// the gate armed (the restart is queued): it must not judge 0.4.0. A
    /// process started after the gate was armed is judged as usual.
    #[test]
    fn replaced_agent_waits_for_the_restart() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let c = cfg(td.path());
        let gate = armed_gate(td.path(), &c);
        let judge = |env: &UpdateEnv, whoami: WhoamiResult, at: Option<DateTime<Utc>>| {
            judge_pending_health(
                gate.clone(),
                &c,
                env,
                whoami,
                Some("timeout"),
                None,
                at,
                &NO_STOP,
            )
        };
        let old = slot_agent(&root, "0.3.0");
        for whoami in [WhoamiResult::Ok, WhoamiResult::Error] {
            // Untouched: no attempt counted, no error, no rollback.
            assert_eq!(judge(&old, whoami, started(-60)), gate, "{whoami:?}");
            assert_eq!(current_name(&root), "0.4.0");
        }

        // The restarted agent runs the gate as usual.
        let out = judge(&slot_agent(&root, "0.4.0"), WhoamiResult::Ok, started(1));
        assert_eq!(out.status, STATUS_IDLE);
        assert_eq!(out.armed_at, None);
        // So does one started from the new slot after the gate was armed that
        // reports another version (a mislabeled build): it fails fast.
        let out = judge(&slot_agent(&root, "0.4.1"), WhoamiResult::Ok, started(1));
        assert_eq!(out.status, STATUS_ROLLED_BACK);
        assert!(failed_health_gate(&out), "{:?}", out.last_error);
        assert_eq!(current_name(&root), "0.3.0");
        flip_current(&root, "0.4.0").unwrap();

        // A gate armed before arm times were recorded: the agent being
        // replaced is the one running the version it rolls back to.
        let legacy = pending(utc_now_plus(120));
        assert_eq!(legacy.armed_at, None);
        let out = judge_pending_health(
            legacy.clone(),
            &c,
            &old,
            WhoamiResult::Ok,
            None,
            None,
            started(1),
            &NO_STOP,
        );
        assert_eq!(out, legacy);
        let mislabeled = slot_agent(&root, "0.4.1");
        let out = judge_pending_health(
            legacy,
            &c,
            &mislabeled,
            WhoamiResult::Ok,
            None,
            None,
            started(-60),
            &NO_STOP,
        );
        assert_eq!(out.status, STATUS_ROLLED_BACK);
        assert_eq!(current_name(&root), "0.3.0");
    }

    /// `current` was not the running slot when the gate was armed: 0.3.5 had
    /// been staged without a restart (`update apply --file` alone) before
    /// `update apply … --restart` activated 0.4.0. The 0.3.0 agent finishing
    /// its cycle is the one being replaced all the same: it must not roll
    /// 0.4.0 back (to 0.3.5) under the restart's feet.
    #[test]
    fn replaced_agent_waits_when_another_slot_was_staged() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let staged = root.join("releases/0.3.5");
        extract_tarball(&build_release(&td.path().join("0.3.5"), "0.3.5"), &staged).unwrap();
        write_version_file(&staged, "0.3.5").unwrap();
        // The agent started from 0.3.0; 0.3.5 was staged after that.
        flip_current(&root, "0.3.0").unwrap();
        let old = slot_agent(&root, "0.3.0");
        flip_current(&root, "0.3.5").unwrap();
        // What `update apply --file 0.4.0 … --restart` does, in order.
        let previous = slot_before_activation(&old);
        assert_eq!(previous.as_deref(), Some("0.3.5"));
        flip_current(&root, "0.4.0").unwrap();
        let c = cfg(td.path());
        let path = td.path().join("update_status.json");
        let gate = arm_health_gate(&c, &path, "0.4.0", previous).unwrap();

        // This test process started before the gate, as that agent did.
        for whoami in [WhoamiResult::Ok, WhoamiResult::Error] {
            let out = process_pending_health(
                gate.clone(),
                &c,
                &old,
                whoami,
                Some("timeout"),
                None,
                &NO_STOP,
            );
            assert_eq!(out, gate, "{whoami:?}");
            assert_eq!(current_name(&root), "0.4.0");
        }
        // The restarted agent passes the gate.
        let new = slot_agent(&root, "0.4.0");
        let out = process_pending_health(gate, &c, &new, WhoamiResult::Ok, None, None, &NO_STOP);
        assert_eq!(out.status, STATUS_IDLE);
        assert_eq!(current_name(&root), "0.4.0");
    }

    /// Still the agent being replaced at the deadline: the restart into 0.4.0
    /// never happened. It rolls back to the slot the activation replaced,
    /// the 0.3.0 it runs (and restarts), but 0.4.0 never ran, so that does
    /// not hold it: an agent started since installs it again when the server
    /// still asks for it. The process whose restart failed does not, or it
    /// would download and pause jobs for a gate on every retry while its
    /// restarts keep failing.
    #[test]
    fn missed_restart_rolls_back_without_holding_the_version() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let c = unsigned_ok(td.path());
        let gate = armed_gate(td.path(), &c);
        let armed_at = gate.armed_at.clone();
        let old = UpdateEnv {
            restart: true,
            ..slot_agent(&root, "0.3.0")
        };
        let late = utc_now_plus(health_gate_seconds(&c) + 1);
        let (out, restarts) = restarts_during(|| {
            judge_pending_health(
                gate,
                &c,
                &old,
                WhoamiResult::Ok,
                None,
                Some(&late),
                started(-60),
                &NO_STOP,
            )
        });
        assert_eq!(out.status, STATUS_ROLLED_BACK);
        assert_eq!(
            out.last_error.as_deref(),
            Some("restart never happened (still running 0.3.0); rolled back to 0.3.0")
        );
        assert_eq!(out.target_version.as_deref(), Some("0.4.0"));
        assert_eq!(out.armed_at, armed_at, "kept: who missed the restart");
        assert_eq!(current_name(&root), "0.3.0");
        assert_eq!(restarts, 1, "services restarted after the rollback");
        assert!(!failed_health_gate(&out));
        assert!(!should_pause_jobs(Some(&out)));
        assert!(restart_missed_here(&out, || started(-60)));
        assert!(!restart_missed_here(&out, || started(1)));
        assert!(!restart_missed_here(&out, || None));

        // This process (the test's, started before the gate) is the one whose
        // restart never came: it does not retry.
        let hb = desire(td.path(), "0.4.0");
        let agent = slot_agent(&root, "0.3.0");
        let st =
            maybe_update_from_heartbeat(&hb, &c, &agent, Some(out.clone()), None, false, &NO_STOP);
        assert_eq!(st.status, STATUS_ROLLED_BACK);
        assert_eq!(current_name(&root), "0.3.0");
        // An agent started since (the restart after the rollback worked):
        // to it, the gate was armed before it started.
        let since = process_started_at().unwrap() - chrono::Duration::seconds(1);
        let restarted_view = UpdateStatus {
            armed_at: Some(since.to_rfc3339_opts(SecondsFormat::Micros, false)),
            ..out
        };
        let st = maybe_update_from_heartbeat(
            &hb,
            &c,
            &agent,
            Some(restarted_view),
            None,
            false,
            &NO_STOP,
        );
        assert_eq!(st.status, STATUS_PENDING_HEALTH, "{:?}", st.last_error);
        assert_eq!(st.previous_version.as_deref(), Some("0.3.0"));
        assert_eq!(current_name(&root), "0.4.0");
    }

    /// `update rollback --restart` while the gate for 0.4.0 is open, the
    /// documented way out of a bad slot: whichever agent finds the gate next
    /// closes it as rolled back at once, so jobs resume, without flipping
    /// `current` or restarting again; and 0.4.0 is held while the server
    /// still asks for it.
    #[test]
    fn manual_rollback_during_the_gate_closes_it() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let c = unsigned_ok(td.path());
        let gate = armed_gate(td.path(), &c);
        assert!(should_pause_jobs(Some(&gate)));
        assert_eq!(rollback(&root, Some("0.3.0"), None).unwrap(), "0.3.0");

        let with_restart = |version: &str| UpdateEnv {
            restart: true,
            ..slot_agent(&root, version)
        };
        // The 0.3.0 agent the rollback started; the 0.3.0 agent the gate was
        // replacing, if its restart had not come yet; the 0.4.0 agent, when
        // the rollback was not followed by a restart.
        for (who, started_at) in [
            ("0.3.0", started(1)),
            ("0.3.0", started(-60)),
            ("0.4.0", started(1)),
        ] {
            let agent = with_restart(who);
            let (out, restarts) = restarts_during(|| {
                judge_pending_health(
                    gate.clone(),
                    &c,
                    &agent,
                    WhoamiResult::Error,
                    Some("timeout"),
                    None,
                    started_at,
                    &NO_STOP,
                )
            });
            let ctx = format!("{who} started {started_at:?}");
            assert_eq!(out.status, STATUS_ROLLED_BACK, "{ctx}");
            assert_eq!(
                out.last_error.as_deref(),
                Some("current changed to 0.3.0 during the health gate"),
                "{ctx}"
            );
            assert_eq!(out.target_version.as_deref(), Some("0.4.0"), "{ctx}");
            assert_eq!(out.previous_version, None, "{ctx}");
            assert_eq!(out.health_deadline_at, None, "{ctx}");
            assert_eq!(out.armed_at, None, "{ctx}");
            assert!(!should_pause_jobs(Some(&out)), "{ctx}");
            assert_eq!(restarts, 0, "{ctx}");
            assert_eq!(current_name(&root), "0.3.0", "{ctx}");
        }

        let fresh = slot_agent(&root, "0.3.0");
        let out = process_pending_health(gate, &c, &fresh, WhoamiResult::Ok, None, None, &NO_STOP);
        assert_eq!(out.status, STATUS_ROLLED_BACK);
        let st = maybe_update_from_heartbeat(
            &desire(td.path(), "0.4.0"),
            &c,
            &fresh,
            Some(out),
            None,
            false,
            &NO_STOP,
        );
        assert_eq!(st.status, STATUS_ROLLED_BACK);
        assert_eq!(current_name(&root), "0.3.0");
    }

    /// `update apply --file 0.5.0` without --restart while the gate for 0.4.0
    /// is open moves `current` forward, not back: the gate closes the same
    /// way, under a label that does not call it a rollback.
    #[test]
    fn newer_apply_during_the_gate_is_not_called_a_rollback() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let c = unsigned_ok(td.path());
        let gate = armed_gate(td.path(), &c);
        let tarball = build_release(&td.path().join("0.5.0"), "0.5.0");
        let m = manifest_for(&tarball, "0.5.0");
        apply_local_release(&m, &env(&root), &tarball, None, false).unwrap();
        let old = slot_agent(&root, "0.3.0");
        let out = process_pending_health(gate, &c, &old, WhoamiResult::Ok, None, None, &NO_STOP);
        assert_eq!(out.status, STATUS_ROLLED_BACK);
        assert_eq!(
            out.last_error.as_deref(),
            Some("current changed to 0.5.0 during the health gate")
        );
        assert_eq!(out.target_version.as_deref(), Some("0.4.0"));
        assert_eq!(current_name(&root), "0.5.0");
    }

    /// The activate helper flipped `current`, then reported failure (a
    /// timeout after its `mv`): the gate is armed for the new version and
    /// the services restart, as after a clean activation. Without the
    /// restart this agent would wait out the gate, then roll back.
    #[test]
    fn activate_error_after_the_flip_still_restarts() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::OpenOptionsExt;
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let c = unsigned_ok(td.path());
        // The manifest comes through a FIFO, so the "helper" below runs once
        // the update has recorded the slot it leaves (0.4.0) and before it
        // activates: it flips `current` to 0.5.0, and leaves `current.new` a
        // non-empty directory so the flip the update then makes fails.
        let body = fs::read(
            url::Url::parse(&local_manifest(td.path(), "0.5.0"))
                .unwrap()
                .to_file_path()
                .unwrap(),
        )
        .unwrap();
        let fifo = td.path().join("manifest.fifo");
        let c_fifo = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_fifo` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c_fifo.as_ptr(), 0o600) }, 0);
        let hb = obj(json!({
            "desired_agent_version": "0.5.0",
            "update_url": url::Url::from_file_path(&fifo).unwrap().to_string(),
        }));
        let helper = {
            let (root, fifo) = (root.clone(), fifo.clone());
            std::thread::spawn(move || {
                // Opens (without blocking) once the update opens the manifest.
                let deadline = std::time::Instant::now() + Duration::from_secs(20);
                let mut manifest = loop {
                    match fs::OpenOptions::new()
                        .write(true)
                        .custom_flags(libc::O_NONBLOCK)
                        .open(&fifo)
                    {
                        Ok(f) => break f,
                        Err(e) if std::time::Instant::now() > deadline => {
                            panic!("the update never read its manifest: {e}")
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(5)),
                    }
                };
                fs::create_dir_all(root.join("releases/0.5.0")).unwrap();
                flip_current(&root, "0.5.0").unwrap();
                fs::create_dir_all(root.join("current.new/busy")).unwrap();
                manifest.write_all(&body).unwrap();
            })
        };
        let agent = UpdateEnv {
            restart: true,
            ..slot_agent(&root, "0.4.0")
        };
        let (st, restarts) = restarts_during(|| {
            maybe_update_from_heartbeat(&hb, &c, &agent, None, None, false, &NO_STOP)
        });
        helper.join().unwrap();
        assert_eq!(st.status, STATUS_PENDING_HEALTH, "{:?}", st.last_error);
        assert_eq!(st.target_version.as_deref(), Some("0.5.0"));
        assert_eq!(st.previous_version.as_deref(), Some("0.4.0"));
        assert!(st.armed_at.is_some());
        assert_eq!(current_name(&root), "0.5.0");
        assert_eq!(restarts, 1, "services restarted into 0.5.0");
    }

    /// After 0.5.0 failed its gate, heartbeats that defer desired 0.5.1
    /// (auto-update off, jobs in flight) must not move the hold onto 0.5.1,
    /// a version this node never tried: once updates resume, 0.5.1 installs.
    /// The same goes for a restart into 0.5.0 that this process missed.
    #[test]
    fn deferred_heartbeat_does_not_move_the_hold() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let c = unsigned_ok(td.path());
        let off = Config {
            auto_update_enabled: false,
            ..c.clone()
        };
        let rolled = UpdateStatus {
            status: STATUS_ROLLED_BACK.into(),
            current_version: "0.4.0".into(),
            target_version: Some("0.5.0".into()),
            last_error: Some("health failed: timeout; rolled back to 0.4.0".into()),
            ..Default::default()
        };
        // Armed after this (the test's) process started: it missed it.
        let mut missed = UpdateStatus::default();
        mark_pending_health(&mut missed, "0.5.0", None, 120, None);
        let missed = UpdateStatus {
            status: STATUS_ROLLED_BACK.into(),
            current_version: "0.4.0".into(),
            last_error: Some(
                "restart never happened (still running 0.4.0); rolled back to 0.4.0".into(),
            ),
            ..missed
        };
        let fix = desire(td.path(), "0.5.1");
        for blocked in [&rolled, &missed] {
            for (how, cfg, jobs_busy) in [("auto_update off", &off, false), ("jobs busy", &c, true)]
            {
                let how = format!("{how} after {:?}", blocked.last_error);
                flip_current(&root, "0.4.0").unwrap();
                let deferred = maybe_update_from_heartbeat(
                    &fix,
                    cfg,
                    &env(&root),
                    Some(blocked.clone()),
                    None,
                    jobs_busy,
                    &NO_STOP,
                );
                assert_eq!(deferred.status, STATUS_ROLLED_BACK, "{how}");
                assert_eq!(deferred.target_version.as_deref(), Some("0.5.0"), "{how}");
                assert_eq!(deferred.last_error, blocked.last_error, "{how}");
                assert_eq!(current_name(&root), "0.4.0", "{how}");

                let st = maybe_update_from_heartbeat(
                    &fix,
                    &c,
                    &env(&root),
                    Some(deferred),
                    None,
                    false,
                    &NO_STOP,
                );
                assert_eq!(
                    st.status, STATUS_PENDING_HEALTH,
                    "{how}: {:?}",
                    st.last_error
                );
                assert_eq!(st.target_version.as_deref(), Some("0.5.1"), "{how}");
                assert_eq!(current_name(&root), "0.5.1", "{how}");
            }
        }

        // The version that failed stays held through a deferral.
        flip_current(&root, "0.4.0").unwrap();
        let again = desire(td.path(), "0.5.0");
        let deferred = maybe_update_from_heartbeat(
            &again,
            &off,
            &env(&root),
            Some(rolled.clone()),
            None,
            false,
            &NO_STOP,
        );
        let st = maybe_update_from_heartbeat(
            &again,
            &c,
            &env(&root),
            Some(deferred),
            None,
            false,
            &NO_STOP,
        );
        assert_eq!(st.status, STATUS_ROLLED_BACK);
        assert_eq!(st.target_version.as_deref(), Some("0.5.0"));
        assert_eq!(current_name(&root), "0.4.0");
    }

    /// The slot to roll back to is on disk before anything can flip
    /// `current`: an install cut off right after the flip (power loss) comes
    /// back as a gate that can still roll back.
    #[test]
    fn interrupted_install_can_still_roll_back() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let c = unsigned_ok(td.path());
        let path = td.path().join("update_status.json");
        let url = |p: &Path| url::Url::from_file_path(p).unwrap().to_string();

        // Stopped while downloading (no manifest there) ...
        let hb = obj(json!({
            "desired_agent_version": "0.5.0",
            "update_url": url(&td.path().join("missing.json")),
        }));
        let st =
            maybe_update_from_heartbeat(&hb, &c, &env(&root), None, Some(&path), false, &NO_STOP);
        assert_eq!(st.status, STATUS_FAILED);
        let on_disk = read_update_status(&path).unwrap();
        assert_eq!(on_disk.status, STATUS_DOWNLOADING);
        assert_eq!(on_disk.previous_version.as_deref(), Some("0.4.0"));
        // ... and while installing (no artifact there).
        let manifest = td.path().join("m-noartifact.json");
        fs::write(
            &manifest,
            json!({
                "version": "0.5.0",
                "artifact_url": url(&td.path().join("missing.tar.gz")),
                "artifact_sha256": "0".repeat(64),
            })
            .to_string(),
        )
        .unwrap();
        let hb = obj(json!({"desired_agent_version": "0.5.0", "update_url": url(&manifest)}));
        let st =
            maybe_update_from_heartbeat(&hb, &c, &env(&root), None, Some(&path), false, &NO_STOP);
        assert_eq!(st.status, STATUS_FAILED);
        let on_disk = read_update_status(&path).unwrap();
        assert_eq!(on_disk.status, STATUS_INSTALLING);
        assert_eq!(on_disk.previous_version.as_deref(), Some("0.4.0"));

        // Power lost right after the flip to 0.5.0. The restarted 0.5.0 agent
        // finds the install interrupted (marked failed at start, as the agent
        // does) and recovers it into the gate.
        let slot = root.join("releases/0.5.0");
        extract_tarball(&build_release(&td.path().join("0.5.0"), "0.5.0"), &slot).unwrap();
        write_version_file(&slot, "0.5.0").unwrap();
        flip_current(&root, "0.5.0").unwrap();
        let interrupted = UpdateStatus {
            status: STATUS_FAILED.into(),
            last_error: Some(crate::agent::UPDATE_INTERRUPTED.into()),
            ..on_disk
        };
        let new = slot_agent(&root, "0.5.0");
        let gate = process_pending_health(
            interrupted,
            &c,
            &new,
            WhoamiResult::Error,
            Some("HTTP 503"),
            None,
            &NO_STOP,
        );
        assert_eq!(gate.status, STATUS_PENDING_HEALTH);
        assert_eq!(gate.previous_version.as_deref(), Some("0.4.0"));
        // 0.5.0 never reaches the API: at the deadline it rolls back.
        let late = utc_now_plus(health_gate_seconds(&c) + 1);
        let out = process_pending_health(
            gate,
            &c,
            &new,
            WhoamiResult::Error,
            Some("HTTP 503"),
            Some(&late),
            &NO_STOP,
        );
        assert_eq!(out.status, STATUS_ROLLED_BACK, "{:?}", out.last_error);
        assert_eq!(current_name(&root), "0.4.0");
    }

    /// A slot of the version being installed that the agent cannot delete
    /// (root unpacked it), and a staging dir a crashed extract left, are
    /// moved aside instead of failing the install on every heartbeat. An
    /// install that can delete them later does.
    #[test]
    fn undeletable_slot_and_staging_are_moved_aside() {
        // SAFETY: geteuid has no preconditions.
        let as_root = unsafe { libc::geteuid() } == 0;
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let c = unsigned_ok(td.path());
        let releases = root.join("releases");
        // Stand-ins for root-owned trees: a directory nobody may write.
        let lock = |dir: &Path, mode: u32| {
            let locked = dir.join("locked");
            fs::create_dir_all(&locked).unwrap();
            if !locked.join("vesyl-print").exists() {
                fs::write(locked.join("vesyl-print"), b"old").unwrap();
            }
            crate::util::set_mode(&locked, mode).unwrap();
        };
        lock(&releases.join("0.5.0"), 0o555);
        lock(&releases.join("0.5.0.staging"), 0o555);

        let st = maybe_update_from_heartbeat(
            &desire(td.path(), "0.5.0"),
            &c,
            &env(&root),
            None,
            None,
            false,
            &NO_STOP,
        );
        assert_eq!(st.status, STATUS_PENDING_HEALTH, "{:?}", st.last_error);
        assert_eq!(current_name(&root), "0.5.0");
        assert!(releases.join("0.5.0/vesyl-print").is_file());
        assert!(!releases.join("0.5.0/locked").exists());
        assert!(!releases.join("0.5.0.staging").exists());
        assert!(!releases.join(".0.5.0.unpack").exists());
        assert_eq!(list_releases(&root), ["0.3.0", "0.4.0", "0.5.0"]);
        // The staging dir left behind, then the slot the install swapped
        // out (at the staging dir's name).
        let aside = [
            releases.join(".0.5.0.staging.stale-1"),
            releases.join(".0.5.0.staging.stale-2"),
        ];
        if !as_root {
            for a in &aside {
                assert!(a.join("locked/vesyl-print").is_file(), "{}", a.display());
                lock(a, 0o755);
            }
        }

        // Deletable now: the next install clears them.
        let st = maybe_update_from_heartbeat(
            &desire(td.path(), "0.5.1"),
            &c,
            &env(&root),
            None,
            None,
            false,
            &NO_STOP,
        );
        assert_eq!(st.status, STATUS_PENDING_HEALTH, "{:?}", st.last_error);
        for a in &aside {
            assert!(!a.exists(), "{}", a.display());
        }
    }

    const SERVICE_USER_TD: &str = "VESYL_TEST_SERVICE_USER_TD";

    /// The case for real: an older `update apply` run as root left a
    /// root-owned `releases/0.5.0` and staging dir in the service user's
    /// `releases/`. The agent, running as that user, still installs 0.5.0
    /// from a heartbeat; root's next install clears what it moved aside.
    /// Needs root (or a user namespace):
    /// `unshare --map-root-user --map-auto <test binary> --include-ignored`.
    #[test]
    #[ignore = "needs root (or a user namespace) to chown and switch users"]
    fn service_user_replaces_a_slot_root_unpacked() {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::process::CommandExt;
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        local_manifest(td.path(), "0.5.0");
        // As setup.sh leaves it: the install tree is the service user's.
        std::os::unix::fs::chown(td.path(), Some(1000), Some(1000)).unwrap();
        crate::util::hand_tree_to_parent_owner(&root).unwrap();
        let releases = root.join("releases");
        for dir in ["0.5.0/bin", "0.5.0.staging/vesyl-print-0.5.0"] {
            fs::create_dir_all(releases.join(dir)).unwrap();
            fs::write(releases.join(dir).join("vesyl-print"), b"root's").unwrap();
        }

        // Through /proc: the service user may not search the directories the
        // test binary sits in.
        let out = std::process::Command::new("/proc/self/exe")
            .args([
                "--exact",
                "update::tests::service_user_install_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(SERVICE_USER_TD, td.path())
            .uid(1000)
            .gid(1000)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "child failed: {stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(current_name(&root), "0.5.0");
        let owner = |p: &Path| fs::symlink_metadata(p).unwrap().uid();
        assert_eq!(owner(&releases.join("0.5.0/vesyl-print")), 1000);
        // The staging dir left behind, then the slot swapped out.
        let aside = [
            releases.join(".0.5.0.staging.stale-1"),
            releases.join(".0.5.0.staging.stale-2"),
        ];
        for a in &aside {
            assert_eq!(owner(a), 0, "{}", a.display());
        }

        let st = maybe_update_from_heartbeat(
            &desire(td.path(), "0.5.1"),
            &unsigned_ok(td.path()),
            &env(&root),
            None,
            None,
            false,
            &NO_STOP,
        );
        assert_eq!(st.status, STATUS_PENDING_HEALTH, "{:?}", st.last_error);
        for a in &aside {
            assert!(!a.exists(), "{}", a.display());
        }
    }

    #[test]
    #[ignore = "child process of service_user_replaces_a_slot_root_unpacked"]
    fn service_user_install_child() {
        let Some(td) = std::env::var_os(SERVICE_USER_TD).map(PathBuf::from) else {
            return;
        };
        let manifest = url::Url::from_file_path(td.join("m-0.5.0.json")).unwrap();
        let hb = obj(json!({"desired_agent_version": "0.5.0", "update_url": manifest.to_string()}));
        let root = td.join("opt");
        let st = maybe_update_from_heartbeat(
            &hb,
            &unsigned_ok(&td),
            &env(&root),
            None,
            None,
            false,
            &NO_STOP,
        );
        assert_eq!(st.status, STATUS_PENDING_HEALTH, "{:?}", st.last_error);
    }

    /// A gate armed on disk after the agent read its status this cycle
    /// (`update apply … --restart`) survives the agent's heartbeat write-back.
    #[test]
    fn heartbeat_keeps_a_gate_armed_meanwhile() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let c = unsigned_ok(td.path());
        let path = td.path().join("update_status.json");
        let old = UpdateEnv {
            running_version: "0.3.0".into(),
            running_from_slot: true,
            ..env(&root)
        };
        let armed = arm_health_gate(&c, &path, "0.4.0", Some("0.3.0".into())).unwrap();
        let stale = [
            None,
            Some(UpdateStatus::default()),
            Some(UpdateStatus {
                status: STATUS_FAILED.into(),
                target_version: Some("0.4.0".into()),
                last_error: Some("network error: Connection refused".into()),
                ..Default::default()
            }),
            // An older gate (another target).
            Some(UpdateStatus {
                target_version: Some("0.3.5".into()),
                ..pending(utc_now_plus(60))
            }),
        ];
        // No desired version, and one that would otherwise be installed.
        for hb in [obj(json!({"ok": true})), desire(td.path(), "0.5.0")] {
            for st in &stale {
                let out = maybe_update_from_heartbeat(
                    &hb,
                    &c,
                    &old,
                    st.clone(),
                    Some(&path),
                    false,
                    &NO_STOP,
                );
                assert_eq!(out.status, STATUS_PENDING_HEALTH, "{st:?}");
                assert_eq!(out.target_version.as_deref(), Some("0.4.0"), "{st:?}");
                assert_eq!(out.previous_version.as_deref(), Some("0.3.0"), "{st:?}");
                assert_eq!(out.health_deadline_at, armed.health_deadline_at);
                assert_eq!(current_name(&root), "0.4.0");
                assert!(!root.join("releases/0.5.0").exists(), "installed 0.5.0");
            }
        }

        // Only a pending_health on disk wins; any other status there does not.
        write_update_status(&path, &UpdateStatus::default()).unwrap();
        let rolled = UpdateStatus {
            status: STATUS_ROLLED_BACK.into(),
            target_version: Some("0.5.0".into()),
            last_error: Some("health failed: timeout; rolled back to 0.4.0".into()),
            ..Default::default()
        };
        let hb = desire(td.path(), "0.5.0");
        let out = maybe_update_from_heartbeat(
            &hb,
            &c,
            &env(&root),
            Some(rolled),
            Some(&path),
            false,
            &NO_STOP,
        );
        assert_eq!(out.status, STATUS_ROLLED_BACK);
        // Without a status path (the CLI's own `update apply`) nothing is re-read.
        arm_health_gate(&c, &path, "0.4.0", Some("0.3.0".into())).unwrap();
        let out = maybe_update_from_heartbeat(
            &obj(json!({"ok": true})),
            &c,
            &old,
            None,
            None,
            false,
            &NO_STOP,
        );
        assert_eq!(out.status, STATUS_IDLE);
    }

    // --- artifact transport (C31) ----------------------------------------------

    use crate::cloud::http_stub::{self, respond};

    #[test]
    fn gzip_encoded_artifact_is_hashed_as_sent() {
        let td = tempfile::tempdir().unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(b"tarball bytes").unwrap();
        let wire = gz.finish().unwrap();
        let sha = hex(&Sha256::digest(&wire));
        let served = wire.clone();
        // A CDN that labels .tar.gz objects with Content-Encoding: gzip.
        let srv =
            http_stub::serve(move |_, s| respond(s, 200, &[("Content-Encoding", "gzip")], &served));
        let dest = td.path().join("a.tar.gz");
        http_download_to_file(&format!("{}/a.tar.gz", srv.base_url), &dest, &sha, &NO_STOP)
            .unwrap();
        assert_eq!(fs::read(&dest).unwrap(), wire);
        assert_eq!(
            srv.requests()[0].header("Accept-Encoding"),
            Some("identity")
        );
    }

    #[test]
    fn artifact_redirects_are_followed_and_errors_reported() {
        let td = tempfile::tempdir().unwrap();
        let srv = http_stub::serve(|req, s| match req.path.as_str() {
            "/download/a.tar.gz" => respond(s, 302, &[("Location", "/objects/a.tar.gz")], b""),
            "/objects/a.tar.gz" => respond(s, 200, &[], b"hello-ota"),
            "/nolocation" => respond(s, 302, &[], b""),
            _ => respond(s, 404, &[], b""),
        });
        let sha = hex(&Sha256::digest(b"hello-ota"));
        let dest = td.path().join("a.tar.gz");
        http_download_to_file(
            &format!("{}/download/a.tar.gz", srv.base_url),
            &dest,
            &sha,
            &NO_STOP,
        )
        .unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"hello-ota");

        let err =
            http_download_to_file(&format!("{}/missing", srv.base_url), &dest, &sha, &NO_STOP)
                .unwrap_err();
        assert_eq!(
            (err.code, err.message.as_str()),
            ("download_failed", "HTTP 404 downloading artifact")
        );
        let err = http_download_to_file(
            &format!("{}/nolocation", srv.base_url),
            &dest,
            &sha,
            &NO_STOP,
        )
        .unwrap_err();
        assert_eq!(err.code, "download_failed");
        assert!(!td.path().join("a.tar.gz.part").exists());
    }

    #[test]
    fn slow_download_is_not_cut_off_at_a_fixed_deadline() {
        // 2.5 s of body, a piece every 250 ms, with 1 s connect, response and
        // idle timeouts: `idle` bounds each read, not the whole download, so
        // a slow but steady download finishes.
        let body: Vec<u8> = (0..40u8).collect();
        let served = body.clone();
        let srv = http_stub::serve(move |_, s| {
            http_stub::trickle(s, &served, 10, Duration::from_millis(250))
        });
        let timeouts = Timeouts {
            connect: Duration::from_secs(1),
            response: Duration::from_secs(1),
            idle: Duration::from_secs(1),
            body: Duration::from_secs(30),
        };
        let mut got = Vec::new();
        open_url(
            &format!("{}/a.tar.gz", srv.base_url),
            timeouts,
            "downloading artifact",
        )
        .unwrap()
        .read_to_end(&mut got)
        .unwrap();
        assert_eq!(got, body);
    }

    #[test]
    fn stalled_download_fails() {
        let srv = http_stub::serve(|_, s| {
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nabc");
            let _ = s.flush();
            std::thread::sleep(Duration::from_secs(3));
        });
        let timeouts = Timeouts {
            connect: Duration::from_secs(1),
            response: Duration::from_secs(1),
            idle: Duration::from_secs(1),
            body: Duration::from_millis(500),
        };
        let started = std::time::Instant::now();
        let mut got = Vec::new();
        let result = open_url(&srv.base_url, timeouts, "downloading artifact")
            .unwrap()
            .read_to_end(&mut got);
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    /// N15: a download whose connection goes silent fails after the idle
    /// timeout (`Timeouts::ARTIFACT.idle`, 300 s per read), not the 30-minute
    /// body budget.
    #[test]
    fn dead_connection_fails_after_the_idle_timeout() {
        let srv = http_stub::serve(|_, s| {
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nabc");
            let _ = s.flush();
            std::thread::sleep(Duration::from_secs(20));
        });
        let timeouts = Timeouts {
            idle: Duration::from_secs(1),
            ..Timeouts::ARTIFACT
        };
        let started = std::time::Instant::now();
        let mut got = Vec::new();
        let err = open_url(&srv.base_url, timeouts, "downloading artifact")
            .unwrap()
            .read_to_end(&mut got)
            .unwrap_err();
        let took = started.elapsed();
        assert!(err.to_string().contains("timeout"), "{err}");
        assert!(took >= Duration::from_millis(900), "{took:?}");
        assert!(took < Duration::from_secs(5), "{took:?}");
        // Per read, at most 120 s for a manifest and 300 s for an artifact.
        assert_eq!(Timeouts::MANIFEST.idle, Duration::from_secs(120));
        assert_eq!(Timeouts::ARTIFACT.idle, Duration::from_secs(300));
    }

    // --- swapping a slot in, release versions -----------------------------------

    use std::sync::Arc;

    /// Each entry under `dir` by relative path: its mode, and a file's bytes.
    /// Equal before and after: the tree was left as it was.
    fn tree(dir: &Path) -> std::collections::BTreeMap<PathBuf, (u32, Option<Vec<u8>>)> {
        use std::os::unix::fs::PermissionsExt;
        let mut out = std::collections::BTreeMap::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(path) = stack.pop() {
            let meta = fs::symlink_metadata(&path).unwrap();
            let bytes = if meta.is_dir() {
                stack.extend(fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
                None
            } else {
                Some(fs::read(&path).unwrap())
            };
            let rel = path.strip_prefix(dir).unwrap().to_path_buf();
            out.insert(rel, (meta.permissions().mode(), bytes));
        }
        out
    }

    /// The names in `dir`, sorted.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// A new directory `name` in `td`.
    fn subdir(td: &Path, name: &str) -> PathBuf {
        let dir = td.join(name);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Run `f` as on a filesystem without RENAME_EXCHANGE.
    fn without_exchange<T>(f: impl FnOnce() -> T) -> T {
        EXCHANGE_UNSUPPORTED.with(|u| u.set(true));
        let out = f();
        EXCHANGE_UNSUPPORTED.with(|u| u.set(false));
        out
    }

    /// Reinstalling the version `current` points at (a repair, a re-apply,
    /// a heartbeat after `current` was switched without a restart) puts the
    /// new slot in place in one step. An artifact that turns out corrupt,
    /// or to lack the binary, leaves the slot exactly as it was and
    /// `current` working, however it arrives; a good one replaces the
    /// slot's files.
    #[test]
    fn reinstalling_the_active_slot_keeps_current_working() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let slot = root.join("releases/0.4.0");
        let canonical = fs::canonicalize(&slot).unwrap();
        let before = tree(&slot);
        // An artifact its checksum matches that is no tarball (a bad
        // upload), and one without the binary.
        let corrupt = subdir(td.path(), "corrupt").join("vesyl-print-0.4.0.tar.gz");
        fs::write(&corrupt, b"definitely not gzip").unwrap();
        let bare = tarball_with(&subdir(td.path(), "bare"), "0.4.0", &[("README", false)]);
        for (tarball, why) in [
            (&corrupt, "extract failed: "),
            (&bare, "archive missing an executable vesyl-print binary"),
        ] {
            let m = manifest_for(tarball, "0.4.0");
            for (how, err) in [
                (
                    "online",
                    apply_release(&m, &env(&root), None, false, &NO_STOP).unwrap_err(),
                ),
                (
                    "--file",
                    apply_local_release(&m, &env(&root), tarball, None, false).unwrap_err(),
                ),
            ] {
                let ctx = format!("{how} {}", tarball.display());
                assert_eq!(err.code, "bad_archive", "{ctx}: {err}");
                assert!(err.message.starts_with(why), "{ctx}: {err}");
                assert_eq!(tree(&slot), before, "{ctx}: the slot changed");
                assert_eq!(current_release_dir(&root), Some(canonical.clone()), "{ctx}");
                assert!(
                    local_slot_healthy(&env(&root), Some("0.4.0")).is_ok(),
                    "{ctx}"
                );
                assert_eq!(names(&root.join("releases")), ["0.3.0", "0.4.0"], "{ctx}");
            }
        }
        assert_eq!(names(&root.join("update")), Vec::<String>::new());

        // A good one is swapped in, online or from a file.
        for (how, marker) in [("online", "online-marker"), ("--file", "file-marker")] {
            let good = tarball_with(
                &subdir(td.path(), marker),
                "0.4.0",
                &[("vesyl-print", true), (marker, false)],
            );
            let m = manifest_for(&good, "0.4.0");
            let installed = match how {
                "online" => apply_release(&m, &env(&root), None, false, &NO_STOP),
                _ => apply_local_release(&m, &env(&root), &good, None, false),
            };
            assert_eq!(installed.unwrap(), slot, "{how}");
            assert!(slot.join(marker).is_file(), "{how}");
            assert!(!slot.join("main.py").exists(), "{how}: old files stayed");
            assert_eq!(fs::read_to_string(slot.join("VERSION")).unwrap(), "0.4.0\n");
            assert_eq!(current_name(&root), "0.4.0");
            assert!(
                local_slot_healthy(&env(&root), Some("0.4.0")).is_ok(),
                "{how}"
            );
            assert_eq!(names(&root.join("releases")), ["0.3.0", "0.4.0"], "{how}");
        }
    }

    /// The heartbeat's way to the same: the agent still runs 0.3.0, `current`
    /// was switched to 0.4.0 without a restart, and the server asks for
    /// 0.4.0. A corrupt artifact leaves the slot as it was; a good one is
    /// swapped in.
    #[test]
    fn a_heartbeat_reinstall_of_the_active_slot_keeps_current_working() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let c = unsigned_ok(td.path());
        let slot = root.join("releases/0.4.0");
        let before = tree(&slot);
        let agent = slot_agent(&root, "0.3.0");
        let asking_for = |tarball: &Path| {
            let manifest = tarball.with_extension("json");
            let m = manifest_for(tarball, "0.4.0");
            fs::write(&manifest, Value::Object(m.raw).to_string()).unwrap();
            let url = url::Url::from_file_path(&manifest).unwrap();
            obj(json!({"desired_agent_version": "0.4.0", "update_url": url.as_str()}))
        };

        let corrupt = subdir(td.path(), "corrupt").join("vesyl-print-0.4.0.tar.gz");
        fs::write(&corrupt, b"definitely not gzip").unwrap();
        let st = maybe_update_from_heartbeat(
            &asking_for(&corrupt),
            &c,
            &agent,
            None,
            None,
            false,
            &NO_STOP,
        );
        assert_eq!(st.status, STATUS_FAILED);
        assert_eq!(st.last_error_code.as_deref(), Some("bad_archive"));
        assert_eq!(tree(&slot), before);
        assert_eq!(current_name(&root), "0.4.0");
        assert!(local_slot_healthy(&env(&root), Some("0.4.0")).is_ok());

        let good = tarball_with(
            &subdir(td.path(), "good"),
            "0.4.0",
            &[("vesyl-print", true), ("marker", false)],
        );
        let st = maybe_update_from_heartbeat(
            &asking_for(&good),
            &c,
            &agent,
            None,
            None,
            false,
            &NO_STOP,
        );
        assert_eq!(st.status, STATUS_PENDING_HEALTH, "{:?}", st.last_error);
        assert!(slot.join("marker").is_file());
        assert_eq!(current_name(&root), "0.4.0");
        assert_eq!(names(&root.join("releases")), ["0.3.0", "0.4.0"]);
    }

    /// While the slot `current` points at is replaced, `current` leads to a
    /// runnable slot at every moment: a unit (re)started then finds its
    /// binary. (The slot used to be removed before its replacement was
    /// moved in.)
    #[test]
    fn current_never_dangles_while_its_slot_is_replaced() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let tarball = build_release(&td.path().join("again"), "0.4.0");
        let m = manifest_for(&tarball, "0.4.0");
        let done = Arc::new(AtomicBool::new(false));
        let watcher = {
            let (binary, done) = (root.join("current/vesyl-print"), done.clone());
            std::thread::spawn(move || {
                let mut looks = 0u64;
                while !done.load(Ordering::SeqCst) {
                    if !binary.is_file() {
                        return Err(looks);
                    }
                    looks += 1;
                }
                Ok(looks)
            })
        };
        for _ in 0..30 {
            apply_local_release(&m, &env(&root), &tarball, None, false).unwrap();
        }
        done.store(true, Ordering::SeqCst);
        let looks = watcher.join().unwrap();
        assert!(
            looks.is_ok(),
            "current/vesyl-print missing after {looks:?} looks"
        );
    }

    /// On a filesystem without RENAME_EXCHANGE, the slot `current` points
    /// at is left as it is and its reinstall refused for good (a retry
    /// would only download it again); any other slot of the version is
    /// replaced as before, cleared first.
    #[test]
    fn without_exchange_the_active_slot_is_never_replaced() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let slot = root.join("releases/0.4.0");
        let before = tree(&slot);
        let tarball = build_release(&td.path().join("again"), "0.4.0");
        let m = manifest_for(&tarball, "0.4.0");
        let err = without_exchange(|| apply_local_release(&m, &env(&root), &tarball, None, false))
            .unwrap_err();
        assert_eq!(err.code, NO_EXCHANGE, "{err}");
        assert!(
            err.message
                .ends_with("`current` points at it, so it was left as it is"),
            "{err}"
        );
        assert!(fails_for_good(err.code));
        assert_eq!(tree(&slot), before);
        assert_eq!(current_name(&root), "0.4.0");
        assert_eq!(names(&root.join("releases")), ["0.3.0", "0.4.0"]);

        // 0.3.0 is not where `current` points: replaced.
        let older = tarball_with(
            &subdir(td.path(), "older"),
            "0.3.0",
            &[("vesyl-print", true), ("marker", false)],
        );
        let m = manifest_for(&older, "0.3.0");
        without_exchange(|| apply_local_release(&m, &env(&root), &older, None, false)).unwrap();
        assert!(root.join("releases/0.3.0/marker").is_file());
        assert_eq!(current_name(&root), "0.3.0");
        assert_eq!(names(&root.join("releases")), ["0.3.0", "0.4.0"]);
    }

    const SWAP_TD: &str = "VESYL_TEST_SWAP_TD";

    /// Every path under `dir` whose owner (lstat) is not `uid`.
    fn not_owned_by(dir: &Path, uid: u32) -> Vec<PathBuf> {
        use std::os::unix::fs::MetadataExt;
        let mut wrong = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(path) = stack.pop() {
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.uid() != uid {
                wrong.push(path.clone());
            }
            if meta.is_dir() {
                stack.extend(fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
            }
        }
        wrong
    }

    /// Root (`sudo vesyl-print update apply --file` of the version that
    /// runs) swaps the slot in and leaves `releases/` the service user's:
    /// the new slot is that user's, the one swapped out deleted. That user
    /// (the agent) then reinstalls the slot, made root's here as an older
    /// root run left it, without write access to it: the swap moves
    /// neither slot out of `releases/`, and the old one is set aside for
    /// root to delete. Needs root (or a user namespace):
    /// `unshare --map-root-user --map-auto <test binary> --include-ignored`.
    #[test]
    #[ignore = "needs root (or a user namespace) to chown and switch users"]
    fn root_reinstall_of_the_active_slot_is_swapped_in() {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::process::CommandExt;
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        // As setup.sh leaves it: the install tree is the service user's.
        std::os::unix::fs::chown(td.path(), Some(1000), Some(1000)).unwrap();
        crate::util::hand_tree_to_parent_owner(&root).unwrap();
        let releases = root.join("releases");
        let slot = releases.join("0.4.0");

        let by_root = tarball_with(
            &subdir(td.path(), "root-run"),
            "0.4.0",
            &[("vesyl-print", true), ("by-root", false)],
        );
        let m = manifest_for(&by_root, "0.4.0");
        apply_local_release(&m, &env(&root), &by_root, None, false).unwrap();
        assert!(slot.join("by-root").is_file());
        assert_eq!(current_name(&root), "0.4.0");
        assert_eq!(not_owned_by(&releases, 1000), Vec::<PathBuf>::new());
        assert_eq!(names(&releases), ["0.3.0", "0.4.0"]);

        // An older root run left the slot root's.
        for path in [slot.clone(), slot.join("vesyl-print"), slot.join("by-root")] {
            std::os::unix::fs::lchown(&path, Some(0), Some(0)).unwrap();
        }
        let by_agent = tarball_with(
            &subdir(td.path(), "agent-run"),
            "0.4.0",
            &[("vesyl-print", true), ("by-agent", false)],
        );
        let m = manifest_for(&by_agent, "0.4.0");
        fs::write(
            td.path().join("agent-run.json"),
            Value::Object(m.raw.clone()).to_string(),
        )
        .unwrap();
        // Through /proc: the service user may not search the directories the
        // test binary sits in.
        let out = std::process::Command::new("/proc/self/exe")
            .args([
                "--exact",
                "update::tests::service_user_reinstall_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(SWAP_TD, td.path())
            .uid(1000)
            .gid(1000)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "child failed: {stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(current_name(&root), "0.4.0");
        assert!(slot.join("by-agent").is_file());
        assert_eq!(not_owned_by(&slot, 1000), Vec::<PathBuf>::new());
        let aside = releases.join(".0.4.0.staging.stale-1");
        assert!(aside.join("by-root").is_file());
        assert_eq!(fs::symlink_metadata(&aside).unwrap().uid(), 0);

        // Root's next install deletes it.
        apply_local_release(&m, &env(&root), &by_agent, None, false).unwrap();
        assert_eq!(names(&releases), ["0.3.0", "0.4.0"]);
        assert_eq!(not_owned_by(&releases, 1000), Vec::<PathBuf>::new());
    }

    #[test]
    #[ignore = "child process of root_reinstall_of_the_active_slot_is_swapped_in"]
    fn service_user_reinstall_child() {
        let Some(td) = std::env::var_os(SWAP_TD).map(PathBuf::from) else {
            return;
        };
        let raw = fs::read_to_string(td.join("agent-run.json")).unwrap();
        let m = ReleaseManifest::from_dict(&obj(serde_json::from_str(&raw).unwrap())).unwrap();
        let root = td.join("opt");
        let dir = apply_release(&m, &env(&root), None, false, &NO_STOP).unwrap();
        assert_eq!(dir, root.join("releases/0.4.0"));
    }

    #[test]
    fn a_leading_v_is_not_part_of_the_version() {
        for (raw, version) in [
            ("0.5.0", "0.5.0"),
            (" v0.5.0\n", "0.5.0"),
            ("v1.2.3-rc.1", "1.2.3-rc.1"),
            ("vv1.0.0", "v1.0.0"),
            ("latest", "latest"),
        ] {
            assert_eq!(normalize_version(raw), version, "{raw:?}");
        }
    }

    /// The manifest fetched for a desired version must be that version's (a
    /// leading `v` aside), and so must the archive it points at: anything
    /// else is refused before it is installed.
    #[test]
    fn a_release_of_another_version_is_refused() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let c = unsigned_ok(td.path());
        // The server asks for 0.5.0, but its update_url is 0.5.1's manifest.
        let wrong = obj(json!({
            "desired_agent_version": "0.5.0",
            "update_url": local_manifest(td.path(), "0.5.1"),
        }));
        let st = maybe_update_from_heartbeat(&wrong, &c, &env(&root), None, None, false, &NO_STOP);
        assert_eq!(st.status, STATUS_FAILED);
        assert_eq!(
            st.last_error.as_deref(),
            Some("manifest is for version 0.5.1, not 0.5.0")
        );
        assert_eq!(st.last_error_code.as_deref(), Some(VERSION_MISMATCH));
        assert!(!root.join("update").exists(), "downloaded");
        assert_eq!(names(&root.join("releases")), ["0.3.0", "0.4.0"]);
        assert_eq!(current_name(&root), "0.4.0");

        // `v0.5.0` is 0.5.0.
        let tagged = obj(json!({
            "desired_agent_version": "v0.5.0",
            "update_url": local_manifest(td.path(), "0.5.0"),
        }));
        let st = maybe_update_from_heartbeat(&tagged, &c, &env(&root), None, None, false, &NO_STOP);
        assert_eq!(st.status, STATUS_PENDING_HEALTH, "{:?}", st.last_error);
        assert_eq!(st.target_version.as_deref(), Some("0.5.0"));
        assert_eq!(current_name(&root), "0.5.0");
        // Running it, there is nothing to do (nothing fetched: the manifest
        // is gone), as for `v1.2.3` on a node that runs 1.2.3, which would
        // never equal 1.2.3 if compared as given.
        let gone = url::Url::from_file_path(td.path().join("gone.json")).unwrap();
        for (running, asked) in [("0.5.0", "v0.5.0"), ("1.2.3", "v1.2.3")] {
            let hb = obj(json!({"desired_agent_version": asked, "update_url": gone.as_str()}));
            let node = UpdateEnv {
                running_version: running.into(),
                ..env(&root)
            };
            let st = maybe_update_from_heartbeat(&hb, &c, &node, None, None, false, &NO_STOP);
            assert_eq!(st.status, STATUS_IDLE, "{asked}: {:?}", st.last_error);
            assert_eq!(st.target_version.as_deref(), Some(running), "{asked}");
        }

        // The right manifest for the artifact of another release.
        let other = build_release(&td.path().join("other"), "0.4.9");
        let m = manifest_for(&other, "0.5.2");
        for err in [
            apply_release(&m, &env(&root), None, false, &NO_STOP).unwrap_err(),
            apply_local_release(&m, &env(&root), &other, None, false).unwrap_err(),
        ] {
            assert_eq!(
                (err.code, err.message.as_str()),
                (
                    VERSION_MISMATCH,
                    "archive holds vesyl-print-0.4.9, not version 0.5.2"
                )
            );
        }
        assert_eq!(names(&root.join("releases")), ["0.3.0", "0.4.0", "0.5.0"]);
        assert_eq!(current_name(&root), "0.5.0");
    }

    // --- holds and backoff ---------------------------------------------------------

    /// What a test release server answers for a path, given its base URL.
    type Answer = Box<dyn Fn(&str) -> (u16, Vec<u8>) + Send + Sync>;

    /// Paths and what is answered for each.
    type Routes = Vec<(String, Answer)>;

    /// Serve `routes`; anything else is a 404.
    fn serve_routes(routes: Routes) -> http_stub::Stub {
        http_stub::serve(move |req, s| {
            let base = format!("http://127.0.0.1:{}", req.port());
            match routes.iter().find(|(path, _)| *path == req.path) {
                Some((_, answer)) => {
                    let (status, body) = answer(&base);
                    respond(s, status, &[], &body);
                }
                None => respond(s, 404, &[], b""),
            }
        })
    }

    /// The routes of a release named `name`: `/<name>.json`, its manifest
    /// for `version` (with `extra` merged in), and `/<name>.tar.gz`,
    /// `artifact`, served with `status`.
    fn release_routes(
        name: &str,
        version: &str,
        artifact: Vec<u8>,
        status: u16,
        extra: Value,
    ) -> Routes {
        let link = format!("/{name}.tar.gz");
        let (version, sha, href) = (
            version.to_string(),
            hex(&Sha256::digest(&artifact)),
            link.clone(),
        );
        let manifest: Answer = Box::new(move |base| {
            let mut m = json!({
                "version": version,
                "artifact_url": format!("{base}{href}"),
                "artifact_sha256": sha,
            });
            if let (Value::Object(m), Value::Object(extra)) = (&mut m, extra.clone()) {
                m.extend(extra);
            }
            (200, m.to_string().into_bytes())
        });
        let served: Answer = Box::new(move |_| (status, artifact.clone()));
        vec![(format!("/{name}.json"), manifest), (link, served)]
    }

    /// A heartbeat asking for `version`, its manifest `/<name>.json` on `srv`.
    fn asking(srv: &http_stub::Stub, name: &str, version: &str) -> JsonObject {
        obj(json!({
            "desired_agent_version": version,
            "update_url": format!("{}/{name}.json", srv.base_url),
        }))
    }

    /// A desired version that cannot be installed (a Python-era release
    /// without the binary, a manifest or signature that does not check
    /// out, …) is not fetched again on every heartbeat: like a version that
    /// failed its health gate, it is held, and shown as failed, until the
    /// desired version changes or an operator applies it.
    #[test]
    fn a_release_that_cannot_be_installed_is_held() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let lab = unsigned_ok(td.path());
        let signed = cfg(td.path());
        let python = fs::read(tarball_with(
            &subdir(td.path(), "py"),
            "0.5.0",
            &[("agent.py", false), ("main.py", false)],
        ))
        .unwrap();
        let good = fs::read(build_release(&td.path().join("good"), "0.5.0")).unwrap();
        let other = fs::read(build_release(&td.path().join("other"), "0.4.9")).unwrap();
        let portal: Answer = Box::new(|_| (200, b"<html>sign in</html>".to_vec()));
        let release = |artifact: &[u8], version: &str, extra: Value| {
            release_routes("r", version, artifact.to_vec(), 200, extra)
        };
        /// A release asked for as 0.5.0 that this node cannot install.
        struct Unfit<'a> {
            what: &'a str,
            routes: Routes,
            config: &'a Config,
            code: &'a str,
            /// How its `last_error` starts.
            error: &'a str,
            /// Requests to the server on each try.
            per_try: usize,
        }
        let cases = [
            Unfit {
                what: "no binary",
                routes: release(&python, "0.5.0", json!({})),
                config: &lab,
                code: "bad_archive",
                error: "archive missing an executable vesyl-print binary",
                per_try: 2,
            },
            Unfit {
                what: "no tarball",
                routes: release(b"not gzip", "0.5.0", json!({})),
                config: &lab,
                code: "bad_archive",
                error: "extract failed: ",
                per_try: 2,
            },
            Unfit {
                what: "checksum",
                routes: release(&good, "0.5.0", json!({"artifact_sha256": "0".repeat(64)})),
                config: &lab,
                code: "bad_checksum",
                error: "artifact sha256 mismatch",
                per_try: 2,
            },
            Unfit {
                what: "unsigned",
                routes: release(&good, "0.5.0", json!({})),
                config: &signed,
                code: "bad_signature",
                error: "manifest missing signature",
                per_try: 1,
            },
            Unfit {
                what: "too old",
                routes: release(&good, "0.5.0", json!({"min_agent_version": "99.0.0"})),
                config: &lab,
                code: "too_old",
                error: "current 0.4.0 < min_agent_version 99.0.0",
                per_try: 1,
            },
            Unfit {
                what: "another version's manifest",
                routes: release(&good, "0.5.1", json!({})),
                config: &lab,
                code: VERSION_MISMATCH,
                error: "manifest is for version 0.5.1, not 0.5.0",
                per_try: 1,
            },
            Unfit {
                what: "another version's archive",
                routes: release(&other, "0.5.0", json!({})),
                config: &lab,
                code: VERSION_MISMATCH,
                error: "archive holds vesyl-print-0.4.9, not version 0.5.0",
                per_try: 2,
            },
            Unfit {
                what: "no manifest",
                routes: vec![("/r.json".into(), portal)],
                config: &lab,
                code: "bad_manifest",
                error: "manifest is not valid JSON",
                per_try: 1,
            },
        ];
        let mut held = UpdateStatus::default();
        for Unfit {
            what,
            routes,
            config,
            code,
            error,
            per_try,
        } in cases
        {
            let srv = serve_routes(routes);
            let hb = asking(&srv, "r", "0.5.0");
            let heartbeat = |st: Option<UpdateStatus>| {
                maybe_update_from_heartbeat(&hb, config, &env(&root), st, None, false, &NO_STOP)
            };
            let failed = heartbeat(None);
            assert_eq!(failed.status, STATUS_FAILED, "{what}");
            assert_eq!(failed.last_error_code.as_deref(), Some(code), "{what}");
            let message = failed.last_error.clone().unwrap();
            assert!(message.starts_with(error), "{what}: {message}");
            assert_eq!(
                (failed.attempts, failed.retry_at.as_deref()),
                (1, None),
                "{what}"
            );
            assert_eq!(srv.requests().len(), per_try, "{what}");
            // Held, and shown as failed; nothing fetched again.
            held = failed.clone();
            for _ in 0..3 {
                held = heartbeat(Some(held));
                assert_eq!(held.status, STATUS_FAILED, "{what}");
                assert_eq!(held.last_error, failed.last_error, "{what}");
                assert_eq!(held.target_version.as_deref(), Some("0.5.0"), "{what}");
            }
            assert_eq!(srv.requests().len(), per_try, "{what}: fetched again");
            // An operator's apply starts from a fresh status.
            heartbeat(None);
            assert_eq!(
                srv.requests().len(),
                2 * per_try,
                "{what}: manual apply held"
            );
            assert_eq!(current_name(&root), "0.4.0", "{what}");
            assert_eq!(names(&root.join("releases")), ["0.3.0", "0.4.0"], "{what}");
        }

        // Another desired version is tried, and installed.
        let fixed = fs::read(build_release(&td.path().join("fixed"), "0.5.1")).unwrap();
        let srv = serve_routes(release_routes("fixed", "0.5.1", fixed, 200, json!({})));
        let hb = asking(&srv, "fixed", "0.5.1");
        let st =
            maybe_update_from_heartbeat(&hb, &lab, &env(&root), Some(held), None, false, &NO_STOP);
        assert_eq!(st.status, STATUS_PENDING_HEALTH, "{:?}", st.last_error);
        assert_eq!(
            (st.last_error_code, st.attempts, st.retry_at),
            (None, 0, None)
        );
        assert_eq!(current_name(&root), "0.5.1");
    }

    /// A failure that may pass (the network, an HTTP error, a full disk) is
    /// retried, but not on every heartbeat: after a minute, then 2, 4, …,
    /// at most an hour apart, kept in `update_status.json` across restarts.
    /// Another desired version, or success, starts afresh.
    #[test]
    fn a_failure_that_may_pass_is_retried_after_a_backoff() {
        assert_eq!(
            [1, 2, 3, 4, 5, 6, 7, 8, 100].map(retry_delay_seconds),
            [60, 120, 240, 480, 960, 1920, 3600, 3600, 3600]
        );
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let c = unsigned_ok(td.path());
        let path = td.path().join("update_status.json");
        let server = |version: &str, status: u16| {
            let tarball = build_release(&td.path().join(version), version);
            serve_routes(release_routes(
                "r",
                version,
                fs::read(tarball).unwrap(),
                status,
                json!({}),
            ))
        };
        let heartbeat = |srv: &http_stub::Stub, version: &str, st: Option<UpdateStatus>| {
            let hb = asking(srv, "r", version);
            let st =
                maybe_update_from_heartbeat(&hb, &c, &env(&root), st, Some(&path), false, &NO_STOP);
            write_update_status(&path, &st).unwrap();
            st
        };
        let wait = |st: &UpdateStatus| {
            (parse_utc(st.retry_at.as_deref().unwrap()).unwrap() - Utc::now()).num_seconds()
        };

        let down = server("0.5.0", 503);
        let st = heartbeat(&down, "0.5.0", None);
        assert_eq!(st.status, STATUS_FAILED);
        assert_eq!(
            st.last_error.as_deref(),
            Some("HTTP 503 downloading artifact")
        );
        assert_eq!(st.last_error_code.as_deref(), Some("download_failed"));
        assert_eq!(st.attempts, 1);
        assert!((50..=60).contains(&wait(&st)), "{:?}", st.retry_at);
        assert_eq!(down.requests().len(), 2);
        // Not before retry_at, read back from disk (a restart) too.
        let st = heartbeat(&down, "0.5.0", read_update_status(&path));
        assert_eq!((st.status.as_str(), st.attempts), (STATUS_FAILED, 1));
        assert_eq!(down.requests().len(), 2, "retried before retry_at");
        // Once it has passed: tried again, then twice as long to wait.
        let passed = UpdateStatus {
            retry_at: Some(utc_now_plus(-1)),
            ..st
        };
        let st = heartbeat(&down, "0.5.0", Some(passed));
        assert_eq!(down.requests().len(), 4);
        assert_eq!(st.attempts, 2);
        assert!((110..=120).contains(&wait(&st)), "{:?}", st.retry_at);
        // A retry_at further off than the longest wait came from a clock
        // that ran ahead: it has passed.
        let ahead = UpdateStatus {
            retry_at: Some(utc_now_plus(2 * RETRY_MAX_SECONDS)),
            ..st
        };
        let st = heartbeat(&down, "0.5.0", Some(ahead));
        assert_eq!(down.requests().len(), 6);
        assert_eq!(st.attempts, 3);

        // Another desired version is tried at once, from its first attempt.
        let also_down = server("0.5.1", 503);
        let st = heartbeat(&also_down, "0.5.1", Some(st));
        assert_eq!(also_down.requests().len(), 2);
        assert_eq!(
            (st.target_version.as_deref(), st.attempts),
            (Some("0.5.1"), 1)
        );
        assert!((50..=60).contains(&wait(&st)), "{:?}", st.retry_at);
        // Success clears it all.
        let up = server("0.5.2", 200);
        let st = heartbeat(&up, "0.5.2", Some(st));
        assert_eq!(st.status, STATUS_PENDING_HEALTH, "{:?}", st.last_error);
        assert_eq!(
            (st.last_error_code, st.attempts, st.retry_at),
            (None, 0, None)
        );
        assert_eq!(current_name(&root), "0.5.2");
    }

    /// `update_status.json` as earlier releases write it, without the
    /// fields for holds and the backoff, still loads; and a status that
    /// needs none of them is written as they write it, so a release rolled
    /// back to reads the file as before.
    #[test]
    fn status_files_of_earlier_releases_still_load() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("update_status.json");
        fs::write(
            &path,
            r#"{"status": "failed", "current_version": "0.4.0", "target_version": "0.5.0",
                "last_error": "network error: Connection refused", "channel": "stable",
                "last_checked_at": "2026-10-08T12:00:00+00:00", "previous_version": "0.4.0",
                "health_deadline_at": null, "health_attempts": 0}"#,
        )
        .unwrap();
        let st = read_update_status(&path).unwrap();
        assert_eq!(
            (
                st.last_error_code.as_deref(),
                st.attempts,
                st.retry_at.as_deref()
            ),
            (None, 0, None)
        );
        // Neither held nor waiting: retried (see only_health_gate_failures_block_a_retry).
        assert!(!failed_for_good(&st) && !retry_pending(&st, Utc::now()));

        let keys = |p: &Path| -> Vec<String> {
            let raw: Value = serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap();
            raw.as_object().unwrap().keys().cloned().collect()
        };
        let earlier = [
            "channel",
            "current_version",
            "health_attempts",
            "health_deadline_at",
            "last_checked_at",
            "last_error",
            "previous_version",
            "status",
            "target_version",
        ];
        write_update_status(&path, &st).unwrap();
        assert_eq!(keys(&path), earlier);
        let backing_off = UpdateStatus {
            last_error_code: Some("download_failed".into()),
            attempts: 2,
            retry_at: Some(utc_now_plus(120)),
            ..st
        };
        write_update_status(&path, &backing_off).unwrap();
        assert_eq!(read_update_status(&path).unwrap(), backing_off);
        let mut now = earlier.to_vec();
        now.extend(["attempts", "last_error_code", "retry_at"]);
        now.sort();
        assert_eq!(keys(&path), now);
    }

    /// A gate that failed its version and could not roll back (`health
    /// failed: …`) has given its verdict: it is not promoted back into a
    /// gate, which would pause jobs again on every cycle. An install cut
    /// off after its activation still is (see `recover_false_failed_then_health_ok`
    /// and `interrupted_install_can_still_roll_back`). Once the version it
    /// failed runs healthy after all (whoami answers again), the failure is
    /// cleared at once, never through `pending_health`: jobs never pause.
    #[test]
    fn a_failed_gate_is_not_armed_again() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let c = cfg(td.path());
        let expired = || pending("2000-01-01T00:00:00+00:00".into());
        let judge_in = |agent: &UpdateEnv, st: UpdateStatus, whoami: WhoamiResult| {
            process_pending_health(st, &c, agent, whoami, Some("timeout"), None, &NO_STOP)
        };
        let judge = |st: UpdateStatus, whoami: WhoamiResult| judge_in(&env(&root), st, whoami);
        let no_previous = judge(
            UpdateStatus {
                previous_version: None,
                ..expired()
            },
            WhoamiResult::Error,
        );
        assert_eq!(
            no_previous.last_error.as_deref(),
            Some("health failed: timeout (no previous slot to roll back to)")
        );
        // The slot to roll back to cannot run.
        crate::util::set_mode(&root.join("releases/0.3.0/vesyl-print"), 0o644).unwrap();
        let no_rollback = judge(expired(), WhoamiResult::Error);
        assert!(
            no_rollback
                .last_error
                .as_deref()
                .unwrap()
                .contains("; rollback error: "),
            "{:?}",
            no_rollback.last_error
        );
        // The agent runs 0.4.0 from a healthy slot: each later cycle used to
        // promote these back to pending_health.
        for failed in [no_previous.clone(), no_rollback] {
            let ctx = failed.last_error.clone().unwrap();
            assert_eq!(failed.status, STATUS_FAILED, "{ctx}");
            assert!(!should_pause_jobs(Some(&failed)), "{ctx}");
            let again = recover_false_update_failure(failed.clone(), &c, &env(&root));
            assert_eq!(again, failed, "{ctx}");
            // Cycle after cycle while whoami fails: failed, as it was.
            let mut st = failed.clone();
            for _ in 0..3 {
                st = judge(st, WhoamiResult::Error);
                assert_eq!(st, failed, "{ctx}");
            }
            // Whoami answers (paired or not): 0.4.0 runs healthy here after
            // all, and its failure is over.
            for whoami in [
                WhoamiResult::Ok,
                WhoamiResult::Unauthorized,
                WhoamiResult::Skipped,
            ] {
                let healed = judge(failed.clone(), whoami);
                assert_eq!(healed.status, STATUS_IDLE, "{ctx}: {whoami:?}");
                assert_eq!(
                    (
                        healed.current_version.as_str(),
                        healed.target_version.as_deref()
                    ),
                    ("0.4.0", Some("0.4.0")),
                    "{ctx}: {whoami:?}"
                );
                assert_eq!(
                    (
                        healed.last_error.as_deref(),
                        healed.previous_version.as_deref(),
                        healed.health_deadline_at.as_deref(),
                        healed.armed_at.as_deref()
                    ),
                    (None, None, None, None),
                    "{ctx}: {whoami:?}"
                );
                assert!(!should_pause_jobs(Some(&healed)), "{ctx}: {whoami:?}");
                // The heartbeat that follows asks for the version it runs.
                let hb = obj(json!({"desired_agent_version": "0.4.0"}));
                let after = maybe_update_from_heartbeat(
                    &hb,
                    &c,
                    &env(&root),
                    Some(healed),
                    None,
                    false,
                    &NO_STOP,
                );
                assert_eq!(after.status, STATUS_IDLE, "{ctx}: {whoami:?}");
            }
            assert_eq!(current_name(&root), "0.4.0", "{ctx}");
        }

        // Not while the version it failed does not run here (another agent),
        // nor while its slot cannot run.
        let other = slot_agent(&root, "0.3.0");
        assert_eq!(
            judge_in(&other, no_previous.clone(), WhoamiResult::Ok),
            no_previous
        );
        crate::util::set_mode(&root.join("releases/0.4.0/vesyl-print"), 0o644).unwrap();
        assert_eq!(judge(no_previous.clone(), WhoamiResult::Ok), no_previous);
    }

    /// `update rollback` from 0.4.0 to 0.3.0 is recorded: the agent does not
    /// install 0.4.0 again while the server still asks for it, only another
    /// version; an open gate is closed with it, and the record wins over a
    /// status an agent read before it was written.
    #[test]
    fn a_manual_rollback_holds_the_version_left() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let c = unsigned_ok(td.path());
        let path = td.path().join("update_status.json");
        let gate = armed_gate(td.path(), &c);
        assert_eq!(rollback(&root, None, None).unwrap(), "0.3.0");
        let st = record_manual_rollback(&path, "0.4.0", "0.3.0").unwrap();
        assert_eq!(read_update_status(&path).unwrap(), st);
        assert_eq!(st.status, STATUS_ROLLED_BACK);
        assert_eq!(st.target_version.as_deref(), Some("0.4.0"));
        assert_eq!(st.current_version, "0.3.0");
        assert_eq!(
            st.last_error.as_deref(),
            Some("manual rollback from 0.4.0 to 0.3.0")
        );
        assert_eq!(
            (
                st.previous_version.as_deref(),
                st.health_deadline_at.as_deref(),
                st.armed_at.as_deref(),
                st.health_attempts
            ),
            (None, None, None, 0)
        );
        assert!(!should_pause_jobs(Some(&st)));
        assert!(failed_health_gate(&st));

        let agent = slot_agent(&root, "0.3.0");
        let slot = tree(&root.join("releases/0.4.0"));
        // As read at the start of the cycle (the gate), and as on disk.
        for read in [Some(gate), Some(st.clone())] {
            let out = maybe_update_from_heartbeat(
                &desire(td.path(), "0.4.0"),
                &c,
                &agent,
                read,
                Some(&path),
                false,
                &NO_STOP,
            );
            assert_eq!(out.status, STATUS_ROLLED_BACK);
            assert_eq!(out.last_error, st.last_error);
            assert_eq!(current_name(&root), "0.3.0");
            assert_eq!(tree(&root.join("releases/0.4.0")), slot, "reinstalled");
        }
        // Another version is installed.
        let out = maybe_update_from_heartbeat(
            &desire(td.path(), "0.4.1"),
            &c,
            &agent,
            Some(st),
            Some(&path),
            false,
            &NO_STOP,
        );
        assert_eq!(out.status, STATUS_PENDING_HEALTH, "{:?}", out.last_error);
        assert_eq!(current_name(&root), "0.4.1");
    }

    /// A rollback, by a gate or by hand, is over once the server asks for
    /// the version running and `current` is that version: idle, with
    /// nothing held. It stayed `rolled_back` (an amber "Rolled back" on the
    /// LCD) until the next update. With no desired version it stays, and so
    /// does a failure, and so it does for the process the rollback left
    /// running (`current` is not the version it runs).
    #[test]
    fn a_rollback_is_over_once_the_version_running_is_desired() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        flip_current(&root, "0.3.0").unwrap();
        let c = unsigned_ok(td.path());
        let agent = slot_agent(&root, "0.3.0");
        let rolled = |why: &str| UpdateStatus {
            status: STATUS_ROLLED_BACK.into(),
            current_version: "0.3.0".into(),
            target_version: Some("0.4.0".into()),
            last_error: Some(why.into()),
            ..Default::default()
        };
        for st in [
            rolled("health failed: timeout; rolled back to 0.3.0"),
            rolled("manual rollback from 0.4.0 to 0.3.0"),
        ] {
            let heartbeat = |hb: Value| {
                maybe_update_from_heartbeat(
                    &obj(hb),
                    &c,
                    &agent,
                    Some(st.clone()),
                    None,
                    false,
                    &NO_STOP,
                )
            };
            let out = heartbeat(json!({"desired_agent_version": "0.3.0"}));
            assert_eq!(out.status, STATUS_IDLE, "{st:?}");
            assert_eq!(out.last_error, None);
            assert_eq!(held_version(&out, "0.3.0"), None);
            let out = heartbeat(json!({}));
            assert_eq!(out.status, STATUS_ROLLED_BACK, "{st:?}");
            // Still held while the server asks for 0.4.0.
            let out = heartbeat(json!({"desired_agent_version": "0.4.0"}));
            assert_eq!(out.status, STATUS_ROLLED_BACK, "{st:?}");
            assert_eq!(held_version(&out, "0.3.0"), Some("0.4.0"));
            assert_eq!(current_name(&root), "0.3.0");
            // The 0.4.0 process the rollback left running until its restart:
            // 0.4.0 runs and is asked for, but `current` is 0.3.0. Not over.
            let old = slot_agent(&root, "0.4.0");
            let hb = obj(json!({"desired_agent_version": "0.4.0"}));
            let out =
                maybe_update_from_heartbeat(&hb, &c, &old, Some(st.clone()), None, false, &NO_STOP);
            assert_eq!(out.status, STATUS_ROLLED_BACK, "{st:?}");
            assert_eq!(out.last_error, st.last_error);
            assert_eq!(held_version(&out, "0.3.0"), Some("0.4.0"));
        }
        let failed = UpdateStatus {
            status: STATUS_FAILED.into(),
            target_version: Some("0.4.0".into()),
            last_error: Some("health failed: timeout (no previous slot to roll back to)".into()),
            ..Default::default()
        };
        let hb = obj(json!({"desired_agent_version": "0.3.0"}));
        let out = maybe_update_from_heartbeat(&hb, &c, &agent, Some(failed), None, false, &NO_STOP);
        assert_eq!(out.status, STATUS_FAILED);
    }

    /// The gate this heartbeat's own activation arms is no record "written
    /// meanwhile by another process": the heartbeat returns its own status,
    /// with no such warning, whether the status file was there before or not.
    #[test]
    fn the_gate_a_heartbeat_arms_itself_is_not_written_meanwhile() {
        for idle_before in [false, true] {
            let td = tempfile::tempdir().unwrap();
            let root = two_slots(td.path());
            flip_current(&root, "0.3.0").unwrap();
            let c = unsigned_ok(td.path());
            let agent = slot_agent(&root, "0.3.0");
            let path = td.path().join("update_status.json");
            let before = UpdateStatus {
                current_version: "0.3.0".into(),
                ..Default::default()
            };
            if idle_before {
                write_update_status(&path, &before).unwrap();
            }
            let (out, logs) = logs_during(|| {
                maybe_update_from_heartbeat(
                    &desire(td.path(), "0.4.1"),
                    &c,
                    &agent,
                    Some(before.clone()),
                    Some(&path),
                    false,
                    &NO_STOP,
                )
            });
            assert_eq!(out.status, STATUS_PENDING_HEALTH, "{:?}", out.last_error);
            assert_eq!(current_name(&root), "0.4.1");
            assert!(
                !logs.iter().any(|(_, l)| l.contains("written meanwhile")),
                "idle before: {idle_before}: {logs:?}"
            );
        }
    }

    /// Of the records that leave `rolled_back`, only a gate's rollback to
    /// its previous slot restarted the services. An agent that sees a manual
    /// rollback (or a gate closed because `current` moved by hand) move
    /// `current` must not wait for a restart.
    #[test]
    fn only_a_gates_rollback_restarted_the_services() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("update_status.json");
        let manual = record_manual_rollback(&path, "0.4.0", "0.3.0").unwrap();
        let rolled = |why: &str| UpdateStatus {
            status: STATUS_ROLLED_BACK.into(),
            last_error: Some(why.into()),
            ..Default::default()
        };
        assert!(!rollback_restarted_services(&manual));
        assert!(!rollback_restarted_services(&rolled(
            "current changed to 0.3.0 during the health gate"
        )));
        assert!(rollback_restarted_services(&rolled(
            "health failed: timeout; rolled back to 0.3.0"
        )));
        assert!(rollback_restarted_services(&rolled(
            "restart never happened (still running 0.3.0); rolled back to 0.3.0"
        )));
        assert!(!rollback_restarted_services(&UpdateStatus::default()));
    }

    // --- stopping ----------------------------------------------------------------

    /// A stop that lands mid-download ends it after the read in progress,
    /// not when systemd kills the agent 90 s on: nothing is left behind (no
    /// `.part`, no artifact, no slot), nothing activated, and the status
    /// lets the next start try again (neither failed nor held).
    #[test]
    fn a_stop_mid_download_leaves_nothing_behind() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let c = unsigned_ok(td.path());
        let path = td.path().join("update_status.json");
        let tarball = build_release(&td.path().join("0.5.0"), "0.5.0");
        let body = fs::read(&tarball).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let first = Arc::new(AtomicBool::new(true));
        let (stopping, served) = (stop.clone(), body.clone());
        // The first download trickles: 40 pieces 200 ms apart, 8 s in all,
        // and the agent is stopped once the first is out. Later ones are
        // served whole.
        let srv = http_stub::serve(move |_, s| {
            if !first.swap(false, Ordering::SeqCst) {
                return respond(s, 200, &[], &served);
            }
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                served.len()
            );
            let pieces: Vec<&[u8]> = served.chunks(served.len().div_ceil(40)).collect();
            let _ = s.write_all(pieces[0]);
            let _ = s.flush();
            stopping.store(true, Ordering::SeqCst);
            for piece in &pieces[1..] {
                std::thread::sleep(Duration::from_millis(200));
                if s.write_all(piece).and_then(|()| s.flush()).is_err() {
                    return;
                }
            }
        });
        let manifest = td.path().join("m.json");
        fs::write(
            &manifest,
            json!({
                "version": "0.5.0",
                "artifact_url": format!("{}/a.tar.gz", srv.base_url),
                "artifact_sha256": sha256_file(&tarball).unwrap(),
            })
            .to_string(),
        )
        .unwrap();
        let hb = obj(json!({
            "desired_agent_version": "0.5.0",
            "update_url": url::Url::from_file_path(&manifest).unwrap().to_string(),
        }));
        let started = std::time::Instant::now();
        let st = maybe_update_from_heartbeat(&hb, &c, &env(&root), None, Some(&path), false, &stop);
        let took = started.elapsed();
        assert!(took < Duration::from_secs(5), "took {took:?}");
        assert_eq!(st.status, STATUS_IDLE, "{:?}", st.last_error);
        assert_eq!(
            st.last_error.as_deref(),
            Some("update stopped while downloading: the agent is stopping")
        );
        assert_eq!(st.target_version.as_deref(), Some("0.5.0"));
        assert_eq!(
            (
                st.last_error_code.as_deref(),
                st.attempts,
                st.retry_at.as_deref()
            ),
            (None, 0, None)
        );
        assert_eq!(st.previous_version, None);
        assert!(!should_pause_jobs(Some(&st)));
        assert_eq!(names(&root.join("update")), Vec::<String>::new());
        assert_eq!(names(&root.join("releases")), ["0.3.0", "0.4.0"]);
        assert_eq!(current_name(&root), "0.4.0");

        // The next start tries again.
        let st = maybe_update_from_heartbeat(
            &hb,
            &c,
            &env(&root),
            Some(st),
            Some(&path),
            false,
            &NO_STOP,
        );
        assert_eq!(st.status, STATUS_PENDING_HEALTH, "{:?}", st.last_error);
        assert_eq!(current_name(&root), "0.5.0");
    }

    /// Run `f` with a stop of its own, which the agent gets once the release
    /// is unpacked: while it is checked, before it is put in place.
    fn stopping_once_unpacked<T>(f: impl FnOnce(&AtomicBool) -> T) -> T {
        let stop = AtomicBool::new(false);
        STOP_ONCE_UNPACKED.with(|s| s.set(true));
        let out = f(&stop);
        STOP_ONCE_UNPACKED.with(|s| s.set(false));
        assert!(stop.load(Ordering::SeqCst), "nothing was unpacked");
        out
    }

    /// A stop that lands once the release is unpacked, while it is checked,
    /// is honored before the release is put in place: nothing is installed
    /// or left behind (no slot, staging dir or download), and a reinstall of
    /// the slot `current` points at leaves that slot as it was. Through the
    /// heartbeat, the status lets the next start try again: neither failed
    /// nor backing off, the failed attempts before it forgotten.
    #[test]
    fn a_stop_before_the_activation_installs_nothing() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let slot = root.join("releases/0.4.0");
        let before = tree(&slot);
        let stopped = "update stopped before the activation: the agent is stopping";
        let assert_stopped = |installed: Result<PathBuf, UpdateError>| {
            let err = installed.unwrap_err();
            assert_eq!((err.code, err.message.as_str()), (STOPPED, stopped));
        };

        // A reinstall of the version `current` points at, downloaded and not.
        let again = build_release(&td.path().join("again"), "0.4.0");
        let m = manifest_for(&again, "0.4.0");
        assert_stopped(stopping_once_unpacked(|stop| {
            apply_release(&m, &env(&root), None, false, stop)
        }));
        assert_stopped(stopping_once_unpacked(|stop| {
            install_release(&m, &env(&root), &File::open(&again).unwrap(), stop)
        }));
        assert_eq!(tree(&slot), before, "the active slot changed");
        assert_eq!(current_name(&root), "0.4.0");
        assert_eq!(names(&root.join("releases")), ["0.3.0", "0.4.0"]);
        assert_eq!(names(&root.join("update")), Vec::<String>::new());

        // A new version, asked for by the heartbeat after two failed tries.
        let c = unsigned_ok(td.path());
        let hb = desire(td.path(), "0.5.0");
        let retrying = UpdateStatus {
            status: STATUS_FAILED.into(),
            target_version: Some("0.5.0".into()),
            last_error: Some("HTTP 503 downloading artifact".into()),
            last_error_code: Some("download_failed".into()),
            attempts: 2,
            retry_at: Some(utc_now_plus(-1)),
            ..Default::default()
        };
        let st = stopping_once_unpacked(|stop| {
            maybe_update_from_heartbeat(&hb, &c, &env(&root), Some(retrying), None, false, stop)
        });
        assert_eq!(st.status, STATUS_IDLE, "{:?}", st.last_error);
        assert_eq!(st.last_error.as_deref(), Some(stopped));
        assert_eq!(st.target_version.as_deref(), Some("0.5.0"));
        assert_eq!(
            (
                st.last_error_code.as_deref(),
                st.attempts,
                st.retry_at.as_deref()
            ),
            (None, 0, None)
        );
        assert_eq!(st.previous_version, None);
        assert!(!should_pause_jobs(Some(&st)));
        assert_eq!(names(&root.join("releases")), ["0.3.0", "0.4.0"]);
        assert_eq!(names(&root.join("update")), Vec::<String>::new());
        assert_eq!(current_name(&root), "0.4.0");

        // The next start installs it.
        let st = maybe_update_from_heartbeat(&hb, &c, &env(&root), Some(st), None, false, &NO_STOP);
        assert_eq!(st.status, STATUS_PENDING_HEALTH, "{:?}", st.last_error);
        assert_eq!(current_name(&root), "0.5.0");
    }

    /// A stop that comes after the download (or before it is checked) is
    /// honored before anything is unpacked.
    #[test]
    fn a_stop_before_the_extract_installs_nothing() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let tarball = build_release(&td.path().join("0.5.0"), "0.5.0");
        let m = manifest_for(&tarball, "0.5.0");
        let tarball = File::open(&tarball).unwrap();
        let err = install_release(&m, &env(&root), &tarball, &AtomicBool::new(true)).unwrap_err();
        assert_eq!(
            (err.code, err.message.as_str()),
            (
                STOPPED,
                "update stopped before the extract: the agent is stopping"
            )
        );
        assert_eq!(names(&root.join("releases")), ["0.3.0", "0.4.0"]);
        assert_eq!(current_name(&root), "0.4.0");
    }

    /// A stop that lands once the new slot is activated leaves
    /// `pending_health` without the restart, which would replace the
    /// `systemctl stop` and bring the agent back: the next start runs the
    /// new slot and its gate.
    #[test]
    fn a_stop_after_the_activation_restarts_nothing() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let c = unsigned_ok(td.path());
        // An activate helper that holds the update until the test has
        // stopped the agent: the stop lands while the activation runs.
        let dir = subdir(td.path(), "helper");
        let (arrived, go) = (dir.join("arrived"), dir.join("go"));
        let helper = dir.join("apply-update");
        fs::write(
            &helper,
            format!(
                "touch '{}'\ni=0\n\
                 while [ ! -e '{}' ] && [ $i -lt 400 ]; do sleep 0.05; i=$((i+1)); done\n",
                arrived.display(),
                go.display()
            ),
        )
        .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopper = {
            let (stop, arrived, go) = (stop.clone(), arrived.clone(), go.clone());
            std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(20);
                while !arrived.exists() {
                    assert!(std::time::Instant::now() < deadline, "never activated");
                    std::thread::sleep(Duration::from_millis(10));
                }
                stop.store(true, Ordering::SeqCst);
                fs::write(go, b"").unwrap();
            })
        };
        let agent = UpdateEnv {
            restart: true,
            apply_helper: Some(helper),
            ..env(&root)
        };
        let hb = desire(td.path(), "0.5.0");
        let (st, restarts) = restarts_during(|| {
            maybe_update_from_heartbeat(&hb, &c, &agent, None, None, false, &stop)
        });
        stopper.join().unwrap();
        assert_eq!(st.status, STATUS_PENDING_HEALTH, "{:?}", st.last_error);
        assert_eq!(st.target_version.as_deref(), Some("0.5.0"));
        assert_eq!(st.previous_version.as_deref(), Some("0.4.0"));
        assert!(should_pause_jobs(Some(&st)));
        assert_eq!(restarts, 0, "restarted while stopping");

        // Not stopping: the services restart into it.
        let accepting = UpdateEnv {
            restart: true,
            apply_helper: Some(fake_helper(&subdir(td.path(), "accepting"), false)),
            ..env(&root)
        };
        let hb = desire(td.path(), "0.5.1");
        let (st, restarts) = restarts_during(|| {
            maybe_update_from_heartbeat(&hb, &c, &accepting, None, None, false, &NO_STOP)
        });
        assert_eq!(st.status, STATUS_PENDING_HEALTH, "{:?}", st.last_error);
        assert_eq!(restarts, 1);
    }

    /// The gate's rollback while the agent stops flips `current` but
    /// restarts nothing: the next start runs the slot rolled back to.
    #[test]
    fn a_gate_rollback_while_stopping_restarts_nothing() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let agent = UpdateEnv {
            restart: true,
            ..env(&root)
        };
        let stopping = AtomicBool::new(true);
        for (stop, expected) in [(&stopping, 0), (&NO_STOP, 1)] {
            flip_current(&root, "0.4.0").unwrap();
            let (out, restarts) = restarts_during(|| {
                process_pending_health(
                    pending("2000-01-01T00:00:00+00:00".into()),
                    &cfg(td.path()),
                    &agent,
                    WhoamiResult::Error,
                    Some("timeout"),
                    None,
                    stop,
                )
            });
            assert_eq!(out.status, STATUS_ROLLED_BACK);
            assert_eq!(current_name(&root), "0.3.0");
            assert_eq!(restarts, expected);
        }
    }

    // --- release versions, URLs shown -------------------------------------------

    /// A release version's digits are ASCII ones, as the scripts' `[0-9]`
    /// takes them: `\d` took any Unicode digit, so a version the helper
    /// refuses (`١.٢.٣`) counted as one here.
    #[test]
    fn a_release_version_has_ascii_digits_only() {
        for v in ["١.٢.٣", "1.2.٣", "１.２.３", "0.5.0-rc.١"] {
            assert!(!is_version(v), "{v}");
            let err = ReleaseManifest::from_dict(&obj(json!({
                "version": v, "artifact_url": "https://x/a.tar.gz", "artifact_sha256": "a".repeat(64),
            })))
            .unwrap_err();
            assert_eq!(err.code, "bad_manifest", "{v}");
        }
        assert!(is_version("10.20.30-rc.1"));
    }

    /// The manifest and the server pick the URLs an update fetches, and a
    /// presigned one carries its signature in the query (credentials may
    /// sit in its userinfo). Errors (`last_error`, which the heartbeat and
    /// the LCD show) and log lines name such a URL without either, and cut
    /// a long one.
    #[test]
    fn urls_are_shown_without_userinfo_or_query() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let c = unsigned_ok(td.path());
        let srv = http_stub::serve(|req, s| {
            let base = format!("http://u:pw@127.0.0.1:{}", req.port());
            let manifest = |artifact_url: String| {
                json!({"version": "0.5.0", "artifact_url": artifact_url, "artifact_sha256": "a".repeat(64)})
                    .to_string()
            };
            match req.path.split('?').next().unwrap_or_default() {
                "/presigned.json" => {
                    let url = format!("{base}/a.tar.gz?X-Amz-Signature=s3cr3t#part");
                    respond(s, 200, &[], manifest(url).as_bytes())
                }
                "/relative.json" => {
                    let url = "/a.tar.gz?X-Amz-Signature=s3cr3t".to_string();
                    respond(s, 200, &[], manifest(url).as_bytes())
                }
                "/a.tar.gz" => respond(s, 503, &[], b""),
                _ => respond(s, 404, &[], b""),
            }
        });
        let base = srv.base_url.clone();
        let with_secrets = |path: &str| {
            let base = base.replacen("://", "://u:pw@", 1);
            format!("{base}{path}?X-Amz-Signature=s3cr3t#frag")
        };
        let leaks =
            |text: &str| text.contains("s3cr3t") || text.contains("pw@") || text.contains('?');
        for (update_url, error, logged) in [
            (
                with_secrets("/m.json"),
                format!("HTTP 404 fetching {base}/m.json"),
                format!("applying update 0.5.0 from {base}/m.json"),
            ),
            (
                with_secrets("/presigned.json"),
                "HTTP 503 downloading artifact".to_string(),
                format!("downloading {base}/a.tar.gz"),
            ),
            (
                with_secrets("/relative.json"),
                "network error: invalid URL \"/a.tar.gz\": relative URL without a base".to_string(),
                "downloading /a.tar.gz".to_string(),
            ),
        ] {
            let hb = obj(json!({"desired_agent_version": "0.5.0", "update_url": update_url}));
            let (st, lines) = logs_during(|| {
                maybe_update_from_heartbeat(&hb, &c, &env(&root), None, None, false, &NO_STOP)
            });
            assert_eq!(
                st.last_error.as_deref(),
                Some(error.as_str()),
                "{update_url}"
            );
            assert!(
                lines.iter().any(|(_, line)| *line == logged),
                "{update_url}: {lines:?}"
            );
            for (_, line) in &lines {
                assert!(!leaks(line), "{line}");
            }
        }
        // As the CLI fetches them.
        let err = fetch_manifest(&with_secrets("/m.json")).unwrap_err();
        assert_eq!(err.message, format!("HTTP 404 fetching {base}/m.json"));
        let dest = td.path().join("a.tar.gz");
        for url in [
            with_secrets("/a.tar.gz"),
            "/a.tar.gz?X-Amz-Signature=s3cr3t".into(),
        ] {
            let err = http_download_to_file(&url, &dest, &"a".repeat(64), &NO_STOP).unwrap_err();
            assert!(!leaks(&err.message), "{err}");
        }
        // The server picks how long a URL is.
        let long = format!("https://cdn.example.test/{}?sig=s3cr3t", "p".repeat(5000));
        let shown = shown_url(&long);
        assert_eq!(shown.chars().count(), SHOWN_URL_CHARS);
        assert!(long.starts_with(&shown) && !leaks(&shown));
        assert_eq!(current_name(&root), "0.4.0");
    }

    /// A redirect target ureq cannot follow (a space, a byte a URL may not
    /// hold, `..` above the root) is quoted whole in ureq's error, and the
    /// server picks it: errors and log lines name it as [`shown_url`] shows
    /// it, without its query or userinfo, for the manifest and the artifact
    /// alike. (ureq's words, the signature included, reached `last_error`.)
    #[test]
    fn a_redirect_target_is_shown_without_userinfo_or_query() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let c = unsigned_ok(td.path());
        let srv = http_stub::serve(|req, s| {
            let target = match req.path.as_str() {
                "/space" => "/b c.tar.gz?X-Amz-Signature=s3cr3t".to_string(),
                "/above" => "../../x.tar.gz?X-Amz-Signature=s3cr3t".to_string(),
                "/byte" => "/b\u{e9}c.tar.gz?X-Amz-Signature=s3cr3t".to_string(),
                "/authority" => format!(
                    "//u:pw@127.0.0.1:{}/b c.tar.gz?X-Amz-Signature=s3cr3t",
                    req.port()
                ),
                // A manifest whose artifact URL redirects there.
                path => match path.strip_suffix(".json") {
                    Some(artifact) => {
                        let manifest = json!({
                            "version": "0.5.0",
                            "artifact_url": format!("http://127.0.0.1:{}{artifact}", req.port()),
                            "artifact_sha256": "a".repeat(64),
                        });
                        return respond(s, 200, &[], manifest.to_string().as_bytes());
                    }
                    None => return respond(s, 404, &[], b""),
                },
            };
            respond(s, 302, &[("Location", &target)], b"")
        });
        let base = srv.base_url.clone();
        let port = base.rsplit(':').next().unwrap().to_string();
        let leaks =
            |text: &str| text.contains("s3cr3t") || text.contains("pw@") || text.contains('?');
        let dest = td.path().join("a.tar.gz");
        for (route, shown) in [
            ("/space", "/b c.tar.gz".to_string()),
            ("/above", "../../x.tar.gz".to_string()),
            ("/byte", "/b\u{e9}c.tar.gz".to_string()),
            ("/authority", format!("//127.0.0.1:{port}/b c.tar.gz")),
        ] {
            let error = format!("network error: protocol: location header is malformed: {shown}");
            let err =
                http_download_to_file(&format!("{base}{route}"), &dest, &"a".repeat(64), &NO_STOP)
                    .unwrap_err();
            assert_eq!(
                (err.code, err.message.as_str()),
                ("download_failed", error.as_str()),
                "{route}"
            );
            // The manifest redirected (the update URL), then the artifact.
            for update_url in [format!("{base}{route}"), format!("{base}{route}.json")] {
                let hb = obj(json!({"desired_agent_version": "0.5.0", "update_url": update_url}));
                let (st, lines) = logs_during(|| {
                    maybe_update_from_heartbeat(&hb, &c, &env(&root), None, None, false, &NO_STOP)
                });
                assert_eq!(
                    st.last_error.as_deref(),
                    Some(error.as_str()),
                    "{update_url}"
                );
                assert!(
                    lines.iter().any(|(_, line)| line.contains(&error)),
                    "{update_url}: {lines:?}"
                );
                for (_, line) in &lines {
                    assert!(!leaks(line), "{line}");
                }
            }
        }
        assert!(!dest.exists());
        assert_eq!(current_name(&root), "0.4.0");
    }

    // --- root in the service user's tree ---------------------------------------------

    use std::cell::Cell;
    use std::rc::Rc;

    /// Renames `dir` away (to `<dir>.moved`) and puts a symlink to `target`
    /// in its place, as the service user may at any moment with anything in
    /// a directory it owns.
    fn swap_for_link(dir: &Path, target: &Path) {
        let mut away = dir.as_os_str().to_owned();
        away.push(".moved");
        fs::rename(dir, away).unwrap();
        std::os::unix::fs::symlink(target, dir).unwrap();
    }

    /// Download `hello-ota` into `<td>/update/a.tar.gz` from a server that
    /// swaps `update/` for a symlink to `victim` when the request comes, so
    /// after the download opened the directory. Returns where it landed.
    fn download_while_update_is_swapped(td: &Path, victim: &Path) -> PathBuf {
        let update = td.join("update");
        let (swapped, target) = (update.clone(), victim.to_path_buf());
        let srv = http_stub::serve(move |_, s| {
            swap_for_link(&swapped, &target);
            respond(s, 200, &[], b"hello-ota")
        });
        let sha = hex(&Sha256::digest(b"hello-ota"));
        let url = format!("{}/a.tar.gz", srv.base_url);
        http_download_to_file(&url, &update.join("a.tar.gz"), &sha, &NO_STOP).unwrap();
        td.join("update.moved/a.tar.gz")
    }

    /// The download goes to the `update/` it opened, `.part` file and
    /// artifact alike: one swapped for a symlink meanwhile takes neither
    /// into the link's target. (Made and renamed by path, both went there.)
    #[test]
    fn a_download_stays_in_the_update_dir_it_opened() {
        let td = tempfile::tempdir().unwrap();
        let victim = tempfile::tempdir().unwrap();
        subdir(td.path(), "update");
        let landed = download_while_update_is_swapped(td.path(), victim.path());
        assert_eq!(fs::read(&landed).unwrap(), b"hello-ota");
        assert_eq!(names(landed.parent().unwrap()), ["a.tar.gz"]);
        assert_eq!(names(victim.path()), Vec::<String>::new());
    }

    /// Once installed, the artifact is removed from the `update/` it was
    /// downloaded in, relative to it: an `update/` swapped for a symlink
    /// meanwhile takes the removal nowhere else, and a file of that name in
    /// the link's target stays. (Removed by path, that file went, and the
    /// artifact stayed.)
    #[test]
    fn the_artifact_is_removed_from_the_update_dir_it_opened() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let victim = tempfile::tempdir().unwrap();
        let theirs = victim.path().join("vesyl-print-0.5.0.tar.gz");
        fs::write(&theirs, b"not the artifact").unwrap();
        let tarball = build_release(&td.path().join("0.5.0"), "0.5.0");
        let served = fs::read(&tarball).unwrap();
        let (update, target) = (root.join("update"), victim.path().to_path_buf());
        let srv = http_stub::serve(move |_, s| {
            swap_for_link(&update, &target);
            respond(s, 200, &[], &served)
        });
        let m = ReleaseManifest::from_dict(&obj(json!({
            "version": "0.5.0",
            "artifact_url": format!("{}/vesyl-print-0.5.0.tar.gz", srv.base_url),
            "artifact_sha256": sha256_file(&tarball).unwrap(),
        })))
        .unwrap();
        apply_release(&m, &env(&root), None, false, &NO_STOP).unwrap();
        assert_eq!(current_name(&root), "0.5.0");
        assert_eq!(names(&root.join("update.moved")), Vec::<String>::new());
        assert_eq!(fs::read(&theirs).unwrap(), b"not the artifact");
    }

    /// [`download_while_update_is_swapped`] as root, in the service user's
    /// `update/`, its symlink to a root-owned directory: nothing lands
    /// there, and the artifact is the service user's. Needs root (or a user
    /// namespace).
    #[test]
    #[ignore = "needs root (or a user namespace) to chown"]
    fn root_download_never_follows_a_swapped_update_dir() {
        if euid() != 0 {
            return;
        }
        let td = tempfile::tempdir().unwrap();
        let victim = tempfile::tempdir().unwrap();
        let update = subdir(td.path(), "update");
        for dir in [td.path(), &update] {
            std::os::unix::fs::chown(dir, Some(1000), Some(1000)).unwrap();
        }
        let landed = download_while_update_is_swapped(td.path(), victim.path());
        assert_eq!(fs::read(&landed).unwrap(), b"hello-ota");
        let meta = fs::symlink_metadata(&landed).unwrap();
        assert_eq!((meta.uid(), meta.gid()), (1000, 1000));
        assert_eq!(names(victim.path()), Vec::<String>::new());
        assert_eq!(fs::metadata(victim.path()).unwrap().uid(), 0);
    }

    /// Run `f` with `hook` called at the step of root's unpack `at` names
    /// ([`AFTER_EACH_MEMBER`], [`AFTER_UNPACK_DIR_MADE`]).
    fn with_hook<T>(
        at: &'static std::thread::LocalKey<TestHook>,
        hook: impl Fn() + 'static,
        f: impl FnOnce() -> T,
    ) -> T {
        at.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
        let out = f();
        at.with(|h| *h.borrow_mut() = None);
        out
    }

    /// A hook for [`AFTER_EACH_MEMBER`]: on its first call it notes the
    /// owner and mode of `unpack` (the directory root unpacks in), then
    /// swaps it for a symlink to `victim`, as the service user could.
    fn swap_unpack_dir_once(
        unpack: &Path,
        victim: &Path,
        seen: &Rc<Cell<Option<(u32, u32)>>>,
    ) -> impl Fn() + 'static {
        let (unpack, victim, seen) = (unpack.to_path_buf(), victim.to_path_buf(), seen.clone());
        move || {
            if seen.get().is_none() {
                let meta = fs::symlink_metadata(&unpack).unwrap();
                seen.set(Some((meta.uid(), meta.mode() & 0o7777)));
                swap_for_link(&unpack, &victim);
            }
        }
    }

    /// A release tarball in `dir` holding only `files` (path, executable),
    /// without a single top-level directory.
    fn flat_tarball(dir: &Path, files: &[(&str, bool)]) -> PathBuf {
        let tarball = dir.join("flat.tar.gz");
        evil_tarball(&tarball, |tar| {
            for (path, exec) in files {
                let mut h = tar::Header::new_gnu();
                h.set_size(4);
                h.set_mode(if *exec { 0o755 } else { 0o644 });
                tar.append_data(&mut h, path, &b"\x7fELF"[..]).unwrap();
            }
        });
        tarball
    }

    /// Root unpacks in `releases/`, where the service user may rename
    /// anything: into a directory of its own, 0700, and through its
    /// descriptor. Swapped for a symlink after the first member, that
    /// directory takes no member to the link's target, and the release is
    /// staged whole, whether the archive has a top-level directory or not.
    /// (Unpacked by path into a directory the service user owned, a flat
    /// release stayed in the link's target.)
    #[test]
    fn unpacking_as_root_takes_no_write_elsewhere() {
        let files = [
            ("vesyl-print", true),
            ("lib/a.py", false),
            ("lib/b.py", false),
            ("main.py", false),
        ];
        let td = tempfile::tempdir().unwrap();
        for (tarball, top) in [
            (
                tarball_with(&subdir(td.path(), "src"), "0.5.0", &files),
                Some("vesyl-print-0.5.0"),
            ),
            (flat_tarball(&subdir(td.path(), "flat"), &files), None),
        ] {
            let victim = tempfile::tempdir().unwrap();
            let install = tempfile::tempdir().unwrap();
            let releases = install.path();
            let seen = Rc::new(Cell::new(None));
            let hook = swap_unpack_dir_once(&releases.join(".0.5.0.unpack"), victim.path(), &seen);
            let staged = with_hook(&AFTER_EACH_MEMBER, hook, || {
                Staged::unpack_as(
                    &File::open(&tarball).unwrap(),
                    &releases.join("0.5.0"),
                    true,
                )
            })
            .unwrap();
            assert_eq!(seen.get(), Some((euid(), 0o700)), "{top:?}");
            assert_eq!(names(victim.path()), Vec::<String>::new(), "{top:?}");
            assert_eq!(staged.dir, releases.join("0.5.0.staging"));
            assert_eq!(staged.top.as_deref(), top);
            for (file, _) in files {
                assert!(staged.dir.join(file).is_file(), "{top:?}: {file}");
            }
            staged.put_in_place(&releases.join("0.5.0"), false).unwrap();
            assert!(slot_is_runnable(&releases.join("0.5.0")), "{top:?}");
        }
    }

    /// An archive without a single top-level directory is the release as a
    /// whole, unpacked as root or not: the staging dir holds its members,
    /// with the mode `create_dir_all` gives (not root's 0700), as root less
    /// write for group and others.
    #[test]
    fn a_flat_archive_is_staged_whole() {
        let td = tempfile::tempdir().unwrap();
        let tarball = flat_tarball(td.path(), &[("vesyl-print", true), ("main.py", false)]);
        let mut modes = Vec::new();
        for as_root in [false, true] {
            let releases = subdir(td.path(), &format!("releases-{as_root}"));
            let staged = Staged::unpack_as(
                &File::open(&tarball).unwrap(),
                &releases.join("0.5.0"),
                as_root,
            )
            .unwrap();
            assert_eq!(staged.top, None, "{as_root}");
            assert_eq!(names(&staged.dir), ["main.py", "vesyl-print"], "{as_root}");
            assert!(slot_is_runnable(&staged.dir), "{as_root}");
            modes.push(fs::metadata(&staged.dir).unwrap().mode() & 0o7777);
            assert_eq!(names(&releases), ["0.5.0.staging"], "{as_root}");
        }
        assert_eq!(modes[1], modes[0] & !0o022);
    }

    /// As root, no mode the archive gives lets others write in the release
    /// before it is handed over: the service user could change it while
    /// root still works on it. The agent keeps the archive's modes.
    #[test]
    fn unpacking_as_root_lets_no_one_else_write() {
        let td = tempfile::tempdir().unwrap();
        let tarball = td.path().join("open.tar.gz");
        evil_tarball(&tarball, |tar| {
            for (path, mode) in [
                ("vesyl-print-0.5.0/", 0o777),
                ("vesyl-print-0.5.0/lib/", 0o777),
                ("vesyl-print-0.5.0/vesyl-print", 0o777),
                ("vesyl-print-0.5.0/lib/a.py", 0o666),
            ] {
                let mut h = tar::Header::new_gnu();
                h.set_mode(mode);
                if path.ends_with('/') {
                    h.set_entry_type(tar::EntryType::Directory);
                    h.set_size(0);
                    tar.append_data(&mut h, path, std::io::empty()).unwrap();
                } else {
                    h.set_size(4);
                    tar.append_data(&mut h, path, &b"\x7fELF"[..]).unwrap();
                }
            }
        });
        for (as_root, others_write) in [(true, false), (false, true)] {
            let install = tempfile::tempdir().unwrap();
            let slot = install.path().join("0.5.0");
            let staged = Staged::unpack_as(&File::open(&tarball).unwrap(), &slot, as_root).unwrap();
            for member in ["", "lib", "vesyl-print", "lib/a.py"] {
                let mode = fs::metadata(staged.dir.join(member)).unwrap().mode();
                assert_eq!(
                    mode & 0o022 != 0,
                    others_write,
                    "{as_root} {member:?}: {mode:o}"
                );
            }
            assert!(slot_is_runnable(&staged.dir), "{as_root}");
        }
    }

    /// Set in the child process of
    /// [`unpacking_as_root_lets_no_one_else_write_whatever_the_umask`].
    const UMASK_CHILD: &str = "VESYL_TEST_UMASK_CHILD";

    /// [`unpacking_as_root_lets_no_one_else_write`] whatever the umask of
    /// the shell that ran `sudo vesyl-print update apply`: what tar makes
    /// with no mode from the archive (the directory a flat release is in, a
    /// directory only a member's path implies, a member whose mode does not
    /// parse) lets no one else write either. In a child process, as the
    /// umask is the whole process's.
    #[test]
    fn unpacking_as_root_lets_no_one_else_write_whatever_the_umask() {
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "update::tests::umask_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(UMASK_CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "child failed: {stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    #[ignore = "child process of unpacking_as_root_lets_no_one_else_write_whatever_the_umask"]
    fn umask_child() {
        if std::env::var_os(UMASK_CHILD).is_none() {
            return;
        }
        // SAFETY: umask has no preconditions; this process runs this test
        // alone.
        unsafe { libc::umask(0) };
        let td = tempfile::tempdir().unwrap();
        fs::create_dir(td.path().join("probe")).unwrap();
        let probe = fs::metadata(td.path().join("probe")).unwrap().mode() & 0o777;
        assert_eq!(probe, 0o777, "umask 0 in effect");

        // No directory entries: the members' paths imply every directory.
        let files = [
            ("vesyl-print", true),
            ("lib/a.py", false),
            ("lib/web/b.py", false),
        ];
        let garbled = td.path().join("garbled.tar.gz");
        evil_tarball(&garbled, |tar| {
            let mut h = tar::Header::new_gnu();
            h.set_size(4);
            h.set_mode(0o755);
            tar.append_data(&mut h, "vesyl-print-0.5.0/vesyl-print", &b"\x7fELF"[..])
                .unwrap();
            let mut h = tar::Header::new_gnu();
            h.set_size(4);
            h.as_old_mut().mode = *b"garbled\0";
            tar.append_data(&mut h, "vesyl-print-0.5.0/main.py", &b"\x7fELF"[..])
                .unwrap();
        });
        for tarball in [
            tarball_with(&subdir(td.path(), "src"), "0.5.0", &files),
            flat_tarball(&subdir(td.path(), "flat"), &files),
            garbled,
        ] {
            let install = tempfile::tempdir().unwrap();
            let staged = Staged::unpack_as(
                &File::open(&tarball).unwrap(),
                &install.path().join("0.5.0"),
                true,
            )
            .unwrap();
            assert!(slot_is_runnable(&staged.dir), "{}", tarball.display());
            assert_eq!(
                others_may_write(&staged.dir),
                Vec::<PathBuf>::new(),
                "{}",
                tarball.display()
            );
        }
    }

    /// What in `dir` (itself included) group or others may write, at any
    /// depth; symlinks aside, whose mode means nothing.
    fn others_may_write(dir: &Path) -> Vec<PathBuf> {
        let mut open = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(path) = stack.pop() {
            let meta = fs::symlink_metadata(&path).unwrap();
            if !meta.file_type().is_symlink() && meta.mode() & 0o022 != 0 {
                open.push(path.clone());
            }
            if meta.is_dir() {
                stack.extend(fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
            }
        }
        open
    }

    /// Root refuses a hard link in an archive: tar finds its target by
    /// name, through `releases/`. The agent unpacks one as before.
    #[test]
    fn unpacking_as_root_refuses_a_hard_link() {
        let td = tempfile::tempdir().unwrap();
        let tarball = td.path().join("linked.tar.gz");
        evil_tarball(&tarball, |tar| {
            let mut h = tar::Header::new_gnu();
            h.set_size(4);
            h.set_mode(0o755);
            tar.append_data(&mut h, "vesyl-print-0.5.0/vesyl-print", &b"\x7fELF"[..])
                .unwrap();
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Link);
            h.set_size(0);
            h.set_mode(0o755);
            tar.append_link(
                &mut h,
                "vesyl-print-0.5.0/agent",
                "vesyl-print-0.5.0/vesyl-print",
            )
            .unwrap();
        });
        let releases = subdir(td.path(), "releases");
        let unpack = |as_root| {
            Staged::unpack_as(
                &File::open(&tarball).unwrap(),
                &releases.join("0.5.0"),
                as_root,
            )
        };
        let err = unpack(true).unwrap_err();
        assert_eq!(
            (err.code, err.message.as_str()),
            (
                "bad_archive",
                "refusing hard link in archive: vesyl-print-0.5.0/agent"
            )
        );
        assert_eq!(names(&releases), Vec::<String>::new());
        let staged = unpack(false).unwrap();
        assert!(staged.dir.join("agent").is_file());
    }

    /// Root's unpack of a `0.5.0` release into `releases`, while the
    /// service user (it owns `releases/`) renames the `.0.5.0.unpack` root
    /// just made away, to `.0.5.0.unpack.moved`, and `plant`s a directory
    /// of its own at that name, before root opens it. The unpack must be
    /// refused before any member is unpacked, in that directory or any
    /// other: in one not root's alone, that user could rearrange the tree
    /// under root's unpack.
    fn refuses_a_dir_put_in_place_of_its_own(releases: &Path, plant: impl Fn(&Path) + 'static) {
        let td = tempfile::tempdir().unwrap();
        let tarball = build_release(td.path(), "0.5.0");
        let unpack = releases.join(".0.5.0.unpack");
        let planted = unpack.clone();
        let swap = move || {
            let mut away = planted.as_os_str().to_owned();
            away.push(".moved");
            fs::rename(&planted, away).unwrap();
            plant(&planted);
        };
        let unpacked_any = Rc::new(Cell::new(false));
        let noted = unpacked_any.clone();
        let result = with_hook(&AFTER_UNPACK_DIR_MADE, swap, || {
            with_hook(
                &AFTER_EACH_MEMBER,
                move || noted.set(true),
                || {
                    Staged::unpack_as(
                        &File::open(&tarball).unwrap(),
                        &releases.join("0.5.0"),
                        true,
                    )
                },
            )
        });
        let err = result.unwrap_err();
        assert_eq!(
            (err.code, err.message),
            (
                INSTALL_FAILED,
                format!("{} is not the directory just made", unpack.display())
            )
        );
        assert!(!unpacked_any.get());
        // Root's own, empty; no staging dir. (The planted one is cleared.)
        assert_eq!(names(releases), [".0.5.0.unpack.moved"]);
        assert_eq!(
            names(&releases.join(".0.5.0.unpack.moved")),
            Vec::<String>::new()
        );
    }

    /// [`refuses_a_dir_put_in_place_of_its_own`], the directory put there
    /// open to others.
    #[test]
    fn unpacking_as_root_refuses_a_dir_put_in_its_place() {
        let td = tempfile::tempdir().unwrap();
        refuses_a_dir_put_in_place_of_its_own(&subdir(td.path(), "releases"), |at| {
            fs::create_dir(at).unwrap();
            crate::util::set_mode(at, 0o755).unwrap();
        });
    }

    /// [`refuses_a_dir_put_in_place_of_its_own`] as root, the directory put
    /// there the service user's, and closed to others as root's own is.
    /// Needs root (or a user namespace).
    #[test]
    #[ignore = "needs root (or a user namespace) to chown"]
    fn root_refuses_the_service_users_dir_in_place_of_its_own() {
        if euid() != 0 {
            return;
        }
        let td = tempfile::tempdir().unwrap();
        let releases = subdir(td.path(), "releases");
        std::os::unix::fs::chown(&releases, Some(1000), Some(1000)).unwrap();
        refuses_a_dir_put_in_place_of_its_own(&releases, |at| {
            fs::create_dir(at).unwrap();
            crate::util::set_mode(at, 0o700).unwrap();
            std::os::unix::fs::chown(at, Some(1000), Some(1000)).unwrap();
        });
    }

    /// As root, in the service user's `releases/`: the release is root's
    /// until it is in place, then the owner of `releases/` gets it all. The
    /// directory root unpacks in is root's and 0700, and swapping it for a
    /// symlink to a root-owned directory midway takes nothing there; a
    /// reinstall still swaps the slot in. Needs root (or a user namespace).
    #[test]
    #[ignore = "needs root (or a user namespace) to chown"]
    fn root_keeps_a_release_its_own_until_it_is_in_place() {
        if euid() != 0 {
            return;
        }
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        // As setup.sh leaves it: the install tree is the service user's.
        std::os::unix::fs::chown(td.path(), Some(1000), Some(1000)).unwrap();
        crate::util::hand_tree_to_parent_owner(&root).unwrap();
        let releases = root.join("releases");

        let tarball = build_release(&td.path().join("staged"), "0.5.0");
        let staged =
            Staged::unpack(&File::open(&tarball).unwrap(), &releases.join("0.5.0")).unwrap();
        assert_eq!(not_owned_by(&staged.dir, 0), Vec::<PathBuf>::new());
        staged.put_in_place(&releases.join("0.5.0"), false).unwrap();
        assert_eq!(not_owned_by(&releases, 1000), Vec::<PathBuf>::new());

        let victim = tempfile::tempdir().unwrap();
        let seen = Rc::new(Cell::new(None));
        for version in ["0.5.1", "0.5.1"] {
            let tarball = build_release(&td.path().join(version), version);
            let m = manifest_for(&tarball, version);
            let unpack = releases.join(format!(".{version}.unpack"));
            seen.set(None);
            let hook = swap_unpack_dir_once(&unpack, victim.path(), &seen);
            with_hook(&AFTER_EACH_MEMBER, hook, || {
                apply_local_release(&m, &env(&root), &tarball, None, false)
            })
            .unwrap();
            assert_eq!(seen.get(), Some((0, 0o700)));
            assert_eq!(names(victim.path()), Vec::<String>::new());
            assert_eq!(fs::metadata(victim.path()).unwrap().uid(), 0);
            assert_eq!(current_name(&root), version);
            let slot = releases.join(version);
            assert!(slot_is_runnable(&slot));
            assert_eq!(fs::read_to_string(slot.join("VERSION")).unwrap(), "0.5.1\n");
            assert_eq!(not_owned_by(&slot, 1000), Vec::<PathBuf>::new());
            // The service user's doing: its rename of root's directory.
            fs::remove_dir_all(releases.join(format!(".{version}.unpack.moved"))).unwrap();
            assert_eq!(names(&releases), ["0.3.0", "0.4.0", "0.5.0", "0.5.1"]);
        }
    }

    // --- logs ----------------------------------------------------------------------

    /// The agent's startup drain asks, before its first heartbeat runs,
    /// whether jobs wait for a gate that heartbeat reopens (an install cut
    /// off after its flip). It gets the heartbeat's answer for every status,
    /// and logs nothing: the promotion is logged once, by the heartbeat
    /// that writes it. (It was logged by both.)
    #[test]
    fn the_startup_decision_logs_no_promotion() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let (c, agent) = (cfg(td.path()), env(&root));
        let cut_off = UpdateStatus {
            status: STATUS_FAILED.into(),
            last_error: Some("update interrupted before it finished".into()),
            ..pending(String::new())
        };
        let statuses = [
            None,
            Some(cut_off.clone()),
            Some(UpdateStatus::with_status(STATUS_IDLE)),
            Some(UpdateStatus::with_status(STATUS_DOWNLOADING)),
            Some(pending(utc_now_plus(60))),
            Some(UpdateStatus::with_status(STATUS_ROLLED_BACK)),
            // Its gate failed it already; another version's; nothing to judge.
            Some(UpdateStatus {
                last_error: Some(
                    "health failed: timeout (no previous slot to roll back to)".into(),
                ),
                ..cut_off.clone()
            }),
            Some(UpdateStatus {
                target_version: Some("0.3.0".into()),
                ..cut_off.clone()
            }),
            Some(UpdateStatus {
                target_version: None,
                ..cut_off.clone()
            }),
        ];
        for st in statuses {
            let (paused, lines) =
                logs_during(|| should_pause_jobs_once_recovered(st.as_ref(), &agent));
            let recovered = st
                .clone()
                .map(|st| recover_false_update_failure(st, &c, &agent));
            assert_eq!(paused, should_pause_jobs(recovered.as_ref()), "{st:?}");
            assert_eq!(lines, [], "{st:?}");
        }
        assert!(should_pause_jobs_once_recovered(Some(&cut_off), &agent));
        let (promoted, lines) = logs_during(|| recover_false_update_failure(cut_off, &c, &agent));
        assert_eq!(promoted.status, STATUS_PENDING_HEALTH);
        assert_eq!(
            lines,
            [(
                log::Level::Info,
                "recovering sticky failed update status for 0.4.0 (slot healthy) → pending_health"
                    .to_string()
            )]
        );
    }

    /// What a heartbeat logs about an update it leaves alone goes to info
    /// when it is news: on a process's first heartbeat (a thread's, here),
    /// when it changes (another desired version), or after a heartbeat that
    /// said nothing. While it repeats every 30 s, it goes to debug: with
    /// auto-update off, "update available" filled the journal at info.
    #[test]
    fn a_reason_to_leave_an_update_alone_is_news_once() {
        use log::Level::{Debug, Info};
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let off = Config {
            auto_update_enabled: false,
            ..cfg(td.path())
        };
        let heartbeat = |desired: Option<&str>| {
            let hb = obj(match desired {
                Some(v) => json!({"desired_agent_version": v}),
                None => json!({"ok": true}),
            });
            let agent = env(&root);
            logs_during(|| {
                maybe_update_from_heartbeat(&hb, &off, &agent, None, None, false, &NO_STOP)
            })
            .1
        };
        let available = |v: &str, level| {
            vec![(
                level,
                format!("update available: 0.4.0 → {v} (auto_update disabled)"),
            )]
        };
        assert_eq!(heartbeat(Some("0.5.0")), available("0.5.0", Info));
        assert_eq!(heartbeat(Some("0.5.0")), available("0.5.0", Debug));
        assert_eq!(heartbeat(Some("0.5.0")), available("0.5.0", Debug));
        assert_eq!(heartbeat(Some("0.5.1")), available("0.5.1", Info));
        assert_eq!(heartbeat(Some("0.5.1")), available("0.5.1", Debug));
        assert_eq!(heartbeat(None), []);
        assert_eq!(heartbeat(Some("0.5.1")), available("0.5.1", Info));
        // A restarted agent says it again.
        std::thread::scope(|s| {
            let restarted = s.spawn(|| heartbeat(Some("0.5.1"))).join().unwrap();
            assert_eq!(restarted, available("0.5.1", Info));
        });
        assert_eq!(heartbeat(Some("0.5.1")), available("0.5.1", Debug));

        // Every other reason to leave one alone, heartbeat after heartbeat.
        let on = unsigned_ok(td.path());
        let rolled_back = UpdateStatus {
            status: STATUS_ROLLED_BACK.into(),
            target_version: Some("0.5.0".into()),
            last_error: Some("health failed: timeout; rolled back to 0.4.0".into()),
            ..Default::default()
        };
        let missed = UpdateStatus {
            last_error: Some(format!(
                "{RESTART_MISSED} (still running 0.4.0); rolled back to 0.4.0"
            )),
            // Armed after this process started.
            armed_at: Some(Utc::now().to_rfc3339_opts(SecondsFormat::Micros, false)),
            ..rolled_back.clone()
        };
        let backing_off = UpdateStatus {
            status: STATUS_FAILED.into(),
            target_version: Some("0.5.0".into()),
            last_error: Some("HTTP 503 downloading artifact".into()),
            last_error_code: Some("download_failed".into()),
            attempts: 1,
            retry_at: Some(utc_now_plus(60)),
            ..Default::default()
        };
        let gate = UpdateStatus {
            target_version: Some("0.5.0".into()),
            ..pending(utc_now_plus(60))
        };
        let hb = obj(json!({"desired_agent_version": "0.5.0"}));
        for (st, jobs_busy, starts) in [
            (
                Some(rolled_back),
                false,
                "not re-applying 0.5.0 (rolled_back on this node",
            ),
            (
                Some(missed),
                false,
                "not re-applying 0.5.0 from this process: the restart into it never came",
            ),
            (Some(backing_off), false, "not retrying 0.5.0 before "),
            (
                Some(gate),
                false,
                "update deferred: pending_health for 0.5.0",
            ),
            (
                None,
                true,
                "update deferred: jobs in flight (0.4.0 → 0.5.0)",
            ),
        ] {
            let mut levels = Vec::new();
            for _ in 0..3 {
                let (_, lines) = logs_during(|| {
                    maybe_update_from_heartbeat(
                        &hb,
                        &on,
                        &env(&root),
                        st.clone(),
                        None,
                        jobs_busy,
                        &NO_STOP,
                    )
                });
                let [(level, line)] = lines.as_slice() else {
                    panic!("{starts}: {lines:?}");
                };
                assert!(line.starts_with(starts), "{line}");
                levels.push(*level);
            }
            assert_eq!(levels, [Info, Debug, Debug], "{starts}");
        }

        // The agent the gate replaces, waiting for its restart.
        let gate = armed_gate(td.path(), &on);
        let old = slot_agent(&root, "0.3.0");
        let mut levels = Vec::new();
        for _ in 0..3 {
            let (out, lines) = logs_during(|| {
                judge_pending_health(
                    gate.clone(),
                    &on,
                    &old,
                    WhoamiResult::Ok,
                    None,
                    None,
                    started(-60),
                    &NO_STOP,
                )
            });
            assert_eq!(out, gate);
            assert_eq!(
                lines
                    .iter()
                    .map(|(_, line)| line.as_str())
                    .collect::<Vec<_>>(),
                ["pending_health for 0.4.0: waiting for the restart (running 0.3.0)"]
            );
            levels.push(lines[0].0);
        }
        assert_eq!(levels, [Info, Debug, Debug]);
    }
}
