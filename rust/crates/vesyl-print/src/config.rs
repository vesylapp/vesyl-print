//! Load vesyl-print configuration (paths, API base URL, intervals).

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::util::{py_int, py_str, truthy};
use crate::JsonObject;

/// Version baked in at build time from the repo-level `VERSION` file.
pub fn agent_version() -> &'static str {
    include_str!("../../../../VERSION").trim()
}

/// Preferred for Pis: direct API host (paths are /print/v1/...).
pub const DEFAULT_API_BASE_URL: &str = "https://wms-api.vesyl.dev";
/// GitHub Releases act as the artifact CDN (see OTA_UPDATES.md / release workflow).
pub const DEFAULT_RELEASES_BASE_URL: &str =
    "https://github.com/vesylapp/vesyl-print/releases/download";

pub const ENV_API_URL: &str = "VESYL_PRINT_API_URL";
pub const ENV_CONFIG_DIR: &str = "VESYL_PRINT_CONFIG_DIR";
pub const ENV_STATE_DIR: &str = "VESYL_PRINT_STATE_DIR";
pub const ENV_INSTALL_ROOT: &str = "VESYL_PRINT_INSTALL_ROOT";

/// e.g. linux-aarch64, linux-x86_64.
pub fn default_platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// After `lp` accepts a job, how to track CUPS completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WaitCups {
    /// Block until the printer finishes (old behavior).
    Sync,
    /// Watch CUPS in the background so the next job can spool immediately.
    #[default]
    Async,
    /// Do not watch CUPS.
    Off,
}

impl WaitCups {
    /// Same normalization as `config._apply_file` / `jobs.normalize_wait_cups`.
    pub fn from_json(raw: &Value) -> Self {
        match raw {
            Value::Bool(true) => return WaitCups::Sync,
            Value::Bool(false) => return WaitCups::Off,
            _ => {}
        }
        match py_str(raw).to_lowercase().as_str() {
            "sync" | "true" | "1" => WaitCups::Sync,
            "off" | "false" | "0" => WaitCups::Off,
            _ => WaitCups::Async,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            WaitCups::Sync => "sync",
            WaitCups::Async => "async",
            WaitCups::Off => "off",
        }
    }
}

fn env_var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn home_dir() -> PathBuf {
    env_var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn user_config_dir(env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    match env("XDG_CONFIG_HOME") {
        Some(xdg) => PathBuf::from(xdg).join("vesyl-print"),
        None => home_dir().join(".config").join("vesyl-print"),
    }
}

fn user_state_dir(env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    match env("XDG_STATE_HOME").or_else(|| env("XDG_DATA_HOME")) {
        Some(xdg) => PathBuf::from(xdg).join("vesyl-print"),
        None => home_dir().join(".local").join("share").join("vesyl-print"),
    }
}

fn resolve_config_dir_with(env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    if let Some(dir) = env(ENV_CONFIG_DIR) {
        return PathBuf::from(dir);
    }
    let system = Path::new("/etc/vesyl-print");
    if system.is_dir() {
        return system.to_path_buf();
    }
    user_config_dir(env)
}

fn resolve_state_dir_with(env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    if let Some(dir) = env(ENV_STATE_DIR) {
        return PathBuf::from(dir);
    }
    let system = Path::new("/var/lib/vesyl-print");
    if system.is_dir() {
        return system.to_path_buf();
    }
    user_state_dir(env)
}

pub fn resolve_config_dir() -> PathBuf {
    resolve_config_dir_with(&env_var)
}

pub fn resolve_state_dir() -> PathBuf {
    resolve_state_dir_with(&env_var)
}

/// Guess ActionCable URL from REST base (used when cable_url omitted).
pub fn derive_cable_url(api_base_url: &str) -> String {
    let base = api_base_url.trim_end_matches('/');
    let origin = base.strip_suffix("/api").unwrap_or(base);
    if let Some(rest) = origin.strip_prefix("https://") {
        return format!("wss://{rest}/print/cable");
    }
    if let Some(rest) = origin.strip_prefix("http://") {
        return format!("ws://{rest}/print/cable");
    }
    format!("{origin}/print/cable")
}

#[derive(Debug, Clone)]
pub struct Config {
    pub api_base_url: String,
    pub cable_url: String,
    pub heartbeat_seconds: i64,
    pub pull_interval_seconds: i64,
    /// Phase C: poll GET /print/v1/jobs/pending (disable if server lacks PR4).
    pub pull_jobs_enabled: bool,
    /// Phase D: ActionCable PrintNodeChannel push (pull remains safety net).
    pub cable_enabled: bool,
    pub wait_cups: WaitCups,
    /// OTA (app): cloud sets desired_agent_version on heartbeat response.
    pub auto_update_enabled: bool,
    pub update_channel: String,
    pub releases_base_url: String,
    pub update_require_signature: bool,
    /// Empty → keys/update_public.pem.
    pub update_public_key_path: String,
    /// Seconds after activate to pass whoami/local health before auto-rollback.
    pub update_health_gate_seconds: i64,
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            api_base_url: DEFAULT_API_BASE_URL.into(),
            cable_url: String::new(),
            heartbeat_seconds: 30,
            pull_interval_seconds: 5,
            pull_jobs_enabled: true,
            cable_enabled: true,
            wait_cups: WaitCups::Async,
            auto_update_enabled: true,
            update_channel: "stable".into(),
            releases_base_url: DEFAULT_RELEASES_BASE_URL.into(),
            update_require_signature: true,
            update_public_key_path: String::new(),
            update_health_gate_seconds: 120,
            config_dir: resolve_config_dir(),
            state_dir: resolve_state_dir(),
        }
        .normalized()
    }
}

impl Config {
    /// Python `__post_init__`: trim trailing slashes, derive cable_url if empty.
    pub fn normalized(mut self) -> Self {
        self.api_base_url = self.api_base_url.trim_end_matches('/').to_string();
        self.releases_base_url = self.releases_base_url.trim_end_matches('/').to_string();
        if self.cable_url.is_empty() {
            self.cable_url = derive_cable_url(&self.api_base_url);
        }
        self
    }

    pub fn credentials_path(&self) -> PathBuf {
        self.config_dir.join("credentials.json")
    }

    pub fn config_path(&self) -> PathBuf {
        self.config_dir.join("config.json")
    }

    pub fn status_path(&self) -> PathBuf {
        self.state_dir.join("status.json")
    }

    pub fn update_status_path(&self) -> PathBuf {
        self.state_dir.join("update_status.json")
    }

    pub fn queue_dir(&self) -> PathBuf {
        self.state_dir.join("queue")
    }

    pub fn processed_dir(&self) -> PathBuf {
        self.state_dir.join("processed")
    }

    /// Create config/state dirs used by agent and CLI (best-effort).
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.config_dir)?;
        fs::create_dir_all(&self.state_dir)?;
        fs::create_dir_all(self.queue_dir())?;
        fs::create_dir_all(self.processed_dir())
    }

    /// Apply config.json keys. Like Python, a bad value stops processing at
    /// that key (earlier keys stay applied).
    fn apply_file(&mut self, data: &JsonObject) -> Result<(), ()> {
        let get = |k: &str| data.get(k);
        if let Some(url) = get("api_base_url").filter(|v| truthy(v)) {
            self.api_base_url = py_str(url).trim_end_matches('/').to_string();
        }
        if let Some(cable) = get("cable_url").filter(|v| truthy(v)) {
            self.cable_url = py_str(cable);
        }
        if let Some(v) = get("heartbeat_seconds") {
            self.heartbeat_seconds = py_int(v).ok_or(())?;
        }
        if let Some(v) = get("pull_interval_seconds") {
            self.pull_interval_seconds = py_int(v).ok_or(())?;
        }
        if let Some(v) = get("pull_jobs_enabled") {
            self.pull_jobs_enabled = truthy(v);
        }
        if let Some(v) = get("cable_enabled") {
            self.cable_enabled = truthy(v);
        }
        if let Some(v) = get("wait_cups") {
            self.wait_cups = WaitCups::from_json(v);
        }
        if let Some(v) = get("auto_update_enabled") {
            self.auto_update_enabled = truthy(v);
        }
        if let Some(ch) = get("update_channel").filter(|v| truthy(v)) {
            self.update_channel = py_str(ch);
        }
        if let Some(rb) = get("releases_base_url").filter(|v| truthy(v)) {
            self.releases_base_url = py_str(rb).trim_end_matches('/').to_string();
        }
        if let Some(v) = get("update_require_signature") {
            self.update_require_signature = truthy(v);
        }
        if let Some(kp) = get("update_public_key_path").filter(|v| truthy(v)) {
            self.update_public_key_path = py_str(kp);
        }
        if let Some(v) = get("update_health_gate_seconds") {
            if let Some(n) = py_int(v) {
                self.update_health_gate_seconds = n.max(15);
            }
        }
        Ok(())
    }
}

/// Load config.json + env overrides. Missing file is fine (defaults).
pub fn load_config(config_dir: Option<&Path>, state_dir: Option<&Path>) -> Config {
    load_config_with(config_dir, state_dir, &env_var)
}

pub(crate) fn load_config_with(
    config_dir: Option<&Path>,
    state_dir: Option<&Path>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Config {
    let cdir = config_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| resolve_config_dir_with(env));
    let sdir = state_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| resolve_state_dir_with(env));
    let mut cfg = Config {
        config_dir: cdir,
        state_dir: sdir,
        ..Config::default()
    };

    let mut file_cable: Option<String> = None;
    let path = cfg.config_path();
    if path.is_file() {
        if let Ok(Value::Object(data)) = fs::read_to_string(&path)
            .map_err(|_| ())
            .and_then(|s| serde_json::from_str::<Value>(&s).map_err(|_| ()))
        {
            if let Some(c) = data.get("cable_url").filter(|v| truthy(v)) {
                file_cable = Some(py_str(c));
            }
            let _ = cfg.apply_file(&data);
        }
    }

    if let Some(url) = env(ENV_API_URL) {
        cfg.api_base_url = url;
    }
    cfg.api_base_url = cfg.api_base_url.trim_end_matches('/').to_string();
    // Env API change should re-derive cable unless config.json set it explicitly.
    cfg.cable_url = file_cable.unwrap_or_else(|| derive_cable_url(&cfg.api_base_url));
    cfg
}

/// Write a starter config.json if missing. Returns path written/existing.
pub fn write_default_config(path: Option<&Path>) -> std::io::Result<PathBuf> {
    let cfg = load_config(None, None);
    let out = path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cfg.config_path());
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent)?;
    }
    if out.is_file() {
        return Ok(out);
    }
    let payload = json!({
        "api_base_url": cfg.api_base_url,
        "cable_url": cfg.cable_url,
        "heartbeat_seconds": cfg.heartbeat_seconds,
        "pull_interval_seconds": cfg.pull_interval_seconds,
        "pull_jobs_enabled": true,
        "cable_enabled": true,
        "wait_cups": "async",
        "auto_update_enabled": true,
        "update_channel": "stable",
        "releases_base_url": DEFAULT_RELEASES_BASE_URL,
    });
    let mut raw = serde_json::to_string_pretty(&payload).expect("static json");
    raw.push('\n');
    fs::write(&out, raw)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn env_api_url_override() {
        let td = tempfile::tempdir().unwrap();
        let cdir = td.path().join("cfg");
        let sdir = td.path().join("state");
        fs::create_dir_all(&cdir).unwrap();
        fs::create_dir_all(&sdir).unwrap();
        fs::write(
            cdir.join("config.json"),
            r#"{"api_base_url": "https://file.example/api", "heartbeat_seconds": 15}"#,
        )
        .unwrap();
        let env: HashMap<&str, String> = HashMap::from([
            (ENV_API_URL, "https://wms-api.vesyl.dev".to_string()),
            (ENV_CONFIG_DIR, cdir.display().to_string()),
            (ENV_STATE_DIR, sdir.display().to_string()),
        ]);
        let cfg = load_config_with(None, None, &|k| env.get(k).cloned());
        assert_eq!(cfg.api_base_url, "https://wms-api.vesyl.dev");
        assert_eq!(cfg.heartbeat_seconds, 15);
        assert_eq!(cfg.cable_url, "wss://wms-api.vesyl.dev/print/cable");
        assert_eq!(cfg.config_dir, cdir);
    }

    #[test]
    fn file_cable_url_wins_over_derived() {
        let td = tempfile::tempdir().unwrap();
        fs::write(
            td.path().join("config.json"),
            r#"{"cable_url": "wss://custom/print/cable", "wait_cups": "sync"}"#,
        )
        .unwrap();
        let cfg = load_config_with(Some(td.path()), Some(td.path()), &|_| None);
        assert_eq!(cfg.cable_url, "wss://custom/print/cable");
        assert_eq!(cfg.wait_cups, WaitCups::Sync);
    }

    #[test]
    fn direct_api_default_cable() {
        let cfg = Config {
            api_base_url: "https://wms.api.vesyl.com".into(),
            cable_url: String::new(),
            ..Config::default()
        }
        .normalized();
        assert_eq!(cfg.cable_url, "wss://wms.api.vesyl.com/print/cable");
    }

    #[test]
    fn edge_api_prefix_cable() {
        let cfg = Config {
            api_base_url: "https://wms.staging.vesyl.com/api".into(),
            cable_url: String::new(),
            ..Config::default()
        }
        .normalized();
        assert_eq!(cfg.cable_url, "wss://wms.staging.vesyl.com/print/cable");
    }

    #[test]
    fn defaults() {
        let cfg = Config::default();
        assert!(cfg.cable_enabled);
        assert_eq!(cfg.wait_cups, WaitCups::Async);
    }

    #[test]
    fn wait_cups_normalization() {
        assert_eq!(WaitCups::from_json(&json!(true)), WaitCups::Sync);
        assert_eq!(WaitCups::from_json(&json!("OFF")), WaitCups::Off);
        assert_eq!(WaitCups::from_json(&json!(0)), WaitCups::Off);
        assert_eq!(WaitCups::from_json(&json!("whatever")), WaitCups::Async);
    }
}
