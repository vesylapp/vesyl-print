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
//! Slots may hold either the Python app (`agent.py` / `main.py`) or the Rust
//! binary, so rollback works in both directions during the migration.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use base64::Engine as _;
use chrono::{SecondsFormat, Utc};
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

/// Files that mark a release slot as runnable (Python app or Rust binary).
const SLOT_ENTRYPOINTS: &[&str] = &["agent.py", "main.py", "vesyl-print", "bin/vesyl-print"];

/// Installed root-owned helper (NOPASSWD sudoers on appliances).
const APPLY_HELPER: &str = "/usr/local/lib/vesyl-print/apply-update";

fn version_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^\d+\.\d+\.\d+([.-][0-9A-Za-z.]+)?$").expect("regex"))
}

pub fn is_version(s: &str) -> bool {
    version_re().is_match(s)
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
        v.as_object().cloned().expect("object")
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
    /// Must match `scripts/build-release.sh` (Python `json.dumps(sort_keys=True,
    /// separators=(",", ":"))`, which also escapes non-ASCII).
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
        python_json(&Value::Object(body), &mut out);
        out.into_bytes()
    }
}

/// Serialize like Python `json.dumps(v, sort_keys=True, separators=(",", ":"))`
/// (default `ensure_ascii=True`). serde_json's Map is a BTreeMap, so keys sort
/// by code point as Python does.
fn python_json(v: &Value, out: &mut String) {
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
                python_json(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (k, val)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                python_json(&Value::String(k.clone()), out);
                out.push(':');
                python_json(val, out);
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
/// HTTP goes through [`net::agent`]: per-phase timeouts (a slow but steady
/// download is not cut off at a fixed deadline, as with urllib), urllib's
/// proxy rules, redirects followed, and no transparent decompression, so the
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

pub fn http_get_bytes(url: &str) -> Result<Vec<u8>, UpdateError> {
    let mut out = Vec::new();
    open_url(url, Timeouts::ARTIFACT, &format!("fetching {url}"))?
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
        fs::create_dir_all(parent).map_err(io_err("download_failed"))?;
    }
    let tmp = PathBuf::from(format!("{}.part", dest.display()));
    let result = (|| {
        let mut reader = open_url(url, Timeouts::ARTIFACT, "downloading artifact")?;
        let mut out = File::create(&tmp).map_err(io_err("download_failed"))?;
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

/// Extract tarball into `dest_dir` (must not already exist). Rejects absolute
/// paths, `..`, and links that point outside the archive.
pub fn extract_tarball(tarball: &Path, dest_dir: &Path) -> Result<(), UpdateError> {
    if dest_dir.exists() {
        return Err(UpdateError::new(
            format!("release dir already exists: {}", dest_dir.display()),
            "exists",
        ));
    }
    let parent = dest_dir.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(io_err("bad_archive"))?;
    let staging = PathBuf::from(format!("{}.staging", dest_dir.display()));
    if staging.exists() {
        let _ = fs::remove_dir_all(&staging);
    }
    fs::create_dir_all(&staging).map_err(io_err("bad_archive"))?;

    let open = || -> Result<tar::Archive<flate2::read::GzDecoder<File>>, UpdateError> {
        let f = File::open(tarball)
            .map_err(|e| UpdateError::new(format!("extract failed: {e}"), "bad_archive"))?;
        Ok(tar::Archive::new(flate2::read::GzDecoder::new(f)))
    };
    let extract = || -> Result<(), UpdateError> {
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
        archive.unpack(&staging).map_err(fail)
    };
    if let Err(e) = extract() {
        let _ = fs::remove_dir_all(&staging);
        return Err(e);
    }

    // If archive has a single top-level dir, peel it.
    let children: Vec<PathBuf> = fs::read_dir(&staging)
        .map_err(io_err("bad_archive"))?
        .flatten()
        .map(|e| e.path())
        .collect();
    if children.len() == 1 && children[0].is_dir() {
        fs::rename(&children[0], dest_dir).map_err(io_err("bad_archive"))?;
        let _ = fs::remove_dir_all(&staging);
    } else {
        fs::rename(&staging, dest_dir).map_err(io_err("bad_archive"))?;
    }
    Ok(())
}

pub fn write_version_file(release_dir: &Path, version: &str) -> std::io::Result<()> {
    fs::write(release_dir.join("VERSION"), format!("{version}\n"))
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

/// `sudo -n <helper> activate <release_dir> <current>`.
fn helper_activate(helper: &Path, release_dir: &Path, current: &Path) -> Result<(), UpdateError> {
    let h = helper.display().to_string();
    let r = release_dir.display().to_string();
    let c = current.display().to_string();
    let out = crate::printers::run_with_timeout(
        "sudo",
        &["-n", &h, "activate", &r, &c],
        Duration::from_secs(60),
    )
    .map_err(|e| {
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

/// Flip current to the previous release (or an explicit version).
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
    let cur_ver = current_release_dir(install_root).map(|c| dir_name(&c));
    let target = match to_version {
        Some(v) => {
            if !releases.iter().any(|r| r == v) {
                return Err(UpdateError::new(
                    format!("unknown release {v}"),
                    "missing_release",
                ));
            }
            v.to_string()
        }
        None => releases
            .iter()
            .rev()
            .find(|v| Some(v.as_str()) != cur_ver.as_deref())
            .cloned()
            .ok_or_else(|| UpdateError::new("no previous release for rollback", "no_rollback"))?,
    };
    let release_dir = install_root.join("releases").join(&target);
    match apply_helper.filter(|h| h.is_file()) {
        Some(helper) => {
            if let Err(e) = helper_activate(helper, &release_dir, &install_root.join("current")) {
                // Lab installs / unit tests: fall back to in-process symlink flip.
                log::warn!(target: LOG, "{e}; flipping current in-process");
                flip_current(install_root, &target)?;
            }
        }
        None => {
            flip_current(install_root, &target)?;
        }
    }
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

/// Restart display + agent. Best-effort and non-blocking: when the agent
/// restarts *itself*, systemd SIGTERMs this process while the helper is still
/// running, so launch it in its own process group and never wait on it.
pub fn restart_services(helper: Option<&Path>) {
    use std::os::unix::process::CommandExt;
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
    /// Version of the running binary (`package_version()` in Python).
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
    if let Some(min) = &manifest.min_agent_version {
        if version_cmp(&env.running_version, min).is_lt() {
            return Err(UpdateError::new(
                format!("current {} < min_agent_version {min}", env.running_version),
                "too_old",
            ));
        }
    }
    verify_manifest(manifest, public_key_pem, require_signature)?;

    let root = &env.install_root;
    let update_dir = root.join("update");
    fs::create_dir_all(&update_dir).map_err(io_err("download_failed"))?;
    let tarball = update_dir.join(format!("vesyl-print-{}.tar.gz", manifest.version));

    log::info!(target: LOG, "downloading {}", manifest.artifact_url);
    http_download_to_file(&manifest.artifact_url, &tarball, &manifest.artifact_sha256)?;

    let release_dir = root.join("releases").join(&manifest.version);
    if release_dir.exists() {
        fs::remove_dir_all(&release_dir).map_err(io_err("bad_archive"))?;
    }
    log::info!(target: LOG, "extracting to {}", release_dir.display());
    extract_tarball(&tarball, &release_dir)?;
    write_version_file(&release_dir, &manifest.version).map_err(io_err("bad_archive"))?;

    // Minimal sanity: an entrypoint is present.
    if !SLOT_ENTRYPOINTS
        .iter()
        .any(|e| release_dir.join(e).is_file())
    {
        let _ = fs::remove_dir_all(&release_dir);
        return Err(UpdateError::new(
            "archive missing agent entrypoint (vesyl-print or agent.py/main.py)",
            "bad_archive",
        ));
    }

    match env.apply_helper.as_deref().filter(|h| h.is_file()) {
        Some(helper) => helper_activate(helper, &release_dir, &root.join("current"))?,
        None => {
            flip_current(root, &manifest.version)?;
        }
    }
    log::info!(target: LOG, "activated version {}", manifest.version);
    let _ = fs::remove_file(&tarball);
    Ok(release_dir)
}

pub fn utc_now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, false)
}

pub fn utc_now_plus(seconds: i64) -> String {
    (Utc::now() + chrono::Duration::seconds(seconds)).to_rfc3339_opts(SecondsFormat::Secs, false)
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
    if channel.is_some() {
        st.channel = channel;
    }
}

/// Fast checks on the active release dir (no network).
pub fn local_slot_healthy(env: &UpdateEnv, expected_version: Option<&str>) -> Result<(), String> {
    let cur = current_release_dir(&env.install_root).ok_or("current symlink missing or broken")?;
    if !SLOT_ENTRYPOINTS.iter().any(|e| cur.join(e).is_file()) {
        return Err("current slot missing agent entrypoint".into());
    }
    if let Some(expected) = expected_version.filter(|e| !e.is_empty()) {
        let name = dir_name(&cur);
        let slot_ver = fs::read_to_string(cur.join("VERSION"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| name.clone());
        if !same_version(&slot_ver, expected) && name != expected {
            return Err(format!(
                "slot version {slot_ver:?} != expected {expected:?}"
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

/// Post-update health gate.
///
/// Declares success only after local slot checks pass and (when paired) whoami
/// reaches the API. On hard failure or deadline expiry: auto-rollback to
/// `previous_version` when available, set `rolled_back`, and restart services.
pub fn process_pending_health(
    st: UpdateStatus,
    cfg: &Config,
    env: &UpdateEnv,
    whoami: WhoamiResult,
    whoami_error: Option<&str>,
    now_iso: Option<&str>,
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
    st.last_checked_at = Some(now.clone());
    st.health_attempts += 1;
    let expected = st
        .target_version
        .clone()
        .unwrap_or_else(|| st.current_version.clone());

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
    if let Some(prev) = st.previous_version.clone().filter(|p| *p != expected) {
        log::error!(target: LOG, "post-update health failed ({reason}) — rolling back to {prev}");
        return match rollback(&env.install_root, Some(&prev), env.apply_helper.as_deref()) {
            Ok(rolled) => {
                st.status = STATUS_ROLLED_BACK.into();
                st.current_version = rolled.clone();
                st.target_version = Some(expected);
                st.previous_version = None;
                st.health_deadline_at = None;
                st.last_error = Some(format!(
                    "{HEALTH_FAILED}: {reason}; rolled back to {rolled}"
                ));
                if env.restart {
                    restart_services(env.apply_helper.as_deref());
                }
                st
            }
            Err(e) => {
                log::error!(target: LOG, "auto-rollback failed: {}", e.message);
                st.status = STATUS_FAILED.into();
                st.last_error = Some(format!(
                    "{HEALTH_FAILED}: {reason}; rollback error: {}",
                    e.message
                ));
                st
            }
        };
    }

    st.status = STATUS_FAILED.into();
    st.health_deadline_at = None;
    st.last_error = Some(format!(
        "{HEALTH_FAILED}: {reason} (no previous slot to roll back to)"
    ));
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
/// this node (rolled back, or failed with no way to roll back).
fn failed_health_gate(st: &UpdateStatus) -> bool {
    st.is(STATUS_ROLLED_BACK)
        || (st.is(STATUS_FAILED)
            && st
                .last_error
                .as_deref()
                .is_some_and(|e| e.starts_with(HEALTH_FAILED)))
}

/// Inspect a heartbeat response and optionally apply an update.
///
/// After a successful activate, status becomes `pending_health` (not idle);
/// the new process must call [`process_pending_health`] after restart. When
/// `jobs_busy`, download/install is deferred so slots never flip mid-print.
/// A version that already failed its health gate here is not re-applied for
/// the same desired version (see below).
pub fn maybe_update_from_heartbeat(
    hb: &JsonObject,
    cfg: &Config,
    env: &UpdateEnv,
    status: Option<UpdateStatus>,
    status_path: Option<&Path>,
    jobs_busy: bool,
) -> UpdateStatus {
    let mut st = status.unwrap_or_default();
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
    if failed_health_gate(&st) && prev_target.is_some_and(|t| same_version(&t, &desired)) {
        log::info!(
            target: LOG,
            "not re-applying {desired}: it failed its health gate on this node ({}); \
             waiting for a different desired version or a manual update",
            st.status
        );
        return st;
    }

    if !cfg.auto_update_enabled {
        if !sticky(&st) {
            st.status = STATUS_IDLE.into();
        }
        log::info!(target: LOG, "update available: {} → {desired} (auto_update disabled)", st.current_version);
        return st;
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
        return st;
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
        // Persist early so the LCD can show "Updating…" during the download.
        persist(&st);
        // With signatures required, an unreadable key fails here (closed).
        let pem = manifest_public_key(cfg)?;
        log::info!(target: LOG, "applying update {desired} from {manifest_url}");
        let manifest = fetch_manifest(&manifest_url)?;
        if !same_version(&manifest.version, &desired) {
            log::info!(target: LOG, "manifest version {} (desired {desired})", manifest.version);
        }
        st.status = STATUS_INSTALLING.into();
        // Persist installing so a crash mid-apply is visible.
        persist(&st);

        apply_release(&manifest, env, pem.as_deref(), cfg.update_require_signature)?;
        let prev = previous
            .clone()
            .filter(|p| !same_version(p, &manifest.version));
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
                return st;
            }
        }
        st.status = STATUS_FAILED.into();
        st.last_error = Some(e.message.clone());
        log::error!(target: LOG, "update failed: {}", e.message);
    }
    st
}

pub fn write_update_status(path: &Path, status: &UpdateStatus) -> std::io::Result<()> {
    let mut raw = serde_json::to_string_pretty(&status.to_dict()).map_err(std::io::Error::other)?;
    raw.push('\n');
    write_durable(path, raw.as_bytes(), 0o644, false)?;
    give_to_dir_owner(path);
    Ok(())
}

/// The CLI often runs as root while the agent runs as the service user that
/// owns the state dir. A root-owned status file cannot be rewritten in place
/// by a rolled-back Python agent, which would leave `pending_health` (and the
/// job pause) stuck, so hand the file to the directory's owner.
fn give_to_dir_owner(path: &Path) {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        return;
    }
    let Some(dir_meta) = path.parent().and_then(|d| fs::metadata(d).ok()) else {
        return;
    };
    if dir_meta.uid() == 0 {
        return;
    }
    // lchown: the directory's owner could swap in a symlink after the rename,
    // and root must never chown whatever such a link points at.
    if let Err(e) = std::os::unix::fs::lchown(path, Some(dir_meta.uid()), Some(dir_meta.gid())) {
        log::warn!(target: LOG, "chown {}: {e}", path.display());
    }
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
        fs::write(src.join("agent.py"), "# fake agent\n").unwrap();
        fs::write(src.join("main.py"), "# fake main\n").unwrap();
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
        // Exactly what Python json.dumps(sort_keys=True, separators=(",", ":")) emits.
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
        assert!(r1.join("agent.py").is_file());
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
        assert!(root.join("releases/0.5.0/agent.py").is_file());
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
        fs::remove_file(cur.join("agent.py")).unwrap();
        fs::remove_file(cur.join("main.py")).unwrap();
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
    fn rust_binary_slot_is_healthy() {
        let td = tempfile::tempdir().unwrap();
        let root = two_slots(td.path());
        let cur = fs::canonicalize(root.join("current")).unwrap();
        fs::remove_file(cur.join("agent.py")).unwrap();
        fs::remove_file(cur.join("main.py")).unwrap();
        fs::write(cur.join("vesyl-print"), b"\x7fELF").unwrap();
        assert!(local_slot_healthy(&env(&root), Some("0.4.0")).is_ok());
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
        // A configured path that does not exist falls back to the bundled key
        // (Python does the same): verification still happens.
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
}
