//! `vesyl-print agent` as a process: SIGTERM lets the request in flight
//! finish, the tools the agent runs start with no signal blocked, and the
//! agent refuses to run as root.
//!
//! The agent runs with its config and state in temp dirs, its API on a
//! loopback stub, the cable and the job pull off, and fake CUPS tools and
//! `ip` first on PATH: printer setup finds no printer and no network to scan.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{mpsc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Held while a test writes an executable and while it starts a child: a
/// child forked meanwhile inherits the open script, and running the script
/// then fails with ETXTBSY (see tests/common).
static EXEC_LOCK: Mutex<()> = Mutex::new(());

fn exec_lock() -> MutexGuard<'static, ()> {
    EXEC_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the agent may run: CUPS tools (printer setup, inventory, jobs) and
/// `ip` (the networks printer setup would scan).
const FAKE_TOOLS: &[&str] = &["lp", "lpstat", "lpinfo", "lpoptions", "lpadmin", "ip"];

/// SIGINT and SIGTERM in a /proc `SigBlk` mask.
const STOP_BITS: u64 = (1 << (libc::SIGINT - 1)) | (1 << (libc::SIGTERM - 1));

/// The heartbeat reply's `last_seen_at`.
const LAST_SEEN: &str = "2026-10-08T12:00:00Z";

fn write_exe(path: &Path, text: &str) {
    let _guard = exec_lock();
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A paired print node in a temp dir: config and state directories, a fake
/// token, and fake tools that find nothing and log the signal mask they
/// started with.
struct Node {
    _td: tempfile::TempDir,
    root: PathBuf,
}

impl Node {
    fn new(api_base_url: &str) -> Node {
        let td = tempfile::tempdir().unwrap();
        let node = Node {
            root: td.path().to_path_buf(),
            _td: td,
        };
        fs::create_dir_all(node.config_dir()).unwrap();
        fs::create_dir_all(node.state_dir()).unwrap();
        let config = json!({
            "api_base_url": api_base_url,
            "cable_enabled": false,
            "pull_jobs_enabled": false,
            "auto_update_enabled": false,
            "heartbeat_seconds": 30,
        });
        fs::write(node.config_dir().join("config.json"), config.to_string()).unwrap();
        let creds = json!({"node_id": "node-1", "device_token": "fake-test-token"});
        fs::write(
            node.config_dir().join("credentials.json"),
            creds.to_string(),
        )
        .unwrap();
        let bin = node.root.join("bin");
        fs::create_dir(&bin).unwrap();
        for tool in FAKE_TOOLS {
            let script = format!(
                "#!/bin/sh\n\
                 # Fake {tool}: finds nothing; logs the signal mask it started with.\n\
                 while read -r key value; do\n\
                 \x20   if [ \"$key\" = SigBlk: ]; then echo \"{tool} $value\" >> '{}'; fi\n\
                 done < /proc/$$/status\n\
                 exit 0\n",
                node.sigblk_log().display()
            );
            write_exe(&bin.join(tool), &script);
        }
        node
    }

    fn config_dir(&self) -> PathBuf {
        self.root.join("etc")
    }

    fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }

    fn sigblk_log(&self) -> PathBuf {
        self.root.join("sigblk.log")
    }

    /// `vesyl-print agent` with none of this process's environment.
    fn start_agent(&self) -> Agent {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_vesyl-print"));
        cmd.arg("agent")
            .env_clear()
            .env("PATH", self.root.join("bin"))
            .env("HOME", &self.root)
            .env("VESYL_PRINT_CONFIG_DIR", self.config_dir())
            .env("VESYL_PRINT_STATE_DIR", self.state_dir())
            .env("VESYL_PRINT_INSTALL_ROOT", self.root.join("opt"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = {
            let _guard = exec_lock();
            cmd.spawn().unwrap()
        };
        let mut stderr = child.stderr.take().unwrap();
        let log = thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
            text
        });
        Agent {
            child,
            log: Some(log),
        }
    }

    fn status(&self) -> Value {
        let raw = fs::read_to_string(self.state_dir().join("status.json")).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    /// `(tool, SigBlk)` for every fake tool the agent ran.
    fn tool_masks(&self) -> Vec<(String, u64)> {
        let raw = fs::read_to_string(self.sigblk_log()).unwrap_or_default();
        raw.lines()
            .map(|line| {
                let (tool, mask) = line.split_once(' ').unwrap();
                (tool.to_string(), u64::from_str_radix(mask, 16).unwrap())
            })
            .collect()
    }

    /// Every path under the node's config and state directories.
    fn tree(&self) -> Vec<PathBuf> {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                out.push(path.clone());
                if fs::symlink_metadata(&path).unwrap().is_dir() {
                    walk(&path, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.config_dir(), &mut out);
        walk(&self.state_dir(), &mut out);
        out.sort();
        out
    }
}

/// A running `vesyl-print agent`. Dropped before [`Agent::finish`] (a
/// failed assertion) it kills the agent and prints its log: std leaves a
/// child running, and an agent left behind outlives its temp dir.
struct Agent {
    child: Child,
    /// Collects the agent's log (its stderr); taken by `finish`.
    log: Option<thread::JoinHandle<String>>,
}

impl Drop for Agent {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        // The log ends when the agent's stderr closes: never wait long for
        // it, as a drop must not hang the test.
        let Some(log) = self.log.take() else { return };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !log.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if log.is_finished() {
            if let Ok(log) = log.join() {
                eprintln!("log of the agent the test left running:\n{log}");
            }
        }
    }
}

impl Agent {
    fn signal(&self, sig: libc::c_int) {
        // SAFETY: kill(2) with our own child's pid; no memory is passed.
        assert_eq!(
            unsafe { libc::kill(self.child.id() as libc::pid_t, sig) },
            0
        );
    }

    fn running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    /// `(name, SigBlk)` of every thread of the agent (names cut to 15 bytes).
    fn thread_masks(&self) -> Vec<(String, u64)> {
        let tasks = format!("/proc/{}/task", self.child.id());
        fs::read_dir(tasks)
            .unwrap()
            .filter_map(|task| {
                let status = fs::read_to_string(task.ok()?.path().join("status")).ok()?;
                let field = |key: &str| {
                    let line = status.lines().find(|l| l.starts_with(key))?;
                    Some(line[key.len()..].trim().to_string())
                };
                let mask = u64::from_str_radix(&field("SigBlk:")?, 16).ok()?;
                Some((field("Name:")?, mask))
            })
            .collect()
    }

    /// Wait for the exit (killing the agent after `within`); its exit
    /// status, or `None` when it had to be killed, and its log.
    fn finish(mut self, within: Duration) -> (Option<ExitStatus>, String) {
        let deadline = Instant::now() + within;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break Some(status);
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                break None;
            }
            thread::sleep(Duration::from_millis(20));
        };
        let log = self.log.take().expect("the log is taken once");
        (status, log.join().unwrap())
    }
}

/// Loopback API stub: whoami answers at once; the heartbeat reply is held
/// until the test releases it.
struct Api {
    base_url: String,
    /// One message per heartbeat request, once it has been read.
    heartbeat: mpsc::Receiver<()>,
    release: mpsc::Sender<()>,
}

fn held_heartbeat_api() -> Api {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let (arrived, heartbeat) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let Some(path) = read_request(&mut stream) else {
                continue;
            };
            let (status, body) = match path.as_str() {
                "/print/v1/whoami" => (200, json!({"node_id": "node-1", "name": "Pack 1"})),
                "/print/v1/heartbeat" => {
                    let _ = arrived.send(());
                    let _ = released.recv_timeout(Duration::from_secs(60));
                    (200, json!({"ok": true, "last_seen_at": LAST_SEEN}))
                }
                _ => (404, json!({"error": "not found"})),
            };
            let body = body.to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    Api {
        base_url,
        heartbeat,
        release,
    }
}

/// Read one request; its path.
fn read_request(stream: &mut TcpStream) -> Option<String> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let path = line.split_whitespace().nth(1)?.to_string();
    let mut len = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).ok()? == 0 {
            break;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((k, v)) = header.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).ok()?;
    Some(path)
}

fn running_as_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// Whether a test that runs the agent must be skipped: the agent refuses
/// to run as root, so those tests need the unprivileged pass (the root pass
/// covers the refusal).
fn skip_as_root() -> bool {
    let root = running_as_root();
    if root {
        eprintln!("skipping: the agent refuses to run as root");
    }
    root
}

/// Start the agent and wait until its first heartbeat is waiting on the
/// API. Its first heartbeat waits for the first printer inventory, so by
/// then the inventory has run the fake `lpstat`.
fn agent_in_heartbeat(node: &Node, api: &Api) -> Agent {
    let mut agent = node.start_agent();
    if api.heartbeat.recv_timeout(Duration::from_secs(60)).is_err() {
        let running = agent.running();
        let (_, log) = agent.finish(Duration::ZERO);
        panic!("no heartbeat arrived (agent running: {running})\n{log}");
    }
    // The request has been read: give the agent a moment to wait on the reply.
    thread::sleep(Duration::from_millis(300));
    agent
}

/// N12: a SIGTERM (systemctl stop/restart) used to interrupt the main
/// thread's request with EINTR, so status.json said offline with the error
/// until the next agent's heartbeat. The agent now finishes the heartbeat,
/// then stops and exits 0.
#[test]
fn sigterm_lets_the_heartbeat_in_flight_finish() {
    if skip_as_root() {
        return;
    }
    let api = held_heartbeat_api();
    let node = Node::new(&api.base_url);
    let mut agent = agent_in_heartbeat(&node, &api);

    // Every thread blocks the stop signals, so none can be interrupted by
    // one, except the thread that takes them: sigwait() unblocks what it
    // waits for while it waits.
    let threads = agent.thread_masks();
    let (taker, others): (Vec<_>, Vec<_>) = threads
        .iter()
        .partition(|(name, _)| name == "vesyl-print-sig");
    assert!(
        taker.len() == 1
            && others.len() > 1
            && others.iter().all(|(_, mask)| mask & STOP_BITS == STOP_BITS),
        "thread signal masks {threads:x?}"
    );

    agent.signal(libc::SIGTERM);
    thread::sleep(Duration::from_secs(1));
    let waited = agent.running();
    api.release.send(()).unwrap();
    let (status, log) = agent.finish(Duration::from_secs(20));
    assert!(waited, "the agent quit with the heartbeat in flight\n{log}");
    assert_eq!(status.and_then(|s| s.code()), Some(0), "{status:?}\n{log}");
    assert!(log.contains("agent stopped"), "{log}");
    assert!(!log.contains("os error 4"), "{log}");

    let st = node.status();
    assert_eq!(st["pairing"], "paired", "{st}");
    assert_eq!(st["cloud"], "online", "{st}");
    assert_eq!(st["last_error"], Value::Null, "{st}");
    assert_eq!(st["last_heartbeat_at"], LAST_SEEN, "{st}");

    // The tools it ran (lpstat, lp, …) started with nothing blocked, so a
    // SIGTERM (systemd's stop signals the whole unit) still ends them. The
    // inventory ran lpstat, and printer setup asked the fake `ip` for the
    // networks to scan (and got none).
    let tools = node.tool_masks();
    for wanted in ["lpstat", "ip"] {
        assert!(
            tools.iter().any(|(tool, _)| tool == wanted),
            "no {wanted} ran: {tools:?}"
        );
    }
    for (tool, mask) in &tools {
        assert_eq!(*mask, 0, "{tool} started with signals blocked: {mask:x}");
    }
}

/// A second stop signal (Ctrl-C twice) ends the agent at once, even with a
/// request still in flight; it dies by that signal.
#[test]
fn a_second_signal_quits_at_once() {
    if skip_as_root() {
        return;
    }
    let api = held_heartbeat_api();
    let node = Node::new(&api.base_url);
    let mut agent = agent_in_heartbeat(&node, &api);
    agent.signal(libc::SIGTERM);
    thread::sleep(Duration::from_millis(300));
    if !agent.running() {
        // How it ended tells an exit (its heartbeat ended?) from death by a
        // signal (one sent from elsewhere?).
        let (status, log) = agent.finish(Duration::ZERO);
        let _ = api.release.send(());
        let status = status.expect("the agent has exited");
        panic!(
            "the first signal must not end the agent: exit code {:?}, signal {:?}\n{log}",
            status.code(),
            status.signal()
        );
    }
    agent.signal(libc::SIGINT);
    let (status, log) = agent.finish(Duration::from_secs(10));
    api.release.send(()).unwrap();
    let status = status.unwrap_or_else(|| panic!("still running after a second signal\n{log}"));
    assert_eq!(status.signal(), Some(libc::SIGINT), "{status:?}\n{log}");
}

/// A test that fails between starting the agent and `finish()` must not
/// leave it running: it would outlive its temp dir, recreate the state
/// there, and run the machine's real CUPS tools once the fake ones are gone.
#[test]
fn a_failing_test_leaves_no_agent_behind() {
    if skip_as_root() {
        return;
    }
    let api = held_heartbeat_api();
    let node = Node::new(&api.base_url);
    let agent = agent_in_heartbeat(&node, &api);
    let pid = agent.child.id() as libc::pid_t;
    // What a failed assertion's unwinding does.
    drop(agent);
    // Killed and reaped, it is no child of ours any more: -1 (ECHILD). A
    // child's pid stays ours until it is reaped, so no other process can
    // answer here.
    // SAFETY: waitpid(2) for one pid, with no status to write.
    let found = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
    if found == 0 {
        // Still running: not leaked by this test either.
        // SAFETY: kill(2) and waitpid(2) on our own child, not yet reaped.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
    }
    let _ = api.release.send(());
    assert_eq!(
        found, -1,
        "the agent outlived its handle (0: running, its pid: unreaped)"
    );
}

/// The name of `uid` in /etc/passwd.
fn passwd_name(uid: u32) -> Option<String> {
    let passwd = fs::read_to_string("/etc/passwd").ok()?;
    let uid = uid.to_string();
    passwd.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        (*fields.get(2)? == uid).then(|| fields[0].to_string())
    })
}

/// A3: as root, the agent would leave root-owned files in the service
/// user's state and follow symlinks that user can plant there. It refuses
/// before touching anything, and says how to run it instead.
/// Needs root (or a user namespace):
/// `unshare --map-root-user --map-auto <test binary> --include-ignored`.
#[test]
#[ignore = "needs root (or a user namespace)"]
fn root_agent_refuses_to_run() {
    if !running_as_root() {
        return;
    }
    // Nothing listens on the discard port: an agent that ran would only
    // fail its heartbeats.
    let node = Node::new("http://127.0.0.1:9");
    // As setup.sh leaves them: the service user owns config and state.
    for dir in [node.config_dir(), node.state_dir()] {
        std::os::unix::fs::chown(&dir, Some(1000), Some(1000)).unwrap();
    }
    let before = node.tree();

    let (status, log) = node.start_agent().finish(Duration::from_secs(10));
    let status = status.unwrap_or_else(|| panic!("a root agent ran\n{log}"));
    assert_eq!(status.code(), Some(1), "{log}");
    assert!(log.contains("must not run as root"), "{log}");
    assert!(
        log.contains(&node.state_dir().display().to_string()),
        "{log}"
    );
    let owner = passwd_name(1000).unwrap_or_else(|| "'#1000'".into());
    assert!(
        log.contains(&format!("sudo -u {owner} vesyl-print agent")),
        "{log}"
    );
    assert_eq!(node.tree(), before, "the refused agent touched files");
    assert!(!node.sigblk_log().exists(), "the refused agent ran tools");
    assert_eq!(fs::metadata(node.state_dir()).unwrap().uid(), 1000);
}
