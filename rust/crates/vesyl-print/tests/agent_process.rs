//! `vesyl-print agent` as a process: SIGTERM lets the request in flight
//! finish but starts no update, the tools the agent runs start with no
//! signal blocked and untranslated, the agent refuses to run as root, and
//! its jobs survive stops, crashes and a health gate's restart: a stop
//! during a CUPS wait ends it, and the job is not printed twice; a job that
//! keeps killing the agent is retired; jobs wait for the restart a rollback
//! asked for.
//!
//! The agent runs with its config and state in temp dirs, a German locale,
//! its API on a loopback stub, the cable and the job pull off, and fake CUPS
//! tools, `ip` and restart tools first on PATH: printer setup finds no
//! printer and no network to scan, and an update's restart would only be
//! logged.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
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

/// What an update's restart runs: `systemctl restart --no-block <unit>`, or
/// `sudo -n <apply-update helper> restart` where the helper is installed.
const RESTART_TOOLS: &[&str] = &["systemctl", "sudo"];

/// SIGINT and SIGTERM in a /proc `SigBlk` mask.
const STOP_BITS: u64 = (1 << (libc::SIGINT - 1)) | (1 << (libc::SIGTERM - 1));

/// The heartbeat reply's `last_seen_at`.
const LAST_SEEN: &str = "2026-10-08T12:00:00Z";

const PNG_1X1_B64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

fn write_exe(path: &Path, text: &str) {
    let _guard = exec_lock();
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A paired print node in a temp dir: config and state directories, a fake
/// token, fake tools that find nothing and log the signal mask they started
/// with, and fake restart tools that log how they were called.
struct Node {
    _td: tempfile::TempDir,
    root: PathBuf,
}

impl Node {
    fn new(api_base_url: &str) -> Node {
        Node::with_config(api_base_url, json!({}))
    }

    /// A node whose config.json also has `overrides`.
    fn with_config(api_base_url: &str, overrides: Value) -> Node {
        let td = tempfile::tempdir().unwrap();
        let node = Node {
            root: td.path().to_path_buf(),
            _td: td,
        };
        fs::create_dir_all(node.config_dir()).unwrap();
        fs::create_dir_all(node.state_dir()).unwrap();
        let mut config = json!({
            "api_base_url": api_base_url,
            "cable_enabled": false,
            "pull_jobs_enabled": false,
            "auto_update_enabled": false,
            "heartbeat_seconds": 30,
        });
        if let Value::Object(overrides) = overrides {
            config.as_object_mut().unwrap().extend(overrides);
        }
        fs::write(node.config_dir().join("config.json"), config.to_string()).unwrap();
        let creds = json!({"node_id": "node-1", "device_token": "fake-test-token"});
        fs::write(
            node.config_dir().join("credentials.json"),
            creds.to_string(),
        )
        .unwrap();
        let bin = node.root.join("bin");
        fs::create_dir(&bin).unwrap();
        fs::create_dir(node.root.join("tmp")).unwrap();
        for tool in FAKE_TOOLS {
            let script = format!(
                "#!/bin/sh\n\
                 # Fake {tool}: finds nothing; logs the signal mask and the\n\
                 # locale it started with.\n\
                 echo \"{tool} ${{LC_ALL:-unset}} ${{LC_MESSAGES:-unset}}\" >> '{}'\n\
                 while read -r key value; do\n\
                 \x20   if [ \"$key\" = SigBlk: ]; then echo \"{tool} $value\" >> '{}'; fi\n\
                 done < /proc/$$/status\n\
                 exit 0\n",
                node.locale_log().display(),
                node.sigblk_log().display()
            );
            write_exe(&bin.join(tool), &script);
        }
        for tool in RESTART_TOOLS {
            let script = format!(
                "#!/bin/sh\n\
                 # Fake {tool}: logs how it was called.\n\
                 echo \"{tool} $*\" >> '{}'\n",
                node.restart_log().display()
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

    /// Where the releases and the `current` link would go.
    fn install_root(&self) -> PathBuf {
        self.root.join("opt")
    }

    fn sigblk_log(&self) -> PathBuf {
        self.root.join("sigblk.log")
    }

    fn locale_log(&self) -> PathBuf {
        self.root.join("locale.log")
    }

    fn restart_log(&self) -> PathBuf {
        self.root.join("restarts.log")
    }

    /// Where the fake `lp` of [`Node::fake_lp`] logs each run.
    fn lp_log(&self) -> PathBuf {
        self.root.join("lp.log")
    }

    /// Replace fake `tool` with a script running `body`.
    fn fake(&self, tool: &str, body: &str) {
        write_exe(
            &self.root.join("bin").join(tool),
            &format!("#!/bin/sh\n{body}\n"),
        );
    }

    /// A fake `lp` that logs each run, then runs `then`.
    fn fake_lp(&self, then: &str) {
        let log = self.lp_log();
        self.fake(
            "lp",
            &format!("echo \"lp $*\" >> '{}'\n{then}", log.display()),
        );
    }

    /// How many times the fake `lp` ran.
    fn lp_runs(&self) -> usize {
        fs::read_to_string(self.lp_log())
            .unwrap_or_default()
            .lines()
            .count()
    }

    fn queue_path(&self, id: &str) -> PathBuf {
        self.state_dir().join("queue").join(format!("{id}.json"))
    }

    /// Leave job `id` (a 1x1 PNG for queue Zebra) queued, as a job the
    /// agent took before it stopped.
    fn queue_job(&self, id: &str) {
        let job = json!({"id": id, "cups_name": "Zebra", "content_type": "png_base64",
                         "content": PNG_1X1_B64, "title": "Ship label"});
        fs::create_dir_all(self.state_dir().join("queue")).unwrap();
        fs::write(self.queue_path(id), job.to_string()).unwrap();
    }

    /// The agent's notes in job `id`'s queue record.
    fn notes(&self, id: &str) -> Value {
        let raw = fs::read_to_string(self.queue_path(id)).unwrap();
        serde_json::from_str::<Value>(&raw).unwrap()["_agent"].clone()
    }

    fn processed(&self, id: &str) -> bool {
        self.state_dir().join("processed").join(id).is_file()
    }

    /// Every call of a restart tool, as `tool args…`.
    fn restarts(&self) -> Vec<String> {
        let raw = fs::read_to_string(self.restart_log()).unwrap_or_default();
        raw.lines().map(String::from).collect()
    }

    /// `vesyl-print agent` with none of this process's environment, on a
    /// node whose locale is German (CUPS would answer in German).
    fn start_agent(&self) -> Agent {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_vesyl-print"));
        cmd.arg("agent")
            .env_clear()
            .env("PATH", self.root.join("bin"))
            .env("HOME", &self.root)
            .env("TMPDIR", self.root.join("tmp"))
            .env("LANG", "de_DE.UTF-8")
            .env("LC_MESSAGES", "de_DE.UTF-8")
            .env("VESYL_PRINT_CONFIG_DIR", self.config_dir())
            .env("VESYL_PRINT_STATE_DIR", self.state_dir())
            .env("VESYL_PRINT_INSTALL_ROOT", self.install_root())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = {
            let _guard = exec_lock();
            cmd.spawn().unwrap()
        };
        let mut stderr = BufReader::new(child.stderr.take().unwrap());
        let (line_tx, lines) = mpsc::channel();
        let log = thread::spawn(move || {
            let (mut text, mut line) = (String::new(), Vec::new());
            while stderr.read_until(b'\n', &mut line).is_ok_and(|n| n > 0) {
                let line = String::from_utf8_lossy(&std::mem::take(&mut line)).into_owned();
                text.push_str(&line);
                let _ = line_tx.send(line);
            }
            text
        });
        Agent {
            child,
            log: Some(log),
            lines,
        }
    }

    fn status(&self) -> Value {
        let raw = fs::read_to_string(self.state_dir().join("status.json")).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    /// `(tool, "LC_ALL LC_MESSAGES")` for every fake tool the agent ran.
    fn tool_locales(&self) -> Vec<(String, String)> {
        let raw = fs::read_to_string(self.locale_log()).unwrap_or_default();
        raw.lines()
            .map(|line| {
                let (tool, locale) = line.split_once(' ').unwrap();
                (tool.to_string(), locale.to_string())
            })
            .collect()
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
    /// Each log line as it comes.
    lines: mpsc::Receiver<String>,
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

    /// Whether the agent logs a line containing `needle` within `within`.
    fn logged(&self, needle: &str, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) if line.contains(needle) => return true,
                Ok(_) => {}
                Err(_) => return false,
            }
        }
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
    /// The path of every request, in order.
    paths: Arc<Mutex<Vec<String>>>,
}

impl Api {
    fn paths(&self) -> Vec<String> {
        self.paths.lock().unwrap().clone()
    }
}

/// What the API stub serves besides whoami and the heartbeat.
#[derive(Default)]
struct Served {
    /// An object whose fields are added to the heartbeat reply (an update
    /// directive).
    heartbeat: Value,
    /// Other paths and their bodies (a release manifest and artifact).
    files: Vec<(String, Vec<u8>)>,
}

fn held_heartbeat_api() -> Api {
    held_heartbeat_api_serving(|_| Served::default())
}

/// [`held_heartbeat_api`] also serving what `served` makes of its base URL.
fn held_heartbeat_api_serving(served: impl FnOnce(&str) -> Served) -> Api {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let served = served(&base_url);
    let (arrived, heartbeat) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let paths: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = paths.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let Some((path, _)) = read_request(&mut stream) else {
                continue;
            };
            seen.lock().unwrap().push(path.clone());
            let api_reply =
                |status: u16, body: Value| (status, "application/json", body.to_string().into());
            let (status, kind, body): (_, _, Vec<u8>) = match path.as_str() {
                "/print/v1/whoami" => {
                    api_reply(200, json!({"node_id": "node-1", "name": "Pack 1"}))
                }
                "/print/v1/heartbeat" => {
                    let _ = arrived.send(());
                    let _ = released.recv_timeout(Duration::from_secs(60));
                    let mut reply = json!({"ok": true, "last_seen_at": LAST_SEEN});
                    if let (Some(reply), Some(extra)) =
                        (reply.as_object_mut(), served.heartbeat.as_object())
                    {
                        reply.extend(extra.clone());
                    }
                    api_reply(200, reply)
                }
                other => match served.files.iter().find(|(p, _)| p == other) {
                    Some((_, bytes)) => (200, "application/octet-stream", bytes.clone()),
                    None => api_reply(404, json!({"error": "not found"})),
                },
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(&body);
        }
    });
    Api {
        base_url,
        heartbeat,
        release,
        paths,
    }
}

/// A release artifact for `version`, laid out as build-release.sh lays them
/// out but holding only the `vesyl-print` entrypoint an install checks for.
fn release_tarball(version: &str) -> Vec<u8> {
    let exe = b"#!/bin/sh\nexit 0\n";
    let mut header = tar::Header::new_gnu();
    header.set_size(exe.len() as u64);
    header.set_mode(0o755);
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);
    let path = format!("vesyl-print-{version}/vesyl-print");
    tar.append_data(&mut header, path, &exe[..]).unwrap();
    tar.into_inner().unwrap().finish().unwrap()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Loopback API stub that answers at once: whoami, the heartbeat, the job
/// pull (with the jobs in `pending`), and `{}` to anything else (job acks
/// and statuses). Records the path and JSON body of every request.
struct LiveApi {
    base_url: String,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
}

impl LiveApi {
    fn count(&self, path: &str) -> usize {
        let requests = self.requests.lock().unwrap();
        requests.iter().filter(|(p, _)| p == path).count()
    }

    /// The `[status, message]` of each status job `id` was reported.
    fn statuses(&self, id: &str) -> Vec<Value> {
        let path = format!("/print/v1/jobs/{id}/status");
        let requests = self.requests.lock().unwrap();
        requests
            .iter()
            .filter(|(p, _)| *p == path)
            .map(|(_, body)| json!([body["status"], body["message"]]))
            .collect()
    }
}

fn live_api(pending: Value) -> LiveApi {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let requests: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    let seen = requests.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let Some((path, body)) = read_request(&mut stream) else {
                continue;
            };
            let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
            seen.lock().unwrap().push((path.clone(), body));
            let reply = match path.as_str() {
                "/print/v1/whoami" => json!({"node_id": "node-1", "name": "Pack 1"}),
                "/print/v1/heartbeat" => json!({"ok": true, "last_seen_at": LAST_SEEN}),
                "/print/v1/jobs/pending" => json!({ "jobs": pending }),
                _ => json!({}),
            }
            .to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
        }
    });
    LiveApi { base_url, requests }
}

/// Wait up to `within` for `cond`; whether it held.
fn eventually(within: Duration, cond: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + within;
    while !cond() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
    true
}

/// Read one request: its path and body.
fn read_request(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
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
    Some((path, body))
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
    // J6: and in the C locale, though the node's is German: CUPS translates
    // what its tools print ("Gerät für" for lpstat's "device for"), and the
    // agent reads English.
    let locales = node.tool_locales();
    assert!(!locales.is_empty());
    for (tool, locale) in &locales {
        assert_eq!(locale, "C.UTF-8 C.UTF-8", "{tool} ran translated");
    }
}

/// A node with `config` (over [`Node::new`]'s), its API on `api`.
fn node_with(api: &LiveApi, config: Value) -> Node {
    Node::with_config(&api.base_url, config)
}

/// Wait for the agent to exit (killing it after `within`); fails the test
/// with its log when it did not.
fn exited(agent: Agent, within: Duration) -> (ExitStatus, String) {
    let (status, log) = agent.finish(within);
    let status = status.unwrap_or_else(|| panic!("still running after {within:?}\n{log}"));
    (status, log)
}

/// J3: SIGTERM while the agent waits on CUPS for a job (synchronous wait,
/// the printer out of paper) used to be ignored for up to 24 h: systemd
/// SIGKILLed the agent after 90 s with the job still queued, and the next
/// start printed the label again. The wait now ends at once, the record
/// says CUPS has the job, and the next start finishes it from what CUPS
/// says, without `lp`.
#[test]
fn a_stop_during_a_cups_wait_ends_it_and_never_prints_twice() {
    if skip_as_root() {
        return;
    }
    let api = live_api(json!([]));
    let node = node_with(&api, json!({"wait_cups": "sync"}));
    node.fake_lp("echo 'request id is Zebra-7 (1 file(s))'");
    let done = node.root.join("cups-done");
    let polls = node.root.join("lpstat.log");
    node.fake(
        "lpstat",
        &format!(
            "case \"$*\" in\n\
             \x20 '-W not-completed')\n\
             \x20   echo polled >> '{polls}'\n\
             \x20   [ -e '{done}' ] || echo 'Zebra-7  ben  1024  Thu 08 Oct 2026 12:00:00' ;;\n\
             \x20 '-W completed -l')\n\
             \x20   [ -e '{done}' ] && printf 'Zebra-7  ben  1024  Thu 08 Oct 2026 12:00:00\\n\\tAlerts: job-completed-successfully\\n' ;;\n\
             esac\n\
             exit 0",
            polls = polls.display(),
            done = done.display()
        ),
    );
    node.queue_job("job-7");

    let agent = node.start_agent();
    let waiting = eventually(Duration::from_secs(30), || polls.exists());
    agent.signal(libc::SIGTERM);
    let stopping = Instant::now();
    let (status, log) = exited(agent, Duration::from_secs(30));
    let took = stopping.elapsed();
    assert!(waiting, "the job never waited on CUPS\n{log}");
    assert_eq!(status.code(), Some(0), "{log}");
    assert!(
        took < Duration::from_secs(10),
        "stopped after {took:?}\n{log}"
    );
    assert_eq!(node.lp_runs(), 1, "{log}");
    assert!(!node.processed("job-7"), "{log}");
    let notes = node.notes("job-7");
    assert_eq!(notes["cups_job_id"], "Zebra-7", "{log}");

    // CUPS finished it meanwhile. The next start reports it printed and
    // never hands it to lp again.
    fs::write(&done, "").unwrap();
    let agent = node.start_agent();
    let finished = eventually(Duration::from_secs(30), || node.processed("job-7"));
    agent.signal(libc::SIGTERM);
    let (status, log) = exited(agent, Duration::from_secs(30));
    assert!(finished, "the job was never finished\n{log}");
    assert_eq!(status.code(), Some(0), "{log}");
    assert_eq!(node.lp_runs(), 1, "printed twice\n{log}");
    assert!(!node.queue_path("job-7").exists(), "{log}");
    assert_eq!(
        api.statuses("job-7"),
        [
            json!(["printing", null]),
            json!(["delivered", "Zebra-7"]),
            json!(["printed", "Zebra-7"]),
        ],
        "{log}"
    );
}

/// J5: a job whose printing kills the agent (here `lp` SIGKILLs it, as the
/// kernel's OOM killer would) used to run again on every start: systemd
/// restarts the agent 5 s later, and it died again before any other job,
/// keeping the node offline. After three such deaths the job is retired
/// as crash_loop and reported failed; the agent lives on.
#[test]
fn a_job_that_kills_the_agent_is_retired_after_three_deaths() {
    if skip_as_root() {
        return;
    }
    let api = live_api(json!([]));
    let node = node_with(&api, json!({}));
    node.fake_lp("kill -KILL $PPID");
    node.queue_job("job-poison");
    for death in 1..=3 {
        let (status, log) = exited(node.start_agent(), Duration::from_secs(60));
        assert_eq!(status.signal(), Some(libc::SIGKILL), "start {death}\n{log}");
        assert_eq!(node.lp_runs(), death, "{log}");
        assert_eq!(node.notes("job-poison")["attempts"], death, "{log}");
    }

    let agent = node.start_agent();
    let failed = node.state_dir().join("queue/failed/job-poison.json");
    let retired = eventually(Duration::from_secs(30), || failed.is_file());
    agent.signal(libc::SIGTERM);
    let (status, log) = exited(agent, Duration::from_secs(30));
    assert!(retired, "the job was not retired\n{log}");
    assert_eq!(status.code(), Some(0), "{log}");
    assert_eq!(node.lp_runs(), 3, "{log}");
    assert!(!node.queue_path("job-poison").exists(), "{log}");
    assert!(!node.processed("job-poison"), "{log}");
    let reported = api.statuses("job-poison");
    assert_eq!(
        reported.last(),
        Some(&json!([
            "error",
            "printing this job ended the agent 3 times (out of memory?) — not tried again"
        ])),
        "{reported:?}\n{log}"
    );
}

/// J1 and J4: after an OTA restart, the health gate of the new slot (here
/// broken: no `vesyl-print` in it) rolls back to the previous slot and
/// restarts the services, this agent included. The agent printed its
/// queued jobs before the gate (the startup drain did not wait for it),
/// and took jobs after it until the SIGTERM came (`rolled_back` pauses
/// nothing). It now takes none until that restart: the agent it starts,
/// from the slot it rolled back to, prints them.
#[test]
fn a_rollback_that_restarts_the_agent_takes_no_job_before_the_restart() {
    if skip_as_root() {
        return;
    }
    let pending = json!([{"id": "job-new", "cups_name": "Zebra",
                          "content_type": "png_base64", "content": PNG_1X1_B64}]);
    let api = live_api(pending);
    let node = node_with(
        &api,
        json!({"pull_jobs_enabled": true, "pull_interval_seconds": 1}),
    );
    node.fake_lp("echo 'request id is Zebra-9 (1 file(s))'");
    let releases = node.install_root().join("releases");
    fs::create_dir_all(releases.join("9.9.9")).unwrap();
    fs::create_dir_all(releases.join("9.9.8")).unwrap();
    write_exe(&releases.join("9.9.8/vesyl-print"), "#!/bin/sh\nexit 0\n");
    std::os::unix::fs::symlink("releases/9.9.9", node.install_root().join("current")).unwrap();
    let gate = json!({
        "status": "pending_health", "current_version": "9.9.9", "target_version": "9.9.9",
        "previous_version": "9.9.8", "health_deadline_at": "2099-01-01T00:00:00+00:00",
        "health_attempts": 0, "last_error": null, "last_checked_at": null, "channel": "stable",
    });
    fs::write(
        node.state_dir().join("update_status.json"),
        gate.to_string(),
    )
    .unwrap();
    node.queue_job("job-old");

    let agent = node.start_agent();
    let restarting = eventually(Duration::from_secs(30), || {
        node.restarts()
            .iter()
            .any(|r| r.ends_with("vesyl-print-agent"))
    });
    // Pulls would come every second, and the queue would drain at once.
    thread::sleep(Duration::from_secs(3));
    agent.signal(libc::SIGTERM);
    let (status, log) = exited(agent, Duration::from_secs(30));
    assert!(restarting, "the gate asked for no restart\n{log}");
    assert_eq!(status.code(), Some(0), "{log}");
    assert_eq!(node.lp_runs(), 0, "a job printed before the restart\n{log}");
    assert_eq!(api.count("/print/v1/jobs/pending"), 0, "{log}");
    assert!(node.queue_path("job-old").is_file(), "{log}");
    let current = fs::read_link(node.install_root().join("current")).unwrap();
    assert_eq!(current, Path::new("releases/9.9.8"), "{log}");
    let raw = fs::read_to_string(node.state_dir().join("update_status.json")).unwrap();
    let st: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(st["status"], "rolled_back", "{log}");
}

/// A stop that lands while the heartbeat is in flight starts no update its
/// reply announces. The update would download, switch slots and restart
/// the services, and that restart replaces the operator's stop job:
/// `systemctl stop` would end with the agent running again, on the new
/// slot. The next start applies the update.
#[test]
fn a_stop_during_the_heartbeat_starts_no_update() {
    if skip_as_root() {
        return;
    }
    let artifact = release_tarball("9.9.9");
    let sha256 = sha256_hex(&artifact);
    let api = held_heartbeat_api_serving(move |base| {
        let manifest = json!({
            "version": "9.9.9",
            "artifact_url": format!("{base}/vesyl-print-9.9.9.tar.gz"),
            "artifact_sha256": sha256,
        });
        Served {
            heartbeat: json!({
                "desired_agent_version": "9.9.9",
                "update_url": format!("{base}/manifest.json"),
            }),
            files: vec![
                ("/manifest.json".into(), manifest.to_string().into_bytes()),
                ("/vesyl-print-9.9.9.tar.gz".into(), artifact),
            ],
        }
    });
    // Updates on; the test release is unsigned.
    let node = Node::with_config(
        &api.base_url,
        json!({"auto_update_enabled": true, "update_require_signature": false}),
    );
    let agent = agent_in_heartbeat(&node, &api);
    agent.signal(libc::SIGTERM);
    // The reply comes once the agent is stopping.
    let stopping = agent.logged("SIGTERM received", Duration::from_secs(20));
    api.release.send(()).unwrap();
    let (status, log) = agent.finish(Duration::from_secs(60));
    assert!(stopping, "the agent did not take the SIGTERM\n{log}");
    assert_eq!(status.and_then(|s| s.code()), Some(0), "{status:?}\n{log}");

    let fetched = api.paths();
    assert!(
        !fetched
            .iter()
            .any(|p| p == "/manifest.json" || p.ends_with(".tar.gz")),
        "an update started after the stop: {fetched:?}\n{log}"
    );
    assert!(log.contains("any update waits for the next start"), "{log}");
    assert!(log.contains("agent stopped"), "{log}");
    let install = node.install_root();
    assert!(
        fs::symlink_metadata(install.join("current")).is_err(),
        "{log}"
    );
    assert!(!install.join("releases").exists(), "{log}");
    assert_eq!(node.restarts(), Vec::<String>::new(), "{log}");
    // The heartbeat itself went through.
    let st = node.status();
    assert_eq!(st["cloud"], "online", "{st}");
    assert_eq!(st["last_heartbeat_at"], LAST_SEEN, "{st}");
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
