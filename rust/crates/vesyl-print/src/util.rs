//! Small helpers shared across modules: Python-ish JSON coercion and durable writes.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use chrono::{SecondsFormat, Utc};
use serde_json::Value;

const LOG: &str = "vesyl-print.util";

/// UTC now as `2026-07-15T20:00:00+00:00` (Python `isoformat()` without micros).
pub fn utc_now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, false)
}

/// Python truthiness for a JSON value.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `bool(obj.get(key))`.
pub fn get_truthy(obj: &serde_json::Map<String, Value>, key: &str) -> bool {
    obj.get(key).is_some_and(truthy)
}

/// Python `str(v)` for scalar JSON values (strings unquoted).
pub fn py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Null => "None".into(),
        other => other.to_string(),
    }
}

/// `str(v)` when present and not null, else `None`.
pub fn opt_str(v: Option<&Value>) -> Option<String> {
    match v {
        None | Some(Value::Null) => None,
        Some(v) => Some(py_str(v)),
    }
}

/// `str(obj[key])` when the value is truthy (the `x.get(k) or None` idiom).
pub fn truthy_str(obj: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key).filter(|v| truthy(v)).map(py_str)
}

/// Python `int(v)`: ints, truncated floats, bools and numeric strings.
pub fn py_int(v: &Value) -> Option<i64> {
    match v {
        Value::Bool(b) => Some(*b as i64),
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Write `data` to `path` atomically: a temp file with a unique name in the
/// same directory gets `mode`, the data and an fsync, then is renamed over
/// `path`. Optionally fsyncs the directory too.
///
/// Every call has its own temp file, so concurrent writers (the cable thread
/// and the main loop both save credentials) never write into each other's.
///
/// When root writes (an operator running the CLI), the new file keeps the
/// owner of the file it replaces, or of the directory for a new file, so the
/// non-root service can still read it, and a rolled-back Python agent, which
/// rewrites some files in place, can still write it. Directories created on
/// the way get the owner of the closest existing one.
pub fn write_durable(path: &Path, data: &[u8], mode: u32, sync_dir: bool) -> io::Result<()> {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    create_dirs(dir)?;
    let mut prefix = OsString::from(".");
    prefix.push(path.file_name().unwrap_or_default());
    prefix.push(".");
    // Created 0600 and removed again on any error before the rename.
    let mut tmp = tempfile::Builder::new()
        .prefix(&prefix)
        .suffix(".tmp")
        .tempfile_in(dir)?;
    keep_service_owner(tmp.as_file(), path, dir);
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    tmp.write_all(data)?;
    tmp.flush()?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    if sync_dir {
        if let Ok(d) = File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// `(uid, gid)` of a file or directory.
type Owner = (u32, u32);

/// Owner for a file that `euid` is about to replace, or `None` to keep the
/// writer's own. Only root changes anything: the replaced file's owner wins,
/// unless the file is missing or itself root-owned (left by an older root
/// run), in which case the directory's owner (the service user) does.
fn owner_for_rewrite(euid: u32, existing: Option<Owner>, dir: Option<Owner>) -> Option<Owner> {
    if euid != 0 {
        return None;
    }
    match existing {
        Some(owner) if owner.0 != 0 => Some(owner),
        _ => dir,
    }
}

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

fn owner_of(path: &Path) -> Option<Owner> {
    fs::metadata(path).ok().map(|m| (m.uid(), m.gid()))
}

/// Apply [`owner_for_rewrite`] to the open temp file before it is renamed
/// into place. Best effort: a failure is logged and the write goes ahead.
fn keep_service_owner(tmp: &File, path: &Path, dir: &Path) {
    let euid = euid();
    if euid != 0 {
        return;
    }
    let Some((uid, gid)) = owner_for_rewrite(euid, owner_of(path), owner_of(dir)) else {
        return;
    };
    if let Err(e) = std::os::unix::fs::fchown(tmp, Some(uid), Some(gid)) {
        log::warn!(
            target: LOG,
            "could not hand {} to uid {uid} gid {gid}: {e}",
            path.display()
        );
    }
}

/// `fs::create_dir_all`, except that directories root creates get the owner
/// of the closest directory that already existed.
fn create_dirs(dir: &Path) -> io::Result<()> {
    let euid = euid();
    if euid != 0 || dir.is_dir() {
        return fs::create_dir_all(dir);
    }
    let missing: Vec<&Path> = dir
        .ancestors()
        .take_while(|p| !p.as_os_str().is_empty() && !p.exists())
        .collect();
    fs::create_dir_all(dir)?;
    let existing = match missing.last().and_then(|top| top.parent()) {
        Some(p) if p.as_os_str().is_empty() => Path::new("."),
        Some(p) => p,
        None => return Ok(()),
    };
    if let Some((uid, gid)) = owner_for_rewrite(euid, None, owner_of(existing)) {
        for d in &missing {
            if let Err(e) = std::os::unix::fs::chown(d, Some(uid), Some(gid)) {
                log::warn!(target: LOG, "could not hand {} to uid {uid}: {e}", d.display());
            }
        }
    }
    Ok(())
}

pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn truthiness_matches_python() {
        assert!(!truthy(&json!(null)));
        assert!(!truthy(&json!("")));
        assert!(!truthy(&json!(0)));
        assert!(!truthy(&json!({})));
        assert!(truthy(&json!("x")));
        assert!(truthy(&json!(1)));
    }

    #[test]
    fn py_int_coerces() {
        assert_eq!(py_int(&json!("15")), Some(15));
        assert_eq!(py_int(&json!(15.9)), Some(15));
        assert_eq!(py_int(&json!(true)), Some(1));
        assert_eq!(py_int(&json!("x")), None);
    }

    #[test]
    fn concurrent_writers_never_share_a_temp_file() {
        // The cable thread (node_config) and the main loop (whoami) both save
        // credentials.json; with one fixed temp name an interleaving left the
        // file as one JSON document plus a stale tail, or failed the rename.
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("credentials.json");
        let payloads: Vec<String> = (0..6)
            .map(|i| format!("{{\"name\": \"{}\"}}\n", "x".repeat(10 + 40 * i)))
            .collect();
        let barrier = std::sync::Barrier::new(payloads.len());
        std::thread::scope(|s| {
            for p in &payloads {
                let (path, barrier) = (&path, &barrier);
                s.spawn(move || {
                    barrier.wait();
                    for _ in 0..40 {
                        write_durable(path, p.as_bytes(), 0o600, false).unwrap();
                    }
                });
            }
        });
        let raw = fs::read_to_string(&path).unwrap();
        assert!(payloads.contains(&raw), "torn file: {raw:?}");
        let names: Vec<String> = fs::read_dir(td.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["credentials.json"], "temp files left behind");
    }

    #[test]
    fn write_durable_sets_exact_mode_and_creates_dirs() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("state").join("update_status.json");
        write_durable(&path, b"{}\n", 0o644, true).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{}\n");
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o644);
        write_durable(&path, b"[]\n", 0o600, false).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "[]\n");
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn root_keeps_the_service_owner() {
        let vesyl = Some((999, 998));
        let root = Some((0, 0));
        // Not root: the writer owns what it writes (the normal agent path).
        assert_eq!(owner_for_rewrite(1000, vesyl, vesyl), None);
        // Root replacing the service user's file keeps that owner.
        assert_eq!(owner_for_rewrite(0, Some((999, 4)), root), Some((999, 4)));
        // New file: the directory's owner (setup.sh makes it the service user).
        assert_eq!(owner_for_rewrite(0, None, vesyl), vesyl);
        // A root-owned file left by an older root run is handed back.
        assert_eq!(owner_for_rewrite(0, root, vesyl), vesyl);
        // Root-owned everything stays root's.
        assert_eq!(owner_for_rewrite(0, root, root), root);
        assert_eq!(owner_for_rewrite(0, None, None), None);
    }

    /// Needs root: `sudo cargo test`, or unprivileged with
    /// `unshare --map-root-user --map-auto cargo test -- --ignored root_write`.
    #[test]
    #[ignore = "needs root (or a user namespace) to chown"]
    fn root_write_hands_files_to_the_directory_owner() {
        if euid() != 0 {
            return;
        }
        let td = tempfile::tempdir().unwrap();
        std::os::unix::fs::chown(td.path(), Some(1000), Some(1000)).unwrap();
        let owner = |p: &Path| {
            let m = fs::metadata(p).unwrap();
            (m.uid(), m.gid(), m.mode() & 0o777)
        };
        let creds = td.path().join("credentials.json");
        write_durable(&creds, b"{}", 0o600, false).unwrap();
        assert_eq!(owner(&creds), (1000, 1000, 0o600));
        // An existing root-owned file is handed back too.
        let status = td.path().join("update_status.json");
        fs::write(&status, "{}").unwrap();
        assert_eq!(owner(&status).0, 0);
        write_durable(&status, b"{}", 0o644, true).unwrap();
        assert_eq!(owner(&status), (1000, 1000, 0o644));
        // Directories created on the way are handed over too.
        let queued = td.path().join("queue").join("new").join("job-1.json");
        write_durable(&queued, b"{}", 0o600, true).unwrap();
        assert_eq!(owner(&queued), (1000, 1000, 0o600));
        assert_eq!(owner(&td.path().join("queue")).0, 1000);
        assert_eq!(owner(queued.parent().unwrap()).0, 1000);
        // status.json goes through the same path.
        let status_json = td.path().join("status.json");
        crate::statusio::write_status(&status_json, &mut Default::default()).unwrap();
        assert_eq!(owner(&status_json), (1000, 1000, 0o600));
    }
}
