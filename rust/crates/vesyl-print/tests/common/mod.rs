//! Helpers for the tests that drive the repository's shell scripts
//! (`scripts/apply-update`, `scripts/build-release.sh`) in temp dirs.
#![allow(dead_code)] // each test crate uses a different subset

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard};

/// Held while a test writes an executable and while it forks a child.
///
/// Tests run in parallel threads: a child forked while another thread still
/// has a fresh script open for writing inherits that descriptor until it
/// execs, and executing the script meanwhile fails with ETXTBSY ("Text file
/// busy"). `Command::spawn` returns only once the child has exec'd (which
/// closes the inherited CLOEXEC descriptors), so serializing both is enough.
static EXEC_LOCK: Mutex<()> = Mutex::new(());

fn exec_lock() -> MutexGuard<'static, ()> {
    EXEC_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// `cmd.output()`, with the fork serialized against executable writes.
pub fn output(cmd: &mut Command) -> Output {
    let child = {
        let _guard = exec_lock();
        cmd.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {cmd:?}: {e}"))
    };
    child.wait_with_output().unwrap()
}

/// The repository root (this crate lives in `rust/crates/vesyl-print`).
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .expect("repository root")
}

fn is_executable_file(p: &Path) -> bool {
    fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `tool` resolved on the test process's PATH.
pub fn which(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(tool))
        .find(|p| is_executable_file(p))
}

/// True when every tool is on PATH. A missing tool skips the test on a
/// developer machine but fails it in CI, where all of them are expected.
pub fn have_tools(tools: &[&str]) -> bool {
    let missing: Vec<&str> = tools
        .iter()
        .copied()
        .filter(|t| which(t).is_none())
        .collect();
    if missing.is_empty() {
        return true;
    }
    assert!(
        std::env::var_os("CI").is_none(),
        "tools missing on the CI runner: {missing:?}"
    );
    eprintln!("skipping: tools not installed: {missing:?}");
    false
}

/// Write `bytes` to `path`, creating parent directories.
pub fn write(path: &Path, bytes: impl AsRef<[u8]>) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, bytes).unwrap();
}

/// Write an executable script.
pub fn write_exe(path: &Path, text: &str) {
    let _guard = exec_lock();
    write(path, text);
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Quote `s` for a POSIX shell.
pub fn sh_quote(s: impl AsRef<OsStr>) -> String {
    let s = s.as_ref().to_string_lossy();
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Outcome of one script run.
#[derive(Debug)]
pub struct Run {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// Both streams, for assertion messages.
    pub fn log(&self) -> String {
        format!(
            "exit {:?}\n--- stdout\n{}--- stderr\n{}",
            self.code, self.stdout, self.stderr
        )
    }
}

/// Run `program args` with exactly `env` (nothing inherited) and no stdin.
pub fn run(program: &Path, args: &[&str], env: &[(String, String)]) -> Run {
    let out = output(
        Command::new(program)
            .args(args)
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null()),
    );
    Run {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// An Ed25519 key pair made with openssl, as the release keys are.
pub struct KeyPair {
    pub private: PathBuf,
    pub public: PathBuf,
}

impl KeyPair {
    pub fn generate(dir: &Path, name: &str) -> KeyPair {
        let private = dir.join(format!("{name}.pem"));
        let public = dir.join(format!("{name}_pub.pem"));
        openssl(&[
            "genpkey",
            "-algorithm",
            "Ed25519",
            "-out",
            path_str(&private),
        ]);
        openssl(&[
            "pkey",
            "-in",
            path_str(&private),
            "-pubout",
            "-out",
            path_str(&public),
        ]);
        KeyPair { private, public }
    }

    pub fn private_pem(&self) -> String {
        fs::read_to_string(&self.private).unwrap()
    }

    pub fn public_pem(&self) -> String {
        fs::read_to_string(&self.public).unwrap()
    }

    /// Raw Ed25519 signature over `message`, base64 (the manifest format).
    pub fn sign_b64(&self, message: &[u8], scratch: &Path) -> String {
        use base64::Engine as _;
        let msg = scratch.join("to-sign.bin");
        let sig = scratch.join("signature.bin");
        fs::write(&msg, message).unwrap();
        openssl(&[
            "pkeyutl",
            "-sign",
            "-inkey",
            path_str(&self.private),
            "-rawin",
            "-in",
            path_str(&msg),
            "-out",
            path_str(&sig),
        ]);
        base64::engine::general_purpose::STANDARD.encode(fs::read(&sig).unwrap())
    }
}

/// openssl's verdict on a raw Ed25519 signature (base64) over `message`.
pub fn openssl_verifies(public: &Path, message: &[u8], sig_b64: &str, scratch: &Path) -> bool {
    use base64::Engine as _;
    let msg = scratch.join("to-verify.bin");
    let sig = scratch.join("to-verify.sig");
    fs::write(&msg, message).unwrap();
    let raw = base64::engine::general_purpose::STANDARD
        .decode(sig_b64)
        .expect("signature is base64");
    fs::write(&sig, raw).unwrap();
    output(
        Command::new("openssl")
            .args(["pkeyutl", "-verify", "-pubin", "-inkey"])
            .arg(public)
            .args(["-rawin", "-in"])
            .arg(&msg)
            .arg("-sigfile")
            .arg(&sig),
    )
    .status
    .success()
}

fn openssl(args: &[&str]) {
    let out = output(Command::new("openssl").args(args));
    assert!(
        out.status.success(),
        "openssl {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

pub fn path_str(p: &Path) -> &str {
    p.to_str().expect("UTF-8 temp path")
}

/// What a path is, for before/after comparisons of a directory tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Dir,
    Link(PathBuf),
    File(Vec<u8>),
}

/// Every path under `root` (relative), skipping `except`.
pub fn snapshot(root: &Path, except: &[&Path]) -> BTreeMap<PathBuf, Node> {
    fn walk(root: &Path, dir: &Path, except: &[&Path], out: &mut BTreeMap<PathBuf, Node>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if except.contains(&path.as_path()) {
                continue;
            }
            let rel = path.strip_prefix(root).unwrap().to_path_buf();
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.file_type().is_symlink() {
                out.insert(rel, Node::Link(fs::read_link(&path).unwrap()));
            } else if meta.is_dir() {
                out.insert(rel, Node::Dir);
                walk(root, &path, except, out);
            } else {
                out.insert(rel, Node::File(fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, except, &mut out);
    out
}
