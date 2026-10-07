//! Discover and provision printers via CUPS.
//!
//! On startup the app polls CUPS for discoverable printers and adds any that
//! are not already configured:
//!
//! 1. USB / direct devices (`lpinfo` `usb://…`) — raw for Zebra/ZPL,
//!    otherwise driverless everywhere with raw fallback
//! 2. IPP / driverless network (`lpinfo` + `lpadmin -m everywhere`)
//! 3. Port-9100 AppSocket scan for Zebra thermal printers not advertised via IPP —
//!    identify via the printer's HTTP homepage, then add as
//!    `socket://IP:9100` with the raw driver (HP JetDirect / AppSocket)
//!
//! The live display lists every configured network **and USB** printer. Requires
//! membership in the `lpadmin` group to add printers (no sudo needed).
//!
//! Subprocess-backed functions have `*_with` / `*_from` variants that take
//! injected command output so the parsing and decision logic is unit tested.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{json, Value};

use crate::BoxError;

const LOG: &str = "vesyl-print.printers";

/// CUPS device-URI schemes that indicate a printer reached over the network
/// (as opposed to usb://, parallel://, serial://, file://, ...).
const NETWORK_URI_SCHEMES: &[&str] = &[
    "ipp", "ipps", "http", "https", "socket", "lpd", "dnssd", "smb",
];
/// Local USB printers (usblp / libusb backends).
const USB_URI_SCHEMES: &[&str] = &["usb"];
/// CUPS admin tools often live in /usr/sbin (not on a minimal user PATH).
const CUPS_BIN_DIRS: &[&str] = &["/usr/sbin", "/usr/bin", "/bin"];

/// JetDirect / AppSocket raw printing (Zebra ZPL, etc.)
pub const SOCKET_PORT: u16 = 9100;

/// Interfaces we never scan for printers (virtual / overlay / mesh).
const SKIP_IFACE_PREFIXES: &[&str] = &[
    "lo",
    "docker",
    "br-",
    "veth",
    "virbr",
    "tailscale",
    "wg",
    "tun",
    "tap",
    "cni",
    "flannel",
    "lxc",
];

fn is_executable(p: &Path) -> bool {
    p.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Find `name` on PATH (like `shutil.which`).
pub fn which(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| is_executable(p))
        .map(|p| p.display().to_string())
}

/// Resolve a CUPS tool (lpinfo lives in /usr/sbin on Debian/Pi OS).
fn cups_cmd(name: &str) -> String {
    if let Some(found) = which(name) {
        return found;
    }
    CUPS_BIN_DIRS
        .iter()
        .map(|d| Path::new(d).join(name))
        .find(|p| is_executable(p))
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| name.to_string())
}

/// Output of a finished command (exit status, stdout, stderr).
pub struct CmdOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Run a command with a timeout (killing it on expiry), like
/// `subprocess.run(..., capture_output=True, timeout=...)`.
pub fn run_with_timeout(cmd: &str, args: &[&str], timeout: Duration) -> std::io::Result<CmdOutput> {
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Drain pipes on threads so a chatty child can't block on a full pipe.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            let _ = tx.send(buf);
        });
        rx
    };
    let out_rx = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let err_rx = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("{cmd} timed out"),
            ));
        }
        thread::sleep(Duration::from_millis(10));
    };
    let text = |rx: mpsc::Receiver<Vec<u8>>| {
        String::from_utf8_lossy(&rx.recv().unwrap_or_default()).into_owned()
    };
    Ok(CmdOutput {
        success: status.success(),
        stdout: text(out_rx),
        stderr: text(err_rx),
    })
}

/// stdout of a CUPS tool, or "" on any failure (Python `printers._run`).
fn run(cmd: &str, args: &[&str], timeout_s: u64) -> String {
    run_with_timeout(&cups_cmd(cmd), args, Duration::from_secs(timeout_s))
        .map(|o| o.stdout)
        .unwrap_or_default()
}

/// stderr, else stdout, trimmed (Python `(stderr or stdout or "").strip()`).
fn err_text(out: &CmdOutput) -> String {
    let s = if !out.stderr.is_empty() {
        &out.stderr
    } else {
        &out.stdout
    };
    s.trim().to_string()
}

fn uri_scheme(uri: &str) -> String {
    if uri.is_empty() || !uri.contains("://") {
        return String::new();
    }
    uri.split(':').next().unwrap_or_default().to_lowercase()
}

fn is_network_uri(uri: &str) -> bool {
    NETWORK_URI_SCHEMES.contains(&uri_scheme(uri).as_str())
}

fn is_usb_uri(uri: &str) -> bool {
    USB_URI_SCHEMES.contains(&uri_scheme(uri).as_str())
}

/// Network or USB queues we auto-provision and show on the LCD.
fn is_managed_uri(uri: &str) -> bool {
    is_network_uri(uri) || is_usb_uri(uri)
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static regex")
}

/// Python `str.partition`: (head, tail-after-sep) or (s, "").
fn partition(s: &str, sep: char) -> (&str, &str) {
    s.split_once(sep).unwrap_or((s, ""))
}

// --- currently configured printers -----------------------------------------

/// Parse `lpstat -v` lines of the form "device for <name>: <uri>".
fn parse_lpstat_v(out: &str) -> Vec<(String, String)> {
    const PREFIX: &str = "device for ";
    out.lines()
        .filter_map(|line| line.strip_prefix(PREFIX))
        .filter_map(|rest| {
            let (name, uri) = partition(rest, ':');
            let uri = uri.trim();
            is_managed_uri(uri).then(|| (name.trim().to_string(), uri.to_string()))
        })
        .collect()
}

/// (queue_name, uri) for every CUPS queue with a network **or USB** device URI.
///
/// Name kept for compatibility; includes `usb://` queues as well as IPP/socket.
pub fn configured_network_queues() -> Vec<(String, String)> {
    parse_lpstat_v(&run("lpstat", &["-v"], 3))
}

/// Display names of all configured network/USB queues (stable order).
pub fn configured_printers() -> Vec<String> {
    configured_network_queues()
        .into_iter()
        .map(|(queue, _)| display_name(&queue))
        .collect()
}

/// Display name of the first configured network/USB queue, or None.
pub fn configured_printer() -> Option<String> {
    configured_printers().into_iter().next()
}

/// Driver names CUPS reports that aren't the real printer model. A driverless
/// (IPP Everywhere) queue reports its make-and-model as "Printer - IPP
/// Everywhere", so we skip those and fall back to the description instead.
const GENERIC_MODELS: &[&str] = &[
    "",
    "unknown",
    "local printer",
    "local raw printer",
    "ipp everywhere",
    "printer - ipp everywhere",
];

fn parse_lpoption(out: &str, key: &str) -> String {
    let k = regex::escape(key);
    // Values may be quoted ('Brother MFC…') or bare (myprinter).
    let quoted = regex(&format!("{k}='([^']*)'"));
    let bare = regex(&format!(r"{k}=(\S+)"));
    quoted
        .captures(out)
        .or_else(|| bare.captures(out))
        .map(|c| c[1].trim().to_string())
        .unwrap_or_default()
}

/// Friendly name from `lpoptions -p` output: real model, else description, else name.
fn display_name_from(lpoptions_out: &str, queue: &str) -> String {
    let model = parse_lpoption(lpoptions_out, "printer-make-and-model");
    if !model.is_empty() && !GENERIC_MODELS.contains(&model.to_lowercase().as_str()) {
        return model;
    }
    // Driverless queue: the real model is stashed in the description (see
    // add_printer's -D), so prefer it over the generic driver name.
    let info = parse_lpoption(lpoptions_out, "printer-info");
    if !info.is_empty() {
        return info;
    }
    if !model.is_empty() {
        return model;
    }
    queue.to_string()
}

/// Friendly name for a queue: real model, else description, else name.
pub fn display_name(queue: &str) -> String {
    display_name_from(&run("lpoptions", &["-p", queue], 3), queue)
}

// --- discovery + provisioning ----------------------------------------------

/// Parse `lpinfo -l -v` into a list of device attribute maps.
fn parse_lpinfo_devices(out: &str) -> Vec<HashMap<String, String>> {
    let mut devices = Vec::new();
    let mut current: Option<HashMap<String, String>> = None;
    for line in out.lines() {
        let stripped = line.trim();
        if stripped.starts_with("Device:") {
            if let Some(d) = current.take() {
                devices.push(d);
            }
            // "Device: uri = <uri>"
            let (_, uri) = partition(stripped, '=');
            current = Some(HashMap::from([("uri".to_string(), uri.trim().to_string())]));
        } else if let Some(d) = current.as_mut() {
            if stripped.contains('=') {
                let (k, v) = partition(stripped, '=');
                d.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
    }
    if let Some(d) = current {
        devices.push(d);
    }
    devices
}

fn attr<'a>(d: &'a HashMap<String, String>, key: &str) -> &'a str {
    d.get(key).map(String::as_str).unwrap_or("")
}

/// Network printers from `lpinfo -l -v` output: class=network, network URI,
/// known model, deduped by URI.
fn network_printers_from_lpinfo(out: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    for d in parse_lpinfo_devices(out) {
        let uri = attr(&d, "uri");
        let model = match attr(&d, "make-and-model") {
            "" => attr(&d, "info"),
            m => m,
        };
        if attr(&d, "class") == "network"
            && is_network_uri(uri)
            && !model.is_empty()
            && model.to_lowercase() != "unknown"
            && seen.insert(uri.to_string())
        {
            found.push((uri.to_string(), model.to_string()));
        }
    }
    found
}

/// All discoverable network printers as (device_uri, make_and_model).
///
/// Uses `lpinfo -l -v`, which browses the network (mDNS) and can take a few
/// seconds. Skips backend placeholders (uri = "ipp", "socket", …) and
/// devices with an unknown model. Dedupes by URI.
pub fn discover_network_printers() -> Vec<(String, String)> {
    network_printers_from_lpinfo(&run("lpinfo", &["-l", "-v"], 25))
}

/// (device_uri, make_and_model) of the first discoverable network printer.
pub fn discover_network_printer() -> Option<(String, String)> {
    discover_network_printers().into_iter().next()
}

/// Clean CUPS USB make-and-model for display / queue naming.
///
/// e.g. `Zebra Technologies ZTC ZD220-203dpi ZPL` → `Zebra ZD220-203dpi ZPL`
pub fn normalize_usb_model(model: &str) -> String {
    let m = model.trim();
    if m.is_empty() {
        return "USB Printer".into();
    }
    static WS: OnceLock<Regex> = OnceLock::new();
    static ZEBRA: OnceLock<Regex> = OnceLock::new();
    static ZTC: OnceLock<Regex> = OnceLock::new();
    let m = WS.get_or_init(|| regex(r"\s+")).replace_all(m, " ");
    // Drop redundant manufacturer tokens common in USB ID strings.
    let m = ZEBRA
        .get_or_init(|| regex(r"(?i)^Zebra Technologies\s+"))
        .replace(&m, "Zebra ");
    let m = ZTC
        .get_or_init(|| regex(r"(?i)\bZTC\s+"))
        .replace_all(&m, "");
    let m = m.trim();
    if m.is_empty() {
        "USB Printer".into()
    } else {
        m.to_string()
    }
}

/// True when the device should use CUPS `raw` (ZPL / label printers).
pub fn model_looks_thermal_raw(model: &str) -> bool {
    let low = model.to_lowercase();
    [
        "zebra",
        "zpl",
        "zdesigner",
        "eltron",
        "datamax",
        "sato",
        "tsc ",
        "godex",
    ]
    .iter()
    .any(|t| low.contains(t))
}

/// Python `urllib.parse.unquote` (UTF-8, invalid sequences replaced).
fn unquote(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// USB printers from `lpinfo -l -v` output: `usb://` URIs with class
/// direct/usb (or no class), deduped by URI.
fn usb_printers_from_lpinfo(out: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    for d in parse_lpinfo_devices(out) {
        let uri = attr(&d, "uri");
        if !is_usb_uri(uri) {
            continue;
        }
        // CUPS reports attached USB printers as class=direct.
        let cls = attr(&d, "class").to_lowercase();
        if !cls.is_empty() && cls != "direct" && cls != "usb" {
            continue;
        }
        let raw_model = [attr(&d, "make-and-model"), attr(&d, "info")]
            .into_iter()
            .find(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| {
                let after = uri.split_once("usb://").map(|(_, r)| r).unwrap_or(uri);
                unquote(partition(after, '?').0)
            });
        let model = normalize_usb_model(&raw_model);
        if model.is_empty() || model.to_lowercase() == "unknown" {
            continue;
        }
        if !seen.insert(uri.to_string()) {
            continue;
        }
        log::info!(target: LOG, "found USB printer {model} → {uri}");
        found.push((uri.to_string(), model));
    }
    found
}

/// Discover local USB printers as (device_uri, make_and_model).
///
/// Uses `lpinfo -l -v` devices with `class=direct` and `usb://` URIs
/// (e.g. Zebra ZD220 on usblp). Skips unknown models and placeholder backends.
pub fn discover_usb_printers() -> Vec<(String, String)> {
    usb_printers_from_lpinfo(&run("lpinfo", &["-l", "-v"], 15))
}

/// A CUPS-safe queue name derived from a model string.
pub fn queue_name(model: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let name = RE
        .get_or_init(|| regex(r"[^A-Za-z0-9._-]+"))
        .replace_all(model, "_");
    let name = name.trim_matches('_');
    if name.is_empty() {
        "printer".into()
    } else {
        name.to_string()
    }
}

/// Hostname or IP from a CUPS device URI (lowercased, like `urlparse().hostname`).
pub fn host_from_uri(uri: &str) -> Option<String> {
    let (_, rest) = uri.split_once("://")?;
    let netloc = rest.split(['/', '?', '#']).next().unwrap_or("");
    let hostport = netloc.rsplit_once('@').map(|(_, h)| h).unwrap_or(netloc);
    let host = if let Some(v6) = hostport.strip_prefix('[') {
        v6.split(']').next().unwrap_or("")
    } else {
        partition(hostport, ':').0
    };
    let host = host.to_lowercase();
    (!host.is_empty()).then_some(host)
}

/// Resolve host to a single IPv4 address string, or None.
pub fn ipv4_from_host(host: Option<&str>) -> Option<String> {
    let host = host.filter(|h| !h.is_empty())?;
    if let Ok(ip) = Ipv4Addr::from_str(host) {
        return Some(ip.to_string());
    }
    (host, 0)
        .to_socket_addrs()
        .ok()?
        .find_map(|a| match a.ip() {
            IpAddr::V4(v4) => Some(v4.to_string()),
            IpAddr::V6(_) => None,
        })
}

/// IPv4 addresses already known from configured / discovered device URIs.
pub fn ips_from_device_uris<'a>(uris: impl IntoIterator<Item = &'a str>) -> HashSet<String> {
    uris.into_iter()
        .filter_map(|u| ipv4_from_host(host_from_uri(u).as_deref()))
        .collect()
}

fn iface_should_skip(iface: &str) -> bool {
    let name = iface.trim().to_lowercase();
    SKIP_IFACE_PREFIXES
        .iter()
        .any(|p| name == *p || name.starts_with(p))
}

/// An IPv4 network (address with host bits cleared + prefix length).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Ipv4Net {
    pub network: Ipv4Addr,
    pub prefix: u8,
}

impl Ipv4Net {
    /// Network containing `ip` (Python `IPv4Network(..., strict=False)`).
    pub fn new(ip: Ipv4Addr, prefix: u8) -> Option<Self> {
        if prefix > 32 {
            return None;
        }
        let mask = if prefix == 0 {
            0
        } else {
            u32::MAX << (32 - prefix)
        };
        Some(Ipv4Net {
            network: Ipv4Addr::from(u32::from(ip) & mask),
            prefix,
        })
    }

    pub fn num_addresses(&self) -> u64 {
        1u64 << (32 - self.prefix)
    }

    /// Usable hosts (Python `IPv4Network.hosts()`): excludes network and
    /// broadcast except for /31 and /32.
    pub fn hosts(&self) -> Vec<Ipv4Addr> {
        let start = u32::from(self.network);
        let n = self.num_addresses();
        let range: Box<dyn Iterator<Item = u64>> = match self.prefix {
            32 => Box::new(0..1),
            31 => Box::new(0..2),
            _ => Box::new(1..n - 1),
        };
        range
            .map(|off| Ipv4Addr::from(start + off as u32))
            .collect()
    }
}

impl fmt::Display for Ipv4Net {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

/// `a.b.c.d/p` interface CIDR → (address, prefix). Prefix defaults to 32.
fn parse_ipv4_interface(cidr: &str) -> Option<(Ipv4Addr, u8)> {
    let (ip, prefix) = partition(cidr, '/');
    let ip = Ipv4Addr::from_str(ip).ok()?;
    let prefix = if prefix.is_empty() {
        32
    } else {
        prefix.parse().ok().filter(|p: &u8| *p <= 32)?
    };
    Some((ip, prefix))
}

/// CIDR following the `inet` token in one `ip -o addr` line.
fn inet_cidr(parts: &[&str]) -> Option<(Ipv4Addr, u8)> {
    let idx = parts.iter().position(|p| *p == "inet")?;
    parse_ipv4_interface(parts.get(idx + 1)?)
}

/// Scan prefixes from `ip -4 -o addr show scope global` output.
fn scan_networks_from_ip(out: &str) -> Vec<Ipv4Net> {
    let mut nets = Vec::new();
    let mut seen = HashSet::new();
    for line in out.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 4 || iface_should_skip(parts[1]) {
            continue;
        }
        let Some((ip, prefix)) = inet_cidr(&parts) else {
            continue;
        };
        let Some(mut net) = Ipv4Net::new(ip, prefix) else {
            continue;
        };
        // Never scan more than a /24 around us.
        if net.num_addresses() > 256 {
            net = Ipv4Net::new(ip, 24).expect("valid prefix");
        }
        if seen.insert(net) {
            nets.push(net);
        }
    }
    nets
}

/// IPv4 LAN prefixes to scan (global scope, non-virtual interfaces).
///
/// Caps each scan range at /24 so a misconfigured /16 does not hammer hosts.
pub fn local_scan_networks() -> Vec<Ipv4Net> {
    scan_networks_from_ip(&run(
        "ip",
        &["-4", "-o", "addr", "show", "scope", "global"],
        3,
    ))
}

fn local_addrs_from_ip(out: &str) -> HashSet<String> {
    out.lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            inet_cidr(&parts).map(|(ip, _)| ip.to_string())
        })
        .collect()
}

/// This host's global IPv4 addresses (never scan ourselves as a printer).
fn local_ipv4_addrs() -> HashSet<String> {
    local_addrs_from_ip(&run(
        "ip",
        &["-4", "-o", "addr", "show", "scope", "global"],
        3,
    ))
}

/// True if TCP connect to `ip:port` succeeds quickly.
pub fn port_open(ip: &str, port: u16, timeout: Duration) -> bool {
    let addrs: Vec<SocketAddr> = match IpAddr::from_str(ip) {
        Ok(a) => vec![SocketAddr::new(a, port)],
        Err(_) => match (ip, port).to_socket_addrs() {
            Ok(it) => it.collect(),
            Err(_) => return false,
        },
    };
    addrs
        .iter()
        .any(|a| TcpStream::connect_timeout(a, timeout).is_ok())
}

/// Probe `candidates` concurrently; return those with `port` open, sorted
/// numerically by octet.
fn probe_hosts(
    candidates: &[String],
    port: u16,
    timeout: Duration,
    max_workers: usize,
) -> Vec<String> {
    if candidates.is_empty() {
        return Vec::new();
    }
    let workers = max_workers.clamp(1, candidates.len());
    let next = AtomicUsize::new(0);
    let open = Mutex::new(Vec::new());
    thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(ip) = candidates.get(i) else { break };
                if port_open(ip, port, timeout) {
                    open.lock().unwrap().push(ip.clone());
                }
            });
        }
    });
    let mut open = open.into_inner().unwrap();
    open.sort_by_key(|s| {
        s.split('.')
            .map(|p| p.parse::<u32>().unwrap_or(0))
            .collect::<Vec<_>>()
    });
    open
}

/// IPs on `networks` with `port` open, excluding `ignore_ips` and self.
pub fn scan_port_open_hosts(
    networks: &[Ipv4Net],
    port: u16,
    ignore_ips: &HashSet<String>,
    timeout: Duration,
    max_workers: usize,
) -> Vec<String> {
    let mut ignore = ignore_ips.clone();
    ignore.extend(local_ipv4_addrs());
    let candidates: Vec<String> = networks
        .iter()
        .flat_map(Ipv4Net::hosts)
        .map(|h| h.to_string())
        .filter(|ip| !ignore.contains(ip))
        .collect();
    probe_hosts(&candidates, port, timeout, max_workers)
}

/// Extract a display model from a Zebra printer home page body.
///
/// Returns None when the page does not look like a Zebra print server.
/// Example page title/body: `Zebra Technologies` / `ZTC ZD421-203dpi ZPL`.
pub fn parse_zebra_http_identity(body: &str) -> Option<String> {
    if body.is_empty() {
        return None;
    }
    let low = body.to_lowercase();
    if !low.contains("zebra") && !low.contains("zdesigner") {
        return None;
    }
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex(r"(?i)ZTC\s+([A-Za-z0-9][A-Za-z0-9\-_. /]*[A-Za-z0-9])"));
    if let Some(c) = re.captures(body) {
        let model = c[1].trim();
        // Prefer "Zebra ZD421-203dpi ZPL" over bare ZTC string.
        if !model.to_lowercase().starts_with("zebra") {
            return Some(format!("Zebra {model}"));
        }
        return Some(model.to_string());
    }
    // Older pages may only say "Zebra Technologies" without ZTC.
    Some("Zebra Printer".into())
}

/// Injectable HTTP fetch for Zebra identification: `(url) -> bytes`.
pub type FetchFn = Arc<dyn Fn(&str) -> Result<Vec<u8>, BoxError> + Send + Sync>;

/// GET `url` with a timeout, reading at most 16 KiB of the body.
fn http_fetch_head(url: &str, timeout: Duration) -> Result<Vec<u8>, BoxError> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .user_agent("vesyl-print-agent")
        .build()
        .into();
    let mut resp = agent.get(url).header("Accept", "text/html, */*").call()?;
    let mut buf = Vec::new();
    resp.body_mut()
        .as_reader()
        .take(16384)
        .read_to_end(&mut buf)?;
    Ok(buf)
}

fn identify_zebra_url(url: &str, timeout: Duration, fetch: Option<&FetchFn>) -> Option<String> {
    let raw = match fetch {
        Some(f) => f(url),
        None => http_fetch_head(url, timeout),
    };
    match raw {
        Ok(bytes) => parse_zebra_http_identity(&String::from_utf8_lossy(&bytes)),
        Err(e) => {
            log::debug!(target: LOG, "zebra HTTP probe failed for {url}: {e}");
            None
        }
    }
}

/// GET `http://ip/` and return a model name if the host is a Zebra.
///
/// `fetch` is injectable for tests: `(url) -> bytes`.
pub fn identify_zebra_http(ip: &str, timeout: Duration, fetch: Option<&FetchFn>) -> Option<String> {
    identify_zebra_url(&format!("http://{ip}/"), timeout, fetch)
}

/// Options for [`discover_zebra_socket_printers`].
#[derive(Clone)]
pub struct ZebraScan {
    /// Known IPP / already configured hosts to skip.
    pub ignore_ips: HashSet<String>,
    /// Networks to scan (default: [`local_scan_networks`]).
    pub networks: Option<Vec<Ipv4Net>>,
    pub port: u16,
    pub scan_timeout: Duration,
    pub http_timeout: Duration,
    pub fetch: Option<FetchFn>,
    /// Skip the port scan and probe these hosts directly.
    pub open_hosts: Option<Vec<String>>,
}

impl Default for ZebraScan {
    fn default() -> Self {
        ZebraScan {
            ignore_ips: HashSet::new(),
            networks: None,
            port: SOCKET_PORT,
            scan_timeout: Duration::from_millis(200),
            http_timeout: Duration::from_secs(2),
            fetch: None,
            open_hosts: None,
        }
    }
}

/// Find Zebra printers reachable via AppSocket (TCP 9100).
///
/// Flow:
///   1. Scan local LAN for open port 9100 (unless `open_hosts` injected)
///   2. Skip `ignore_ips` (known IPP / already configured hosts)
///   3. Confirm Zebra via HTTP homepage
///   4. Return `(ip, model)` pairs for `socket://ip:9100` provisioning
pub fn discover_zebra_socket_printers(opts: &ZebraScan) -> Vec<(String, String)> {
    let ignore = &opts.ignore_ips;
    let hosts = match &opts.open_hosts {
        Some(open) => open
            .iter()
            .filter(|h| !ignore.contains(*h))
            .cloned()
            .collect(),
        None => {
            let nets = opts.networks.clone().unwrap_or_else(local_scan_networks);
            if nets.is_empty() {
                log::debug!(target: LOG, "no local networks to scan for port {}", opts.port);
                return Vec::new();
            }
            log::info!(
                target: LOG,
                "scanning {} for TCP {} (skipping {} known IP(s))",
                nets.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", "),
                opts.port,
                ignore.len()
            );
            scan_port_open_hosts(&nets, opts.port, ignore, opts.scan_timeout, 64)
        }
    };

    let mut found = Vec::new();
    for ip in hosts {
        match identify_zebra_http(&ip, opts.http_timeout, opts.fetch.as_ref()) {
            Some(model) => {
                log::info!(target: LOG, "found Zebra at {ip} ({model}) via socket/{}", opts.port);
                found.push((ip, model));
            }
            None => {
                log::debug!(target: LOG, "port {} open on {ip} but not identified as Zebra", opts.port);
            }
        }
    }
    found
}

/// `lpadmin` args for a driverless (IPP Everywhere) queue.
fn lpadmin_everywhere_args(queue: &str, uri: &str, model: &str) -> Vec<String> {
    // -D stashes the real model as the description; a driverless queue
    // otherwise reports its model as the generic "IPP Everywhere".
    [
        "-p",
        queue,
        "-v",
        uri,
        "-m",
        "everywhere",
        "-D",
        model,
        "-E",
    ]
    .map(String::from)
    .to_vec()
}

/// `lpadmin` args for a raw queue (`-m raw`, optional `-L location`).
fn lpadmin_raw_args(queue: &str, uri: &str, model: &str, location: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = ["-p", queue, "-v", uri, "-m", "raw", "-D", model, "-E"]
        .map(String::from)
        .to_vec();
    if let Some(loc) = location.filter(|l| !l.is_empty()) {
        args.extend(["-L".to_string(), loc.to_string()]);
    }
    args
}

fn lpadmin(args: &[String]) -> std::io::Result<CmdOutput> {
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    run_with_timeout(&cups_cmd("lpadmin"), &argv, Duration::from_secs(30))
}

/// Add a driverless (IPP Everywhere) queue named after the model.
///
/// Returns the queue name on success, or None on failure.
pub fn add_printer(uri: &str, model: &str, queue: Option<&str>) -> Option<String> {
    let queue_name = queue.map(String::from).unwrap_or_else(|| queue_name(model));
    let out = lpadmin(&lpadmin_everywhere_args(&queue_name, uri, model)).ok()?;
    if !out.success {
        log::warn!(target: LOG, "lpadmin everywhere failed for {uri}: {}", err_text(&out));
        return None;
    }
    Some(queue_name)
}

/// Add a raw AppSocket/HP JetDirect queue: `socket://ip:port` + `-m raw`.
///
/// Zebra thermal printers take ZPL on port 9100; raw avoids CUPS filters that
/// would mangle the payload. Returns the queue name on success.
pub fn add_raw_socket_printer(
    ip: &str,
    model: &str,
    port: u16,
    queue: Option<&str>,
) -> Option<String> {
    let uri = format!("socket://{ip}:{port}");
    add_raw_printer(&uri, model, queue, Some(&format!("AppSocket {ip}:{port}")))
}

/// Add a CUPS queue with `-m raw` (USB Zebra, socket:// thermal, etc.).
pub fn add_raw_printer(
    uri: &str,
    model: &str,
    queue: Option<&str>,
    location: Option<&str>,
) -> Option<String> {
    let queue_name = queue.map(String::from).unwrap_or_else(|| queue_name(model));
    let out = match lpadmin(&lpadmin_raw_args(&queue_name, uri, model, location)) {
        Ok(o) => o,
        Err(e) => {
            log::warn!(target: LOG, "lpadmin raw failed for {uri}: {e}");
            return None;
        }
    };
    if !out.success {
        log::warn!(target: LOG, "lpadmin raw failed for {uri}: {}", err_text(&out));
        return None;
    }
    log::info!(target: LOG, "added raw queue {queue_name} → {uri}");
    Some(queue_name)
}

/// Provision a USB printer into CUPS.
///
/// Zebra / ZPL and other thermal devices use the raw driver so label payloads
/// are not filtered. Other USB devices try IPP Everywhere first, then raw.
pub fn add_usb_printer(uri: &str, model: &str, queue: Option<&str>) -> Option<String> {
    let queue_name = queue.map(String::from).unwrap_or_else(|| queue_name(model));
    if model_looks_thermal_raw(model) || uri.to_lowercase().contains("zpl") {
        return add_raw_printer(uri, model, Some(&queue_name), Some("USB"));
    }
    if let Some(added) = add_printer(uri, model, None) {
        return Some(added);
    }
    log::info!(target: LOG, "USB everywhere failed for {uri} — trying raw");
    add_raw_printer(uri, model, Some(&queue_name), Some("USB"))
}

/// Test page sent to a printer right after the app auto-provisions it:
/// `base.jpg` next to the executable, else in the installed `current` slot.
pub fn test_image() -> PathBuf {
    let beside_exe = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("base.jpg")));
    match beside_exe {
        Some(p) if p.is_file() => p,
        _ => PathBuf::from("/opt/vesyl-print/current/base.jpg"),
    }
}

/// Send the test image to a queue. Returns true if the job was accepted.
pub fn print_test_page(queue: &str) -> bool {
    let img = test_image();
    if !img.exists() {
        return false;
    }
    let img = img.display().to_string();
    run_with_timeout("lp", &["-d", queue, &img], Duration::from_secs(30)).is_ok_and(|o| o.success)
}

/// The CUPS operations [`ensure_printers_with`] needs (real: [`SystemCups`]).
pub trait Provisioner {
    fn configured_network_queues(&self) -> Vec<(String, String)>;
    fn configured_printers(&self) -> Vec<String>;
    fn discover_usb_printers(&self) -> Vec<(String, String)>;
    fn discover_network_printers(&self) -> Vec<(String, String)>;
    fn discover_zebra_socket_printers(&self, ignore_ips: &HashSet<String>)
        -> Vec<(String, String)>;
    fn add_printer(&self, uri: &str, model: &str) -> Option<String>;
    fn add_usb_printer(&self, uri: &str, model: &str, queue: &str) -> Option<String>;
    fn add_raw_socket_printer(&self, ip: &str, model: &str, queue: &str) -> Option<String>;
}

/// [`Provisioner`] backed by the real CUPS tools and LAN scan.
pub struct SystemCups;

impl Provisioner for SystemCups {
    fn configured_network_queues(&self) -> Vec<(String, String)> {
        configured_network_queues()
    }
    fn configured_printers(&self) -> Vec<String> {
        configured_printers()
    }
    fn discover_usb_printers(&self) -> Vec<(String, String)> {
        discover_usb_printers()
    }
    fn discover_network_printers(&self) -> Vec<(String, String)> {
        discover_network_printers()
    }
    fn discover_zebra_socket_printers(
        &self,
        ignore_ips: &HashSet<String>,
    ) -> Vec<(String, String)> {
        discover_zebra_socket_printers(&ZebraScan {
            ignore_ips: ignore_ips.clone(),
            ..ZebraScan::default()
        })
    }
    fn add_printer(&self, uri: &str, model: &str) -> Option<String> {
        add_printer(uri, model, None)
    }
    fn add_usb_printer(&self, uri: &str, model: &str, queue: &str) -> Option<String> {
        add_usb_printer(uri, model, Some(queue))
    }
    fn add_raw_socket_printer(&self, ip: &str, model: &str, queue: &str) -> Option<String> {
        add_raw_socket_printer(ip, model, SOCKET_PORT, Some(queue))
    }
}

/// `serial=` query value from a USB URI, or "".
fn usb_serial(uri: &str) -> &str {
    uri.split_once("serial=")
        .map(|(_, rest)| partition(rest, '&').0)
        .unwrap_or("")
}

/// Ensure every discoverable printer has a CUPS queue.
///
/// 1. USB / direct (`usb://…`) from `lpinfo`
/// 2. IPP / driverless network devices from `lpinfo`
/// 3. Port-9100 Zebra scan (skips IPs already known from step 2 / CUPS)
///
/// Returns display names of all configured printers (including any that were
/// already present). Discovery browses USB + the network and can take several
/// seconds, so call this off the render loop.
pub fn ensure_printers() -> Vec<String> {
    ensure_printers_with(&SystemCups)
}

pub fn ensure_printers_with(cups: &dyn Provisioner) -> Vec<String> {
    let existing = cups.configured_network_queues();
    let mut existing_uris: HashSet<String> = existing.iter().map(|(_, u)| u.clone()).collect();
    let mut existing_names: HashSet<String> = existing.into_iter().map(|(n, _)| n).collect();
    let mut known_ips = ips_from_device_uris(existing_uris.iter().map(String::as_str));

    // --- 0. USB (Zebra ZD220 etc.) ---
    for (uri, model) in cups.discover_usb_printers() {
        if existing_uris.contains(&uri) {
            continue;
        }
        // Same serial already configured under a slightly different URI form.
        let base = partition(&uri, '?').0;
        let same_base = existing_uris
            .iter()
            .any(|u| is_usb_uri(u) && partition(u, '?').0 == base);
        if same_base {
            // Prefer exact serial match when both have query strings.
            let serial = usb_serial(&uri);
            if !serial.is_empty()
                && existing_uris
                    .iter()
                    .any(|u| is_usb_uri(u) && u.contains(serial))
            {
                continue;
            }
        }
        let mut queue = queue_name(&model);
        if existing_names.contains(&queue) {
            // Disambiguate second USB of same model via serial suffix.
            let serial: Vec<char> = usb_serial(&uri).chars().collect();
            let tail: String = serial[serial.len().saturating_sub(6)..].iter().collect();
            queue = if tail.is_empty() {
                queue_name(&format!("{model}_USB"))
            } else {
                queue_name(&format!("{model}_{tail}"))
            };
        }
        if existing_names.contains(&queue) {
            continue;
        }
        if let Some(added) = cups.add_usb_printer(&uri, &model, &queue) {
            log::info!(target: LOG, "provisioned USB printer queue {added}");
            existing_uris.insert(uri);
            existing_names.insert(added);
        }
    }

    // --- 1. IPP Everywhere ---
    for (uri, model) in cups.discover_network_printers() {
        let ip = ipv4_from_host(host_from_uri(&uri).as_deref());
        if let Some(ip) = &ip {
            known_ips.insert(ip.clone());
        }
        if existing_uris.contains(&uri) || existing_names.contains(&queue_name(&model)) {
            continue;
        }
        if let Some(added) = cups.add_printer(&uri, &model) {
            existing_uris.insert(uri);
            existing_names.insert(added);
        }
    }

    // --- 2. AppSocket 9100 + Zebra HTTP identity ---
    for (ip, model) in cups.discover_zebra_socket_printers(&known_ips) {
        let uri = format!("socket://{ip}:{SOCKET_PORT}");
        if existing_uris.contains(&uri) {
            continue;
        }
        // Also skip if some other queue already points at this host:9100.
        if existing_uris
            .iter()
            .any(|u| host_from_uri(u).as_deref() == Some(ip.as_str()) && u.contains(":9100"))
        {
            continue;
        }
        let mut queue = queue_name(&model);
        // Disambiguate if an IPP queue already took the same model name.
        if existing_names.contains(&queue) {
            let last = ip.rsplit('.').next().unwrap_or(&ip);
            queue = queue_name(&format!("{model}_{last}"));
        }
        if existing_names.contains(&queue) {
            continue;
        }
        if let Some(added) = cups.add_raw_socket_printer(&ip, &model, &queue) {
            existing_uris.insert(uri);
            existing_names.insert(added);
            known_ips.insert(ip);
        }
    }

    cups.configured_printers()
}

/// Back-compat: first configured printer display name after [`ensure_printers`].
pub fn ensure_printer() -> Option<String> {
    ensure_printers().into_iter().next()
}

// --- status ----------------------------------------------------------------

/// CUPS / IPP printer-state-reasons → operator-facing labels (subset of IPP).
fn reason_label(reason: &str) -> Option<&'static str> {
    Some(match reason {
        "media-empty" | "media-empty-error" | "media-needed" => "Out of paper",
        "media-jam" | "media-jam-error" => "Paper jam",
        "media-low" => "Paper low",
        "toner-empty" | "toner-empty-error" => "Toner empty",
        "toner-low" => "Toner low",
        "marker-supply-empty" => "Supply empty",
        "marker-supply-low" => "Supply low",
        "door-open" | "door-open-error" => "Door open",
        "cover-open" => "Cover open",
        "input-tray-missing" => "Tray missing",
        "output-tray-missing" => "Output tray missing",
        "paused" => "Paused",
        "offline" | "offline-report" => "Offline",
        "connecting-to-device" => "Connecting",
        "cups-insecure-filter-warning" => "Filter warning",
        "cups-missing-filter-warning" => "Missing filter",
        "shutdown" => "Shutdown",
        "timed-out" => "Timed out",
        "stopped" => "Stopped",
        // Job-level reasons that surface during paper-out holds
        "job-hold-until-specified" => "Held",
        "resources-are-not-ready" => "Resources not ready",
        "printer-stopped" | "printer-stopped-partly" => "Printer stopped",
        _ => return None,
    })
}

/// Reasons that mean the queue is effectively offline (not just stopped).
const OFFLINE_REASONS: &[&str] = &[
    "offline",
    "offline-report",
    "shutdown",
    "connecting-to-device",
    "cups-printer-missing",
];

/// Actionable device conditions → force status "stopped" for admin.
const ACTIONABLE_REASON_PREFIXES: &[&str] = &[
    "media-empty",
    "media-needed",
    "media-jam",
    "toner-empty",
    "marker-supply-empty",
    "door-open",
    "cover-open",
    "input-tray-missing",
    "output-tray-missing",
    "paused",
    "stopped",
    "printer-stopped",
    "resources-are-not-ready",
];

/// Noise IPP always reports when healthy — ignore for status_message.
const BENIGN_REASONS: &[&str] = &[
    "none",
    "-",
    "",
    // Informational CUPS progress tokens — not operator faults.
    "cups-waiting-for-job-completed",
    "job-printing",
    "job-completed-successfully",
    "processing-to-stop-point",
    "moving-to-paused",
];

fn is_benign(r: &str) -> bool {
    BENIGN_REASONS.contains(&r)
}

/// IPP printer-state enum → our status strings.
fn ipp_state(token: &str) -> &'static str {
    match token {
        "3" | "idle" => "idle",
        "4" | "processing" => "printing",
        "5" | "stopped" => "stopped",
        _ => "unknown",
    }
}

/// Embedded Get-Printer-Attributes test used with ipptool (no external file dep).
const IPP_GET_PRINTER_TEST: &str = "\
{
OPERATION Get-Printer-Attributes
GROUP operation-attributes-tag
ATTR charset attributes-charset utf-8
ATTR naturalLanguage attributes-natural-language en
ATTR uri printer-uri $uri
ATTR keyword requested-attributes printer-state,printer-state-reasons,printer-state-message,queued-job-count,printer-is-accepting-jobs
}
";

const IPP_GET_JOBS_TEST: &str = "\
{
OPERATION Get-Jobs
GROUP operation-attributes-tag
ATTR charset attributes-charset utf-8
ATTR naturalLanguage attributes-natural-language en
ATTR uri printer-uri $uri
ATTR keyword which-jobs not-completed
ATTR keyword requested-attributes job-id,job-state,job-state-reasons,job-printer-state-reasons,job-printer-state-message,job-state-message
}
";

fn normalize_reason(raw: &str) -> String {
    let r = raw.trim().to_lowercase().replace('_', "-");
    // Strip surrounding quotes from ipptool dumps.
    r.trim_matches(['"', '\'']).to_string()
}

fn dedupe_reasons(reasons: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    reasons
        .into_iter()
        .filter(|r| !is_benign(r) && seen.insert(r.clone()))
        .collect()
}

/// Prefer explicit IPP printer-state-message, else map reasons to labels.
pub fn human_status_message(reasons: &[String], state_message: Option<&str>) -> Option<String> {
    if let Some(msg) = state_message.map(str::trim) {
        if !msg.is_empty() && !["none", "-", "n/a"].contains(&msg.to_lowercase().as_str()) {
            return Some(msg.to_string());
        }
    }
    static SUFFIX: OnceLock<Regex> = OnceLock::new();
    let suffix = SUFFIX.get_or_init(|| regex(r"-(warning|report|error)$"));
    let mut labels: Vec<String> = Vec::new();
    for r in reasons.iter().filter(|r| !is_benign(r)) {
        // media-empty-warning etc. → try base token before -warning/-report
        let label = reason_label(r)
            .or_else(|| reason_label(&suffix.replace(r, "")))
            .map(String::from)
            .unwrap_or_else(|| {
                let mut l = r.replace('-', " ").trim().to_string();
                if let Some(s) = l.strip_suffix(" error") {
                    l = s.to_string();
                }
                let mut chars = l.chars();
                match chars.next() {
                    Some(c) => c.to_uppercase().chain(chars).collect(),
                    None => r.clone(),
                }
            });
        if !label.is_empty() && !labels.contains(&label) {
            labels.push(label);
        }
    }
    (!labels.is_empty()).then(|| labels.join("; "))
}

fn reason_is_actionable(reason: &str) -> bool {
    if is_benign(reason) {
        return false;
    }
    if reason.ends_with("-error") {
        return true;
    }
    ACTIONABLE_REASON_PREFIXES
        .iter()
        .any(|p| reason == *p || reason.starts_with(&format!("{p}-")))
}

/// Upgrade idle/printing → stopped/offline when device conditions present.
fn status_from_state_and_reasons(base_status: &str, reasons: &[String]) -> String {
    if reasons
        .iter()
        .any(|r| OFFLINE_REASONS.contains(&r.as_str()))
    {
        return "offline".into();
    }
    if reasons.iter().any(|r| reason_is_actionable(r)) {
        return "stopped".into();
    }
    match base_status {
        "idle" | "printing" | "stopped" | "offline" | "unknown" => base_status.into(),
        _ => "unknown".into(),
    }
}

/// Extract values for an attribute from `ipptool -tv` output.
///
/// Line-based: empty values like `message = ` must not swallow the next
/// attribute. 1setOf values on one line are comma-separated.
fn parse_ipp_attr_values(text: &str, attr_name: &str) -> Vec<String> {
    let pat = regex(&format!(
        r"(?i)^\s*{}\s*\([^)]*\)\s*=[ \t]*(.*)$",
        regex::escape(attr_name)
    ));
    static COMMA: OnceLock<Regex> = OnceLock::new();
    let comma = COMMA.get_or_init(|| regex(r"\s*,\s*"));
    let mut values = Vec::new();
    for line in text.lines() {
        let Some(c) = pat.captures(line) else {
            continue;
        };
        let raw = c[1].trim();
        if raw.is_empty() {
            continue;
        }
        for p in comma.split(raw) {
            let p = p.trim().trim_matches(['"', '\'']);
            if !p.is_empty() {
                values.push(p.to_string());
            }
        }
    }
    values
}

/// Parsed Get-Printer-Attributes dump.
#[derive(Debug, Clone, PartialEq)]
struct IppPrinterAttrs {
    status: String,
    status_reasons: Vec<String>,
    status_message: Option<String>,
    queued_job_count: i64,
    base_status: String,
}

/// Parse Get-Printer-Attributes ipptool -tv dump → status fields.
fn parse_ipp_printer_attrs(text: &str) -> IppPrinterAttrs {
    let states = parse_ipp_attr_values(text, "printer-state");
    let reason_raw = parse_ipp_attr_values(text, "printer-state-reasons");
    let messages = parse_ipp_attr_values(text, "printer-state-message");
    let queued = parse_ipp_attr_values(text, "queued-job-count");

    // enum may be "processing" or integer "4"
    let base = states
        .first()
        .map(|s| ipp_state(&s.trim().to_lowercase()))
        .unwrap_or("unknown")
        .to_string();
    let reasons = dedupe_reasons(reason_raw.iter().map(|r| normalize_reason(r)));
    let state_message = messages
        .first()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty());
    let queued_job_count = queued.first().and_then(|q| q.parse().ok()).unwrap_or(0);

    IppPrinterAttrs {
        status: status_from_state_and_reasons(&base, &reasons),
        status_message: human_status_message(&reasons, state_message.as_deref()),
        status_reasons: reasons,
        queued_job_count,
        base_status: base,
    }
}

/// Pull condition reasons from active jobs (paper-out often lives here).
fn parse_ipp_jobs_reasons(text: &str) -> Vec<String> {
    let mut reasons = Vec::new();
    for attr in [
        "job-printer-state-reasons",
        "job-state-reasons",
        "job-printer-state-message",
        "job-state-message",
    ] {
        for v in parse_ipp_attr_values(text, attr) {
            if v.contains(' ') && attr.ends_with("message") {
                // Free-text message — keep as pseudo-reason only if known phrase.
                let low = v.trim().to_lowercase();
                if low.contains("out of paper")
                    || low.contains("paper empty")
                    || low.contains("no paper")
                {
                    reasons.push("media-empty".to_string());
                } else if low.contains("jam") {
                    reasons.push("media-jam".to_string());
                } else if low.contains("door") || low.contains("cover") {
                    reasons.push("door-open".to_string());
                }
                continue;
            }
            reasons.push(normalize_reason(&v));
        }
    }
    dedupe_reasons(reasons)
}

/// Run `ipptool -tv` against `uri` with an inline test file; return stdout+stderr.
fn ipptool(uri: &str, test_body: &str, timeout_s: f64) -> String {
    let file = tempfile::Builder::new()
        .prefix("vesyl-ipp-")
        .suffix(".test")
        .tempfile()
        .and_then(|mut f| {
            f.write_all(test_body.as_bytes())?;
            f.flush()?;
            Ok(f)
        });
    let file = match file {
        Ok(f) => f,
        Err(e) => {
            log::debug!(target: LOG, "ipptool {uri} failed: {e}");
            return String::new();
        }
    };
    let t = (timeout_s as i64).max(1).to_string();
    let path = file.path().display().to_string();
    match run_with_timeout(
        "ipptool",
        &["-tv", "-T", &t, uri, &path],
        Duration::from_secs_f64(timeout_s + 2.0),
    ) {
        Ok(o) => o.stdout + &o.stderr,
        Err(e) => {
            log::debug!(target: LOG, "ipptool {uri} failed: {e}");
            String::new()
        }
    }
}

/// CUPS always exposes local queues on the loopback IPP service.
fn ipp_local_uri(queue: &str) -> String {
    format!("ipp://localhost/printers/{queue}")
}

/// Live status for one queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueStatus {
    /// idle | printing | stopped | offline | unknown
    pub status: String,
    pub status_reasons: Vec<String>,
    pub status_message: Option<String>,
}

/// `(uri, test_body, timeout_s) -> ipptool output`.
type IpptoolFn<'a> = &'a dyn Fn(&str, &str, f64) -> String;

/// Best-effort IPP status for a CUPS queue (+ optional device URI merge).
fn ipp_query_queue_with(
    queue: &str,
    device_uri: Option<&str>,
    ipp: IpptoolFn,
) -> Option<QueueStatus> {
    let local_uri = ipp_local_uri(queue);
    let raw = ipp(&local_uri, IPP_GET_PRINTER_TEST, 4.0);
    if raw.is_empty() || !raw.contains("printer-state") {
        return None;
    }
    let mut parsed = parse_ipp_printer_attrs(&raw);

    // Active jobs often carry media-empty / jam when the queue still says
    // "processing" with reasons=none (common on driverless IPP Everywhere).
    let jobs_raw = ipp(&local_uri, IPP_GET_JOBS_TEST, 4.0);
    if !jobs_raw.is_empty() {
        let job_reasons = parse_ipp_jobs_reasons(&jobs_raw);
        if !job_reasons.is_empty() {
            let merged = dedupe_reasons(parsed.status_reasons.iter().cloned().chain(job_reasons));
            parsed.status = status_from_state_and_reasons(&parsed.base_status, &merged);
            parsed.status_message = human_status_message(&merged, parsed.status_message.as_deref());
            parsed.status_reasons = merged;
        }
    }

    // When the local queue still has no actionable reason, ask the device itself.
    // Skip implicitclass / non-IPP backends (no Get-Printer-Attributes).
    if let Some(dev_uri) = device_uri.filter(|u| !u.is_empty()) {
        let actionable = parsed
            .status_reasons
            .iter()
            .any(|r| reason_is_actionable(r));
        let scheme = dev_uri.split(':').next().unwrap_or("").to_lowercase();
        if !actionable && ["ipp", "ipps", "http", "https"].contains(&scheme.as_str()) {
            let dev_raw = ipp(dev_uri, IPP_GET_PRINTER_TEST, 3.0);
            if !dev_raw.is_empty() && dev_raw.contains("printer-state") {
                let dev = parse_ipp_printer_attrs(&dev_raw);
                if !dev.status_reasons.is_empty() {
                    let merged = dedupe_reasons(
                        parsed
                            .status_reasons
                            .iter()
                            .cloned()
                            .chain(dev.status_reasons.iter().cloned()),
                    );
                    // Prefer device base state when more severe / informative.
                    parsed.status = status_from_state_and_reasons(&dev.base_status, &merged);
                    let msg = dev.status_message.or(parsed.status_message);
                    parsed.status_message = human_status_message(&merged, msg.as_deref());
                    parsed.status_reasons = merged;
                }
            }
        }
    }

    Some(QueueStatus {
        status: parsed.status,
        status_reasons: parsed.status_reasons,
        status_message: parsed.status_message,
    })
}

/// Parse `lpstat -p <queue> -l` into (status, reasons) — fallback only.
///
/// `lpstat` rarely exposes media-empty; prefer IPP.
/// Status values match wms-api `Constants::Print::PrinterStates`.
fn parse_lpstat_printer_block(text: &str, _queue: &str) -> (String, Vec<String>) {
    if text.trim().is_empty() {
        return ("unknown".into(), Vec::new());
    }
    let first = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let low = first.to_lowercase();

    static SPLIT: OnceLock<Regex> = OnceLock::new();
    let split = SPLIT.get_or_init(|| regex(r"[,;]+"));
    let push_parts = |rest: &str, reasons: &mut Vec<String>| {
        for part in split.split(rest) {
            let r = normalize_reason(part);
            if !r.is_empty() && !is_benign(&r) {
                reasons.push(r);
            }
        }
    };

    let mut reasons = Vec::new();
    for line in text.lines().skip(1) {
        let stripped = line.trim();
        if stripped.is_empty() {
            continue;
        }
        let lower = stripped.to_lowercase();
        if lower.starts_with("printer ") && !line.starts_with(char::is_whitespace) {
            break;
        }
        if lower.starts_with("alert") {
            push_parts(partition(stripped, ':').1, &mut reasons);
            continue;
        }
        if !stripped.contains(':') && !stripped.contains(' ') {
            let r = normalize_reason(stripped);
            if !r.is_empty() && !is_benign(&r) {
                reasons.push(r);
            }
            continue;
        }
        if lower.contains("reason") && stripped.contains(':') {
            push_parts(partition(stripped, ':').1, &mut reasons);
        }
    }
    let reasons = dedupe_reasons(reasons);

    let base =
        if low.contains(" is offline") || low.ends_with(" offline.") || low.contains(" offline ") {
            "offline"
        } else if low.contains("disabled") {
            "stopped"
        } else if low.contains("printing") {
            "printing"
        } else if low.contains(" is idle") || low.ends_with(" idle.") {
            "idle"
        } else if low.contains("stopped") {
            "stopped"
        } else if !first.is_empty() && low.contains("enable") {
            "idle"
        } else {
            "unknown"
        };
    (status_from_state_and_reasons(base, &reasons), reasons)
}

/// [`cups_queue_status`] with injected `ipptool` and `lpstat -p <queue> -l` runners.
fn cups_queue_status_with(
    queue: &str,
    device_uri: Option<&str>,
    ipp: IpptoolFn,
    lpstat: &dyn Fn(&str) -> String,
) -> QueueStatus {
    if let Some(st) = ipp_query_queue_with(queue, device_uri, ipp) {
        return st;
    }
    let (status, reasons) = parse_lpstat_printer_block(&lpstat(queue), queue);
    QueueStatus {
        status,
        status_message: human_status_message(&reasons, None),
        status_reasons: reasons,
    }
}

/// Live CUPS/IPP status for one queue.
///
/// Prefers IPP Get-Printer-Attributes (via `ipptool`) so media-empty / jam
/// surface correctly. `lpstat -p` only reports idle/printing and is fallback.
pub fn cups_queue_status(queue: &str, device_uri: Option<&str>) -> QueueStatus {
    cups_queue_status_with(queue, device_uri, &ipptool, &|q| {
        run("lpstat", &["-p", q, "-l"], 5)
    })
}

// --- raw-queue probe -------------------------------------------------------

/// Single `lpoptions -p` value for a queue (quoted or bare).
fn lpoption(queue: &str, key: &str) -> String {
    parse_lpoption(&run("lpoptions", &["-p", queue], 3), key)
}

/// Raw-path decision from already-fetched CUPS facts (pure; unit tested).
fn supports_raw_from(model: &str, info: &str, uri: &str) -> bool {
    let (model, info) = (model.to_lowercase(), info.to_lowercase());
    // lpadmin -m raw → "Local Raw Printer"
    if model.contains("raw") || info.contains("raw") {
        return true;
    }
    let uri = uri.trim();
    if uri.is_empty() {
        return false;
    }
    let lower = uri.to_lowercase();
    let scheme = lower.split(':').next().unwrap_or_default();
    // Port-9100 style raw TCP is the usual Zebra / thermal path.
    if scheme == "socket" {
        return true;
    }
    // USB Zebra / usblp queues we provision as raw.
    if scheme == "usb" && (lower.contains("zpl") || lower.contains("zebra")) {
        return true;
    }
    lower.contains("raw")
}

/// [`queue_supports_raw`] with injected `lpoptions` lookup and queue list.
fn queue_supports_raw_with(
    queue: &str,
    device_uri: Option<&str>,
    lpopt: &dyn Fn(&str, &str) -> String,
    queues: &dyn Fn() -> Vec<(String, String)>,
) -> bool {
    let model = lpopt(queue, "printer-make-and-model");
    if model.to_lowercase().contains("raw") {
        return true;
    }
    // printer-info sometimes holds the model when make-and-model is generic.
    let info = lpopt(queue, "printer-info");
    if info.to_lowercase().contains("raw") {
        return true;
    }
    let uri = match device_uri.map(str::trim).filter(|u| !u.is_empty()) {
        Some(u) => u.to_string(),
        None => queues()
            .into_iter()
            .find(|(name, _)| name == queue)
            .map(|(_, u)| u)
            .unwrap_or_default(),
    };
    supports_raw_from(&model, &info, &uri)
}

/// Whether this CUPS queue is a sensible target for `lp -o raw` / ZPL.
///
/// Heuristic only — separate from the WMS user preference for "ZPL printer".
/// Driverless IPP Everywhere queues usually filter raw payloads, so they
/// report false. Dedicated raw queues (`Local Raw Printer`) and classic
/// thermal socket URIs (`socket://host:9100`) report true.
pub fn queue_supports_raw(queue: &str, device_uri: Option<&str>) -> bool {
    queue_supports_raw_with(queue, device_uri, &lpoption, &configured_network_queues)
}

// --- inventory -------------------------------------------------------------

fn inventory_item(
    queue: &str,
    uri: &str,
    display: String,
    st: QueueStatus,
    supports_raw: bool,
) -> Value {
    json!({
        "cups_name": queue,
        "uri": uri,
        "display_name": display,
        "status": st.status,
        "status_reasons": st.status_reasons,
        "status_message": st.status_message,
        "supports_raw": supports_raw,
    })
}

/// CUPS printer inventory (network + USB) for heartbeat / report_printers.
///
/// Each item includes: `cups_name`, `uri`, `display_name`, `status`,
/// `status_reasons` (list), `status_message` (str|null), `supports_raw`
/// (bool — CUPS/raw-path capability heuristic).
pub fn inventory_payload() -> Vec<Value> {
    configured_network_queues()
        .into_iter()
        .map(|(queue, uri)| {
            let st = cups_queue_status(&queue, Some(&uri));
            let raw = queue_supports_raw(&queue, Some(&uri));
            inventory_item(&queue, &uri, display_name(&queue), st, raw)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::net::TcpListener;

    #[test]
    fn parses_lpstat_v() {
        let out = "device for Zebra_ZD421: socket://10.0.0.5:9100\n\
                   device for PDF: cups-pdf:/\n\
                   device for HP: ipp://hp.local/ipp/print\n";
        assert_eq!(
            parse_lpstat_v(out),
            vec![
                (
                    "Zebra_ZD421".to_string(),
                    "socket://10.0.0.5:9100".to_string()
                ),
                ("HP".to_string(), "ipp://hp.local/ipp/print".to_string()),
            ]
        );
    }

    #[test]
    fn parses_lpoptions() {
        let out = "copies=1 printer-info='Zebra ZD421' printer-make-and-model='Local Raw Printer' sides=one-sided";
        assert_eq!(
            parse_lpoption(out, "printer-make-and-model"),
            "Local Raw Printer"
        );
        assert_eq!(parse_lpoption(out, "sides"), "one-sided");
        assert_eq!(parse_lpoption(out, "missing"), "");
    }

    #[test]
    fn display_name_prefers_real_model_then_info() {
        assert_eq!(
            display_name_from("printer-make-and-model='Brother MFC-L2750DW'", "Q"),
            "Brother MFC-L2750DW"
        );
        assert_eq!(
            display_name_from(
                "printer-info='Brother HL' printer-make-and-model='Printer - IPP Everywhere'",
                "Q"
            ),
            "Brother HL"
        );
        assert_eq!(
            display_name_from("printer-make-and-model='Local Raw Printer'", "Q"),
            "Local Raw Printer"
        );
        assert_eq!(display_name_from("", "Queue_1"), "Queue_1");
    }

    #[test]
    fn raw_heuristic() {
        assert!(supports_raw_from("Local Raw Printer", "", ""));
        assert!(supports_raw_from("", "", "socket://10.0.0.5:9100"));
        assert!(supports_raw_from(
            "",
            "",
            "usb://Zebra%20Technologies/ZTC%20ZD421"
        ));
        assert!(!supports_raw_from(
            "Printer - IPP Everywhere",
            "",
            "ipp://hp.local/ipp/print"
        ));
        assert!(!supports_raw_from("", "", ""));
    }

    fn no_queues() -> Vec<(String, String)> {
        Vec::new()
    }

    #[test]
    fn queue_supports_raw_cases() {
        // socket URI with no lpoptions info
        assert!(queue_supports_raw_with(
            "Z",
            Some("socket://192.168.1.10:9100"),
            &|_, _| String::new(),
            &no_queues
        ));
        // IPP Everywhere is not raw
        let ipp = |_: &str, k: &str| match k {
            "printer-make-and-model" => "Printer - IPP Everywhere".to_string(),
            "printer-info" => "Brother HL".to_string(),
            _ => String::new(),
        };
        assert!(!queue_supports_raw_with(
            "Brother",
            Some("ipp://brother.local/ipp/print"),
            &ipp,
            &no_queues
        ));
        // Local Raw Printer model
        let raw = |_: &str, k: &str| match k {
            "printer-make-and-model" => "Local Raw Printer".to_string(),
            "printer-info" => "Zebra".to_string(),
            _ => String::new(),
        };
        assert!(queue_supports_raw_with(
            "Zebra",
            Some("usb://Zebra/ZD420"),
            &raw,
            &|| panic!("queue list not needed")
        ));
        // URI looked up from configured queues when not given
        assert!(queue_supports_raw_with(
            "Z9",
            None,
            &|_, _| String::new(),
            &|| vec![("Z9".to_string(), "socket://10.0.0.9:9100".to_string())]
        ));
    }

    #[test]
    fn run_with_timeout_kills() {
        let err = run_with_timeout("sleep", &["5"], Duration::from_millis(100))
            .err()
            .unwrap();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }

    // --- lpstat status parsing (tests/test_printer_status.py) ---

    fn lpstat(text: &str) -> (String, Vec<String>) {
        parse_lpstat_printer_block(text, "Zebra_1")
    }

    #[test]
    fn lpstat_idle() {
        let (st, reasons) =
            lpstat("printer Zebra_1 is idle.  enabled since Mon 01 Jan 2026 10:00:00 AM UTC\n");
        assert_eq!(st, "idle");
        assert!(reasons.is_empty());
    }

    #[test]
    fn lpstat_printing() {
        let (st, _) =
            lpstat("printer Zebra_1 now printing Zebra_1-42.  enabled since Mon 01 Jan 2026\n");
        assert_eq!(st, "printing");
    }

    #[test]
    fn lpstat_media_empty_stopped() {
        let (st, reasons) = lpstat(
            "printer Zebra_1 now printing Zebra_1-42.  enabled since Mon 01 Jan 2026\n\
             \tAlert: media-empty-error\n\
             \tmedia-empty\n",
        );
        assert_eq!(st, "stopped");
        assert!(reasons.contains(&"media-empty-error".to_string()));
        assert!(reasons.contains(&"media-empty".to_string()));
        assert_eq!(
            human_status_message(&reasons, None).as_deref(),
            Some("Out of paper")
        );
    }

    #[test]
    fn lpstat_paper_jam() {
        let (st, reasons) = lpstat(
            "printer Zebra_1 is idle.  enabled since Mon 01 Jan 2026\n\
             \tAlerts: media-jam-error, media-jam\n",
        );
        assert_eq!(st, "stopped");
        assert_eq!(
            human_status_message(&reasons, None).as_deref(),
            Some("Paper jam")
        );
    }

    #[test]
    fn lpstat_disabled() {
        let (st, _) = lpstat("printer Zebra_1 disabled since Mon 01 Jan 2026 -\n\tPaused\n");
        assert_eq!(st, "stopped");
    }

    #[test]
    fn lpstat_offline_reason() {
        let (st, _) =
            lpstat("printer Zebra_1 is idle.  enabled since Mon 01 Jan 2026\n\toffline\n");
        assert_eq!(st, "offline");
    }

    #[test]
    fn lpstat_empty_is_unknown() {
        assert_eq!(lpstat("  \n"), ("unknown".to_string(), Vec::new()));
    }

    #[test]
    fn human_message_fallbacks() {
        let r = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // -warning suffix maps to base label
        assert_eq!(
            human_status_message(&r(&["media-empty-warning"]), None).as_deref(),
            Some("Out of paper")
        );
        // unknown token → humanized, trailing " error" dropped
        assert_eq!(
            human_status_message(&r(&["fuser-over-temp-error"]), None).as_deref(),
            Some("Fuser over temp")
        );
        // explicit state message wins unless placeholder
        assert_eq!(
            human_status_message(&r(&["media-jam"]), Some(" Load labels ")).as_deref(),
            Some("Load labels")
        );
        assert_eq!(
            human_status_message(&r(&["media-jam"]), Some("none")).as_deref(),
            Some("Paper jam")
        );
        assert_eq!(human_status_message(&r(&["none"]), None), None);
    }

    // --- IPP parsing ---

    #[test]
    fn ipp_processing_with_media_empty() {
        let parsed = parse_ipp_printer_attrs(
            "
        printer-state (enum) = processing
        printer-state-reasons (1setOf keyword) = media-empty,media-needed
        printer-state-message (textWithoutLanguage) =
        queued-job-count (integer) = 1
        ",
        );
        assert_eq!(parsed.status, "stopped");
        assert!(parsed.status_reasons.contains(&"media-empty".to_string()));
        assert_eq!(parsed.status_message.as_deref(), Some("Out of paper"));
        assert_eq!(parsed.queued_job_count, 1);
        assert_eq!(parsed.base_status, "printing");
    }

    #[test]
    fn ipp_processing_media_empty_error_single() {
        let parsed = parse_ipp_printer_attrs(
            "
        printer-state (enum) = processing
        printer-state-reasons (keyword) = media-empty-error
        printer-state-message (textWithoutLanguage) = The printer is out of paper.
        queued-job-count (integer) = 1
        ",
        );
        assert_eq!(parsed.status, "stopped");
        assert_eq!(
            parsed.status_message.as_deref(),
            Some("The printer is out of paper.")
        );
    }

    #[test]
    fn ipp_idle_none() {
        let parsed = parse_ipp_printer_attrs(
            "
        printer-state (enum) = idle
        printer-state-reasons (keyword) = none
        printer-state-message (textWithoutLanguage) =
        queued-job-count (integer) = 0
        ",
        );
        assert_eq!(parsed.status, "idle");
        assert!(parsed.status_reasons.is_empty());
        assert_eq!(parsed.status_message, None);
    }

    #[test]
    fn ipp_numeric_state_and_jam() {
        let parsed = parse_ipp_printer_attrs(
            "
        printer-state (enum) = 5
        printer-state-reasons (keyword) = media-jam-error
        ",
        );
        assert_eq!(parsed.status, "stopped");
        assert_eq!(parsed.status_message.as_deref(), Some("Paper jam"));
    }

    #[test]
    fn job_reasons_media_empty() {
        let reasons = parse_ipp_jobs_reasons(
            "
        job-id (integer) = 42
        job-state (enum) = processing
        job-printer-state-reasons (1setOf keyword) = media-empty-error
        job-state-reasons (keyword) = job-printing
        job-state-message (textWithoutLanguage) = Printer is out of paper
        ",
        );
        assert!(reasons.contains(&"media-empty-error".to_string()));
        assert!(reasons.contains(&"media-empty".to_string()));
        // job-printing is benign and filtered
        assert!(!reasons.contains(&"job-printing".to_string()));
    }

    #[test]
    fn cups_queue_status_prefers_ipp() {
        let dump = "
        printer-state (enum) = processing
        printer-state-reasons (keyword) = media-empty
        printer-state-message (textWithoutLanguage) =
        queued-job-count (integer) = 1
        ";
        let calls = RefCell::new(0);
        let ipp = |_: &str, _: &str, _: f64| {
            *calls.borrow_mut() += 1;
            if *calls.borrow() == 1 {
                dump.to_string()
            } else {
                String::new()
            }
        };
        let st = cups_queue_status_with("Z", None, &ipp, &|_| {
            panic!("lpstat fallback should not run")
        });
        assert_eq!(st.status, "stopped");
        assert_eq!(st.status_message.as_deref(), Some("Out of paper"));
        assert!(st.status_reasons.contains(&"media-empty".to_string()));
    }

    #[test]
    fn cups_queue_status_falls_back_to_lpstat() {
        let st = cups_queue_status_with("Z", None, &|_, _, _| String::new(), &|q| {
            assert_eq!(q, "Z");
            "printer Z is idle.  enabled since Mon\n\tmedia-jam\n".to_string()
        });
        assert_eq!(st.status, "stopped");
        assert_eq!(st.status_message.as_deref(), Some("Paper jam"));
    }

    #[test]
    fn device_uri_merge_when_local_reasons_empty() {
        let local = "
        printer-state (enum) = processing
        printer-state-reasons (keyword) = none
        queued-job-count (integer) = 1
        ";
        let device = "
        printer-state (enum) = stopped
        printer-state-reasons (keyword) = media-empty-error
        printer-state-message (textWithoutLanguage) = Out of paper
        ";
        let ipp = |uri: &str, body: &str, _: f64| {
            if uri.contains("localhost") && body.contains("Get-Jobs") {
                String::new()
            } else if uri.contains("localhost") {
                local.to_string()
            } else {
                device.to_string()
            }
        };
        let st = cups_queue_status_with(
            "Brother_X",
            Some("ipps://brother.local/ipp/print"),
            &ipp,
            &|_| String::new(),
        );
        assert_eq!(st.status, "stopped");
        assert!(st.status_reasons.contains(&"media-empty-error".to_string()));
        assert_eq!(st.status_message.as_deref(), Some("Out of paper"));
    }

    #[test]
    fn inventory_item_shape() {
        let item = inventory_item(
            "Zebra_1",
            "ipp://printer/ipp",
            "Zebra ZD420".into(),
            QueueStatus {
                status: "stopped".into(),
                status_reasons: vec!["media-empty".into()],
                status_message: Some("Out of paper".into()),
            },
            false,
        );
        assert_eq!(item["cups_name"], "Zebra_1");
        assert_eq!(item["uri"], "ipp://printer/ipp");
        assert_eq!(item["display_name"], "Zebra ZD420");
        assert_eq!(item["status"], "stopped");
        assert_eq!(item["status_reasons"], json!(["media-empty"]));
        assert_eq!(item["status_message"], "Out of paper");
        assert_eq!(item["supports_raw"], false);
        let idle = inventory_item(
            "Z",
            "socket://1.2.3.4:9100",
            "Z".into(),
            QueueStatus {
                status: "idle".into(),
                status_reasons: vec![],
                status_message: None,
            },
            true,
        );
        assert!(idle["status_message"].is_null());
        assert_eq!(idle["supports_raw"], true);
    }

    // --- discovery (tests/test_zebra_discovery.py) ---

    const ZEBRA_HOME_HTML: &str = r#"<!DOCTYPE HTML PUBLIC "-//W3C//DTD HTML 3.2 Final//EN">
<HTML>
<HEAD><TITLE>D8J241010914 - READY</TITLE></HEAD>
<BODY><CENTER>
<H1>Zebra Technologies<BR>
ZTC ZD421-203dpi ZPL</H1>
<H2>D8J241010914</H2>
Internal Wired PrintServer<H3>Status: <FONT COLOR="GREEN">READY</FONT></H3>
Home: <A HREF="https://www.zebra.com">https://www.zebra.com</A>
</BODY></HTML>
"#;

    const BROTHER_HOME_HTML: &str = r#"
<html><head><title>Brother HL-L3280CDW</title></head>
<body><h1>Brother</h1><p>Printer Status</p></body></html>
"#;

    #[test]
    fn zebra_identity_parsing() {
        assert_eq!(
            parse_zebra_http_identity(ZEBRA_HOME_HTML).as_deref(),
            Some("Zebra ZD421-203dpi ZPL")
        );
        assert_eq!(parse_zebra_http_identity(BROTHER_HOME_HTML), None);
        assert_eq!(parse_zebra_http_identity(""), None);
        assert_eq!(parse_zebra_http_identity("<html>ok</html>"), None);
        assert_eq!(
            parse_zebra_http_identity("<html>Zebra Technologies print server zebra.com</html>")
                .as_deref(),
            Some("Zebra Printer")
        );
    }

    const LPINFO_USB_SNIPPET: &str = "\
Device: uri = usb://Zebra%20Technologies/ZTC%20ZD220-203dpi%20ZPL?serial=D4N261201258
        class = direct
        info = Zebra Technologies ZTC ZD220-203dpi ZPL
        make-and-model = Zebra Technologies ZTC ZD220-203dpi ZPL
        device-id = MANUFACTURER:Zebra Technologies ;COMMAND SET:ZPL;MODEL:ZTC ZD220-203dpi ZPL;
        location =
Device: uri = ipp
        class = network
        info = Internet Printing Protocol (ipp)
        make-and-model = Unknown
        device-id =
        location =
Device: uri = dnssd://Brother%20HL-L3280CDW%20series._ipp._tcp.local/
        class = network
        info = Brother HL-L3280CDW series
        make-and-model = Brother HL-L3280CDW series
        device-id =
        location =
";

    #[test]
    fn normalize_usb_model_cases() {
        assert_eq!(
            normalize_usb_model("Zebra Technologies ZTC ZD220-203dpi ZPL"),
            "Zebra ZD220-203dpi ZPL"
        );
        assert_eq!(normalize_usb_model("   "), "USB Printer");
        assert_eq!(normalize_usb_model("HP   LaserJet"), "HP LaserJet");
    }

    #[test]
    fn discover_usb_from_lpinfo() {
        let found = usb_printers_from_lpinfo(LPINFO_USB_SNIPPET);
        assert_eq!(found.len(), 1);
        let (uri, model) = &found[0];
        assert!(uri.starts_with("usb://"));
        assert!(model.contains("ZD220"));
        assert!(model_looks_thermal_raw(model));
    }

    #[test]
    fn usb_model_from_uri_when_attrs_missing() {
        let out = "Device: uri = usb://HP/LaserJet%20Pro?serial=1\n        class = direct\n";
        assert_eq!(
            usb_printers_from_lpinfo(out),
            vec![(
                "usb://HP/LaserJet%20Pro?serial=1".to_string(),
                "HP/LaserJet Pro".to_string()
            )]
        );
    }

    #[test]
    fn discover_network_from_lpinfo() {
        let found = network_printers_from_lpinfo(LPINFO_USB_SNIPPET);
        assert_eq!(
            found,
            vec![(
                "dnssd://Brother%20HL-L3280CDW%20series._ipp._tcp.local/".to_string(),
                "Brother HL-L3280CDW series".to_string()
            )]
        );
    }

    #[test]
    fn host_and_ip_helpers() {
        assert_eq!(
            host_from_uri("socket://10.0.0.172:9100").as_deref(),
            Some("10.0.0.172")
        );
        assert_eq!(
            host_from_uri("ipp://printer.local/ipp/print").as_deref(),
            Some("printer.local")
        );
        assert_eq!(
            host_from_uri("ipps://user@[fe80::1]:631/ipp").as_deref(),
            Some("fe80::1")
        );
        assert_eq!(host_from_uri("not-a-uri"), None);
        assert_eq!(host_from_uri("usb:///x"), None);

        let ips = ips_from_device_uris([
            "ipp://10.0.0.50/ipp/print",
            "socket://10.0.0.172:9100",
            "dnssd://Something._ipp._tcp.local/",
        ]);
        assert!(ips.contains("10.0.0.50"));
        assert!(ips.contains("10.0.0.172"));
    }

    #[test]
    fn unquote_matches_python() {
        assert_eq!(unquote("ZTC%20ZD220"), "ZTC ZD220");
        assert_eq!(unquote("100%"), "100%");
        assert_eq!(unquote("%zz%2"), "%zz%2");
        assert_eq!(unquote("caf%C3%A9"), "café");
    }

    #[test]
    fn queue_name_sanitizes() {
        assert_eq!(
            queue_name("Zebra ZD421-203dpi ZPL"),
            "Zebra_ZD421-203dpi_ZPL"
        );
        assert_eq!(queue_name("  !!  "), "printer");
        assert_eq!(queue_name("HP/LaserJet Pro"), "HP_LaserJet_Pro");
    }

    #[test]
    fn identify_zebra_with_fetch() {
        let ok: FetchFn = Arc::new(|url| {
            assert_eq!(url, "http://10.0.0.172/");
            Ok(ZEBRA_HOME_HTML.as_bytes().to_vec())
        });
        assert_eq!(
            identify_zebra_http("10.0.0.172", Duration::from_secs(2), Some(&ok)).as_deref(),
            Some("Zebra ZD421-203dpi ZPL")
        );
        let boom: FetchFn = Arc::new(|_| Err("down".into()));
        assert_eq!(
            identify_zebra_http("10.0.0.1", Duration::from_secs(2), Some(&boom)),
            None
        );
    }

    #[test]
    fn identify_zebra_over_real_http() {
        let srv = crate::testutil::serve(vec![(200, ZEBRA_HOME_HTML), (404, "nope zebra")]);
        let url = format!("{}/", srv.base_url);
        assert_eq!(
            identify_zebra_url(&url, Duration::from_secs(2), None).as_deref(),
            Some("Zebra ZD421-203dpi ZPL")
        );
        // HTTP errors are probe failures, like urllib's HTTPError.
        assert_eq!(identify_zebra_url(&url, Duration::from_secs(2), None), None);
        let reqs = srv.requests.lock().unwrap();
        assert_eq!(reqs[0].header("User-Agent"), Some("vesyl-print-agent"));
    }

    #[test]
    fn discover_zebra_ignores_known_and_identifies() {
        let fetch: FetchFn = Arc::new(|url| {
            Ok(if url.contains("10.0.0.172") {
                ZEBRA_HOME_HTML
            } else {
                BROTHER_HOME_HTML
            }
            .as_bytes()
            .to_vec())
        });
        let found = discover_zebra_socket_printers(&ZebraScan {
            ignore_ips: HashSet::from(["10.0.0.50".to_string()]),
            open_hosts: Some(vec![
                "10.0.0.50".into(),
                "10.0.0.172".into(),
                "10.0.0.200".into(),
            ]),
            fetch: Some(fetch),
            ..ZebraScan::default()
        });
        assert_eq!(
            found,
            vec![(
                "10.0.0.172".to_string(),
                "Zebra ZD421-203dpi ZPL".to_string()
            )]
        );
    }

    #[test]
    fn discover_zebra_no_hosts() {
        let found = discover_zebra_socket_printers(&ZebraScan {
            open_hosts: Some(vec![]),
            fetch: Some(Arc::new(|_| Ok(Vec::new()))),
            ..ZebraScan::default()
        });
        assert!(found.is_empty());
        let none = discover_zebra_socket_printers(&ZebraScan {
            networks: Some(vec![]),
            ..ZebraScan::default()
        });
        assert!(none.is_empty());
    }

    #[test]
    fn lpadmin_raw_socket_command() {
        let args = lpadmin_raw_args(
            &queue_name("Zebra ZD421-203dpi ZPL"),
            "socket://10.0.0.172:9100",
            "Zebra ZD421-203dpi ZPL",
            Some("AppSocket 10.0.0.172:9100"),
        );
        let at = |flag: &str| args[args.iter().position(|a| a == flag).unwrap() + 1].clone();
        assert_eq!(at("-p"), "Zebra_ZD421-203dpi_ZPL");
        assert_eq!(at("-v"), "socket://10.0.0.172:9100");
        assert_eq!(at("-m"), "raw");
        assert_eq!(at("-D"), "Zebra ZD421-203dpi ZPL");
        assert_eq!(at("-L"), "AppSocket 10.0.0.172:9100");
        assert!(args.contains(&"-E".to_string()));

        let everywhere =
            lpadmin_everywhere_args("Brother_HL", "ipp://10.0.0.50/ipp/print", "Brother HL");
        assert_eq!(
            everywhere,
            [
                "-p",
                "Brother_HL",
                "-v",
                "ipp://10.0.0.50/ipp/print",
                "-m",
                "everywhere",
                "-D",
                "Brother HL",
                "-E"
            ]
        );
        assert!(!lpadmin_raw_args("Q", "usb://x", "M", None).contains(&"-L".to_string()));
    }

    #[test]
    fn local_scan_networks_skips_virtual() {
        let sample = "2: enp7s0    inet 10.0.0.164/24 brd 10.0.0.255 scope global enp7s0\n\
                      6: docker0    inet 172.17.0.1/16 brd 172.17.255.255 scope global docker0\n\
                      5: tailscale0    inet 100.109.119.86/32 scope global tailscale0\n\
                      3: wlan0    inet 192.168.4.20/16 brd 192.168.255.255 scope global wlan0\n";
        let nets = scan_networks_from_ip(sample);
        let shown: Vec<String> = nets.iter().map(|n| n.to_string()).collect();
        // /16 capped to the /24 around us
        assert_eq!(shown, ["10.0.0.0/24", "192.168.4.0/24"]);
        let addrs = local_addrs_from_ip(sample);
        assert!(addrs.contains("10.0.0.164") && addrs.contains("172.17.0.1"));
    }

    #[test]
    fn ipv4_net_hosts() {
        let n = Ipv4Net::new(Ipv4Addr::new(10, 0, 0, 164), 24).unwrap();
        let hosts = n.hosts();
        assert_eq!(hosts.len(), 254);
        assert_eq!(hosts[0], Ipv4Addr::new(10, 0, 0, 1));
        assert_eq!(hosts[253], Ipv4Addr::new(10, 0, 0, 254));
        assert_eq!(
            Ipv4Net::new(Ipv4Addr::new(10, 0, 0, 5), 32)
                .unwrap()
                .hosts(),
            [Ipv4Addr::new(10, 0, 0, 5)]
        );
        assert_eq!(
            Ipv4Net::new(Ipv4Addr::new(10, 0, 0, 5), 31)
                .unwrap()
                .hosts()
                .len(),
            2
        );
    }

    #[test]
    fn scan_finds_open_port_on_loopback() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let net = Ipv4Net::new(Ipv4Addr::LOCALHOST, 32).unwrap();
        let open = scan_port_open_hosts(
            &[net],
            port,
            &HashSet::new(),
            Duration::from_millis(500),
            64,
        );
        assert_eq!(open, ["127.0.0.1"]);
        // Ignored IPs are never probed.
        let ignored = scan_port_open_hosts(
            &[net],
            port,
            &HashSet::from(["127.0.0.1".to_string()]),
            Duration::from_millis(500),
            64,
        );
        assert!(ignored.is_empty());
        drop(listener);
    }

    #[test]
    fn probe_hosts_sorts_numerically() {
        let a = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = a.local_addr().unwrap().port();
        // 127.0.0.10 / 127.0.0.2 also route to loopback on Linux; bind both.
        let b = TcpListener::bind(("127.0.0.10", port));
        let c = TcpListener::bind(("127.0.0.2", port));
        if b.is_err() || c.is_err() {
            return; // port taken on alias; nothing to assert
        }
        let cands: Vec<String> = ["127.0.0.10", "127.0.0.2", "127.0.0.1"]
            .map(String::from)
            .to_vec();
        let open = probe_hosts(&cands, port, Duration::from_millis(500), 2);
        assert_eq!(open, ["127.0.0.1", "127.0.0.2", "127.0.0.10"]);
    }

    // --- ensure_printers with a fake CUPS ---

    #[derive(Default)]
    struct FakeCups {
        queues: Vec<(String, String)>,
        usb: Vec<(String, String)>,
        network: Vec<(String, String)>,
        zebras: Vec<(String, String)>,
        zebra_ignore: RefCell<Option<HashSet<String>>>,
        calls: RefCell<Vec<String>>,
    }

    impl Provisioner for FakeCups {
        fn configured_network_queues(&self) -> Vec<(String, String)> {
            self.queues.clone()
        }
        fn configured_printers(&self) -> Vec<String> {
            vec!["final".into()]
        }
        fn discover_usb_printers(&self) -> Vec<(String, String)> {
            self.usb.clone()
        }
        fn discover_network_printers(&self) -> Vec<(String, String)> {
            self.network.clone()
        }
        fn discover_zebra_socket_printers(
            &self,
            ignore: &HashSet<String>,
        ) -> Vec<(String, String)> {
            *self.zebra_ignore.borrow_mut() = Some(ignore.clone());
            self.zebras.clone()
        }
        fn add_printer(&self, uri: &str, model: &str) -> Option<String> {
            self.calls.borrow_mut().push(format!("ipp {uri} {model}"));
            Some(queue_name(model))
        }
        fn add_usb_printer(&self, uri: &str, model: &str, queue: &str) -> Option<String> {
            self.calls
                .borrow_mut()
                .push(format!("usb {uri} {model} {queue}"));
            Some(queue.to_string())
        }
        fn add_raw_socket_printer(&self, ip: &str, model: &str, queue: &str) -> Option<String> {
            self.calls
                .borrow_mut()
                .push(format!("raw {ip} {model} {queue}"));
            Some(queue.to_string())
        }
    }

    #[test]
    fn ensure_adds_usb_raw() {
        let cups = FakeCups {
            usb: vec![(
                "usb://Zebra%20Technologies/ZTC%20ZD220-203dpi%20ZPL?serial=1".into(),
                "Zebra ZD220-203dpi ZPL".into(),
            )],
            ..FakeCups::default()
        };
        assert_eq!(ensure_printers_with(&cups), ["final"]);
        let calls = cups.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].starts_with("usb usb://"));
        assert!(calls[0].ends_with(" Zebra_ZD220-203dpi_ZPL"));
    }

    #[test]
    fn ensure_usb_second_same_model_gets_serial_suffix() {
        let cups = FakeCups {
            queues: vec![(
                "Zebra_ZD220".into(),
                "usb://Zebra/ZD220?serial=AAA111".into(),
            )],
            usb: vec![
                // Same device under its existing serial → skipped.
                (
                    "usb://Zebra/ZD220?serial=AAA111&x=1".into(),
                    "Zebra ZD220".into(),
                ),
                // Second unit → disambiguated by last 6 of serial.
                (
                    "usb://Zebra/ZD220?serial=D4N261201258".into(),
                    "Zebra ZD220".into(),
                ),
            ],
            ..FakeCups::default()
        };
        ensure_printers_with(&cups);
        let calls = cups.calls.borrow();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(calls[0].ends_with(" Zebra_ZD220_201258"), "{calls:?}");
    }

    #[test]
    fn ensure_adds_zebra_after_ipp() {
        let cups = FakeCups {
            network: vec![("ipp://10.0.0.50/ipp/print".into(), "Brother HL".into())],
            zebras: vec![("10.0.0.172".into(), "Zebra ZD421-203dpi ZPL".into())],
            ..FakeCups::default()
        };
        assert_eq!(ensure_printers_with(&cups), ["final"]);
        let calls = cups.calls.borrow();
        assert_eq!(calls[0], "ipp ipp://10.0.0.50/ipp/print Brother HL");
        // Known IPP IP must be passed so we do not re-probe that host.
        assert!(cups
            .zebra_ignore
            .borrow()
            .as_ref()
            .unwrap()
            .contains("10.0.0.50"));
        assert_eq!(
            calls[1],
            "raw 10.0.0.172 Zebra ZD421-203dpi ZPL Zebra_ZD421-203dpi_ZPL"
        );
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn ensure_skips_zebra_ip_already_in_cups() {
        let cups = FakeCups {
            queues: vec![("Z".into(), "socket://10.0.0.172:9100".into())],
            zebras: vec![("10.0.0.172".into(), "Zebra ZD421".into())],
            ..FakeCups::default()
        };
        ensure_printers_with(&cups);
        assert!(cups.calls.borrow().is_empty());
        assert!(cups
            .zebra_ignore
            .borrow()
            .as_ref()
            .unwrap()
            .contains("10.0.0.172"));
    }

    #[test]
    fn ensure_zebra_name_clash_uses_last_octet() {
        let cups = FakeCups {
            queues: vec![("Zebra_ZD421".into(), "ipp://10.0.0.9/ipp".into())],
            zebras: vec![("10.0.0.172".into(), "Zebra ZD421".into())],
            ..FakeCups::default()
        };
        ensure_printers_with(&cups);
        assert_eq!(
            *cups.calls.borrow(),
            ["raw 10.0.0.172 Zebra ZD421 Zebra_ZD421_172"]
        );
    }
}
