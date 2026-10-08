//! vesyl-print CLI: claim, enroll, status, queues, unpair, agent, version,
//! update, print-test.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};

use crate::agent::{status_from_creds, Agent};
use crate::auth::{self, Credentials};
use crate::cloud::{CloudClient, CloudError, HeartbeatBody};
use crate::config::{agent_version, default_platform, load_config, write_default_config, Config};
use crate::jobs::{self, JobStore, Pipeline};
use crate::statusio::{self, CloudState, PairingState};
use crate::update::{self, ReleaseManifest, UpdateEnv};
use crate::{printers, sysinfo, JsonObject};

/// Parsing follows Python's argparse, which this CLI replaces: unambiguous
/// prefixes of long options are accepted (`--ch` for `--check`) and a
/// repeated option keeps its last value. Both settings reach every subcommand.
#[derive(Parser, Debug)]
#[command(
    name = "vesyl-print",
    version = agent_version(),
    about = "VESYL print node — claim, enroll, status, queues, agent, print-test, update",
    infer_long_args = true,
    args_override_self = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Pair this node with an 8-char claim code
    Claim {
        /// Claim code (dashes optional)
        #[arg(allow_negative_numbers = true)]
        code: String,
        /// Optional display name for this node
        #[arg(long)]
        name: Option<String>,
    },
    /// Pair with a headless enrollment token
    Enroll {
        /// Enrollment token
        token: String,
        /// Optional display name for this node
        #[arg(long)]
        name: Option<String>,
    },
    /// Show local pairing and cloud status
    Status {
        /// Call whoami against the API
        #[arg(long)]
        check: bool,
    },
    /// List configured CUPS printer queues
    Queues {
        /// Print queue inventory as JSON
        #[arg(long)]
        json: bool,
    },
    /// Delete local credentials only
    Unpair,
    /// Run the cloud agent (heartbeat loop)
    Agent {
        #[arg(short, long)]
        verbose: bool,
    },
    /// Show agent version and install slots
    Version,
    /// Check / apply / rollback app OTA
    Update {
        #[command(subcommand)]
        action: UpdateAction,
    },
    /// Print a local file via durable queue + CUPS (no cloud)
    PrintTest(PrintTestArgs),
}

#[derive(Subcommand, Debug)]
pub enum UpdateAction {
    /// Show current vs cloud desired version
    Check,
    /// Download+install update
    Apply {
        /// Target version (uses releases_base_url)
        #[arg(long)]
        version: Option<String>,
        /// Direct manifest URL
        #[arg(long)]
        manifest_url: Option<String>,
        /// Local release tarball (with --manifest)
        #[arg(long)]
        file: Option<PathBuf>,
        /// Local manifest JSON (with --file)
        #[arg(long)]
        manifest: Option<PathBuf>,
        /// Restart agent/display after apply
        #[arg(long)]
        restart: bool,
    },
    /// Activate previous release slot
    Rollback {
        /// Explicit version to roll back to
        #[arg(long)]
        version: Option<String>,
        #[arg(long)]
        restart: bool,
    },
}

#[derive(Args, Debug)]
pub struct PrintTestArgs {
    /// Path to PDF/PNG/JPEG/ZPL (or any file CUPS accepts)
    #[arg(short, long)]
    file: PathBuf,
    /// CUPS queue name (default: first network printer)
    #[arg(short, long)]
    queue: Option<String>,
    /// Job title for lp -t
    #[arg(long)]
    title: Option<String>,
    /// Number of copies (default 1)
    #[arg(long, default_value_t = 1, allow_negative_numbers = true)]
    copies: i64,
    /// Submit with lp -o raw (ZPL/EPL thermal queues)
    #[arg(long)]
    raw: bool,
}

/// Fatal CLI error: printed to stderr, exit status 1 (Python `_die`).
#[derive(Debug)]
pub struct Die(pub String);

impl<E: std::fmt::Display> From<E> for Die {
    fn from(e: E) -> Self {
        Die(e.to_string())
    }
}

type CmdResult = Result<u8, Die>;

fn die<T>(msg: impl Into<String>) -> Result<T, Die> {
    Err(Die(msg.into()))
}

fn cloud_msg(prefix: &str, e: &CloudError) -> String {
    match &e.code {
        Some(code) => format!("{prefix}: {} ({code})", e.message),
        None => format!("{prefix}: {}", e.message),
    }
}

/// Python `oct(mode)`.
fn oct(mode: u32) -> String {
    format!("0o{mode:o}")
}

/// External effects the CLI depends on, injectable for tests.
pub struct Deps {
    pub cfg: Config,
    pub update_env: UpdateEnv,
    pub inventory: Box<dyn Fn() -> Result<Vec<Value>, String>>,
}

impl Deps {
    pub fn system() -> Self {
        let cfg = load_config(None, None);
        Deps {
            update_env: UpdateEnv::detect(&cfg),
            inventory: Box::new(|| Ok(printers::inventory_payload())),
            cfg,
        }
    }

    fn client(&self) -> CloudClient {
        CloudClient::new(&self.cfg.api_base_url)
    }
}

/// Binary entry point.
pub fn main() -> ExitCode {
    let cli = Cli::parse();
    let deps = Deps::system();
    let mut out = std::io::stdout().lock();
    match run(cli.command, &deps, &mut out) {
        Ok(code) => ExitCode::from(code),
        Err(Die(msg)) => {
            let _ = out.flush();
            eprintln!("{msg}");
            ExitCode::FAILURE
        }
    }
}

pub fn run(cmd: Command, deps: &Deps, out: &mut dyn Write) -> CmdResult {
    match cmd {
        Command::Claim { code, name } => cmd_claim(deps, out, &code, name.as_deref()),
        Command::Enroll { token, name } => cmd_enroll(deps, out, &token, name.as_deref()),
        Command::Status { check } => cmd_status(deps, out, check),
        Command::Queues { json } => cmd_queues(deps, out, json),
        Command::Unpair => cmd_unpair(deps, out),
        Command::Agent { verbose } => cmd_agent(deps, verbose),
        Command::Version => cmd_version(deps, out),
        Command::Update { action } => cmd_update(deps, out, action),
        Command::PrintTest(args) => cmd_print_test(deps, out, args),
    }
}

/// Save credentials and mark the LCD paired/offline until the first heartbeat.
fn save_pairing(cfg: &Config, data: &JsonObject, what: &str) -> Result<Credentials, Die> {
    if !data.get("device_token").is_some_and(crate::util::truthy) {
        return die(format!("{what} response missing device_token"));
    }
    let creds = auth::credentials_from_pair_response(data)?;
    auth::save_credentials(&cfg.credentials_path(), &creds)?;
    let mut st = status_from_creds(
        Some(&creds),
        PairingState::Paired,
        CloudState::Offline,
        None,
        None,
    );
    statusio::write_status(&cfg.status_path(), &mut st)?;
    Ok(creds)
}

fn prepare_dirs(cfg: &Config) -> Result<(), Die> {
    cfg.ensure_dirs()?;
    write_default_config(Some(&cfg.config_path()))?;
    Ok(())
}

pub fn normalize_claim_code(code: &str) -> String {
    code.trim().replace(['-', ' '], "").to_uppercase()
}

fn cmd_claim(deps: &Deps, out: &mut dyn Write, code: &str, name: Option<&str>) -> CmdResult {
    let cfg = &deps.cfg;
    prepare_dirs(cfg)?;
    let code = normalize_claim_code(code);
    if code.len() < 6 {
        return die("claim code looks too short");
    }
    let data = deps
        .client()
        .claim(
            &code,
            &sysinfo::hostname(),
            agent_version(),
            &default_platform(),
            name,
        )
        .map_err(|e| Die(cloud_msg("claim failed", &e)))?;
    let creds = save_pairing(cfg, &data, "claim")?;
    let mode = auth::credentials_mode(&cfg.credentials_path()).unwrap_or(0);
    writeln!(out, "Paired successfully.")?;
    writeln!(out, "  node_id:      {}", creds.node_id)?;
    writeln!(
        out,
        "  name:         {}",
        creds.name.as_deref().unwrap_or("—")
    )?;
    writeln!(
        out,
        "  organization: {}",
        creds.organization_name.as_deref().unwrap_or("—")
    )?;
    writeln!(out, "  warehouse:    {}", creds.warehouse_label())?;
    writeln!(
        out,
        "  credentials:  {} (mode {})",
        cfg.credentials_path().display(),
        oct(mode)
    )?;
    writeln!(out, "  (device_token stored; not shown)")?;
    writeln!(out, "Restart or wait for vesyl-print-agent to heartbeat.")?;
    Ok(0)
}

fn cmd_enroll(deps: &Deps, out: &mut dyn Write, token: &str, name: Option<&str>) -> CmdResult {
    let cfg = &deps.cfg;
    prepare_dirs(cfg)?;
    let token = token.trim();
    if token.is_empty() {
        return die("enrollment token required");
    }
    let data = deps
        .client()
        .enroll(
            token,
            &sysinfo::hostname(),
            agent_version(),
            Some(&default_platform()),
            name,
        )
        .map_err(|e| Die(cloud_msg("enroll failed", &e)))?;
    let creds = save_pairing(cfg, &data, "enroll")?;
    writeln!(out, "Enrolled successfully.")?;
    writeln!(out, "  node_id:      {}", creds.node_id)?;
    writeln!(
        out,
        "  organization: {}",
        creds.organization_name.as_deref().unwrap_or("—")
    )?;
    writeln!(out, "  warehouse:    {}", creds.warehouse_label())?;
    writeln!(out, "  credentials:  {}", cfg.credentials_path().display())?;
    writeln!(out, "  (device_token stored; not shown)")?;
    Ok(0)
}

fn pairing_str(p: PairingState) -> &'static str {
    match p {
        PairingState::Unpaired => "unpaired",
        PairingState::Paired => "paired",
        PairingState::Revoked => "revoked",
    }
}

fn cloud_str(c: CloudState) -> &'static str {
    match c {
        CloudState::Unknown => "unknown",
        CloudState::Online => "online",
        CloudState::Offline => "offline",
    }
}

fn cmd_status(deps: &Deps, out: &mut dyn Write, check: bool) -> CmdResult {
    let cfg = &deps.cfg;
    let creds = auth::load_credentials(&cfg.credentials_path());
    let st = statusio::read_status(&cfg.status_path());

    writeln!(out, "api_base_url:  {}", cfg.api_base_url)?;
    writeln!(out, "config:        {}", cfg.config_path().display())?;
    writeln!(out, "credentials:   {}", cfg.credentials_path().display())?;
    writeln!(out, "status file:   {}", cfg.status_path().display())?;
    writeln!(out, "agent_version: {}", agent_version())?;
    writeln!(out)?;

    let Some(creds) = creds else {
        let pairing = st.as_ref().map(|s| s.pairing).unwrap_or_default();
        writeln!(out, "pairing:       {}", pairing_str(pairing))?;
        if pairing == PairingState::Revoked {
            writeln!(out, "  Re-pair required: vesyl-print claim <CODE>")?;
        } else {
            writeln!(out, "  Not paired. Claim with: vesyl-print claim <CODE>")?;
        }
        if let Some(st) = &st {
            writeln!(out, "cloud:         {}", cloud_str(st.cloud))?;
            if let Some(err) = &st.last_error {
                writeln!(out, "last_error:    {err}")?;
            }
        }
        return Ok(0);
    };

    writeln!(out, "pairing:       paired (local credentials present)")?;
    writeln!(out, "node_id:       {}", creds.node_id)?;
    writeln!(
        out,
        "name:          {}",
        creds.name.as_deref().unwrap_or("—")
    )?;
    writeln!(
        out,
        "organization:  {}",
        creds.organization_name.as_deref().unwrap_or("—")
    )?;
    writeln!(out, "warehouse:     {}", creds.warehouse_label())?;
    let mode = auth::credentials_mode(&cfg.credentials_path()).map(oct);
    writeln!(out, "cred mode:     {}", mode.as_deref().unwrap_or("—"))?;
    if let Some(st) = &st {
        writeln!(out, "cloud:         {}", cloud_str(st.cloud))?;
        writeln!(
            out,
            "last_heartbeat:{}",
            st.last_heartbeat_at.as_deref().unwrap_or("—")
        )?;
        if let Some(err) = &st.last_error {
            writeln!(out, "last_error:    {err}")?;
        }
    }

    if check {
        writeln!(out)?;
        match deps.client().whoami(&creds.device_token) {
            Ok(who) => {
                writeln!(out, "whoami: OK")?;
                // Public fields only.
                let public: JsonObject = [
                    "node_id",
                    "name",
                    "hostname",
                    "status",
                    "warehouse",
                    "organization",
                ]
                .iter()
                .filter_map(|k| who.get(*k).map(|v| (k.to_string(), v.clone())))
                .collect();
                writeln!(out, "{}", serde_json::to_string_pretty(&public)?)?;
            }
            Err(e) => {
                let code = e
                    .code
                    .as_ref()
                    .map(|c| format!(" ({c})"))
                    .unwrap_or_default();
                writeln!(out, "whoami: FAILED — {}{code}", e.message)?;
                return Ok(1);
            }
        }
    }
    Ok(0)
}

/// Formats offered for a queue: PDF+ZPL on raw/Zebra, PDF only otherwise.
pub fn test_print_formats(supports_raw: bool) -> &'static [&'static str] {
    if supports_raw {
        &["pdf", "zpl"]
    } else {
        &["pdf"]
    }
}

fn str_field(item: &Value, key: &str) -> String {
    item.get(key)
        .filter(|v| crate::util::truthy(v))
        .map(crate::util::py_str)
        .unwrap_or_default()
}

/// Public fields for `vesyl-print queues`, including test-print formats.
pub fn queue_rows(items: &[Value]) -> Vec<Value> {
    items
        .iter()
        .map(|item| {
            let raw = item.get("supports_raw").is_some_and(crate::util::truthy);
            let status = str_field(item, "status");
            json!({
                "cups_name": str_field(item, "cups_name"),
                "display_name": str_field(item, "display_name"),
                "status": if status.is_empty() { "unknown".into() } else { status },
                "status_message": item.get("status_message").cloned().unwrap_or(Value::Null),
                "uri": str_field(item, "uri"),
                "supports_raw": raw,
                "test_formats": test_print_formats(raw),
            })
        })
        .collect()
}

/// Prefer a human reason next to the CUPS state when one exists.
fn queue_status_label(row: &Value) -> String {
    let status = str_field(row, "status");
    let status = if status.is_empty() {
        "unknown".to_string()
    } else {
        status
    };
    let message = str_field(row, "status_message").trim().to_string();
    if !message.is_empty() && message != status {
        format!("{status} ({message})")
    } else {
        status
    }
}

/// Human listing of configured CUPS queues. Empty inventory is one line.
pub fn format_queues(items: &[Value]) -> String {
    let rows = queue_rows(items);
    if rows.is_empty() {
        return "No CUPS queues configured.".into();
    }
    let or_dash = |s: String| if s.is_empty() { "—".to_string() } else { s };
    rows.iter()
        .map(|row| {
            let name = or_dash(str_field(row, "cups_name"));
            let display = Some(str_field(row, "display_name"))
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| name.clone());
            let formats: Vec<&str> = row["test_formats"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let formats = or_dash(formats.join(", "));
            let raw = if row["supports_raw"] == Value::Bool(true) {
                "yes"
            } else {
                "no"
            };
            [
                name,
                format!("  display:  {display}"),
                format!("  status:   {}", queue_status_label(row)),
                format!("  raw:      {raw}"),
                format!("  formats:  {formats}"),
                format!("  uri:      {}", or_dash(str_field(row, "uri"))),
            ]
            .join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// List configured CUPS queues (the names `print-test --queue` accepts).
fn cmd_queues(deps: &Deps, out: &mut dyn Write, as_json: bool) -> CmdResult {
    let items = (deps.inventory)().map_err(|e| Die(format!("could not list CUPS queues: {e}")))?;
    if as_json {
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&queue_rows(&items))?
        )?;
    } else {
        writeln!(out, "{}", format_queues(&items))?;
    }
    Ok(0)
}

fn cmd_unpair(deps: &Deps, out: &mut dyn Write) -> CmdResult {
    let cfg = &deps.cfg;
    let removed = auth::clear_credentials(&cfg.credentials_path())?;
    let mut st = status_from_creds(
        None,
        PairingState::Unpaired,
        CloudState::Unknown,
        None,
        None,
    );
    statusio::write_status(&cfg.status_path(), &mut st)?;
    if removed {
        writeln!(
            out,
            "Removed credentials at {}",
            cfg.credentials_path().display()
        )?;
    } else {
        writeln!(out, "No local credentials to remove.")?;
    }
    writeln!(out, "Local unpair only — cloud node record is unchanged.")?;
    Ok(0)
}

fn cmd_agent(deps: &Deps, verbose: bool) -> CmdResult {
    crate::logging::init(verbose);
    let stop = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(sig, stop.clone())?;
    }
    Agent::new(deps.cfg.clone()).run(stop);
    Ok(0)
}

fn cmd_version(deps: &Deps, out: &mut dyn Write) -> CmdResult {
    let env = &deps.update_env;
    let cur = update::current_release_dir(&env.install_root);
    writeln!(out, "agent_version:  {}", update::package_version())?;
    writeln!(out, "platform:       {}", default_platform())?;
    writeln!(out, "install_root:   {}", env.install_root.display())?;
    match cur {
        Some(c) => writeln!(out, "current_slot:   {}", c.display())?,
        None => writeln!(out, "current_slot:   —(running from source tree)")?,
    }
    let releases = update::list_releases(&env.install_root);
    if !releases.is_empty() {
        writeln!(out, "releases:       {}", releases.join(", "))?;
    }
    if let Some(ust) = update::read_update_status(&deps.cfg.update_status_path()) {
        writeln!(out, "update_status:  {}", ust.status)?;
        if let Some(v) = &ust.target_version {
            writeln!(out, "target_version: {v}")?;
        }
        if let Some(v) = &ust.previous_version {
            writeln!(out, "previous_slot:  {v}")?;
        }
        if let Some(v) = &ust.health_deadline_at {
            writeln!(out, "health_deadline:{v}")?;
        }
        if let Some(v) = &ust.last_error {
            writeln!(out, "last_error:     {v}")?;
        }
    }
    Ok(0)
}

fn heartbeat_now(deps: &Deps, creds: &Credentials) -> Result<JsonObject, Die> {
    let body = HeartbeatBody {
        agent_version: Some(update::package_version().into()),
        hostname: Some(sysinfo::hostname()),
        platform: Some(default_platform()),
        ..Default::default()
    };
    deps.client()
        .heartbeat(&creds.device_token, &body)
        .map_err(|e| Die(format!("heartbeat failed: {}", e.message)))
}

fn hb_str(hb: &JsonObject, a: &str, b: &str) -> Option<String> {
    hb.get(a)
        .filter(|v| crate::util::truthy(v))
        .or_else(|| hb.get(b).filter(|v| crate::util::truthy(v)))
        .map(crate::util::py_str)
}

fn cmd_update(deps: &Deps, out: &mut dyn Write, action: UpdateAction) -> CmdResult {
    let cfg = &deps.cfg;
    cfg.ensure_dirs()?;
    let env = &deps.update_env;

    match action {
        UpdateAction::Check => {
            writeln!(out, "current:  {}", update::package_version())?;
            writeln!(out, "channel:  {}", cfg.update_channel)?;
            writeln!(out, "releases: {}", cfg.releases_base_url)?;
            writeln!(
                out,
                "auto:     {}",
                if cfg.auto_update_enabled {
                    "True"
                } else {
                    "False"
                }
            )?;
            let Some(creds) = auth::load_credentials(&cfg.credentials_path()) else {
                writeln!(out, "paired:   no — claim first for cloud desired version")?;
                return Ok(0);
            };
            let hb = heartbeat_now(deps, &creds)?;
            let desired = hb_str(&hb, "desired_agent_version", "desired_version");
            writeln!(
                out,
                "desired:  {}",
                desired
                    .as_deref()
                    .unwrap_or("—(server did not set desired_agent_version)")
            )?;
            if let Some(m) = hb_str(&hb, "update_url", "manifest_url") {
                writeln!(out, "manifest: {m}")?;
            }
            if desired.is_some_and(|d| update::version_cmp(&d, update::package_version()).is_ne()) {
                writeln!(out, "status:   update available")?;
            } else {
                writeln!(out, "status:   up to date (or no desired version)")?;
            }
            Ok(0)
        }

        UpdateAction::Apply {
            file: Some(file),
            manifest,
            restart,
            ..
        } => apply_local(deps, out, &file, manifest.as_deref(), restart),

        UpdateAction::Apply {
            manifest_url: Some(url),
            restart,
            ..
        } => {
            // Signatures required: a key that cannot be loaded is an error.
            let pem = update::manifest_public_key(cfg)?;
            let manifest = update::fetch_manifest(&url)?;
            let previous = update::slot_before_activation(env);
            update::apply_release(&manifest, env, pem.as_deref(), cfg.update_require_signature)?;
            writeln!(out, "applied {}", manifest.version)?;
            after_manual_activation(deps, out, &manifest.version, previous, restart)?;
            Ok(0)
        }

        UpdateAction::Apply { version, .. } => {
            // Online: heartbeat desired + update_url, or explicit --version.
            let Some(creds) = auth::load_credentials(&cfg.credentials_path()) else {
                return die("not paired and no --manifest-url / --file");
            };
            let mut hb = heartbeat_now(deps, &creds)?;
            if let Some(v) = version {
                let url = hb_str(&hb, "update_url", "update_url")
                    .unwrap_or_else(|| update::default_manifest_url(&cfg.releases_base_url, &v));
                hb.insert("desired_agent_version".into(), json!(v));
                hb.insert("update_url".into(), json!(url));
            }
            // Restart only once update_status.json says pending_health, so the
            // restarted agent always finds the health gate armed.
            let apply_env = UpdateEnv {
                restart: false,
                ..env.clone()
            };
            let ust = update::maybe_update_from_heartbeat(&hb, cfg, &apply_env, None, None, false);
            update::write_update_status(&cfg.update_status_path(), &ust)?;
            if ust.status == update::STATUS_PENDING_HEALTH && env.restart {
                update::restart_services(env.apply_helper.as_deref());
            }
            writeln!(out, "{}", serde_json::to_string_pretty(&ust.to_dict())?)?;
            Ok(if ust.status == update::STATUS_FAILED {
                1
            } else {
                0
            })
        }

        UpdateAction::Rollback { version, restart } => {
            let ver = update::rollback(
                &env.install_root,
                version.as_deref(),
                env.apply_helper.as_deref(),
            )?;
            writeln!(out, "rolled back to {ver}")?;
            if restart {
                update::restart_services(env.apply_helper.as_deref());
            }
            Ok(0)
        }
    }
}

/// Offline tarball + manifest: verify, extract and flip `current` in-process.
fn apply_local(
    deps: &Deps,
    out: &mut dyn Write,
    file: &Path,
    manifest: Option<&Path>,
    restart: bool,
) -> CmdResult {
    let cfg = &deps.cfg;
    let Some(manifest_path) = manifest.filter(|p| p.is_file()) else {
        return die("--manifest PATH required with --file");
    };
    let data: Value = serde_json::from_str(&fs::read_to_string(manifest_path)?)?;
    let Value::Object(data) = data else {
        return die("manifest must be a JSON object");
    };
    let manifest = ReleaseManifest::from_dict(&data)?;
    let tarball = fs::canonicalize(file)
        .ok()
        .filter(|p| p.is_file())
        .ok_or_else(|| Die(format!("file not found: {}", file.display())))?;
    let sha = update::sha256_file(&tarball)?;
    if sha != manifest.artifact_sha256 {
        return die(format!(
            "sha256 mismatch: file={sha} manifest={}",
            manifest.artifact_sha256
        ));
    }
    // Verification is skipped only when signatures are disabled in config; an
    // unreadable configured key is an error.
    let pem = update::manifest_public_key(cfg)?;
    if cfg.update_require_signature {
        update::verify_manifest(&manifest, pem.as_deref(), true)?;
    }
    let root = &deps.update_env.install_root;
    let previous = update::slot_before_activation(&deps.update_env);
    let release_dir = root.join("releases").join(&manifest.version);
    if release_dir.exists() {
        fs::remove_dir_all(&release_dir)?;
    }
    update::extract_tarball(&tarball, &release_dir)?;
    update::write_version_file(&release_dir, &manifest.version)?;
    update::flip_current(root, &manifest.version)?;
    writeln!(
        out,
        "activated {} at {}",
        manifest.version,
        root.join("current").display()
    )?;
    after_manual_activation(deps, out, &manifest.version, previous, restart)?;
    Ok(0)
}

/// After `update apply --file/--manifest-url` activated `version`.
///
/// With `--restart`, arm the post-update health gate first (as the heartbeat
/// path does), so a new slot that cannot reach the API rolls itself back to
/// `previous`. The agent being replaced may still finish a cycle after the
/// restart is queued; it leaves the gate to its successor (see
/// [`update::process_pending_health`]). Without `--restart` the old agent
/// keeps running, so a gate armed now would expire unrestarted and roll the
/// activation back.
fn after_manual_activation(
    deps: &Deps,
    out: &mut dyn Write,
    version: &str,
    previous: Option<String>,
    restart: bool,
) -> Result<(), Die> {
    if !restart {
        writeln!(
            out,
            "{version} starts on the next service restart (--restart also arms the post-update health gate)"
        )?;
        return Ok(());
    }
    let st = update::arm_health_gate(&deps.cfg, &deps.cfg.update_status_path(), version, previous)
        .map_err(|e| {
            Die(format!(
                "activated {version} but could not arm the health gate ({e}); services not restarted"
            ))
        })?;
    let rollback = match &st.previous_version {
        Some(prev) => format!("rollback to {prev}"),
        None => "no previous slot to roll back to".into(),
    };
    writeln!(
        out,
        "pending_health until {} ({rollback})",
        st.health_deadline_at.as_deref().unwrap_or("?")
    )?;
    // `restart` is false only in tests (UpdateEnv::detect always sets it).
    if deps.update_env.restart {
        update::restart_services(deps.update_env.apply_helper.as_deref());
        writeln!(out, "services restarted")?;
    }
    Ok(())
}

/// Submit a local file through the durable job pipeline (no cloud).
fn cmd_print_test(deps: &Deps, out: &mut dyn Write, args: PrintTestArgs) -> CmdResult {
    let cfg = &deps.cfg;
    cfg.ensure_dirs()?;
    if !args.file.is_file() {
        return die(format!("file not found: {}", args.file.display()));
    }
    let queue = match args.queue {
        Some(q) => q,
        None => {
            // Prefer first configured CUPS network queue name (not display name).
            let Some((name, _)) = printers::configured_network_queues().into_iter().next() else {
                return die("no CUPS network printers; pass --queue <cups_name>");
            };
            writeln!(out, "Using CUPS queue: {name}")?;
            name
        }
    };
    let job = jobs::job_from_local_file(
        &args.file,
        &queue,
        None,
        args.title.as_deref(),
        args.copies,
        args.raw,
    )
    .map_err(|e| Die(e.message))?;
    let store = JobStore::from_config(cfg);
    writeln!(out, "job_id:     {}", job.id)?;
    writeln!(out, "file:       {}", args.file.display())?;
    writeln!(out, "cups_name:  {queue}")?;
    writeln!(
        out,
        "raw:        {}",
        if args.raw { "True" } else { "False" }
    )?;
    writeln!(out, "queue_dir:  {}", store.queue_dir.display())?;
    out.flush()?;

    let state = Pipeline::default()
        .process(&job, &store)
        .map_err(|e| Die(format!("print failed: {} ({})", e.message, e.code)))?;
    writeln!(out, "result:     {}", state.as_str())?;
    if store.is_processed(&job.id) {
        writeln!(
            out,
            "processed:  {}",
            store.processed_path(&job.id).display()
        )?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::InventoryFn;
    use crate::cloud::http_stub::{self, respond};
    use crate::testutil::serve;
    use std::sync::atomic::Ordering;
    use std::sync::{mpsc, Mutex};
    use std::thread;
    use std::time::Duration;

    fn sample() -> Vec<Value> {
        serde_json::from_value(json!([
            {
                "cups_name": "Zebra_ZD220",
                "display_name": "Zebra ZD220-203dpi ZPL",
                "uri": "socket://192.168.1.50:9100",
                "status": "idle",
                "status_reasons": [],
                "status_message": null,
                "supports_raw": true,
            },
            {
                "cups_name": "Brother_HL",
                "display_name": "Brother HL-L3280CDW",
                "uri": "ipp://brother.local/ipp/print",
                "status": "stopped",
                "status_reasons": ["media-empty"],
                "status_message": "Out of paper",
                "supports_raw": false,
            },
        ]))
        .unwrap()
    }

    fn deps(td: &Path, base_url: &str) -> Deps {
        let cfg = Config {
            api_base_url: base_url.into(),
            config_dir: td.join("cfg"),
            state_dir: td.join("state"),
            ..Config::default()
        }
        .normalized();
        Deps {
            update_env: UpdateEnv {
                install_root: td.join("install"),
                apply_helper: None,
                running_version: agent_version().into(),
                running_from_slot: false,
                restart: false,
            },
            inventory: Box::new(|| Ok(sample())),
            cfg,
        }
    }

    fn run_args(deps: &Deps, argv: &[&str]) -> (CmdResult, String) {
        let cli = Cli::try_parse_from(std::iter::once("vesyl-print").chain(argv.iter().copied()))
            .unwrap();
        let mut out = Vec::new();
        let r = run(cli.command, deps, &mut out);
        (r, String::from_utf8(out).unwrap())
    }

    #[test]
    fn format_queues_lists_name_status_and_formats() {
        let text = format_queues(&sample());
        for needle in [
            "Zebra_ZD220",
            "display:  Zebra ZD220-203dpi ZPL",
            "status:   idle",
            "raw:      yes",
            "formats:  pdf, zpl",
            "socket://192.168.1.50:9100",
            "Brother_HL",
            "status:   stopped (Out of paper)",
            "raw:      no",
            "formats:  pdf",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in\n{text}");
        }
        assert!(!text.split_once("Brother_HL").unwrap().1.contains("zpl"));
    }

    #[test]
    fn format_queues_empty() {
        assert_eq!(format_queues(&[]), "No CUPS queues configured.");
    }

    #[test]
    fn queues_json() {
        let td = tempfile::tempdir().unwrap();
        let (r, out) = run_args(
            &deps(td.path(), "http://127.0.0.1:9"),
            &["queues", "--json"],
        );
        assert_eq!(r.unwrap(), 0);
        let data: Vec<Value> = serde_json::from_str(&out).unwrap();
        let names: Vec<&str> = data
            .iter()
            .map(|r| r["cups_name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["Zebra_ZD220", "Brother_HL"]);
        assert_eq!(data[0]["test_formats"], json!(["pdf", "zpl"]));
        assert_eq!(data[1]["test_formats"], json!(["pdf"]));
        assert_eq!(data[0]["supports_raw"], true);
        assert_eq!(data[1]["supports_raw"], false);
    }

    #[test]
    fn queues_human() {
        let td = tempfile::tempdir().unwrap();
        let (r, out) = run_args(&deps(td.path(), "http://127.0.0.1:9"), &["queues"]);
        assert_eq!(r.unwrap(), 0);
        assert_eq!(out.trim_end_matches('\n'), format_queues(&sample()));
    }

    #[test]
    fn queues_inventory_error() {
        let td = tempfile::tempdir().unwrap();
        let mut d = deps(td.path(), "http://127.0.0.1:9");
        d.inventory = Box::new(|| Err("lpstat missing".into()));
        let (r, _) = run_args(&d, &["queues"]);
        assert_eq!(
            r.unwrap_err().0,
            "could not list CUPS queues: lpstat missing"
        );
    }

    #[test]
    fn claim_normalizes_code_and_saves_credentials() {
        let td = tempfile::tempdir().unwrap();
        let srv = serve(vec![(
            201,
            r#"{"node_id":"n1","device_token":"secret-tok","name":"Pack 1",
                "organization":{"name":"Acme"},"warehouse":{"name":"Main","code":"MAIN"}}"#,
        )]);
        let d = deps(td.path(), &srv.base_url);
        let (r, out) = run_args(&d, &["claim", "ab7k-2q9m", "--name", "Pack 1"]);
        assert_eq!(r.unwrap(), 0);
        assert!(out.contains("Paired successfully."));
        assert!(out.contains("(mode 0o600)"));
        assert!(!out.contains("secret-tok"));
        let body: Value = serde_json::from_slice(&srv.requests.lock().unwrap()[0].body).unwrap();
        assert_eq!(body["code"], "AB7K2Q9M");
        assert_eq!(body["name"], "Pack 1");
        assert_eq!(
            auth::load_credentials(&d.cfg.credentials_path())
                .unwrap()
                .device_token,
            "secret-tok"
        );
        let st = statusio::read_status(&d.cfg.status_path()).unwrap();
        assert_eq!(
            (st.pairing, st.cloud),
            (PairingState::Paired, CloudState::Offline)
        );
        assert!(d.cfg.config_path().is_file(), "default config.json written");
    }

    #[test]
    fn claim_rejects_short_code_and_reports_api_error() {
        let td = tempfile::tempdir().unwrap();
        let srv = serve(vec![(
            422,
            r#"{"error":{"code":"invalid_code","message":"Unknown claim code"}}"#,
        )]);
        let d = deps(td.path(), &srv.base_url);
        assert_eq!(
            run_args(&d, &["claim", "ab-1"]).0.unwrap_err().0,
            "claim code looks too short"
        );
        assert_eq!(
            run_args(&d, &["claim", "ZZZZZZZZ"]).0.unwrap_err().0,
            "claim failed: Unknown claim code (invalid_code)"
        );
    }

    #[test]
    fn status_and_unpair() {
        let td = tempfile::tempdir().unwrap();
        let srv = serve(vec![(
            200,
            r#"{"node_id":"n1","name":"Pack 1","device_token":"x"}"#,
        )]);
        let d = deps(td.path(), &srv.base_url);
        let (r, out) = run_args(&d, &["status"]);
        assert_eq!(r.unwrap(), 0);
        assert!(out.contains("pairing:       unpaired"));

        let creds = auth::credentials_from_pair_response(
            json!({"node_id": "n1", "device_token": "tok", "name": "Pack 1"})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        auth::save_credentials(&d.cfg.credentials_path(), &creds).unwrap();
        let (r, out) = run_args(&d, &["status", "--check"]);
        assert_eq!(r.unwrap(), 0);
        assert!(out.contains("pairing:       paired (local credentials present)"));
        assert!(out.contains("cred mode:     0o600"));
        assert!(out.contains("whoami: OK"));
        assert!(
            !out.contains("device_token"),
            "whoami output filtered to public fields"
        );

        let (r, out) = run_args(&d, &["unpair"]);
        assert_eq!(r.unwrap(), 0);
        assert!(out.starts_with("Removed credentials at"));
        assert!(auth::load_credentials(&d.cfg.credentials_path()).is_none());
        let (_, out) = run_args(&d, &["unpair"]);
        assert!(out.starts_with("No local credentials to remove."));
    }

    #[test]
    fn status_check_failure_exits_1() {
        let td = tempfile::tempdir().unwrap();
        let srv = serve(vec![(
            401,
            r#"{"error":{"code":"unauthorized","message":"bad token"}}"#,
        )]);
        let d = deps(td.path(), &srv.base_url);
        let creds = auth::credentials_from_pair_response(
            json!({"node_id": "n1", "device_token": "tok"})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        auth::save_credentials(&d.cfg.credentials_path(), &creds).unwrap();
        let (r, out) = run_args(&d, &["status", "--check"]);
        assert_eq!(r.unwrap(), 1);
        assert!(
            out.contains("whoami: FAILED — bad token (unauthorized)"),
            "{out}"
        );
    }

    #[test]
    fn version_and_rollback() {
        let td = tempfile::tempdir().unwrap();
        let d = deps(td.path(), "http://127.0.0.1:9");
        let root = &d.update_env.install_root;
        for v in ["0.3.0", "0.4.0"] {
            fs::create_dir_all(root.join("releases").join(v)).unwrap();
        }
        update::flip_current(root, "0.4.0").unwrap();
        let (r, out) = run_args(&d, &["version"]);
        assert_eq!(r.unwrap(), 0);
        assert!(out.contains("releases:       0.3.0, 0.4.0"), "{out}");
        let (r, out) = run_args(&d, &["update", "rollback"]);
        assert_eq!(r.unwrap(), 0);
        assert_eq!(out, "rolled back to 0.3.0\n");
        assert_eq!(
            update::current_release_version(root).as_deref(),
            Some("0.3.0")
        );
    }

    /// Release tarball (Rust binary entrypoint) and its unsigned manifest,
    /// whose `artifact_url` points at the tarball (`file://`).
    fn release(td: &Path, version: &str) -> (PathBuf, PathBuf) {
        let src = td.join(format!("src-{version}"));
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("vesyl-print"), b"bin").unwrap();
        let tarball = td.join(format!("vesyl-print-{version}.tar.gz"));
        let gz = flate2::write::GzEncoder::new(
            fs::File::create(&tarball).unwrap(),
            flate2::Compression::default(),
        );
        let mut tar = tar::Builder::new(gz);
        tar.append_dir_all(format!("vesyl-print-{version}"), &src)
            .unwrap();
        tar.into_inner().unwrap().finish().unwrap();
        let manifest = td.join(format!("m-{version}.json"));
        fs::write(
            &manifest,
            json!({
                "version": version,
                "artifact_url": url::Url::from_file_path(&tarball).unwrap().to_string(),
                "artifact_sha256": update::sha256_file(&tarball).unwrap(),
            })
            .to_string(),
        )
        .unwrap();
        (tarball, manifest)
    }

    fn file_url(p: &Path) -> String {
        url::Url::from_file_path(p).unwrap().to_string()
    }

    fn unsigned_deps(td: &Path) -> Deps {
        let d = deps(td, "http://127.0.0.1:9");
        Deps {
            cfg: Config {
                update_require_signature: false,
                ..d.cfg
            },
            ..d
        }
    }

    /// The slot an operator activates the new release from.
    fn installed_slot(d: &Deps, version: &str) {
        let root = &d.update_env.install_root;
        fs::create_dir_all(root.join("releases").join(version)).unwrap();
        update::flip_current(root, version).unwrap();
    }

    #[test]
    fn update_apply_local_file() {
        let td = tempfile::tempdir().unwrap();
        let d = unsigned_deps(td.path());
        let (tarball, manifest) = release(td.path(), "0.9.0");

        let (r, out) = run_args(
            &d,
            &[
                "update",
                "apply",
                "--file",
                tarball.to_str().unwrap(),
                "--manifest",
                manifest.to_str().unwrap(),
            ],
        );
        assert_eq!(r.unwrap(), 0, "{out}");
        assert!(out.starts_with("activated 0.9.0"));
        assert_eq!(
            update::current_release_version(&d.update_env.install_root).as_deref(),
            Some("0.9.0")
        );
        // Without --restart the old agent keeps running: no gate is armed
        // (it would expire unrestarted and roll the activation back), only
        // a hint is printed.
        assert!(out.contains("--restart also arms the post-update health gate"));
        assert!(!d.cfg.update_status_path().exists());

        fs::write(&manifest, json!({"version": "0.9.1", "artifact_url": "file:///x", "artifact_sha256": "0".repeat(64)}).to_string()).unwrap();
        let (r, _) = run_args(
            &d,
            &[
                "update",
                "apply",
                "--file",
                tarball.to_str().unwrap(),
                "--manifest",
                manifest.to_str().unwrap(),
            ],
        );
        assert!(r.unwrap_err().0.starts_with("sha256 mismatch"));
    }

    #[test]
    fn update_apply_requires_pairing_or_source() {
        let td = tempfile::tempdir().unwrap();
        let d = deps(td.path(), "http://127.0.0.1:9");
        let (r, _) = run_args(&d, &["update", "apply"]);
        assert_eq!(
            r.unwrap_err().0,
            "not paired and no --manifest-url / --file"
        );
    }

    /// `update apply … --restart` must leave pending_health for the restarted
    /// agent, with the slot it replaced as the rollback target.
    fn assert_gate_armed(d: &Deps, out: &str, version: &str, previous: &str) {
        let st = update::read_update_status(&d.cfg.update_status_path()).expect("status");
        assert_eq!(st.status, update::STATUS_PENDING_HEALTH, "{out}");
        assert_eq!(st.target_version.as_deref(), Some(version));
        assert_eq!(st.previous_version.as_deref(), Some(previous));
        assert!(st.health_deadline_at.is_some());
        assert!(out.contains(&format!("(rollback to {previous})")), "{out}");
        // UpdateEnv.restart is false in tests: nothing was really restarted.
        assert!(!out.contains("services restarted"));

        // The armed gate does its job: the new slot never reaches the API.
        let expired = update::UpdateStatus {
            health_deadline_at: Some("2000-01-01T00:00:00+00:00".into()),
            ..st
        };
        let after = update::process_pending_health(
            expired,
            &d.cfg,
            &d.update_env,
            update::WhoamiResult::Error,
            Some("timeout"),
            None,
        );
        assert_eq!(after.status, update::STATUS_ROLLED_BACK);
        assert_eq!(
            update::current_release_version(&d.update_env.install_root).as_deref(),
            Some(previous)
        );
    }

    #[test]
    fn update_apply_file_with_restart_arms_health_gate() {
        let td = tempfile::tempdir().unwrap();
        let d = unsigned_deps(td.path());
        installed_slot(&d, "0.8.0");
        let (tarball, manifest) = release(td.path(), "0.9.0");
        let (r, out) = run_args(
            &d,
            &[
                "update",
                "apply",
                "--file",
                tarball.to_str().unwrap(),
                "--manifest",
                manifest.to_str().unwrap(),
                "--restart",
            ],
        );
        assert_eq!(r.unwrap(), 0, "{out}");
        assert!(out.starts_with("activated 0.9.0"));
        assert_gate_armed(&d, &out, "0.9.0", "0.8.0");
    }

    #[test]
    fn update_apply_manifest_url_with_restart_arms_health_gate() {
        let td = tempfile::tempdir().unwrap();
        let d = unsigned_deps(td.path());
        installed_slot(&d, "0.8.0");
        let (_, manifest) = release(td.path(), "0.9.0");
        let url = file_url(&manifest);
        let (r, out) = run_args(
            &d,
            &["update", "apply", "--manifest-url", &url, "--restart"],
        );
        assert_eq!(r.unwrap(), 0, "{out}");
        assert!(out.starts_with("applied 0.9.0"));
        assert_gate_armed(&d, &out, "0.9.0", "0.8.0");

        // Without --restart: activated, but no gate (the old agent still runs).
        let td = tempfile::tempdir().unwrap();
        let d = unsigned_deps(td.path());
        installed_slot(&d, "0.8.0");
        let (_, manifest) = release(td.path(), "0.9.0");
        let (r, out) = run_args(
            &d,
            &["update", "apply", "--manifest-url", &file_url(&manifest)],
        );
        assert_eq!(r.unwrap(), 0, "{out}");
        assert!(!d.cfg.update_status_path().exists());
    }

    // --- `update apply … --restart` while the agent it replaces is mid-cycle --

    /// Where the replaced agent's cycle is held while the CLI runs.
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum HeldIn {
        Whoami,
        Inventory,
    }

    /// Holds the first call that reaches it until the test lets it go.
    struct Hold {
        first: AtomicBool,
        arrived: mpsc::Sender<()>,
        go: Mutex<mpsc::Receiver<()>>,
    }

    impl Hold {
        fn here(&self) {
            if self.first.swap(false, Ordering::SeqCst) {
                let _ = self.arrived.send(());
                let _ = self
                    .go
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(20));
            }
        }
    }

    /// A paired agent at `version`, started from its slot (`current`).
    fn slot_agent(d: &Deps, version: &str, inventory: InventoryFn) -> Agent {
        Agent {
            cfg: d.cfg.clone(),
            client: CloudClient::new(&d.cfg.api_base_url),
            store: JobStore::from_config(&d.cfg),
            inventory,
            update_env: UpdateEnv {
                running_version: version.into(),
                running_from_slot: true,
                ..d.update_env.clone()
            },
            pipeline: Pipeline::default(),
        }
    }

    /// `update apply <how> --restart` while the 0.8.0 agent it replaces is
    /// held at `held_in`. systemd lets that agent finish its cycle after the
    /// restart is queued; it must neither judge 0.9.0 (and roll it back) nor
    /// write back the status it read before the gate was armed. The restarted
    /// 0.9.0 agent then finds the gate and passes it.
    fn apply_while_replaced_agent_is_mid_cycle(how: &str, held_in: HeldIn) {
        let ctx = format!("update apply {how} with the old agent held in {held_in:?}");
        let td = tempfile::tempdir().unwrap();
        let (tarball, manifest) = release(td.path(), "0.9.0");
        let url = file_url(&manifest);
        let hb = match how {
            "online" => json!({"ok": true, "desired_agent_version": "0.9.0", "update_url": url}),
            _ => json!({"ok": true}),
        }
        .to_string();

        let (arrived, arrived_rx) = mpsc::channel();
        let (go_tx, go) = mpsc::channel();
        let hold = Arc::new(Hold {
            first: AtomicBool::new(true),
            arrived,
            go: Mutex::new(go),
        });
        let api_hold = hold.clone();
        let srv = http_stub::serve(move |req, stream| {
            if req.path.ends_with("/whoami") {
                if held_in == HeldIn::Whoami {
                    api_hold.here();
                }
                respond(stream, 200, &[], br#"{"node_id":"n1"}"#);
            } else {
                respond(stream, 200, &[], hb.as_bytes());
            }
        });
        let base = deps(td.path(), &srv.base_url);
        let d = Deps {
            cfg: Config {
                update_require_signature: false,
                ..base.cfg
            },
            ..base
        };
        d.cfg.ensure_dirs().unwrap();
        installed_slot(&d, "0.8.0");
        let creds = auth::credentials_from_pair_response(
            json!({"node_id": "n1", "device_token": "tok"})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        auth::save_credentials(&d.cfg.credentials_path(), &creds).unwrap();
        let status_path = d.cfg.update_status_path();
        // Left by the old agent's earlier cycles.
        let idle = update::UpdateStatus {
            current_version: "0.8.0".into(),
            ..Default::default()
        };
        update::write_update_status(&status_path, &idle).unwrap();

        let inventory_hold = hold.clone();
        let old = slot_agent(
            &d,
            "0.8.0",
            Arc::new(move || {
                if held_in == HeldIn::Inventory {
                    inventory_hold.here();
                }
                Some(Vec::new())
            }),
        );
        let (t, m) = (tarball.to_str().unwrap(), manifest.to_str().unwrap());
        let argv = match how {
            "--file" => vec!["update", "apply", "--file", t, "--manifest", m, "--restart"],
            "--manifest-url" => vec!["update", "apply", "--manifest-url", &url, "--restart"],
            _ => vec!["update", "apply", "--restart"],
        };
        let cycle = thread::scope(|s| {
            // Dropped on a panic below, which lets the held call go.
            let go_tx = go_tx;
            let cycle = s.spawn(|| old.run_once(false));
            arrived_rx
                .recv_timeout(Duration::from_secs(10))
                .unwrap_or_else(|e| panic!("{ctx}: the agent never got there: {e}"));
            let (r, out) = run_args(&d, &argv);
            assert_eq!(r.unwrap(), 0, "{ctx}: {out}");
            go_tx.send(()).unwrap();
            cycle.join().unwrap()
        });

        assert_eq!(cycle.last_error, None, "{ctx}");
        let root = &d.update_env.install_root;
        assert_eq!(
            update::current_release_version(root).as_deref(),
            Some("0.9.0"),
            "{ctx}: the old agent rolled the activation back"
        );
        let st = update::read_update_status(&status_path).expect("status");
        assert_eq!(st.status, update::STATUS_PENDING_HEALTH, "{ctx}: {st:?}");
        assert_eq!(st.target_version.as_deref(), Some("0.9.0"), "{ctx}");
        assert_eq!(st.previous_version.as_deref(), Some("0.8.0"), "{ctx}");

        // After the restart.
        let new = slot_agent(&d, "0.9.0", Arc::new(|| Some(Vec::new())));
        assert_eq!(new.run_once(false).last_error, None, "{ctx}");
        let st = update::read_update_status(&status_path).expect("status");
        assert_eq!(st.status, update::STATUS_IDLE, "{ctx}: {st:?}");
        assert_eq!(st.current_version, "0.9.0", "{ctx}");
        assert_eq!(
            update::current_release_version(root).as_deref(),
            Some("0.9.0"),
            "{ctx}"
        );
    }

    #[test]
    fn update_apply_file_restart_survives_the_replaced_agent() {
        apply_while_replaced_agent_is_mid_cycle("--file", HeldIn::Whoami);
        apply_while_replaced_agent_is_mid_cycle("--file", HeldIn::Inventory);
    }

    #[test]
    fn update_apply_manifest_url_restart_survives_the_replaced_agent() {
        apply_while_replaced_agent_is_mid_cycle("--manifest-url", HeldIn::Whoami);
        apply_while_replaced_agent_is_mid_cycle("--manifest-url", HeldIn::Inventory);
    }

    /// The online `update apply` (heartbeat desired version) had the same race.
    #[test]
    fn update_apply_online_survives_the_replaced_agent() {
        apply_while_replaced_agent_is_mid_cycle("online", HeldIn::Whoami);
        apply_while_replaced_agent_is_mid_cycle("online", HeldIn::Inventory);
    }

    #[test]
    fn update_apply_fails_closed_on_unreadable_key() {
        let td = tempfile::tempdir().unwrap();
        // A DER key where PEM is expected (or a corrupt / unreadable file).
        let der = td.path().join("update_public.der");
        fs::write(
            &der,
            [0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0xff],
        )
        .unwrap();
        let base = deps(td.path(), "http://127.0.0.1:9");
        let d = Deps {
            cfg: Config {
                update_public_key_path: der.display().to_string(),
                ..base.cfg
            },
            ..base
        };
        assert!(d.cfg.update_require_signature);
        installed_slot(&d, "0.8.0");
        // Unsigned: had verification been skipped, these would install.
        let (tarball, manifest) = release(td.path(), "0.9.0");
        let url = file_url(&manifest);
        for argv in [
            vec!["update", "apply", "--manifest-url", &url, "--restart"],
            vec![
                "update",
                "apply",
                "--file",
                tarball.to_str().unwrap(),
                "--manifest",
                manifest.to_str().unwrap(),
            ],
        ] {
            let (r, out) = run_args(&d, &argv);
            let err = r.unwrap_err().0;
            assert!(err.contains("is not a PEM file"), "{argv:?}: {err} {out}");
            assert_eq!(
                update::current_release_version(&d.update_env.install_root).as_deref(),
                Some("0.8.0"),
                "{argv:?} activated a release"
            );
        }
        assert!(!d.cfg.update_status_path().exists());

        // Signatures turned off in config is the only way to skip the check.
        let lab = Deps {
            cfg: Config {
                update_require_signature: false,
                ..d.cfg.clone()
            },
            ..deps(td.path(), "http://127.0.0.1:9")
        };
        let (r, out) = run_args(&lab, &["update", "apply", "--manifest-url", &url]);
        assert_eq!(r.unwrap(), 0, "{out}");
        assert_eq!(
            update::current_release_version(&lab.update_env.install_root).as_deref(),
            Some("0.9.0")
        );
    }

    #[test]
    fn update_apply_rejects_unsigned_manifest_with_bundled_key() {
        let td = tempfile::tempdir().unwrap();
        let d = deps(td.path(), "http://127.0.0.1:9");
        installed_slot(&d, "0.8.0");
        let (tarball, manifest) = release(td.path(), "0.9.0");
        let url = file_url(&manifest);
        for argv in [
            vec!["update", "apply", "--manifest-url", &url],
            vec![
                "update",
                "apply",
                "--file",
                tarball.to_str().unwrap(),
                "--manifest",
                manifest.to_str().unwrap(),
            ],
        ] {
            let (r, _) = run_args(&d, &argv);
            assert_eq!(r.unwrap_err().0, "manifest missing signature", "{argv:?}");
        }
        assert_eq!(
            update::current_release_version(&d.update_env.install_root).as_deref(),
            Some("0.8.0")
        );
    }

    #[test]
    fn argparse_style_abbreviations_and_repeats() {
        let parse = |argv: &[&str]| {
            Cli::try_parse_from(std::iter::once("vesyl-print").chain(argv.iter().copied()))
        };
        let cmd = |argv: &[&str]| {
            parse(argv)
                .unwrap_or_else(|e| panic!("{argv:?}: {e}"))
                .command
        };

        assert!(matches!(
            cmd(&["status", "--ch"]),
            Command::Status { check: true }
        ));
        assert!(matches!(
            cmd(&["status", "--check", "--check"]),
            Command::Status { check: true }
        ));
        match cmd(&[
            "update",
            "rollback",
            "--vers",
            "0.3.17",
            "--rest",
            "--restart",
        ]) {
            Command::Update {
                action: UpdateAction::Rollback { version, restart },
            } => {
                assert_eq!(version.as_deref(), Some("0.3.17"));
                assert!(restart);
            }
            other => panic!("{other:?}"),
        }
        match cmd(&[
            "update",
            "apply",
            "--manifest-u",
            "https://x/m.json",
            "--manifest",
            "m.json",
        ]) {
            Command::Update {
                action:
                    UpdateAction::Apply {
                        manifest_url,
                        manifest,
                        ..
                    },
            } => {
                assert_eq!(manifest_url.as_deref(), Some("https://x/m.json"));
                // An exact name beats the longer option it prefixes.
                assert_eq!(manifest.as_deref(), Some(Path::new("m.json")));
            }
            other => panic!("{other:?}"),
        }
        match cmd(&[
            "print-test",
            "--fi",
            "l.pdf",
            "--q",
            "Zebra",
            "--cop",
            "2",
            "--cop",
            "3",
        ]) {
            Command::PrintTest(a) => {
                assert_eq!(a.file, Path::new("l.pdf"));
                assert_eq!(a.queue.as_deref(), Some("Zebra"));
                assert_eq!(a.copies, 3);
            }
            other => panic!("{other:?}"),
        }
        match cmd(&["claim", "AB7K2Q9M", "--na", "Pack 1", "--name", "Pack 2"]) {
            Command::Claim { code, name } => {
                assert_eq!(code, "AB7K2Q9M");
                assert_eq!(name.as_deref(), Some("Pack 2"));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            cmd(&["agent", "--verb", "-v"]),
            Command::Agent { verbose: true }
        ));
        // Negative numbers are values, as in argparse (copies < 1 prints one).
        match cmd(&["print-test", "--file", "l.pdf", "--copies", "-1"]) {
            Command::PrintTest(a) => assert_eq!(a.copies, -1),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            cmd(&["claim", "-12345678"]),
            Command::Claim { code, .. } if code == "-12345678"
        ));

        // What argparse rejects stays rejected (exit status 2).
        for bad in [
            &["update", "apply", "--man", "x"][..],
            &["claim", "AB7K2Q9M", "EXTRA"],
            &["status", "--bogus"],
            &["update", "apply", "--version"],
        ] {
            let err = parse(bad).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{bad:?}: {err}");
        }
    }

    #[test]
    fn cli_parses_all_commands() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
        for argv in [
            vec![
                "print-test",
                "-f",
                "x.pdf",
                "-q",
                "Zebra",
                "--copies",
                "2",
                "--raw",
            ],
            vec!["update", "apply", "--version", "0.4.0", "--restart"],
            vec!["agent", "-v"],
            vec!["enroll", "tok", "--name", "n"],
        ] {
            Cli::try_parse_from(std::iter::once("vesyl-print").chain(argv)).unwrap();
        }
    }
}
