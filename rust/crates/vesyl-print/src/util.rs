//! Small helpers shared across modules: Python-ish JSON coercion, home
//! directories, and durable writes that root can make in the service user's
//! trees.

use std::collections::VecDeque;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use chrono::{SecondsFormat, Utc};
use serde_json::Value;

const LOG: &str = "vesyl-print.util";
/// The uid whose symlinks root's walk ([`walk_dir`]) may follow: root's own.
const ROOT: u32 = 0;
/// Symlinks one walk follows at most, as many as the kernel would.
const MAX_LINKS: usize = 40;
/// Temp file names [`write_durable`] tries before it gives up.
const TEMP_TRIES: usize = 100;

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

/// The home directory Python's `os.path.expanduser("~")`, and so
/// `Path.home()`, finds: `$HOME` when it is set, even to "" (which means
/// `/`), else the passwd entry of the real uid; trailing slashes dropped.
/// `None` when neither is known. `env` reads one environment variable
/// (`None` when unset), so tests can inject it.
pub fn home_dir(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let home = match env("HOME") {
        Some(home) => OsString::from(home),
        // SAFETY: getuid has no preconditions and cannot fail.
        None => passwd_home(Account::Uid(unsafe { libc::getuid() }))?,
    };
    Some(trim_home(home))
}

/// Python's `Path(p).expanduser()`, which the previous agent applied to
/// `local_path` job content and print-test files: `~` and `~/…` start at
/// [`home_dir`], `~name/…` at that account's passwd home. A path whose home
/// cannot be found comes back unchanged.
pub fn expand_user(p: &str) -> PathBuf {
    expand_user_with(p, &|key| std::env::var(key).ok())
}

/// [`expand_user`], reading the environment through `env`.
fn expand_user_with(p: &str, env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    let Some(rest) = p.strip_prefix('~') else {
        return PathBuf::from(p);
    };
    let (user, tail) = rest.split_once('/').unwrap_or((rest, ""));
    let home = if user.is_empty() {
        home_dir(env)
    } else {
        passwd_home(Account::Name(user)).map(trim_home)
    };
    match (home, tail.trim_start_matches('/')) {
        (Some(home), "") => home,
        (Some(home), tail) => home.join(tail),
        (None, _) => PathBuf::from(p),
    }
}

/// `home.rstrip("/") or "/"`: how `os.path.expanduser` uses a home.
fn trim_home(home: OsString) -> PathBuf {
    let bytes = home.as_bytes();
    match bytes.iter().rposition(|&b| b != b'/') {
        Some(last) => PathBuf::from(OsStr::from_bytes(&bytes[..=last])),
        None => PathBuf::from("/"),
    }
}

/// A passwd database entry to look up.
enum Account<'a> {
    Uid(u32),
    Name(&'a str),
}

/// The home directory (`pw_dir`) of `account` (getpwuid_r / getpwnam_r),
/// or `None` when the database has no such entry.
fn passwd_home(account: Account) -> Option<OsString> {
    let name = match account {
        Account::Name(name) => Some(CString::new(name).ok()?),
        Account::Uid(_) => None,
    };
    // SAFETY: sysconf has no preconditions.
    let mut size = match unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) } {
        n if n > 0 => n as usize,
        _ => 1024,
    };
    loop {
        let mut buf = vec![0 as libc::c_char; size];
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: every pointer is valid for the call, and `buf` holds the
        // `size` bytes the call may use for the entry's strings.
        let rc = unsafe {
            match (&account, &name) {
                (Account::Uid(uid), _) => {
                    libc::getpwuid_r(*uid, entry.as_mut_ptr(), buf.as_mut_ptr(), size, &mut found)
                }
                (Account::Name(_), Some(name)) => libc::getpwnam_r(
                    name.as_ptr(),
                    entry.as_mut_ptr(),
                    buf.as_mut_ptr(),
                    size,
                    &mut found,
                ),
                (Account::Name(_), None) => return None,
            }
        };
        if rc == libc::ERANGE && size < 1 << 20 {
            size *= 2;
            continue;
        }
        if rc != 0 || found.is_null() {
            return None;
        }
        // SAFETY: on success `found` points at `entry`, whose strings live in
        // `buf`, NUL-terminated.
        let dir = unsafe { (*found).pw_dir };
        if dir.is_null() {
            return None;
        }
        // SAFETY: as above.
        let dir = unsafe { CStr::from_ptr(dir) };
        return Some(OsStr::from_bytes(dir.to_bytes()).to_owned());
    }
}

/// Write `data` to `path` atomically: a temp file with a unique name in the
/// same directory gets `mode`, the data and an fsync, then is renamed over
/// `path`. With `sync_dir` the directory is fsynced too, and a failure to do
/// so is an error: the file is in place, but might not survive a power cut.
/// A filesystem that cannot fsync a directory at all counts as synced.
///
/// Every call has its own temp file, so concurrent writers (the cable thread
/// and the main loop both save credentials) never write into each other's.
/// The directory is opened once, and the temp file is made, renamed and
/// synced relative to that descriptor, so a name on the way swapped
/// meanwhile cannot move the write anywhere else.
///
/// When root writes (an operator running the CLI), the directory is reached,
/// and made if missing, as [`create_dir_all_owned`] does it: never through a
/// symlink the service user could have planted. The new file keeps the
/// owner of the file it replaces, or of the directory for a new file, so the
/// non-root service can still read it (credentials.json is 0600), and no
/// root-owned file is left in the service user's trees.
pub fn write_durable(path: &Path, data: &[u8], mode: u32, sync_dir: bool) -> io::Result<()> {
    write_durable_with(path, data, mode, sync_dir, root_walk(), &|_| {})
}

/// [`write_durable`]. `walk` is [`root_walk`]'s answer (tests run root's
/// walk unprivileged with their own); `after_open` runs once the directory
/// is open, so tests can swap a name on the way, as the service user could.
fn write_durable_with(
    path: &Path,
    data: &[u8],
    mode: u32,
    sync_dir: bool,
    walk: Option<u32>,
    after_open: &dyn Fn(&Path),
) -> io::Result<()> {
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{}: not a file name", path.display()),
        )
    })?;
    let dir_path = parent_or_cwd(path);
    let dir = match walk {
        Some(trusted) => walk_dir(dir_path, trusted, true, &|_| {})?,
        None => {
            fs::create_dir_all(dir_path)?;
            open_dir(dir_path)?
        }
    };
    after_open(dir_path);
    let (tmp_name, tmp) = create_temp(&dir, name)?;
    let written = (|| {
        keep_service_owner(&tmp, &dir, name, path);
        tmp.set_permissions(fs::Permissions::from_mode(mode))?;
        (&tmp).write_all(data)?;
        tmp.sync_all()?;
        rename_at(&dir, &tmp_name, name)
    })();
    if let Err(e) = written {
        let _ = unlink_at(&dir, &tmp_name);
        return Err(e);
    }
    if sync_dir {
        fsync_dir(&dir)?;
    }
    Ok(())
}

/// The directory `path` is in: its parent, or "." for a bare name.
fn parent_or_cwd(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

/// A new file `.<name>.<random>.tmp` in `dir`, mode 0600, open for writing.
/// O_EXCL: never a file that was there already, nor a symlink planted at
/// that name.
fn create_temp(dir: &File, name: &OsStr) -> io::Result<(OsString, File)> {
    let flags = libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let mut taken = None;
    for _ in 0..TEMP_TRIES {
        let random = uuid::Uuid::new_v4().simple().to_string();
        let mut tmp = OsString::from(".");
        tmp.push(name);
        tmp.push(format!(".{}.tmp", &random[..12]));
        let c = CString::new(tmp.as_bytes())?;
        // SAFETY: `dir` is an open descriptor and `c` a NUL-terminated
        // string; O_CREAT reads the mode passed.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), flags, 0o600 as libc::c_uint) };
        if fd >= 0 {
            // SAFETY: `fd` was just opened here and nothing else owns it.
            return Ok((tmp, unsafe { File::from_raw_fd(fd) }));
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::AlreadyExists {
            return Err(e);
        }
        taken = Some(e);
    }
    Err(taken.unwrap_or_else(|| io::Error::from(io::ErrorKind::AlreadyExists)))
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
/// into place as `name` in `dir`. Best effort: a failure is logged and the
/// write goes ahead.
fn keep_service_owner(tmp: &File, dir: &File, name: &OsStr, path: &Path) {
    let euid = euid();
    if euid != 0 {
        return;
    }
    // lstat: a symlink planted at `name` must not pick who owns the new file
    // (for credentials.json, who can read the token). The rename replaces
    // the link itself, not its target.
    let existing = lstat_at(dir, name).ok().map(|m| owner(&m));
    let dir_owner = dir.metadata().ok().map(|m| owner(&m));
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

/// `Some(ROOT)` when root runs this (an operator's CLI run): its paths are
/// then walked by [`walk_dir`], which follows root's symlinks only. `None`
/// for everyone else, whose paths the kernel resolves as usual.
fn root_walk() -> Option<u32> {
    (euid() == 0).then_some(ROOT)
}

/// `fs::create_dir_all`, except when root runs it (an operator running the
/// CLI). The service user owns the trees root works in here (the state,
/// config and install dirs), so it can rename anything in them and put a
/// symlink in its place, or have planted one already; a mkdir or chown by
/// path would then make, or hand to that user, a directory wherever the
/// link points (say /etc). So root walks the path from `/` (or the working
/// directory) one name at a time, each opened relative to the one before
/// without following a symlink there, and makes a missing directory with
/// mkdirat and hands it, through its descriptor, to the owner of the
/// closest directory that already existed.
///
/// A symlink on the way is followed only when nobody but root could have
/// put it there or can replace it: the link is root's, in a root-owned
/// directory nobody else may write to (or a sticky one, like /tmp, where
/// only a link's owner may rename or remove it). Its target is walked by
/// the same rule, and nothing missing in it is made. So an admin's
/// `/var/lib/vesyl-print -> /data/vesyl-print` still works, while for root
/// a symlink in a directory the service user owns (`queue` inside the
/// state dir, `.config` in its home), or one that user owns, fails the
/// call with ENOTDIR, and a warning names it. As the service user (the
/// agent), nothing changes.
pub fn create_dir_all_owned(dir: &Path) -> io::Result<()> {
    match root_walk() {
        Some(trusted) => walk_dir(dir, trusted, true, &|_| {}).map(drop),
        None => fs::create_dir_all(dir),
    }
}

/// One step of [`walk_dir`].
enum Step {
    /// Start again at `/`.
    Root,
    /// `..`: up from the directory reached so far.
    Up,
    /// A name in the directory reached so far, and whether it may be made
    /// when missing: the caller's names may, those of a symlink's target
    /// never.
    Name(OsString, bool),
}

/// The steps that walk `path`.
fn steps(path: &Path, make: bool) -> Vec<Step> {
    path.components()
        .filter_map(|part| match part {
            Component::RootDir => Some(Step::Root),
            Component::ParentDir => Some(Step::Up),
            Component::Normal(name) => Some(Step::Name(name.to_owned(), make)),
            Component::CurDir | Component::Prefix(_) => None,
        })
        .collect()
}

/// Root's way into the directory `dir`, as [`create_dir_all_owned`]
/// describes it; returns it open. With `make`, missing directories are made
/// and handed to the owner of the closest one that existed. Symlinks are
/// followed by the rule there, with `trusted` as root (tests that run this
/// unprivileged pass their own uid). `after_mkdir` runs right after a
/// directory is made, so tests can swap it for a symlink.
fn walk_dir(dir: &Path, trusted: u32, make: bool, after_mkdir: &dyn Fn(&Path)) -> io::Result<File> {
    let mut todo: VecDeque<Step> = steps(dir, make).into();
    // A relative path starts at the working directory.
    let mut cur = open_dir(Path::new(if dir.has_root() { "/" } else { "." }))?;
    // Where the walk is, for `after_mkdir` and messages.
    let mut at = PathBuf::new();
    // The owner of what this walk makes, from the first one's parent.
    let mut new_owner: Option<Option<Owner>> = None;
    let mut links = 0;
    while let Some(step) = todo.pop_front() {
        let (name, may_make) = match step {
            Step::Root => {
                cur = open_dir(Path::new("/"))?;
                at = PathBuf::from("/");
                continue;
            }
            Step::Up => {
                cur = open_dir_at(&cur, OsStr::new(".."))?;
                at.push("..");
                continue;
            }
            Step::Name(name, may_make) => (name, may_make),
        };
        at.push(&name);
        let opened = match open_dir_at(&cur, &name) {
            Err(e) if may_make && e.kind() == io::ErrorKind::NotFound => {
                // Only a name this walk makes is handed over, never one
                // another writer made first.
                let made = mkdir_at(&cur, &name)?;
                if made {
                    after_mkdir(&at);
                }
                let child = open_dir_at(&cur, &name);
                if let (true, Ok(child)) = (made, &child) {
                    let to =
                        *new_owner.get_or_insert_with(|| cur.metadata().ok().map(|m| owner(&m)));
                    hand_new_dir_over(child, to, &at);
                }
                child
            }
            other => other,
        };
        match opened {
            Ok(child) => cur = child,
            // Past here only a symlink root may follow: its target is
            // walked like the rest, from the link's directory (or `/`).
            Err(_) if followable_link(&cur, &name, trusted, &at) => {
                links += 1;
                if links > MAX_LINKS {
                    return Err(io::Error::from_raw_os_error(libc::ELOOP));
                }
                let target = read_link_at(&cur, &name)?;
                at.pop();
                for step in steps(&target, false).into_iter().rev() {
                    todo.push_front(step);
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(cur)
}

/// Whether `name` in `dir` is a symlink root's walk may follow: only when
/// nobody but root could have put it there or can replace it. The link is
/// `trusted`'s, and so is `dir`, which nobody else may write to unless it
/// is sticky. Any other symlink is refused with a warning naming it (`at`).
fn followable_link(dir: &File, name: &OsStr, trusted: u32, at: &Path) -> bool {
    let Ok(link) = lstat_at(dir, name) else {
        return false;
    };
    if !link.file_type().is_symlink() {
        return false;
    }
    let only_root = dir.metadata().is_ok_and(|d| {
        let others_write = d.mode() & 0o022 != 0;
        let sticky = d.mode() & libc::S_ISVTX != 0;
        d.uid() == trusted && (!others_write || sticky)
    });
    if only_root && link.uid() == trusted {
        return true;
    }
    log::warn!(
        target: LOG,
        "not following symlink {} as root: only a root-owned link in a directory only root can change is followed",
        at.display()
    );
    false
}

/// The directory at `path`, symlinks followed as usual, opened only as a
/// handle to resolve names in (O_PATH: search permission is enough, as for
/// any path through it).
fn open_dir(path: &Path) -> io::Result<File> {
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
        .open(path)
}

/// The directory `name` in `dir`, as [`open_dir`] opens one. A symlink
/// there is not followed: the open fails (ENOTDIR, as O_DIRECTORY is
/// checked first).
fn open_dir_at(dir: &File, name: &OsStr) -> io::Result<File> {
    open_at(
        dir,
        name,
        libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW,
    )
}

/// openat(2) `name` in `dir` with `flags` (never O_CREAT; O_CLOEXEC added).
fn open_at(dir: &File, name: &OsStr, flags: libc::c_int) -> io::Result<File> {
    let c = CString::new(name.as_bytes())?;
    // SAFETY: `dir` is an open descriptor and `c` a NUL-terminated string;
    // without O_CREAT, openat reads no mode argument.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just opened here and nothing else owns it.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// lstat(2) `name` in `dir`: a symlink is described, never followed. O_PATH
/// only locates the file: no read permission needed, a FIFO does not block.
fn lstat_at(dir: &File, name: &OsStr) -> io::Result<fs::Metadata> {
    open_at(dir, name, libc::O_PATH | libc::O_NOFOLLOW)?.metadata()
}

/// readlink(2) `name` in `dir`.
fn read_link_at(dir: &File, name: &OsStr) -> io::Result<PathBuf> {
    let c = CString::new(name.as_bytes())?;
    let mut buf = vec![0u8; 256];
    loop {
        // SAFETY: `dir` is open, `c` NUL-terminated, and `buf` has room for
        // the `buf.len()` bytes readlinkat may write.
        let n = unsafe {
            libc::readlinkat(
                dir.as_raw_fd(),
                c.as_ptr(),
                buf.as_mut_ptr().cast(),
                buf.len(),
            )
        };
        let n = usize::try_from(n).map_err(|_| io::Error::last_os_error())?;
        if n < buf.len() {
            buf.truncate(n);
            return Ok(PathBuf::from(OsString::from_vec(buf)));
        }
        // Perhaps cut short: again, with more room.
        buf.resize(buf.len() * 2, 0);
    }
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

/// Hand `dir`, just made by root's walk (at `at`), to `to` through its
/// descriptor. Best effort: a failure is logged.
fn hand_new_dir_over(dir: &File, to: Option<Owner>, at: &Path) {
    let Some(to) = to else {
        return;
    };
    if let Err(e) = chown_at(dir, OsStr::new(""), to, libc::AT_EMPTY_PATH) {
        log::warn!(target: LOG, "could not hand {} to uid {}: {e}", at.display(), to.0);
    }
}

/// fchownat(2) `name` in `dir` with `flags`: AT_SYMLINK_NOFOLLOW changes a
/// symlink itself, AT_EMPTY_PATH with "" changes `dir` itself.
fn chown_at(dir: &File, name: &OsStr, (uid, gid): Owner, flags: libc::c_int) -> io::Result<()> {
    let c = CString::new(name.as_bytes())?;
    // SAFETY: `dir` is an open descriptor and `c` a NUL-terminated string.
    os_result(unsafe { libc::fchownat(dir.as_raw_fd(), c.as_ptr(), uid, gid, flags) })
}

/// rename(2) `from` to `to`, both in `dir`.
fn rename_at(dir: &File, from: &OsStr, to: &OsStr) -> io::Result<()> {
    let (from, to) = (CString::new(from.as_bytes())?, CString::new(to.as_bytes())?);
    // SAFETY: `dir` is an open descriptor and both strings are NUL-terminated.
    os_result(unsafe {
        libc::renameat(dir.as_raw_fd(), from.as_ptr(), dir.as_raw_fd(), to.as_ptr())
    })
}

/// unlink(2) `name` in `dir`.
fn unlink_at(dir: &File, name: &OsStr) -> io::Result<()> {
    let c = CString::new(name.as_bytes())?;
    // SAFETY: as above.
    os_result(unsafe { libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), 0) })
}

/// A libc return code as an `io::Result` (errno on failure).
fn os_result(rc: libc::c_int) -> io::Result<()> {
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// fsync(2) the directory `dir`, reopened for reading (a handle from
/// [`open_dir`] cannot sync).
fn fsync_dir(dir: &File) -> io::Result<()> {
    let readable = open_at(dir, OsStr::new("."), libc::O_RDONLY | libc::O_DIRECTORY)?;
    dir_synced(readable.sync_all())
}

/// fsync(2) the directory at `path`, for a caller that finds an entry it
/// wrote earlier already in place (a redelivered job's queue file) and must
/// know it is durable before it goes on. Errors as for [`write_durable`]'s
/// `sync_dir`.
pub fn sync_dir(path: &Path) -> io::Result<()> {
    dir_synced(File::open(path)?.sync_all())
}

/// A directory fsync's result. EINVAL or ENOTSUP: the filesystem cannot
/// fsync a directory at all, which counts as done.
fn dir_synced(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(e) if matches!(e.raw_os_error(), Some(libc::EINVAL | libc::ENOTSUP)) => {
            log::debug!(target: LOG, "no directory fsync on this filesystem: {e}");
            Ok(())
        }
        other => other,
    }
}

/// Hand `file`, just created in `dir`, to `dir`'s owner when root created it
/// (an operator running the CLI), as [`write_durable`] does with the files it
/// writes. Through the open file, so nothing is resolved by path. Best
/// effort: a failure is logged.
pub fn hand_new_file_to_dir_owner(file: &File, dir: &Path) {
    let euid = euid();
    if euid != 0 {
        return;
    }
    let dir_owner = fs::metadata(dir).ok().map(|m| owner(&m));
    let Some((uid, gid)) = owner_for_rewrite(euid, None, dir_owner) else {
        return;
    };
    if let Err(e) = std::os::unix::fs::fchown(file, Some(uid), Some(gid)) {
        log::warn!(
            target: LOG,
            "could not hand a new file in {} to uid {uid} gid {gid}: {e}",
            dir.display()
        );
    }
}

/// After root (an operator running the CLI) unpacked a tree into a directory
/// the service user owns — a release slot under `releases/` — hand every
/// entry to that directory's owner, so the non-root agent can later replace
/// or remove it. No-op unless running as root.
///
/// That user may rename anything in a directory once it owns it, so a
/// directory is handed over only after everything in it (until then it is
/// root's, and nothing in it can be swapped), and every change goes through
/// a descriptor: the parent is reached as [`create_dir_all_owned`] reaches
/// it, a directory is entered without following a symlink, and an entry is
/// changed with lchown's semantics, so a symlink in the tree is changed
/// itself and never followed. Names are listed by path: a path swapped
/// meanwhile only changes which names are tried, never what is changed.
pub fn hand_tree_to_parent_owner(tree: &Path) -> io::Result<()> {
    hand_tree_with(tree, root_walk(), &|_| {})
}

/// [`hand_tree_to_parent_owner`]. `walk` is [`root_walk`]'s answer;
/// `before_entries` runs for each directory once it is open, before its
/// entries are handed over, so tests can look at it or swap them.
fn hand_tree_with(
    tree: &Path,
    walk: Option<u32>,
    before_entries: &dyn Fn(&Path),
) -> io::Result<()> {
    let Some(trusted) = walk else {
        return Ok(());
    };
    let name = tree.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{}: not a file name", tree.display()),
        )
    })?;
    let parent = walk_dir(parent_or_cwd(tree), trusted, false, &|_| {})?;
    let to = owner(&parent.metadata()?);
    if to.0 == 0 {
        return Ok(());
    }
    hand_entry_over(&parent, name, tree, to, before_entries)
}

/// Hand `name` in `dir` (at `path`), and when it is a directory everything
/// in it first, to `to`.
fn hand_entry_over(
    dir: &File,
    name: &OsStr,
    path: &Path,
    to: Owner,
    before_entries: &dyn Fn(&Path),
) -> io::Result<()> {
    if !lstat_at(dir, name)?.is_dir() {
        return chown_at(dir, name, to, libc::AT_SYMLINK_NOFOLLOW);
    }
    let sub = open_dir_at(dir, name)?;
    before_entries(path);
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        hand_entry_over(&sub, &entry.file_name(), &entry.path(), to, before_entries)?;
    }
    chown_at(&sub, OsStr::new(""), to, libc::AT_EMPTY_PATH)
}

pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::{Cell, RefCell};

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

    /// `pw_dir` of the /etc/passwd line `pick` accepts (its fields), to check
    /// the passwd lookup against; `None` for an account only NSS knows.
    fn etc_passwd_home(pick: impl Fn(&[&str]) -> bool) -> Option<PathBuf> {
        fs::read_to_string("/etc/passwd")
            .ok()?
            .lines()
            .find_map(|line| {
                let fields: Vec<&str> = line.split(':').collect();
                (fields.len() >= 7 && pick(&fields)).then(|| PathBuf::from(fields[5]))
            })
    }

    /// This process's real uid's home in /etc/passwd.
    fn own_passwd_home() -> Option<PathBuf> {
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() }.to_string();
        etc_passwd_home(|f| f[2] == uid)
    }

    /// An environment with only HOME set.
    fn home(value: &'static str) -> impl Fn(&str) -> Option<String> {
        move |key| (key == "HOME").then(|| value.to_string())
    }

    /// `Path.home()`: HOME wins even when empty ("/"), trailing slashes go,
    /// and without HOME the passwd entry is used, never `/`.
    #[test]
    fn home_dir_is_pythons_path_home() {
        assert_eq!(home_dir(&home("/home/vesyl")), Some("/home/vesyl".into()));
        assert_eq!(home_dir(&home("/home/vesyl//")), Some("/home/vesyl".into()));
        assert_eq!(home_dir(&home("")), Some("/".into()));
        assert_eq!(home_dir(&home("/")), Some("/".into()));
        assert_eq!(home_dir(&home("relative")), Some("relative".into()));
        if let Some(passwd) = own_passwd_home() {
            assert_eq!(home_dir(&|_| None), Some(passwd.clone()));
            assert_ne!(passwd, Path::new("/"));
        }
    }

    /// `Path(p).expanduser()`, as CPython answers it.
    #[test]
    fn expand_user_matches_python() {
        let set = home("/home/vesyl/");
        for (p, want) in [
            ("~/x", "/home/vesyl/x"),
            ("~", "/home/vesyl"),
            ("~/", "/home/vesyl"),
            ("~//x/y", "/home/vesyl/x/y"),
            ("x/~", "x/~"),
            ("/abs/~/x", "/abs/~/x"),
            (
                "~no-such-user-vesyl-print-test/x",
                "~no-such-user-vesyl-print-test/x",
            ),
        ] {
            assert_eq!(expand_user_with(p, &set), Path::new(want), "{p}");
        }
        // HOME="" is `/`, as in Python (it once gave the relative "x").
        assert_eq!(expand_user_with("~/x", &home("")), Path::new("/x"));
        assert_eq!(expand_user_with("~", &home("")), Path::new("/"));
        // Unset HOME: the passwd home (it once stayed "~/x").
        if let Some(passwd) = own_passwd_home() {
            assert_eq!(expand_user_with("~/x", &|_| None), passwd.join("x"));
        }
        // `~name`: that account's passwd home.
        if let Some(root) = etc_passwd_home(|f| f[0] == "root") {
            assert_eq!(expand_user_with("~root/x", &set), root.join("x"));
        }
    }

    const HOME_CHILD: &str = "VESYL_TEST_UTIL_HOME_CHILD";

    /// With HOME unset in the real environment, `~` is the passwd home, not
    /// `/` and not left as it was. HOME is process-global, so the check runs
    /// in a child copy of this test binary started without it.
    #[test]
    fn unset_home_uses_the_passwd_entry() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "util::tests::unset_home_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_remove("HOME")
            .env(HOME_CHILD, "1")
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
    #[ignore = "child process of unset_home_uses_the_passwd_entry"]
    fn unset_home_child() {
        if std::env::var_os(HOME_CHILD).is_none() {
            return;
        }
        assert!(std::env::var_os("HOME").is_none());
        let Some(passwd) = own_passwd_home() else {
            println!("no /etc/passwd entry for this uid; nothing to compare");
            return;
        };
        assert_eq!(expand_user("~/label.pdf"), passwd.join("label.pdf"));
        assert_eq!(home_dir(&|key| std::env::var(key).ok()), Some(passwd));
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
        // A path without a file name is refused before anything is touched.
        let err = write_durable(Path::new("/"), b"x", 0o600, false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    /// The agent's (non-root) writes resolve the directory as before: a
    /// symlinked directory is followed.
    #[test]
    fn service_user_writes_follow_a_symlinked_dir() {
        let td = tempfile::tempdir().unwrap();
        let real = td.path().join("real");
        fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, td.path().join("state")).unwrap();
        let path = td.path().join("state").join("status.json");
        write_durable_with(&path, b"{}", 0o600, true, None, &|_| {}).unwrap();
        assert_eq!(fs::read(real.join("status.json")).unwrap(), b"{}");
    }

    /// Renames `name` (in `dir`) away and puts a symlink to `target` there,
    /// as the service user could at any moment in a directory it owns.
    fn swap_dir_for_link(dir: &Path, target: &Path) {
        let mut away = dir.as_os_str().to_owned();
        away.push(".moved");
        fs::rename(dir, away).unwrap();
        std::os::unix::fs::symlink(target, dir).unwrap();
    }

    /// Once the directory is open, swapping it (or a name above it) for a
    /// symlink cannot move the write: the file lands in the directory that
    /// was opened, for the agent and for root alike, and nothing is written
    /// where the link points.
    #[test]
    fn a_dir_swapped_after_the_open_cannot_redirect_the_write() {
        for walk in [None, Some(ROOT)] {
            let td = tempfile::tempdir().unwrap();
            let elsewhere = tempfile::tempdir().unwrap();
            let queue = td.path().join("state").join("queue");
            fs::create_dir_all(&queue).unwrap();
            let path = queue.join("job-1.json");
            let swap = |_: &Path| swap_dir_for_link(&td.path().join("state"), elsewhere.path());
            write_durable_with(&path, b"{\"id\":1}", 0o600, true, walk, &swap).unwrap();
            let landed = td.path().join("state.moved/queue/job-1.json");
            assert_eq!(fs::read(&landed).unwrap(), b"{\"id\":1}", "{walk:?}");
            assert_eq!(
                fs::read_dir(elsewhere.path()).unwrap().count(),
                0,
                "{walk:?}"
            );
            let left: Vec<_> = fs::read_dir(td.path().join("state.moved/queue"))
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(left, ["job-1.json"], "temp file left behind ({walk:?})");
        }
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
        swapped: &'a Cell<bool>,
    ) -> impl Fn(&Path) + 'a {
        move |made: &Path| {
            if made.ends_with(name) && !swapped.get() {
                swap_dir_for_link(made, target);
                swapped.set(true);
            }
        }
    }

    /// The walk's open met the planted link and refused it: ENOTDIR on Linux,
    /// where O_DIRECTORY is checked before O_NOFOLLOW's ELOOP.
    fn refused_link(e: &io::Error) -> bool {
        matches!(e.raw_os_error(), Some(libc::ENOTDIR | libc::ELOOP))
    }

    /// A temp dir standing in for a tree the service user owns: as root
    /// (`unshare`), it is handed to uid 1000, so links root plants in it
    /// are no more trusted than the service user's.
    fn service_tree() -> tempfile::TempDir {
        let td = tempfile::tempdir().unwrap();
        if euid() == 0 {
            std::os::unix::fs::chown(td.path(), Some(1000), Some(1000)).unwrap();
        }
        td
    }

    #[test]
    fn root_dir_walk_never_follows_a_swapped_in_symlink() {
        // Root's walk, run unprivileged: handing a directory to our own uid
        // is allowed, so everything but the ownership itself is exercised.
        let td = service_tree();
        let deep = td.path().join("queue").join("new");
        walk_dir(&deep, ROOT, true, &|_| {}).unwrap();
        assert!(deep.is_dir());
        walk_dir(&deep, ROOT, true, &|_| {}).unwrap();
        let rel = td.path().join("rel");
        walk_dir(&rel.join("..").join("rel").join("x"), ROOT, true, &|_| {}).unwrap();
        assert!(rel.join("x").is_dir());

        // "jobs" is swapped for a link before root opens it, so "new" would
        // be made, and chowned, inside the link's target (/etc, say).
        let elsewhere = tempfile::tempdir().unwrap();
        let swapped = Cell::new(false);
        let err = walk_dir(
            &td.path().join("jobs").join("new"),
            ROOT,
            true,
            &swap_for_link("jobs", elsewhere.path(), &swapped),
        )
        .unwrap_err();
        assert!(swapped.get());
        assert!(refused_link(&err), "{err}");
        assert_eq!(fs::read_dir(elsewhere.path()).unwrap().count(), 0);

        // The same for the last directory: nothing past the link is touched.
        let swapped = Cell::new(false);
        let err = walk_dir(
            &td.path().join("spool").join("new"),
            ROOT,
            true,
            &swap_for_link("new", elsewhere.path(), &swapped),
        )
        .unwrap_err();
        assert!(swapped.get());
        assert!(refused_link(&err), "{err}");
        assert_eq!(fs::read_dir(elsewhere.path()).unwrap().count(), 0);

        // Without making anything, a missing directory is just missing.
        let err = walk_dir(&td.path().join("absent"), ROOT, false, &|_| {}).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(!td.path().join("absent").exists());
    }

    /// A symlink already in the service user's tree (no race needed) never
    /// takes root's write, or a directory root makes, into its target.
    #[test]
    fn root_refuses_a_symlink_planted_in_the_service_tree() {
        let td = service_tree();
        let elsewhere = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), td.path().join("queue")).unwrap();
        for path in [
            td.path().join("queue").join("job-1.json"),
            td.path().join("queue").join("new").join("job-1.json"),
        ] {
            let err =
                write_durable_with(&path, b"{}", 0o600, true, Some(ROOT), &|_| {}).unwrap_err();
            assert!(refused_link(&err), "{err}");
        }
        assert_eq!(fs::read_dir(elsewhere.path()).unwrap().count(), 0);
    }

    /// Run unprivileged, the walk trusts our own uid as it trusts root: a
    /// link ours, in a directory only we can change, is followed and its
    /// target walked by the same rule, so the write lands there; one in a
    /// directory others may write to is not, unless that directory is
    /// sticky; and a trusted link's target is never made.
    #[test]
    fn root_walk_follows_only_links_nobody_else_could_plant() {
        let me = euid();
        let td = tempfile::tempdir().unwrap();
        let disk = tempfile::tempdir().unwrap();
        let data = disk.path().join("vesyl-print");
        fs::create_dir(&data).unwrap();
        // /var/lib/vesyl-print -> /data/vesyl-print, made by the admin.
        let lib = td.path().join("lib");
        fs::create_dir(&lib).unwrap();
        set_mode(&lib, 0o755).unwrap();
        std::os::unix::fs::symlink(&data, lib.join("vesyl-print")).unwrap();
        let path = lib.join("vesyl-print").join("queue").join("job-1.json");
        write_durable_with(&path, b"{}", 0o600, true, Some(me), &|_| {}).unwrap();
        assert_eq!(fs::read(data.join("queue/job-1.json")).unwrap(), b"{}");

        // Others may write next to the link: refused, unless sticky.
        let shared = td.path().join("shared");
        fs::create_dir(&shared).unwrap();
        std::os::unix::fs::symlink(&data, shared.join("state")).unwrap();
        let in_shared = shared.join("state").join("status.json");
        for (mode, followed) in [(0o777, false), (0o775, false), (0o1777, true)] {
            set_mode(&shared, mode).unwrap();
            let result = write_durable_with(&in_shared, b"{}", 0o600, false, Some(me), &|_| {});
            assert_eq!(result.is_ok(), followed, "mode {mode:o}: {result:?}");
        }
        set_mode(&shared, 0o755).unwrap();
        assert!(data.join("status.json").is_file());

        // A trusted link whose target goes through one in a shared directory
        // is refused there.
        set_mode(&shared, 0o777).unwrap();
        std::os::unix::fs::symlink(shared.join("state"), td.path().join("hop")).unwrap();
        let err = walk_dir(&td.path().join("hop"), me, true, &|_| {}).unwrap_err();
        assert!(refused_link(&err), "{err}");
        set_mode(&shared, 0o755).unwrap();

        // A dangling link: its target is not made.
        std::os::unix::fs::symlink(disk.path().join("unmounted"), td.path().join("gone")).unwrap();
        let err = walk_dir(&td.path().join("gone").join("queue"), me, true, &|_| {}).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(!disk.path().join("unmounted").exists());

        // A loop of trusted links ends.
        std::os::unix::fs::symlink(td.path().join("b"), td.path().join("a")).unwrap();
        std::os::unix::fs::symlink(td.path().join("a"), td.path().join("b")).unwrap();
        let err = walk_dir(&td.path().join("a"), me, true, &|_| {}).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::ELOOP), "{err}");
    }

    /// A directory fsync that fails is an error now (it was dropped), so a
    /// queue entry is never taken for durable when it may not be; without
    /// `sync_dir` the same write still succeeds. EINVAL / ENOTSUP mean the
    /// filesystem has no directory fsync, which counts as done.
    #[test]
    fn dir_fsync_failures_are_returned() {
        assert!(dir_synced(Err(io::Error::from_raw_os_error(libc::EINVAL))).is_ok());
        assert!(dir_synced(Err(io::Error::from_raw_os_error(libc::ENOTSUP))).is_ok());
        assert!(dir_synced(Err(io::Error::from_raw_os_error(libc::EIO))).is_err());
        if euid() == 0 {
            // Root reads any directory; the mode below cannot stop it.
            return;
        }
        let td = tempfile::tempdir().unwrap();
        let queue = td.path().join("queue");
        fs::create_dir(&queue).unwrap();
        // Writable and searchable, but not readable: it cannot be opened
        // for the fsync.
        set_mode(&queue, 0o300).unwrap();
        let path = queue.join("job-1.json");
        write_durable(&path, b"{}", 0o600, false).unwrap();
        let err = write_durable(&path, b"{}", 0o600, true).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        assert_eq!(
            sync_dir(&queue).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        set_mode(&queue, 0o755).unwrap();
        sync_dir(&queue).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"{}");
    }

    /// Needs root: `sudo cargo test`, or unprivileged with
    /// `unshare --map-root-user --map-auto cargo test -- --ignored root_write`.
    #[test]
    #[ignore = "needs root (or a user namespace) to chown"]
    fn root_hands_an_unpacked_tree_to_the_parent_owner() {
        if euid() != 0 {
            return;
        }
        let td = tempfile::tempdir().unwrap();
        let releases = td.path().join("releases");
        fs::create_dir(&releases).unwrap();
        std::os::unix::fs::chown(&releases, Some(1000), Some(1000)).unwrap();
        let slot = releases.join("0.9.0");
        fs::create_dir_all(slot.join("assets")).unwrap();
        fs::write(slot.join("vesyl-print"), b"bin").unwrap();
        fs::write(slot.join("assets/logo.png"), b"png").unwrap();
        // A symlink in the tree is changed itself, never its target.
        let outside = td.path().join("outside");
        fs::write(&outside, b"x").unwrap();
        std::os::unix::fs::symlink(&outside, slot.join("link")).unwrap();
        hand_tree_to_parent_owner(&slot).unwrap();
        for p in [
            &slot,
            &slot.join("assets"),
            &slot.join("vesyl-print"),
            &slot.join("assets/logo.png"),
        ] {
            assert_eq!(fs::metadata(p).unwrap().uid(), 1000, "{}", p.display());
        }
        assert_eq!(fs::symlink_metadata(slot.join("link")).unwrap().uid(), 1000);
        assert_eq!(
            fs::metadata(&outside).unwrap().uid(),
            0,
            "symlink target untouched"
        );
        // Directories created for a new slot get the owner too.
        let update = td.path().join("install").join("update");
        std::os::unix::fs::chown(td.path(), Some(1000), Some(1000)).unwrap();
        create_dir_all_owned(&update).unwrap();
        assert_eq!(fs::metadata(&update).unwrap().uid(), 1000);
    }

    /// The tree goes over children first: every directory is still root's
    /// while its entries are handed over (so the service user cannot swap
    /// them yet), and an entry swapped for a symlink anyway is changed
    /// itself: nothing is chowned where it points. Needs root (or a user
    /// namespace).
    #[test]
    #[ignore = "needs root (or a user namespace) to chown"]
    fn root_tree_handover_goes_children_first_through_descriptors() {
        if euid() != 0 {
            return;
        }
        let td = tempfile::tempdir().unwrap();
        let releases = td.path().join("releases");
        fs::create_dir(&releases).unwrap();
        std::os::unix::fs::chown(&releases, Some(1000), Some(1000)).unwrap();
        let slot = releases.join("0.9.0");
        fs::create_dir_all(slot.join("assets/icons")).unwrap();
        fs::write(slot.join("assets/icons/wifi.png"), b"png").unwrap();
        fs::create_dir(slot.join("lib")).unwrap();
        fs::write(slot.join("lib/a.py"), b"py").unwrap();
        let victim = tempfile::tempdir().unwrap();
        fs::write(victim.path().join("passwd"), b"root:x:0:0").unwrap();

        let owners = RefCell::new(Vec::new());
        let before_entries = |dir: &Path| {
            owners
                .borrow_mut()
                .push((dir.to_path_buf(), fs::metadata(dir).unwrap().uid()));
            if dir == slot.as_path() {
                // lib/ goes, a link to the victim takes its name.
                swap_dir_for_link(&slot.join("lib"), victim.path());
            }
        };
        hand_tree_with(&slot, Some(ROOT), &before_entries).unwrap();
        let owners = owners.into_inner();
        assert_eq!(owners.len(), 4, "{owners:?}");
        assert!(owners.iter().all(|(_, uid)| *uid == 0), "{owners:?}");
        for p in [
            slot.clone(),
            slot.join("assets"),
            slot.join("assets/icons"),
            slot.join("assets/icons/wifi.png"),
            slot.join("lib.moved"),
            slot.join("lib.moved/a.py"),
            slot.join("lib"),
        ] {
            let m = fs::symlink_metadata(&p).unwrap();
            assert_eq!((m.uid(), m.gid()), (1000, 1000), "{}", p.display());
        }
        for p in [victim.path().to_path_buf(), victim.path().join("passwd")] {
            assert_eq!(fs::metadata(&p).unwrap().uid(), 0, "{}", p.display());
        }
    }

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
        let swapped = Cell::new(false);
        let made = td.path().join("jobs").join("new");
        let err = walk_dir(
            &made,
            ROOT,
            true,
            &swap_for_link("jobs", rooted.path(), &swapped),
        )
        .unwrap_err();
        assert!(swapped.get());
        assert!(refused_link(&err), "{err}");
        assert_eq!(owner(rooted.path()), before);
        assert_eq!(fs::read_dir(rooted.path()).unwrap().count(), 0);
        let swapped = Cell::new(false);
        let made = td.path().join("spool").join("new");
        let err = walk_dir(
            &made,
            ROOT,
            true,
            &swap_for_link("new", rooted.path(), &swapped),
        )
        .unwrap_err();
        assert!(swapped.get());
        assert!(refused_link(&err), "{err}");
        assert_eq!(owner(rooted.path()), before);
    }

    /// Root's writes into the service user's tree, as an operator's CLI run
    /// makes them: a symlink the service user planted (or one root made in
    /// that tree) is refused, a directory swapped for one after the walk
    /// cannot redirect the write, and an admin's root-owned link in a
    /// root-owned directory (/var/lib/vesyl-print -> /data/vesyl-print) is
    /// still followed, the files going to the owner of its target. Needs
    /// root (or a user namespace).
    #[test]
    #[ignore = "needs root (or a user namespace) to chown"]
    fn root_write_never_follows_the_service_users_symlinks() {
        if euid() != 0 {
            return;
        }
        let owner = |p: &Path| {
            let m = fs::symlink_metadata(p).unwrap();
            (m.uid(), m.gid())
        };
        let td = service_tree();
        let rooted = tempfile::tempdir().unwrap();
        let before = owner(rooted.path());
        // Planted beforehand, by the service user or by root.
        for link_owner in [1000, 0] {
            let link = td.path().join(format!("queue-{link_owner}"));
            std::os::unix::fs::symlink(rooted.path(), &link).unwrap();
            std::os::unix::fs::lchown(&link, Some(link_owner), Some(link_owner)).unwrap();
            for path in [link.join("job-1.json"), link.join("new").join("job-1.json")] {
                let err = write_durable(&path, b"{}", 0o600, true).unwrap_err();
                assert!(refused_link(&err), "{err}");
            }
            let err = create_dir_all_owned(&link.join("new")).unwrap_err();
            assert!(refused_link(&err), "{err}");
        }
        // Swapped in after the walk opened the directory.
        let queue = td.path().join("state").join("queue");
        create_dir_all_owned(&queue).unwrap();
        let swap = |_: &Path| swap_dir_for_link(&queue, rooted.path());
        write_durable_with(
            &queue.join("job-2.json"),
            b"{}",
            0o600,
            true,
            Some(ROOT),
            &swap,
        )
        .unwrap();
        let landed = td.path().join("state/queue.moved/job-2.json");
        assert_eq!(owner(&landed), (1000, 1000));
        assert_eq!(fs::read_dir(rooted.path()).unwrap().count(), 0);
        assert_eq!(owner(rooted.path()), before);

        // An admin's link: root's, in a root-owned 0755 directory.
        let lib = tempfile::tempdir().unwrap();
        set_mode(lib.path(), 0o755).unwrap();
        let disk = tempfile::tempdir().unwrap();
        let data = disk.path().join("vesyl-print");
        fs::create_dir(&data).unwrap();
        std::os::unix::fs::chown(&data, Some(1000), Some(1000)).unwrap();
        std::os::unix::fs::symlink(&data, lib.path().join("vesyl-print")).unwrap();
        let status = lib.path().join("vesyl-print/queue/job-3.json");
        write_durable(&status, b"{}", 0o600, true).unwrap();
        assert_eq!(owner(&data.join("queue")), (1000, 1000));
        assert_eq!(owner(&data.join("queue/job-3.json")), (1000, 1000));
        // The same link, the service user's: refused.
        std::os::unix::fs::lchown(lib.path().join("vesyl-print"), Some(1000), Some(1000)).unwrap();
        let err = write_durable(&status, b"{}", 0o600, true).unwrap_err();
        assert!(refused_link(&err), "{err}");
    }
}
