//! Small helpers shared across modules: Python-ish JSON coercion and durable writes.

use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path};

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
/// writer's own. Only root changes anything: the replaced file's owner wins
/// (a symlink's own owner, not its target's), unless the file is missing or
/// itself root-owned (left by an older root run), in which case the
/// directory's owner (the service user) does.
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

fn owner(m: &fs::Metadata) -> Owner {
    (m.uid(), m.gid())
}

/// Apply [`owner_for_rewrite`] to the open temp file before it is renamed
/// into place. Best effort: a failure is logged and the write goes ahead.
fn keep_service_owner(tmp: &File, path: &Path, dir: &Path) {
    let euid = euid();
    if euid != 0 {
        return;
    }
    // lstat: a symlink planted at `path` must not pick who owns the new file
    // (for credentials.json, who can read the token). The rename replaces
    // the link itself, not its target. The directory's own symlink, if any
    // (to a data partition, say), is followed.
    let existing = fs::symlink_metadata(path).ok().map(|m| owner(&m));
    let dir_owner = fs::metadata(dir).ok().map(|m| owner(&m));
    let Some((uid, gid)) = owner_for_rewrite(euid, existing, dir_owner) else {
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
    if euid() != 0 || dir.is_dir() {
        return fs::create_dir_all(dir);
    }
    create_dirs_as_root(dir, &|_| {})
}

/// Root's half of [`create_dirs`]. The service user owns the directories this
/// usually runs in, so it can rename a directory root has just made and put a
/// symlink in its place; a chown by path would then hand the link's target
/// (say /etc) to that user. So each missing directory is made with mkdirat
/// relative to its open parent, reopened there with O_NOFOLLOW, and chowned
/// through that descriptor: no name made here, the last one or one on the
/// way to it, is resolved through a path again, and a swapped-in symlink
/// fails the call instead. `after_mkdir` lets tests do the swap.
fn create_dirs_as_root(dir: &Path, after_mkdir: &dyn Fn(&Path)) -> io::Result<()> {
    // The closest ancestor that exists; it may be a symlink (to a data
    // partition, say), and is followed like any existing directory.
    let base = dir
        .ancestors()
        .find(|p| p.as_os_str().is_empty() || p.exists())
        .unwrap_or(Path::new(""));
    let rest = dir.strip_prefix(base).map_err(io::Error::other)?;
    let mut parent = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(if base.as_os_str().is_empty() {
            Path::new(".")
        } else {
            base
        })?;
    let new_owner = owner_for_rewrite(0, None, parent.metadata().ok().map(|m| owner(&m)));
    let mut made = base.to_path_buf();
    for part in rest.components() {
        let name = part.as_os_str();
        made.push(name);
        // Only names this call makes are handed over, never "..", nor a
        // directory another writer made first.
        let created = matches!(part, Component::Normal(_)) && mkdir_at(&parent, name)?;
        if created {
            after_mkdir(&made);
        }
        let child = open_dir_at(&parent, name)?;
        if let (true, Some((uid, gid))) = (created, new_owner) {
            if let Err(e) = std::os::unix::fs::fchown(&child, Some(uid), Some(gid)) {
                log::warn!(target: LOG, "could not hand {} to uid {uid}: {e}", made.display());
            }
        }
        parent = child;
    }
    Ok(())
}

/// mkdirat(2) with `create_dir_all`'s mode: `Ok(true)` when this call made
/// `name`, `Ok(false)` when something already has that name.
fn mkdir_at(parent: &File, name: &OsStr) -> io::Result<bool> {
    let c = CString::new(name.as_bytes())?;
    // SAFETY: `parent` is an open descriptor and `c` a NUL-terminated string.
    if unsafe { libc::mkdirat(parent.as_raw_fd(), c.as_ptr(), 0o777) } == 0 {
        return Ok(true);
    }
    let e = io::Error::last_os_error();
    if e.kind() == io::ErrorKind::AlreadyExists {
        Ok(false)
    } else {
        Err(e)
    }
}

/// Open the directory `name` inside `parent`. A symlink there is not
/// followed: the open fails (ENOTDIR, as O_DIRECTORY is checked first).
fn open_dir_at(parent: &File, name: &OsStr) -> io::Result<File> {
    let c = CString::new(name.as_bytes())?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: as above; without O_CREAT, openat reads no mode argument.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), c.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just opened here and nothing else owns it.
    Ok(unsafe { File::from_raw_fd(fd) })
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

    /// Plants a symlink to `target` where the walk has just made `name`, the
    /// way the service user could between root's mkdir and its chown.
    fn swap_for_link<'a>(
        name: &'a str,
        target: &'a Path,
        swapped: &'a std::cell::Cell<bool>,
    ) -> impl Fn(&Path) + 'a {
        move |made: &Path| {
            if made.ends_with(name) && !swapped.get() {
                let mut away = made.as_os_str().to_owned();
                away.push(".moved");
                fs::rename(made, away).unwrap();
                std::os::unix::fs::symlink(target, made).unwrap();
                swapped.set(true);
            }
        }
    }

    /// The walk's open met the planted link and refused it: ENOTDIR on Linux,
    /// where O_DIRECTORY is checked before O_NOFOLLOW's ELOOP.
    fn refused_link(e: &io::Error) -> bool {
        matches!(e.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP))
    }

    #[test]
    fn root_dir_walk_never_follows_a_swapped_in_symlink() {
        // Root's walk, run unprivileged: handing a directory to our own uid
        // is allowed, so everything but the ownership itself is exercised.
        let td = tempfile::tempdir().unwrap();
        let deep = td.path().join("queue").join("new");
        create_dirs_as_root(&deep, &|_| {}).unwrap();
        assert!(deep.is_dir());
        create_dirs_as_root(&deep, &|_| {}).unwrap();
        let rel = td.path().join("rel");
        create_dirs_as_root(&rel.join("..").join("rel").join("x"), &|_| {}).unwrap();
        assert!(rel.join("x").is_dir());

        // "jobs" is swapped for a link before root opens it, so "new" would
        // be made, and chowned, inside the link's target (/etc, say).
        let elsewhere = tempfile::tempdir().unwrap();
        let swapped = std::cell::Cell::new(false);
        let err = create_dirs_as_root(
            &td.path().join("jobs").join("new"),
            &swap_for_link("jobs", elsewhere.path(), &swapped),
        )
        .unwrap_err();
        assert!(swapped.get());
        assert!(refused_link(&err), "{err}");
        assert_eq!(fs::read_dir(elsewhere.path()).unwrap().count(), 0);

        // The same for the last directory: nothing past the link is touched.
        let swapped = std::cell::Cell::new(false);
        let err = create_dirs_as_root(
            &td.path().join("spool").join("new"),
            &swap_for_link("new", elsewhere.path(), &swapped),
        )
        .unwrap_err();
        assert!(swapped.get());
        assert!(refused_link(&err), "{err}");
        assert_eq!(fs::read_dir(elsewhere.path()).unwrap().count(), 0);
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

        // A symlink planted at the target does not pick the new file's owner:
        // the link (root's here) is replaced, and its target is not touched.
        let elsewhere = tempfile::tempdir().unwrap();
        let victim = elsewhere.path().join("victim");
        fs::write(&victim, "keep").unwrap();
        std::os::unix::fs::chown(&victim, Some(2000), Some(2000)).unwrap();
        let linked = td.path().join("credentials-linked.json");
        std::os::unix::fs::symlink(&victim, &linked).unwrap();
        write_durable(&linked, b"{}", 0o600, false).unwrap();
        assert!(fs::symlink_metadata(&linked).unwrap().is_file());
        assert_eq!(owner(&linked), (1000, 1000, 0o600));
        assert_eq!(owner(&victim).0, 2000);
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");

        // A directory root just made, swapped for a link to a root-owned
        // directory, never gets that directory chowned to the service user.
        let rooted = tempfile::tempdir().unwrap();
        let before = owner(rooted.path());
        assert_eq!(before.0, 0);
        let swapped = std::cell::Cell::new(false);
        let made = td.path().join("jobs").join("new");
        let err = create_dirs_as_root(&made, &swap_for_link("jobs", rooted.path(), &swapped))
            .unwrap_err();
        assert!(swapped.get());
        assert!(refused_link(&err), "{err}");
        assert_eq!(owner(rooted.path()), before);
        assert_eq!(fs::read_dir(rooted.path()).unwrap().count(), 0);
        let swapped = std::cell::Cell::new(false);
        let made = td.path().join("spool").join("new");
        let err =
            create_dirs_as_root(&made, &swap_for_link("new", rooted.path(), &swapped)).unwrap_err();
        assert!(swapped.get());
        assert!(refused_link(&err), "{err}");
        assert_eq!(owner(rooted.path()), before);
    }
}
