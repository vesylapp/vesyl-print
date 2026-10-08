//! App OTA: download, verify, atomic install, rollback.
//!
//! Production layout:
//!
//! ```text
//! /opt/vesyl-print/
//!   current -> releases/0.4.0
//!   releases/0.3.0/
//!   releases/0.4.0/
//!   update/                  # staging
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

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
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
use crate::util::{opt_str, py_int, py_str, truthy, write_durable};
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

/// [`extract_tarball`] unpacks into `<slot>` + this, beside the slot.
const STAGING_SUFFIX: &str = ".staging";

/// Installed root-owned helper (NOPASSWD sudoers on appliances).
const APPLY_HELPER: &str = "/usr/local/lib/vesyl-print/apply-update";

fn version_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\d+\.\d+\.\d+([.-][0-9A-Za-z.]+)?$").expect("regex"))
}

/// A release version: the pattern `scripts/apply-update` (and
/// build-release.sh, setup.sh) checks, except that a last dot-component of
/// `staging` is refused. `<version>.staging` is the directory an extract
/// leaves beside its slot if it dies midway, so such a name never counts as
/// a release ([`list_releases`], `current`), and no manifest can name a slot
/// that is another version's staging dir.
pub fn is_version(s: &str) -> bool {
    version_re().is_match(s) && !s.ends_with(STAGING_SUFFIX)
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
        if let Some(armed) = &self.armed_at {
            d.insert("armed_at".into(), armed.clone().into());
        }
        d
    }

    fn is(&self, status: &str) -> bool {
        self.status == status
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

/// Ordering of two semver-ish version strings (numeric, missing parts = 0).
pub fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut ta, mut tb) = (parse_version(a), parse_version(b));
    let n = ta.len().max(tb.len());
    ta.resize(n, 0);
    tb.resize(n, 0);
    ta.cmp(&tb)
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
    let mut f = File::open(path)?;
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

/// Open a URL for streaming. `file://` is supported for lab installs/tests.
///
/// HTTP goes through [`net::agent`]: urllib's timeouts (every read waits at
/// most `timeouts.idle`, so a dead connection fails while a slow but steady
/// download is not cut off at a fixed deadline), urllib's proxy rules chosen
/// again on every redirect hop, and no transparent decompression, so the
/// SHA-256 always covers the bytes the server sent.
fn open_url(
    url: &str,
    timeouts: Timeouts,
    what: &str,
) -> Result<Box<dyn Read + Send>, UpdateError> {
    if let Some(path) = url::Url::parse(url)
        .ok()
        .filter(|u| u.scheme() == "file")
        .and_then(|u| u.to_file_path().ok())
    {
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
        .map_err(|e| UpdateError::new(format!("network error: {e}"), "download_failed"))?;
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
    open_url(url, Timeouts::MANIFEST, &format!("fetching {url}"))?
        .read_to_end(&mut out)
        .map_err(|e| UpdateError::new(format!("network error: {e}"), "download_failed"))?;
    Ok(out)
}

/// Stream `url` to `dest` via `dest.part`, verifying SHA-256.
pub fn http_download_to_file(
    url: &str,
    dest: &Path,
    expected_sha256: &str,
) -> Result<(), UpdateError> {
    if let Some(parent) = dest.parent() {
        crate::util::create_dir_all_owned(parent).map_err(io_err("download_failed"))?;
    }
    let tmp = PathBuf::from(format!("{}.part", dest.display()));
    let result = (|| {
        let mut reader = open_url(url, Timeouts::ARTIFACT, "downloading artifact")?;
        let mut out = fresh_part_file(&tmp).map_err(io_err("download_failed"))?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
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
        fs::rename(&tmp, dest).map_err(io_err("download_failed"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// A new, empty download file at `path`. One an earlier download left is
/// unlinked, never reopened: a crashed `sudo vesyl-print update apply`
/// leaves it root's, which the agent could not open for writing again (it
/// owns `update/`, so it can unlink it), and opening a symlink planted there
/// would write through it. Root hands the new file to the directory's owner.
fn fresh_part_file(path: &Path) -> std::io::Result<File> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let file = File::options().write(true).create_new(true).open(path)?;
    if let Some(dir) = path.parent() {
        crate::util::hand_new_file_to_dir_owner(&file, dir);
    }
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

/// Remove `dir` — a release slot about to be unpacked again, or a staging
/// dir an extract left behind — so it can be unpacked afresh. One the agent
/// cannot delete, because root unpacked it (an `update apply` run as root
/// before slots were handed to the install owner, or one that died
/// midway), is renamed aside to `.<name>.stale-<n>` in the same directory
/// instead. That needs write access to that directory only, and the service
/// user owns `releases/`. [`clean_stale`] deletes such leftovers once an
/// install can. A missing `dir` is fine.
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
            "bad_archive",
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
    Staged::unpack(tarball, dest_dir)?.put_in_place(dest_dir)
}

/// A release unpacked into `<slot>.staging`, beside its slot, and not yet in
/// place: an install checks it there, before it touches the slot itself.
struct Staged {
    /// The staging dir.
    dir: PathBuf,
    /// The release in it: the archive's single top-level directory, else
    /// the staging dir itself.
    tree: PathBuf,
}

impl Staged {
    /// Unpack `tarball` into a fresh staging dir for the slot `dest_dir`.
    /// Rejects absolute paths, `..`, and links that point outside the archive.
    fn unpack(tarball: &Path, dest_dir: &Path) -> Result<Staged, UpdateError> {
        let parent = dest_dir.parent().unwrap_or(Path::new("."));
        crate::util::create_dir_all_owned(parent).map_err(io_err("bad_archive"))?;
        clean_stale(parent);
        let mut staging = dest_dir.as_os_str().to_owned();
        staging.push(STAGING_SUFFIX);
        let staging = PathBuf::from(staging);
        // Left by an extract that died midway, perhaps one run as root.
        clear_release_dir(&staging)?;
        // Root hands it to the owner of `releases/`, so the agent can clear
        // whatever a crash leaves in it.
        crate::util::create_dir_all_owned(&staging).map_err(io_err("bad_archive"))?;

        let open = || -> Result<tar::Archive<flate2::read::GzDecoder<File>>, UpdateError> {
            let f = File::open(tarball)
                .map_err(|e| UpdateError::new(format!("extract failed: {e}"), "bad_archive"))?;
            Ok(tar::Archive::new(flate2::read::GzDecoder::new(f)))
        };
        let extract = || -> Result<Vec<fs::DirEntry>, UpdateError> {
            let fail =
                |e: std::io::Error| UpdateError::new(format!("extract failed: {e}"), "bad_archive");
            // Pass 1: validate every member before writing anything.
            for entry in open()?.entries().map_err(fail)? {
                let entry = entry.map_err(fail)?;
                let path = entry.path().map_err(fail)?.into_owned();
                let link = entry.link_name().map_err(fail)?.map(|l| l.into_owned());
                if unsafe_archive_path(&path) || link.as_deref().is_some_and(unsafe_archive_path) {
                    return Err(UpdateError::new(
                        format!("refusing unsafe path in archive: {}", path.display()),
                        "bad_archive",
                    ));
                }
            }
            // Pass 2: unpack.
            let mut archive = open()?;
            archive.set_preserve_permissions(false);
            archive.unpack(&staging).map_err(fail)?;
            Ok(fs::read_dir(&staging).map_err(fail)?.flatten().collect())
        };
        let children = match extract() {
            Ok(children) => children,
            Err(e) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(e);
            }
        };
        // If archive has a single top-level dir, peel it. Never a symlink:
        // moved to where the slot goes, its target would resolve elsewhere.
        let tree = match children.as_slice() {
            [only] if only.file_type().is_ok_and(|t| t.is_dir()) => only.path(),
            _ => staging.clone(),
        };
        Ok(Staged { dir: staging, tree })
    }

    /// Move the release to `dest_dir`, which must not exist, and remove the
    /// staging dir. An operator running `update apply` as root must not leave
    /// a root-owned slot the non-root agent can never replace or remove, so
    /// the slot, `VERSION` and all, goes to the owner of `releases/`.
    fn put_in_place(self, dest_dir: &Path) -> Result<(), UpdateError> {
        if let Err(e) = fs::rename(&self.tree, dest_dir) {
            self.discard();
            return Err(UpdateError::new(e.to_string(), "bad_archive"));
        }
        if self.tree != self.dir {
            let _ = fs::remove_dir_all(&self.dir);
        }
        crate::util::hand_tree_to_parent_owner(dest_dir).map_err(io_err("bad_archive"))
    }

    fn discard(self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
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

/// What runs the helper: `sudo -n` (NOPASSWD on appliances). Unit tests run
/// a stand-in helper script with `sh` instead, so no test ever runs sudo.
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
pub fn restart_commands(helper: Option<&Path>) -> Vec<Vec<String>> {
    match helper.filter(|h| h.is_file()) {
        Some(h) => vec![vec![
            "sudo".into(),
            "-n".into(),
            h.display().to_string(),
            "restart".into(),
        ]],
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
    /// here instead of run (see `tests::restarts_during`).
    static RESTARTS_SEEN: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// Restart display + agent. Best-effort and non-blocking: when the agent
/// restarts *itself*, systemd SIGTERMs this process while the helper is still
/// running, so launch it in its own process group and never wait on it.
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
        let spawned = Command::new(&argv[0])
            .args(&argv[1..])
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            // Reap in the background so it doesn't linger as a zombie.
            Ok(mut child) => {
                std::thread::spawn(move || child.wait());
            }
            Err(e) => log::warn!(target: LOG, "restart {:?} failed: {e}", argv.last()),
        }
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
/// Does **not** run the post-update health gate or restart services.
pub fn apply_release(
    manifest: &ReleaseManifest,
    env: &UpdateEnv,
    public_key_pem: Option<&str>,
    require_signature: bool,
) -> Result<PathBuf, UpdateError> {
    check_manifest(manifest, env, public_key_pem, require_signature)?;

    let update_dir = env.install_root.join("update");
    crate::util::create_dir_all_owned(&update_dir).map_err(io_err("download_failed"))?;
    let tarball = update_dir.join(format!("vesyl-print-{}.tar.gz", manifest.version));

    log::info!(target: LOG, "downloading {}", manifest.artifact_url);
    http_download_to_file(&manifest.artifact_url, &tarball, &manifest.artifact_sha256)?;

    let release_dir = install_release(manifest, env, &tarball)?;
    let _ = fs::remove_file(&tarball);
    Ok(release_dir)
}

/// [`apply_release`] for an artifact already on disk (`update apply
/// --file`): the same checks, install and activation, with `tarball`
/// checked against the manifest's SHA-256 instead of downloaded.
pub fn apply_local_release(
    manifest: &ReleaseManifest,
    env: &UpdateEnv,
    tarball: &Path,
    public_key_pem: Option<&str>,
    require_signature: bool,
) -> Result<PathBuf, UpdateError> {
    check_manifest(manifest, env, public_key_pem, require_signature)?;
    let sha = sha256_file(tarball).map_err(|e| {
        UpdateError::new(
            format!("cannot read {}: {e}", tarball.display()),
            "bad_archive",
        )
    })?;
    if sha != manifest.artifact_sha256 {
        return Err(UpdateError::new(
            format!(
                "sha256 mismatch: file={sha} manifest={}",
                manifest.artifact_sha256
            ),
            "bad_checksum",
        ));
    }
    install_release(manifest, env, tarball)
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
        if version_cmp(&env.running_version, min).is_lt() {
            return Err(UpdateError::new(
                format!("current {} < min_agent_version {min}", env.running_version),
                "too_old",
            ));
        }
    }
    verify_manifest(manifest, public_key_pem, require_signature)
}

/// Install the verified `tarball` as the slot for `manifest.version`, then
/// [`activate`] it. The archive is unpacked into a staging dir beside the
/// slot and must hold a runnable slot ([`slot_is_runnable`]) before its
/// `VERSION` is written and it replaces the slot: a bad archive leaves an
/// installed slot of the same version as it was.
fn install_release(
    manifest: &ReleaseManifest,
    env: &UpdateEnv,
    tarball: &Path,
) -> Result<PathBuf, UpdateError> {
    let root = &env.install_root;
    let release_dir = root.join("releases").join(&manifest.version);
    log::info!(target: LOG, "extracting to {}", release_dir.display());
    let staged = Staged::unpack(tarball, &release_dir)?;
    let checked = (|| {
        if !slot_is_runnable(&staged.tree) {
            return Err(UpdateError::new(
                "archive missing an executable vesyl-print binary",
                "bad_archive",
            ));
        }
        write_version_file(&staged.tree, &manifest.version).map_err(io_err("bad_archive"))?;
        // A slot of this version from an earlier install. If the agent cannot
        // delete it (root unpacked it), it is moved aside: failing here would
        // download the artifact again on every heartbeat, and never install.
        clear_release_dir(&release_dir)
    })();
    if let Err(e) = checked {
        staged.discard();
        return Err(e);
    }
    staged.put_in_place(&release_dir)?;

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
    // Whether it can run was checked when `current` was pointed here (the
    // helper, else `slot_is_runnable`); here, that it is still in place.
    if !cur.join(SLOT_BINARY).is_file() {
        return Err("current slot missing the vesyl-print binary".into());
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

/// If OTA activated successfully but status was marked failed (e.g. SIGTERM
/// during self-restart), promote back to `pending_health` so the gate runs.
pub fn recover_false_update_failure(
    mut st: UpdateStatus,
    cfg: &Config,
    env: &UpdateEnv,
) -> UpdateStatus {
    if !st.is(STATUS_FAILED) {
        return st;
    }
    let target = st
        .target_version
        .clone()
        .unwrap_or_default()
        .trim()
        .to_string();
    if target.is_empty() || !same_version(&env.running_version, &target) {
        return st;
    }
    if local_slot_healthy(env, Some(&target)).is_err() {
        return st;
    }
    log::info!(target: LOG, "recovering sticky failed update status for {target} (slot healthy) → pending_health");
    let (prev, channel) = (st.previous_version.clone(), st.channel.clone());
    mark_pending_health(&mut st, &target, prev, health_gate_seconds(cfg), channel);
    st
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
pub fn process_pending_health(
    st: UpdateStatus,
    cfg: &Config,
    env: &UpdateEnv,
    whoami: WhoamiResult,
    whoami_error: Option<&str>,
    now_iso: Option<&str>,
) -> UpdateStatus {
    judge_pending_health(
        st,
        cfg,
        env,
        whoami,
        whoami_error,
        now_iso,
        process_started_at(),
    )
}

/// [`process_pending_health`] for a process started at `started_at`.
fn judge_pending_health(
    st: UpdateStatus,
    cfg: &Config,
    env: &UpdateEnv,
    whoami: WhoamiResult,
    whoami_error: Option<&str>,
    now_iso: Option<&str>,
    started_at: Option<DateTime<Utc>>,
) -> UpdateStatus {
    let mut st = if st.is(STATUS_FAILED) {
        recover_false_update_failure(st, cfg, env)
    } else {
        st
    };
    if !st.is(STATUS_PENDING_HEALTH) {
        return st;
    }

    let now = now_iso.map(String::from).unwrap_or_else(utc_now);
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
        log::info!(
            target: LOG,
            "pending_health for {expected}: waiting for the restart (running {})",
            env.running_version
        );
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
        let mut st = close_failed_gate(st, env, &expected, &why);
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
    // Local slot broken (wrong version / missing entrypoints) → fail fast.
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
    close_failed_gate(st, env, &expected, &format!("{HEALTH_FAILED}: {reason}"))
}

/// Close a gate for `expected` that did not pass: roll back to
/// `previous_version` and restart the services, else mark it failed. `why`
/// leads `last_error`, which [`failed_health_gate`] reads.
fn close_failed_gate(
    mut st: UpdateStatus,
    env: &UpdateEnv,
    expected: &str,
    why: &str,
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
                if env.restart {
                    restart_services(env.apply_helper.as_deref());
                }
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

/// True when this status records that its target failed the health gate on
/// this node: rolled back (by the gate, or by hand while it was open), or
/// failed with no way to roll back. A rollback because the restart into the
/// target never happened is not one: that version never ran.
fn failed_health_gate(st: &UpdateStatus) -> bool {
    let err = st.last_error.as_deref().unwrap_or_default();
    (st.is(STATUS_ROLLED_BACK) && !err.starts_with(RESTART_MISSED))
        || (st.is(STATUS_FAILED) && err.starts_with(HEALTH_FAILED))
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

/// The status to start a heartbeat update from: `status` as the caller read
/// it earlier in its cycle, unless `status_path` now holds a `pending_health`
/// it does not have. That is a gate armed meanwhile by another process
/// (`update apply … --restart`); the caller writes the result back, so its
/// stale copy would disarm the gate the restarted agent needs.
fn current_status(status: Option<UpdateStatus>, status_path: Option<&Path>) -> UpdateStatus {
    let Some(armed) = status_path
        .and_then(read_update_status)
        .filter(|on_disk| on_disk.is(STATUS_PENDING_HEALTH))
    else {
        return status.unwrap_or_default();
    };
    if status.as_ref() != Some(&armed) {
        log::info!(
            target: LOG,
            "update status changed on disk: pending_health for {} (was {})",
            armed.target_version.as_deref().unwrap_or("?"),
            status.as_ref().map_or("none", |s| s.status.as_str())
        );
    }
    armed
}

/// Inspect a heartbeat response and optionally apply an update.
///
/// After a successful activate, status becomes `pending_health` (not idle);
/// the new process must call [`process_pending_health`] after restart. When
/// `jobs_busy`, download/install is deferred so slots never flip mid-print.
/// A version that already failed its health gate here is not re-applied for
/// the same desired version (see below), nor is one the services never
/// restarted into by the process that stayed (see `restart_missed_here`).
/// With `status_path`, a gate armed there since `status` was read wins over
/// `status` (see `current_status`).
pub fn maybe_update_from_heartbeat(
    hb: &JsonObject,
    cfg: &Config,
    env: &UpdateEnv,
    status: Option<UpdateStatus>,
    status_path: Option<&Path>,
    jobs_busy: bool,
) -> UpdateStatus {
    let mut st = current_status(status, status_path);
    st.current_version = env.running_version.clone();
    st.last_checked_at = Some(utc_now());

    // Never start another OTA while health gate is open.
    if st.is(STATUS_PENDING_HEALTH) {
        log::info!(
            target: LOG,
            "update deferred: pending_health for {}",
            st.target_version.as_deref().unwrap_or("?")
        );
        return st;
    }

    let desired = hb
        .get("desired_agent_version")
        .filter(|v| truthy(v))
        .or_else(|| hb.get("desired_version").filter(|v| truthy(v)))
        .map(|v| py_str(v).trim().to_string());
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
        if !sticky(&st) {
            st.status = STATUS_IDLE.into();
        }
        return st;
    }

    // Re-applying a version that failed its health gate here would loop
    // download → activate → restart → gate → rollback for as long as the
    // server asks for it. Hold until the desired version changes; a manual
    // `vesyl-print update apply` (which starts from a fresh status) still works.
    let held = failed_health_gate(&st);
    let missed_here = restart_missed_here(&st, process_started_at);
    let same_target = prev_target
        .as_deref()
        .is_some_and(|t| same_version(t, &desired));
    if held && same_target {
        log::info!(
            target: LOG,
            "not re-applying {desired}: it failed its health gate on this node ({}); \
             waiting for a different desired version or a manual update",
            st.status
        );
        return st;
    }
    if missed_here && same_target {
        log::info!(
            target: LOG,
            "not re-applying {desired} from this process: the restart into it never came; \
             the agent tries again once restarted"
        );
        return st;
    }
    // A heartbeat that defers `desired` keeps a status that blocks a retry
    // about the version it blocks. Recording `desired` in it would block
    // `desired`, a version this node has not tried, once updates resume.
    let defer = |mut st: UpdateStatus| {
        if held || missed_here {
            st.target_version = prev_target.clone();
        }
        st
    };

    if !cfg.auto_update_enabled {
        if !sticky(&st) {
            st.status = STATUS_IDLE.into();
        }
        log::info!(target: LOG, "update available: {} → {desired} (auto_update disabled)", st.current_version);
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
        log::info!(target: LOG, "update deferred: jobs in flight ({} → {desired})", st.current_version);
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
            st.status = STATUS_FAILED.into();
            st.last_error =
                Some("desired version set but no update_url or releases_base_url".into());
            log::warn!(target: LOG, "{}", st.last_error.as_deref().unwrap_or_default());
            return st;
        }
    };

    let root = &env.install_root;
    let previous = slot_before_activation(env);

    let persist = |st: &UpdateStatus| {
        if let Some(p) = status_path {
            if let Err(e) = write_update_status(p, st) {
                log::warn!(target: LOG, "write update status: {e}");
            }
        }
    };

    let result = (|| -> Result<(), UpdateError> {
        st.status = STATUS_DOWNLOADING.into();
        // The slot to roll back to, on disk before anything can flip
        // `current`: an install cut off after the flip (power loss) comes
        // back as a gate (see `recover_false_update_failure`), and that
        // gate must still be able to roll back.
        st.previous_version = previous.clone().filter(|p| !same_version(p, &desired));
        // Persist early so the LCD can show "Updating…" during the download.
        persist(&st);
        // With signatures required, an unreadable key fails here (closed).
        let pem = manifest_public_key(cfg)?;
        log::info!(target: LOG, "applying update {desired} from {manifest_url}");
        let manifest = fetch_manifest(&manifest_url)?;
        if !same_version(&manifest.version, &desired) {
            log::info!(target: LOG, "manifest version {} (desired {desired})", manifest.version);
        }
        let prev = previous
            .clone()
            .filter(|p| !same_version(p, &manifest.version));
        st.status = STATUS_INSTALLING.into();
        st.previous_version = prev.clone();
        // Persist installing so a crash mid-apply is visible.
        persist(&st);

        apply_release(&manifest, env, pem.as_deref(), cfg.update_require_signature)?;
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
        if env.restart {
            restart_services(env.apply_helper.as_deref());
        }
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
                if env.restart {
                    restart_services(env.apply_helper.as_deref());
                }
                return st;
            }
        }
        st.status = STATUS_FAILED.into();
        st.last_error = Some(e.message.clone());
        log::error!(target: LOG, "update failed: {}", e.message);
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
        assert_eq!(version_cmp("0.3.17-rc.1", "0.3.17"), Equal);
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
        let st =
            maybe_update_from_heartbeat(&hb, &cfg(td.path()), &env(td.path()), None, None, true);
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
        apply_release(&m, &env(&root), None, false).unwrap();
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
        let st = maybe_update_from_heartbeat(&hb, &c, &env(&root), None, Some(&status_path), false);
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
        let st = maybe_update_from_heartbeat(&hb, &cfg(td.path()), &env(&root), None, None, false);
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
        http_download_to_file(&url, &dest, &sha).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"hello-ota");
        let err =
            http_download_to_file(&url, &td.path().join("bad.bin"), &"0".repeat(64)).unwrap_err();
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
        http_download_to_file(&url, &dest, &sha).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"hello-ota");
        assert!(fs::symlink_metadata(&part).is_err());

        let victim = td.path().join("victim");
        fs::write(&victim, b"keep").unwrap();
        std::os::unix::fs::symlink(&victim, &part).unwrap();
        http_download_to_file(&url, &dest, &sha).unwrap();
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
        http_download_to_file(&url, &dest, &hex(&Sha256::digest(b"hello-ota"))).unwrap();
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
        );
        assert_eq!(out.status, STATUS_ROLLED_BACK);
        assert_eq!(current_name(&root), "0.3.0");
    }

    #[test]
    fn slot_needs_the_rust_binary() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        assert!(local_slot_healthy(&env(&root), Some("0.4.0")).is_ok());
        // The units exec <slot>/vesyl-print: the binary only under bin/ fails.
        let cur = fs::canonicalize(root.join("current")).unwrap();
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
        let err = apply_release(&m, &env(&root), None, false).unwrap_err();
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
                apply_release(&m, &env(&root), None, false).unwrap_err(),
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
        let err =
            apply_release(&manifest_for(&bad, "0.4.0"), &env(&root), None, false).unwrap_err();
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
        m.min_agent_version = Some("0.4.0".into());
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
            apply_release(&m, &refused, None, false).unwrap_err(),
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
        assert_eq!(
            &cmds[0][..3],
            &["sudo", "-n", &helper.display().to_string()]
        );
        let fallback = restart_commands(None);
        assert_eq!(
            fallback[0],
            ["systemctl", "restart", "--no-block", "vesyl-print-agent"]
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
        let st = maybe_update_from_heartbeat(&hb, &c, &env(&root), Some(held), None, false);
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
        let st = maybe_update_from_heartbeat(&hb, &c, &env(&root), None, None, false);
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
        let st = maybe_update_from_heartbeat(&hb, &lab, &env(&root), None, None, false);
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
        let st = maybe_update_from_heartbeat(&hb, &c, &env(&root), None, None, false);
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
        let st = maybe_update_from_heartbeat(&hb, &c, &env(&root), None, None, false);
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

    /// Count the restarts `f` asks for (on this thread) instead of running them.
    fn restarts_during<T>(f: impl FnOnce() -> T) -> (T, usize) {
        RESTARTS_SEEN.with(|n| n.set(Some(0)));
        let out = f();
        (out, RESTARTS_SEEN.with(|n| n.take()).unwrap_or(0))
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
            judge_pending_health(gate.clone(), &c, env, whoami, Some("timeout"), None, at)
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
            let out = process_pending_health(gate.clone(), &c, &old, whoami, Some("timeout"), None);
            assert_eq!(out, gate, "{whoami:?}");
            assert_eq!(current_name(&root), "0.4.0");
        }
        // The restarted agent passes the gate.
        let new = slot_agent(&root, "0.4.0");
        let out = process_pending_health(gate, &c, &new, WhoamiResult::Ok, None, None);
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
        let st = maybe_update_from_heartbeat(&hb, &c, &agent, Some(out.clone()), None, false);
        assert_eq!(st.status, STATUS_ROLLED_BACK);
        assert_eq!(current_name(&root), "0.3.0");
        // An agent started since (the restart after the rollback worked):
        // to it, the gate was armed before it started.
        let since = process_started_at().unwrap() - chrono::Duration::seconds(1);
        let restarted_view = UpdateStatus {
            armed_at: Some(since.to_rfc3339_opts(SecondsFormat::Micros, false)),
            ..out
        };
        let st = maybe_update_from_heartbeat(&hb, &c, &agent, Some(restarted_view), None, false);
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
        let out = process_pending_health(gate, &c, &fresh, WhoamiResult::Ok, None, None);
        assert_eq!(out.status, STATUS_ROLLED_BACK);
        let st = maybe_update_from_heartbeat(
            &desire(td.path(), "0.4.0"),
            &c,
            &fresh,
            Some(out),
            None,
            false,
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
        let out = process_pending_health(gate, &c, &old, WhoamiResult::Ok, None, None);
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
        let (st, restarts) =
            restarts_during(|| maybe_update_from_heartbeat(&hb, &c, &agent, None, None, false));
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
                );
                assert_eq!(deferred.status, STATUS_ROLLED_BACK, "{how}");
                assert_eq!(deferred.target_version.as_deref(), Some("0.5.0"), "{how}");
                assert_eq!(deferred.last_error, blocked.last_error, "{how}");
                assert_eq!(current_name(&root), "0.4.0", "{how}");

                let st =
                    maybe_update_from_heartbeat(&fix, &c, &env(&root), Some(deferred), None, false);
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
        );
        let st = maybe_update_from_heartbeat(&again, &c, &env(&root), Some(deferred), None, false);
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
        let st = maybe_update_from_heartbeat(&hb, &c, &env(&root), None, Some(&path), false);
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
        let st = maybe_update_from_heartbeat(&hb, &c, &env(&root), None, Some(&path), false);
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
        );
        assert_eq!(st.status, STATUS_PENDING_HEALTH, "{:?}", st.last_error);
        assert_eq!(current_name(&root), "0.5.0");
        assert!(releases.join("0.5.0/vesyl-print").is_file());
        assert!(!releases.join("0.5.0/locked").exists());
        assert!(!releases.join("0.5.0.staging").exists());
        assert_eq!(list_releases(&root), ["0.3.0", "0.4.0", "0.5.0"]);
        let aside = [
            releases.join(".0.5.0.stale-1"),
            releases.join(".0.5.0.staging.stale-1"),
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
        let aside = [
            releases.join(".0.5.0.stale-1"),
            releases.join(".0.5.0.staging.stale-1"),
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
        let st =
            maybe_update_from_heartbeat(&hb, &unsigned_ok(&td), &env(&root), None, None, false);
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
                let out =
                    maybe_update_from_heartbeat(&hb, &c, &old, st.clone(), Some(&path), false);
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
        let out =
            maybe_update_from_heartbeat(&hb, &c, &env(&root), Some(rolled), Some(&path), false);
        assert_eq!(out.status, STATUS_ROLLED_BACK);
        // Without a status path (the CLI's own `update apply`) nothing is re-read.
        arm_health_gate(&c, &path, "0.4.0", Some("0.3.0".into())).unwrap();
        let out =
            maybe_update_from_heartbeat(&obj(json!({"ok": true})), &c, &old, None, None, false);
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
        http_download_to_file(&format!("{}/a.tar.gz", srv.base_url), &dest, &sha).unwrap();
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
        http_download_to_file(&format!("{}/download/a.tar.gz", srv.base_url), &dest, &sha).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"hello-ota");

        let err =
            http_download_to_file(&format!("{}/missing", srv.base_url), &dest, &sha).unwrap_err();
        assert_eq!(
            (err.code, err.message.as_str()),
            ("download_failed", "HTTP 404 downloading artifact")
        );
        let err = http_download_to_file(&format!("{}/nolocation", srv.base_url), &dest, &sha)
            .unwrap_err();
        assert_eq!(err.code, "download_failed");
        assert!(!td.path().join("a.tar.gz.part").exists());
    }

    #[test]
    fn slow_download_is_not_cut_off_at_a_fixed_deadline() {
        // 2.5 s of body with 1 s connect/response timeouts: urllib's timeout is
        // per socket operation, so the Python agent finishes this download.
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
    /// timeout (Python's per-read 300 s), not the 30-minute body budget.
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
        // Python's per-operation values: manifest 120 s, artifact 300 s.
        assert_eq!(Timeouts::MANIFEST.idle, Duration::from_secs(120));
        assert_eq!(Timeouts::ARTIFACT.idle, Duration::from_secs(300));
    }
}
