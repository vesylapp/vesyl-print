//! CUPS queue helpers.
//!
//! Only the pieces the job pipeline needs are ported so far
//! (`queue_supports_raw` and its dependencies). Discovery, provisioning,
//! status and inventory from `printers.py` are still Python-only.

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use regex::Regex;

const NETWORK_URI_SCHEMES: &[&str] = &[
    "ipp", "ipps", "http", "https", "socket", "lpd", "dnssd", "smb",
];
/// Local USB printers (usblp / libusb backends).
const USB_URI_SCHEMES: &[&str] = &["usb"];
/// CUPS admin tools often live in /usr/sbin (not on a minimal user PATH).
const CUPS_BIN_DIRS: &[&str] = &["/usr/sbin", "/usr/bin", "/bin"];

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

fn uri_scheme(uri: &str) -> String {
    if uri.is_empty() || !uri.contains("://") {
        return String::new();
    }
    uri.split(':').next().unwrap_or_default().to_lowercase()
}

/// Network or USB queues we auto-provision and show on the LCD.
fn is_managed_uri(uri: &str) -> bool {
    let s = uri_scheme(uri);
    NETWORK_URI_SCHEMES.contains(&s.as_str()) || USB_URI_SCHEMES.contains(&s.as_str())
}

/// Parse `lpstat -v` lines of the form "device for <name>: <uri>".
fn parse_lpstat_v(out: &str) -> Vec<(String, String)> {
    const PREFIX: &str = "device for ";
    out.lines()
        .filter_map(|line| line.strip_prefix(PREFIX))
        .filter_map(|rest| {
            let (name, uri) = rest.split_once(':').unwrap_or((rest, ""));
            let uri = uri.trim();
            is_managed_uri(uri).then(|| (name.trim().to_string(), uri.to_string()))
        })
        .collect()
}

/// (queue_name, uri) for every CUPS queue with a network **or USB** device URI.
pub fn configured_network_queues() -> Vec<(String, String)> {
    parse_lpstat_v(&run("lpstat", &["-v"], 3))
}

fn parse_lpoption(out: &str, key: &str) -> String {
    let k = regex::escape(key);
    let quoted = Regex::new(&format!("{k}='([^']*)'")).expect("regex");
    let bare = Regex::new(&format!(r"{k}=(\S+)")).expect("regex");
    quoted
        .captures(out)
        .or_else(|| bare.captures(out))
        .map(|c| c[1].trim().to_string())
        .unwrap_or_default()
}

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

/// Whether this CUPS queue is a sensible target for `lp -o raw` / ZPL.
///
/// Heuristic only — separate from the WMS user preference for "ZPL printer".
/// Driverless IPP Everywhere queues usually filter raw payloads, so they
/// report false. Dedicated raw queues (`Local Raw Printer`) and classic
/// thermal socket URIs (`socket://host:9100`) report true.
pub fn queue_supports_raw(queue: &str, device_uri: Option<&str>) -> bool {
    let model = lpoption(queue, "printer-make-and-model");
    if model.to_lowercase().contains("raw") {
        return true;
    }
    // printer-info sometimes holds the model when make-and-model is generic.
    let info = lpoption(queue, "printer-info");
    let uri = match device_uri.map(str::trim).filter(|u| !u.is_empty()) {
        Some(u) => u.to_string(),
        None => configured_network_queues()
            .into_iter()
            .find(|(name, _)| name == queue)
            .map(|(_, u)| u)
            .unwrap_or_default(),
    };
    supports_raw_from(&model, &info, &uri)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn run_with_timeout_kills() {
        let err = run_with_timeout("sleep", &["5"], Duration::from_millis(100))
            .err()
            .unwrap();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }
}
