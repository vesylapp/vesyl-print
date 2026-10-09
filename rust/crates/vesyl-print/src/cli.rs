//! vesyl-print CLI: claim, enroll, status, queues, unpair, agent, version,
//! update, print-test, test-print.

use std::ffi::OsString;
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
use crate::config::{
    agent_version, default_platform, load_config, write_default_config, Config, WaitCups,
};
use crate::jobs::{self, JobError, JobOutcome, JobStore, Pipeline};
use crate::statusio::{self, CloudState, PairingState};
use crate::update::{self, ReleaseManifest, UpdateEnv};
use crate::{printers, sysinfo, JsonObject};

/// Parsing follows Python's argparse, as the Python CLI this replaced did, so
/// existing invocations keep working: unambiguous prefixes of long options
/// are accepted (`--ch` for `--check`) and a repeated option keeps its last
/// value. Both settings reach every subcommand.
#[derive(Parser, Debug)]
#[command(
    name = "vesyl-print",
    version = agent_version(),
    about = "VESYL print node — claim, enroll, status, queues, agent, print-test, test-print, update",
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
        /// Print the result (or the failure) as one JSON object
        #[arg(long)]
        json: bool,
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
    /// Print the built-in VESYL test label (the LCD's Test button)
    TestPrint(TestPrintArgs),
}

#[derive(Subcommand, Debug)]
pub enum UpdateAction {
    /// Show current vs cloud desired version
    Check,
    /// Download+install update
    Apply {
        /// Target version (uses releases_base_url; with --manifest-url or
        /// --file, the version their manifest must be for)
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

#[derive(Args, Debug)]
pub struct TestPrintArgs {
    /// CUPS queue name (see `vesyl-print queues`)
    #[arg(long)]
    queue: String,
    /// Label format: pdf (any queue) or zpl (raw / Zebra queues)
    #[arg(long)]
    format: String,
    /// Print the result (or the failure) as one JSON object
    #[arg(long)]
    json: bool,
}

/// Fatal CLI error: printed to stderr, exit status 1.
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

/// A file mode as `claim` and `status` print it: `0o600`.
fn oct(mode: u32) -> String {
    format!("0o{mode:o}")
}

/// External effects the CLI depends on, injectable for tests.
pub struct Deps {
    pub cfg: Config,
    pub update_env: UpdateEnv,
    pub inventory: Box<dyn Fn() -> Result<Vec<Value>, String>>,
    /// The job pipeline `print-test` runs, and `test-print` with
    /// `wait_cups` off.
    pub pipeline: Pipeline,
    /// Holds `test-labels/` (see [`default_assets_dir`]).
    pub assets_dir: PathBuf,
    /// Where `test-print` makes its private job store.
    pub temp_dir: PathBuf,
}

impl Deps {
    pub fn system() -> Self {
        let cfg = load_config(None, None);
        Deps {
            update_env: UpdateEnv::detect(&cfg),
            inventory: Box::new(|| Ok(printers::inventory_payload())),
            pipeline: Pipeline::default(),
            assets_dir: default_assets_dir(),
            temp_dir: std::env::temp_dir(),
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
    // The agent installs its own, at its own level (see `cmd_agent`).
    if !matches!(cli.command, Command::Agent { .. }) {
        crate::logging::init_command();
    }
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
        Command::Claim { code, name, json } => cmd_claim(deps, out, &code, name.as_deref(), json),
        Command::Enroll { token, name } => cmd_enroll(deps, out, &token, name.as_deref()),
        Command::Status { check } => cmd_status(deps, out, check),
        Command::Queues { json } => cmd_queues(deps, out, json),
        Command::Unpair => cmd_unpair(deps, out),
        Command::Agent { verbose } => cmd_agent(deps, verbose),
        Command::Version => cmd_version(deps, out),
        Command::Update { action } => cmd_update(deps, out, action),
        Command::PrintTest(args) => cmd_print_test(deps, out, args),
        Command::TestPrint(args) => cmd_test_print(deps, out, args),
    }
}

/// `--json` output: one JSON object on one line.
fn write_json(out: &mut dyn Write, body: &Value) -> Result<(), Die> {
    writeln!(out, "{}", serde_json::to_string(body)?)?;
    Ok(())
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

/// Why a claim failed, for plain and `--json` output.
#[derive(Debug)]
struct ClaimFailure {
    /// The `error` of `claim --json`.
    message: String,
    /// HTTP status of the cloud's answer; 400 for a malformed code, 0 for a
    /// transport error or a local failure.
    status: u16,
    /// The cloud's error code, if it sent one.
    code: Option<String>,
    /// What plain `claim` prints to stderr.
    plain: String,
}

impl ClaimFailure {
    fn local(Die(message): Die, status: u16) -> Self {
        ClaimFailure {
            plain: message.clone(),
            message,
            status,
            code: None,
        }
    }

    fn cloud(e: &CloudError) -> Self {
        ClaimFailure {
            message: if e.message.is_empty() {
                "claim failed".into()
            } else {
                e.message.clone()
            },
            status: e.status,
            code: e.code.clone(),
            plain: cloud_msg("claim failed", e),
        }
    }

    fn to_json(&self) -> Value {
        json!({"ok": false, "error": self.message, "status": self.status, "code": self.code})
    }
}

/// Claim this node with `code`: save the credentials and mark the LCD
/// paired (offline until the agent's first heartbeat).
fn claim_node(deps: &Deps, code: &str, name: Option<&str>) -> Result<Credentials, ClaimFailure> {
    let cfg = &deps.cfg;
    prepare_dirs(cfg).map_err(|e| ClaimFailure::local(e, 0))?;
    let code = normalize_claim_code(code);
    if code.len() < 6 {
        return Err(ClaimFailure::local(
            Die("claim code looks too short".into()),
            400,
        ));
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
        .map_err(|e| ClaimFailure::cloud(&e))?;
    save_pairing(cfg, &data, "claim").map_err(|e| ClaimFailure::local(e, 0))
}

/// Public fields of a fresh pairing (never the device token).
fn claim_json(creds: &Credentials) -> Value {
    json!({
        "ok": true,
        "node_id": creds.node_id,
        "name": creds.name,
        "organization_name": creds.organization_name,
        "warehouse_name": creds.warehouse_label(),
    })
}

fn cmd_claim(
    deps: &Deps,
    out: &mut dyn Write,
    code: &str,
    name: Option<&str>,
    as_json: bool,
) -> CmdResult {
    let claimed = claim_node(deps, code, name);
    if as_json {
        return match claimed {
            Ok(creds) => write_json(out, &claim_json(&creds)).map(|()| 0),
            Err(f) => write_json(out, &f.to_json()).map(|()| 1),
        };
    }
    let creds = claimed.map_err(|f| Die(f.plain))?;
    let cfg = &deps.cfg;
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
    // load_credentials treats an unreadable file as "not paired"; say why.
    if let Err(e) = fs::File::open(cfg.credentials_path()) {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            writeln!(
                out,
                "  WARNING: credentials file exists but this user cannot read it ({e})"
            )?;
        }
    }
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
    // Before anything is written: as root the agent would leave root-owned
    // files in the service user's state.
    crate::agent::refuse_root(&deps.cfg).map_err(Die)?;
    crate::logging::init(verbose);
    let stop = Arc::new(AtomicBool::new(false));
    // Before Agent::run starts a thread: they all inherit the blocked stop
    // signals, so only the signal thread ever takes them.
    crate::agent::stop_on_signals(stop.clone())?;
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

/// `--version` as given: a release version, a leading `v` allowed (`v0.5.0`
/// is 0.5.0, as in the release tags).
fn version_arg(raw: &str) -> Result<String, Die> {
    let v = update::normalize_version(raw);
    if !update::is_version(v) {
        return die(format!(
            "invalid --version {raw:?}: expected a release version such as 0.5.0"
        ));
    }
    Ok(v.to_string())
}

/// A file argument with a leading `~/` taken from `$HOME`, as a shell
/// expands it in a word of its own but not in `--file=~/…`.
fn expand_path(p: &Path) -> PathBuf {
    match p.to_str() {
        Some(s) => jobs::expand_user(s),
        None => p.to_path_buf(),
    }
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
                // Without a presigned URL's query, where its signature is.
                writeln!(out, "manifest: {}", update::shown_url(&m))?;
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
            version,
            restart,
            ..
        } => {
            let version = version.as_deref().map(version_arg).transpose()?;
            apply_local(
                deps,
                out,
                &file,
                manifest.as_deref(),
                version.as_deref(),
                restart,
            )
        }

        UpdateAction::Apply {
            manifest_url: Some(url),
            version,
            restart,
            ..
        } => {
            // With a source of its own, --version is what it must hold.
            let version = version.as_deref().map(version_arg).transpose()?;
            // Signatures required: a key that cannot be loaded is an error.
            let pem = update::manifest_public_key(cfg)?;
            let manifest = update::fetch_manifest(&url)?;
            if let Some(v) = &version {
                update::check_manifest_version(&manifest, v)?;
            }
            let previous = update::slot_before_activation(env);
            // No stop to honor: a signal ends the CLI outright.
            update::apply_release(
                &manifest,
                env,
                pem.as_deref(),
                cfg.update_require_signature,
                &AtomicBool::new(false),
            )?;
            writeln!(out, "applied {}", manifest.version)?;
            after_manual_activation(deps, out, &manifest.version, previous, restart)?;
            Ok(0)
        }

        UpdateAction::Apply { version, .. } => {
            // Online: heartbeat desired + update_url, or explicit --version.
            let version = version.as_deref().map(version_arg).transpose()?;
            let Some(creds) = auth::load_credentials(&cfg.credentials_path()) else {
                return die("not paired and no --manifest-url / --file");
            };
            let mut hb = heartbeat_now(deps, &creds)?;
            if let Some(v) = version {
                // The heartbeat's update_url is the manifest of the server's
                // desired version: it serves for `v` only when that is `v` as
                // written, a leading `v` aside (version_cmp would take 0.3
                // for 0.3.0). Any other version's manifest comes
                // from releases_base_url. Either way, a manifest for another
                // version is refused before anything is installed.
                let server_wants_v = hb_str(&hb, "desired_agent_version", "desired_version")
                    .is_some_and(|d| update::normalize_version(&d) == v);
                let offered = hb_str(&hb, "update_url", "manifest_url").filter(|_| server_wants_v);
                let url = match offered {
                    Some(url) => url,
                    None if !cfg.releases_base_url.trim().is_empty() => {
                        update::default_manifest_url(&cfg.releases_base_url, &v)
                    }
                    None => {
                        return die(format!(
                            "no manifest URL for {v}: releases_base_url is not set, and the \
                             server does not offer {v} (pass --manifest-url)"
                        ))
                    }
                };
                hb.insert("desired_agent_version".into(), json!(v));
                hb.insert("update_url".into(), json!(url));
            }
            // Restart only once update_status.json says pending_health, so the
            // restarted agent always finds the health gate armed.
            let apply_env = UpdateEnv {
                restart: false,
                ..env.clone()
            };
            // `auto_update_enabled: false` keeps the agent from installing a
            // desired version on its own; asked for here, it is installed, as
            // with --file and --manifest-url.
            let manual = Config {
                auto_update_enabled: true,
                ..cfg.clone()
            };
            // From a fresh status: what holds or backs off the agent's own
            // attempts does not apply to one asked for here.
            let ust = update::maybe_update_from_heartbeat(
                &hb,
                &manual,
                &apply_env,
                None,
                None,
                false,
                &AtomicBool::new(false),
            );
            // Idle: nothing was installed, the version asked for runs
            // already. A hold on another version (`update rollback` away
            // from it, its failed gate) stays: written over, the agent
            // would install that version again at its next heartbeat.
            let path = cfg.update_status_path();
            if ust.status == update::STATUS_IDLE {
                if let Some(kept) = update::read_update_status(&path)
                    .filter(|on_disk| update::held_version(on_disk, &env.running_version).is_some())
                {
                    writeln!(out, "{}", serde_json::to_string_pretty(&kept.to_dict())?)?;
                    return Ok(0);
                }
            }
            update::write_update_status(&path, &ust)?;
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
            let version = version.as_deref().map(version_arg).transpose()?;
            let left = update::current_release_version(&env.install_root);
            let ver = update::rollback(
                &env.install_root,
                version.as_deref(),
                env.apply_helper.as_deref(),
            )?;
            writeln!(out, "rolled back to {ver}")?;
            // Recorded, or the agent installs the version left again at its
            // next heartbeat while the server still asks for it.
            if let Some(left) = left.filter(|l| *l != ver) {
                update::record_manual_rollback(&cfg.update_status_path(), &left, &ver).map_err(
                    |e| {
                        Die(format!(
                            "rolled back to {ver} but could not record it ({e}): the agent may \
                             install {left} again; services not restarted"
                        ))
                    },
                )?;
                writeln!(
                    out,
                    "holding {left}: the agent will not install it again until the server \
                     asks for another version (or `update apply` does)"
                )?;
            }
            // `restart` is false only in tests (UpdateEnv::detect always sets it).
            if restart && env.restart {
                update::restart_services(env.apply_helper.as_deref());
                writeln!(out, "services restarted")?;
            }
            Ok(0)
        }
    }
}

/// Offline tarball + manifest: installed and activated like an online apply
/// ([`update::apply_local_release`]), only not downloaded. With `version`
/// (`--version`), the manifest must be for that version.
fn apply_local(
    deps: &Deps,
    out: &mut dyn Write,
    file: &Path,
    manifest: Option<&Path>,
    version: Option<&str>,
    restart: bool,
) -> CmdResult {
    let cfg = &deps.cfg;
    let (file, manifest) = (expand_path(file), manifest.map(expand_path));
    let Some(manifest_path) = manifest.filter(|p| p.is_file()) else {
        return die("--manifest PATH required with --file");
    };
    let data: Value = serde_json::from_str(&fs::read_to_string(manifest_path)?)?;
    let Value::Object(data) = data else {
        return die("manifest must be a JSON object");
    };
    let manifest = ReleaseManifest::from_dict(&data)?;
    if let Some(v) = version {
        update::check_manifest_version(&manifest, v)?;
    }
    let tarball = fs::canonicalize(&file)
        .ok()
        .filter(|p| p.is_file())
        .ok_or_else(|| Die(format!("file not found: {}", file.display())))?;
    // Verification is skipped only when signatures are disabled in config; an
    // unreadable configured key is an error.
    let pem = update::manifest_public_key(cfg)?;
    let env = &deps.update_env;
    let previous = update::slot_before_activation(env);
    update::apply_local_release(
        &manifest,
        env,
        &tarball,
        pem.as_deref(),
        cfg.update_require_signature,
    )?;
    writeln!(
        out,
        "activated {} at {}",
        manifest.version,
        env.install_root.join("current").display()
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
    let path = deps.cfg.update_status_path();
    // A reinstall of the slot `current` held already (a repair) has no other
    // slot to roll back to, so its gate could only mark it failed. Armed, it
    // would write over a hold on another version (`update rollback` away
    // from it, its failed gate), and the agent would install that version
    // again once this one passed: the hold is kept instead.
    let reinstall = previous
        .as_deref()
        .is_some_and(|p| update::version_cmp(p, version).is_eq());
    let held = update::read_update_status(&path)
        .filter(|_| reinstall)
        .and_then(|st| update::held_version(&st, version).map(String::from));
    if let Some(held) = held {
        writeln!(
            out,
            "reinstalled {version} without a health gate (no other slot to roll back to); \
             still holding {held}"
        )?;
    } else {
        arm_gate(deps, out, &path, version, previous)?;
    }
    // `restart` is false only in tests (UpdateEnv::detect always sets it).
    if deps.update_env.restart {
        update::restart_services(deps.update_env.apply_helper.as_deref());
        writeln!(out, "services restarted")?;
    }
    Ok(())
}

/// Arm the health gate for `version` (see [`after_manual_activation`]) at
/// `path`, and say so.
fn arm_gate(
    deps: &Deps,
    out: &mut dyn Write,
    path: &Path,
    version: &str,
    previous: Option<String>,
) -> Result<(), Die> {
    let st = update::arm_health_gate(&deps.cfg, path, version, previous).map_err(|e| {
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
    Ok(())
}

/// Submit a local file through the durable job pipeline (no cloud).
fn cmd_print_test(deps: &Deps, out: &mut dyn Write, args: PrintTestArgs) -> CmdResult {
    let cfg = &deps.cfg;
    cfg.ensure_dirs()?;
    let file = expand_path(&args.file);
    if !file.is_file() {
        return die(format!("file not found: {}", file.display()));
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
        &file,
        &queue,
        None,
        args.title.as_deref(),
        args.copies,
        args.raw,
    )
    .map_err(|e| Die(e.message))?;
    let store = JobStore::from_config(cfg);
    writeln!(out, "job_id:     {}", job.id)?;
    writeln!(out, "file:       {}", file.display())?;
    writeln!(out, "cups_name:  {queue}")?;
    writeln!(
        out,
        "raw:        {}",
        if args.raw { "True" } else { "False" }
    )?;
    writeln!(out, "queue_dir:  {}", store.queue_dir.display())?;
    out.flush()?;

    let state = deps
        .pipeline
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

/// Tracking number printed on the built-in test label.
pub const TEST_LABEL_TRACKING: &str = "1Z999VES014200042";

/// Where `test-print` finds `test-labels/`: `$VESYL_PRINT_ASSETS_DIR`, else
/// `assets/` next to the running executable (the release slot).
pub fn default_assets_dir() -> PathBuf {
    assets_dir_from(
        std::env::var_os("VESYL_PRINT_ASSETS_DIR"),
        std::env::current_exe().ok(),
    )
}

fn assets_dir_from(env_dir: Option<OsString>, exe: Option<PathBuf>) -> PathBuf {
    if let Some(dir) = env_dir.filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    exe.as_deref()
        .and_then(Path::parent)
        .map(|slot| slot.join("assets"))
        .unwrap_or_else(|| PathBuf::from("/opt/vesyl-print/current/assets"))
}

/// The built-in test label in `format` (`pdf` or `zpl`).
pub fn test_label_path(assets_dir: &Path, format: &str) -> PathBuf {
    assets_dir
        .join("test-labels")
        .join(format!("vesyl-roadrunner-4x6.{format}"))
}

/// A test label `lp` accepted.
#[derive(Debug)]
struct AcceptedTestLabel {
    job_id: String,
    queue: String,
    format: String,
    file: PathBuf,
    outcome: JobOutcome,
}

/// Print the built-in test label. The job runs through a private job store,
/// removed afterwards, so it never touches the agent's queue, and returns
/// once `lp` has it (CUPS is not watched).
fn submit_test_label(
    deps: &Deps,
    queue: &str,
    format: &str,
) -> Result<AcceptedTestLabel, JobError> {
    let kind = format.trim().to_lowercase();
    if kind != "pdf" && kind != "zpl" {
        return Err(JobError::new(
            format!("unsupported test format: {format}"),
            "invalid_job",
        ));
    }
    let queue = queue.trim();
    if queue.is_empty() {
        return Err(JobError::new("missing cups_name", "invalid_job"));
    }
    let file = test_label_path(&deps.assets_dir, &kind);
    let title = format!("VESYL test {} {TEST_LABEL_TRACKING}", kind.to_uppercase());
    let job = jobs::job_from_local_file(&file, queue, None, Some(&title), 1, kind == "zpl")?;
    let private = tempfile::Builder::new()
        .prefix("vesyl-test-print-")
        .tempdir_in(&deps.temp_dir)
        .map_err(|e| JobError::new(format!("private job store: {e}"), "job_error"))?;
    let store = JobStore::new(private.path().join("q"), private.path().join("p"));
    let pipeline = Pipeline {
        wait_cups: WaitCups::Off,
        ..deps.pipeline.clone()
    };
    let outcome = pipeline.process(&job, &store)?;
    Ok(AcceptedTestLabel {
        job_id: job.id,
        queue: queue.to_string(),
        format: kind,
        file,
        outcome,
    })
}

fn cmd_test_print(deps: &Deps, out: &mut dyn Write, args: TestPrintArgs) -> CmdResult {
    let printed = submit_test_label(deps, &args.queue, &args.format);
    if args.json {
        return match printed {
            Ok(t) => write_json(
                out,
                &json!({
                    "ok": true,
                    "state": t.outcome.as_str(),
                    "job_id": t.job_id,
                    "queue": t.queue,
                    "format": t.format,
                }),
            )
            .map(|()| 0),
            Err(e) => write_json(
                out,
                &json!({"ok": false, "error": e.message, "code": e.code}),
            )
            .map(|()| 1),
        };
    }
    let t = printed.map_err(|e| Die(format!("test print failed: {} ({})", e.message, e.code)))?;
    writeln!(out, "job_id:     {}", t.job_id)?;
    writeln!(out, "file:       {}", t.file.display())?;
    writeln!(out, "cups_name:  {}", t.queue)?;
    writeln!(out, "format:     {}", t.format)?;
    writeln!(out, "result:     {}", t.outcome.as_str())?;
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
        let temp_dir = td.join("tmp");
        fs::create_dir_all(&temp_dir).unwrap();
        Deps {
            update_env: UpdateEnv {
                install_root: td.join("install"),
                apply_helper: None,
                running_version: agent_version().into(),
                running_from_slot: false,
                restart: false,
            },
            inventory: Box::new(|| Ok(sample())),
            // Never real CUPS from a test (see `FakeCups`).
            pipeline: Pipeline {
                lp: Arc::new(|_, _, _| Err(JobError::new("no lp in tests", "lp_error"))),
                supports_raw: Arc::new(|_| Err("no CUPS in tests".into())),
                ..Pipeline::default()
            },
            assets_dir: td.join("assets"),
            temp_dir,
            cfg,
        }
    }

    /// The one JSON object a `--json` run printed, on one line.
    fn json_line(out: &str) -> Value {
        assert_eq!(out.lines().count(), 1, "{out:?}");
        assert!(out.ends_with('\n'), "{out:?}");
        serde_json::from_str(out).unwrap()
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

    const CLAIMED: &str = r#"{"node_id":"n1","device_token":"secret-tok","name":"Pack 1",
        "organization":{"name":"Acme"},"warehouse":{"name":"Main","code":"MAIN"}}"#;

    /// `claim --json` prints the public pairing fields (never the token) and
    /// leaves exactly the files a plain `claim` does.
    #[test]
    fn claim_json_reports_public_fields_and_saves_like_plain_claim() {
        let td = tempfile::tempdir().unwrap();
        let srv = serve(vec![(201, CLAIMED)]);
        let d = deps(td.path(), &srv.base_url);
        let (r, out) = run_args(&d, &["claim", "ab7k-2q9m", "--name", "Pack 1", "--json"]);
        assert_eq!(r.unwrap(), 0, "{out}");
        assert_eq!(
            json_line(&out),
            json!({"ok": true, "node_id": "n1", "name": "Pack 1",
                   "organization_name": "Acme", "warehouse_name": "Main"})
        );
        assert!(!out.contains("secret-tok") && !out.contains("device_token"));
        let body: Value = serde_json::from_slice(&srv.requests.lock().unwrap()[0].body).unwrap();
        assert_eq!(body["code"], "AB7K2Q9M");
        assert_eq!(body["name"], "Pack 1");

        let plain_td = tempfile::tempdir().unwrap();
        let srv = serve(vec![(201, CLAIMED)]);
        let plain = deps(plain_td.path(), &srv.base_url);
        let (r, out) = run_args(&plain, &["claim", "ab7k-2q9m", "--name", "Pack 1"]);
        assert_eq!(r.unwrap(), 0, "{out}");
        assert!(
            out.starts_with("Paired successfully.\n"),
            "plain output unchanged"
        );

        let creds = |d: &Deps| auth::load_credentials(&d.cfg.credentials_path()).unwrap();
        assert_eq!(creds(&d), creds(&plain));
        assert_eq!(creds(&d).device_token, "secret-tok");
        let status = |d: &Deps| statusio::AgentStatus {
            updated_at: None,
            ..statusio::read_status(&d.cfg.status_path()).unwrap()
        };
        assert_eq!(status(&d), status(&plain));
        assert_eq!(
            (status(&d).pairing, status(&d).cloud),
            (PairingState::Paired, CloudState::Offline)
        );
        for dd in [&d, &plain] {
            assert_eq!(
                auth::credentials_mode(&dd.cfg.credentials_path()),
                Some(0o600)
            );
            assert!(
                dd.cfg.config_path().is_file(),
                "default config.json written"
            );
        }
    }

    /// `sudo vesyl-print claim` on a fresh device leaves everything it makes
    /// (config, state, queue and processed dirs, config.json, credentials,
    /// status.json) to the service user that owns the parent, as `setup.sh`
    /// leaves it. Needs root (or a user namespace): `unshare --map-root-user
    /// --map-auto <test binary> --include-ignored`.
    #[test]
    #[ignore = "needs root (or a user namespace) to chown"]
    fn root_claim_leaves_its_files_to_the_service_user() {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        let td = tempfile::tempdir().unwrap();
        std::os::unix::fs::chown(td.path(), Some(1000), Some(1000)).unwrap();
        let srv = serve(vec![(201, CLAIMED)]);
        let d = deps(td.path(), &srv.base_url);
        let (r, out) = run_args(&d, &["claim", "AB7K2Q9M", "--json"]);
        assert_eq!(r.unwrap(), 0, "{out}");
        for path in [
            d.cfg.config_path(),
            d.cfg.credentials_path(),
            d.cfg.status_path(),
            d.cfg.queue_dir(),
            d.cfg.processed_dir(),
        ] {
            assert!(path.exists(), "{}", path.display());
        }
        for dir in [&d.cfg.config_dir, &d.cfg.state_dir] {
            assert_eq!(not_owned_by(dir, 1000), Vec::<PathBuf>::new());
        }
        assert_eq!(
            auth::credentials_mode(&d.cfg.credentials_path()),
            Some(0o600)
        );
    }

    /// Every failed `claim --json` prints one JSON object, exits 1 and
    /// writes no credentials.
    #[test]
    fn claim_json_failures() {
        let td = tempfile::tempdir().unwrap();
        let failed = |d: &Deps, argv: &[&str]| -> Value {
            let (r, out) = run_args(d, argv);
            assert_eq!(r.unwrap(), 1, "{argv:?}: {out}");
            assert!(auth::load_credentials(&d.cfg.credentials_path()).is_none());
            json_line(&out)
        };

        // A short code never reaches the cloud.
        let srv = serve(vec![(201, CLAIMED)]);
        let d = deps(td.path(), &srv.base_url);
        assert_eq!(
            failed(&d, &["claim", "ab-1", "--json"]),
            json!({"ok": false, "error": "claim code looks too short", "status": 400, "code": null})
        );
        assert!(srv.requests.lock().unwrap().is_empty());

        for (status, body, expected) in [
            (
                422,
                r#"{"error":{"code":"invalid_code","message":"Unknown claim code"}}"#,
                json!({"ok": false, "error": "Unknown claim code", "status": 422, "code": "invalid_code"}),
            ),
            (
                500,
                r#"{"error":"boom"}"#,
                json!({"ok": false, "error": "boom", "status": 500, "code": null}),
            ),
            // An answer without a device token is refused here: status 0.
            (
                201,
                r#"{"node_id":"n1"}"#,
                json!({"ok": false, "error": "claim response missing device_token",
                       "status": 0, "code": null}),
            ),
        ] {
            let srv = serve(vec![(status, body)]);
            let d = deps(td.path(), &srv.base_url);
            assert_eq!(failed(&d, &["claim", "AB7K2Q9M", "--json"]), expected);
            assert_eq!(srv.requests.lock().unwrap().len(), 1);
        }

        // Transport errors are status 0 too. Nothing listens on port 9, and
        // no test can (it is privileged); a port just closed could be bound
        // by any process before both claims below have been refused.
        let d = deps(td.path(), "http://127.0.0.1:9");
        let err = failed(&d, &["claim", "AB7K2Q9M", "--json"]);
        assert_eq!(
            (&err["ok"], &err["status"], &err["code"]),
            (&json!(false), &json!(0), &Value::Null)
        );
        let message = err["error"].as_str().unwrap();
        assert!(message.starts_with("network error"), "{err}");
        // Plain claim: the same failure on stderr, as before.
        assert_eq!(
            run_args(&d, &["claim", "AB7K2Q9M"]).0.unwrap_err().0,
            format!("claim failed: {message}")
        );
    }

    // --- test-print ----------------------------------------------------------

    /// One `lp` run of a test print.
    #[derive(Debug, Clone)]
    struct LpCall {
        queue: String,
        file: PathBuf,
        title: Option<String>,
        copies: i64,
        raw: bool,
        argv: Vec<String>,
        /// Queue files in each private job store under `Deps::temp_dir`
        /// while `lp` ran.
        private_queues: Vec<Vec<String>>,
    }

    /// Stands in for CUPS in a test print: records each `lp` run and
    /// whether the job waited on CUPS.
    #[derive(Clone, Default)]
    struct FakeCups {
        lp: Arc<Mutex<Vec<LpCall>>>,
        waited: Arc<AtomicBool>,
    }

    fn file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    impl FakeCups {
        /// A pipeline whose `lp` answers `answer`. Its `wait_cups` is the
        /// default (sync): `test-print` must turn it off.
        fn pipeline(&self, temp_dir: &Path, answer: Result<Option<String>, JobError>) -> Pipeline {
            let (calls, waited) = (self.lp.clone(), self.waited.clone());
            let temp_dir = temp_dir.to_path_buf();
            Pipeline {
                lp: Arc::new(move |queue, file, args| {
                    let private_queues = file_names(&temp_dir)
                        .iter()
                        .filter(|n| n.starts_with("vesyl-test-print-"))
                        .map(|n| file_names(&temp_dir.join(n).join("q")))
                        .collect();
                    calls.lock().unwrap().push(LpCall {
                        queue: queue.into(),
                        file: file.into(),
                        title: args.title.map(String::from),
                        copies: args.copies,
                        raw: args.raw,
                        argv: jobs::lp_args(queue, file, args),
                        private_queues,
                    });
                    answer.clone()
                }),
                supports_raw: Arc::new(|_| Ok(false)),
                wait_cups_job: Arc::new(move |_, _| {
                    waited.store(true, Ordering::SeqCst);
                    jobs::CupsOutcome::Printed
                }),
                ..Pipeline::default()
            }
        }

        fn calls(&self) -> Vec<LpCall> {
            self.lp.lock().unwrap().clone()
        }
    }

    /// Deps with both test labels installed and the agent's own queue
    /// directories in place, printing through `FakeCups`.
    fn test_print_deps(td: &Path, answer: Result<Option<String>, JobError>) -> (Deps, FakeCups) {
        let labels = td.join("assets/test-labels");
        fs::create_dir_all(&labels).unwrap();
        fs::write(labels.join("vesyl-roadrunner-4x6.pdf"), "%PDF-1.4\n%test\n").unwrap();
        fs::write(
            labels.join("vesyl-roadrunner-4x6.zpl"),
            "^XA^FDtest^FS^XZ\n",
        )
        .unwrap();
        let cups = FakeCups::default();
        let mut d = deps(td, "http://127.0.0.1:9");
        d.pipeline = cups.pipeline(&d.temp_dir, answer);
        d.cfg.ensure_dirs().unwrap();
        (d, cups)
    }

    /// The private job store is gone and the agent's queue untouched.
    fn assert_no_trace(d: &Deps) {
        assert_eq!(
            file_names(&d.temp_dir),
            Vec::<String>::new(),
            "private job store left behind"
        );
        for dir in [d.cfg.queue_dir(), d.cfg.processed_dir()] {
            assert_eq!(file_names(&dir), Vec::<String>::new(), "{}", dir.display());
        }
    }

    #[test]
    fn test_print_pdf_runs_through_a_private_store() {
        let td = tempfile::tempdir().unwrap();
        let (d, cups) = test_print_deps(td.path(), Ok(Some("Brother_HL-7".into())));
        let (r, out) = run_args(
            &d,
            &[
                "test-print",
                "--queue",
                " Brother_HL ",
                "--format",
                "PDF",
                "--json",
            ],
        );
        assert_eq!(r.unwrap(), 0, "{out}");
        let got = json_line(&out);
        let job_id = got["job_id"].as_str().unwrap().to_string();
        assert!(uuid::Uuid::parse_str(&job_id).is_ok(), "{got}");
        assert_eq!(
            got,
            json!({"ok": true, "state": "delivered", "job_id": job_id,
                   "queue": "Brother_HL", "format": "pdf"})
        );

        let calls = cups.calls();
        assert_eq!(calls.len(), 1);
        let lp = &calls[0];
        let file = fs::canonicalize(
            td.path()
                .join("assets/test-labels/vesyl-roadrunner-4x6.pdf"),
        )
        .unwrap();
        assert_eq!((lp.queue.as_str(), &lp.file), ("Brother_HL", &file));
        let title = "VESYL test PDF 1Z999VES014200042";
        assert_eq!(
            (lp.title.as_deref(), lp.copies, lp.raw),
            (Some(title), 1, false)
        );
        assert_eq!(
            lp.argv,
            ["-d", "Brother_HL", "-t", title, file.to_str().unwrap()]
        );
        // Queued durably in its own store, not the agent's...
        assert_eq!(lp.private_queues, [[format!("{job_id}.json")]]);
        // ...returned once lp had it, without waiting on CUPS...
        assert!(!cups.waited.load(Ordering::SeqCst));
        assert_eq!(d.pipeline.cups_watcher.pending(), 0);
        // ...and cleaned up.
        assert_no_trace(&d);
    }

    #[test]
    fn test_print_zpl_is_sent_raw() {
        let td = tempfile::tempdir().unwrap();
        let (d, cups) = test_print_deps(td.path(), Ok(None));
        let (r, out) = run_args(
            &d,
            &[
                "test-print",
                "--queue",
                "Zebra_ZD220",
                "--format",
                "zpl",
                "--json",
            ],
        );
        assert_eq!(r.unwrap(), 0, "{out}");
        let got = json_line(&out);
        assert_eq!(
            (&got["ok"], &got["state"], &got["queue"], &got["format"]),
            (
                &json!(true),
                &json!("delivered"),
                &json!("Zebra_ZD220"),
                &json!("zpl")
            )
        );
        let lp = &cups.calls()[0];
        let file = fs::canonicalize(
            td.path()
                .join("assets/test-labels/vesyl-roadrunner-4x6.zpl"),
        )
        .unwrap();
        let title = "VESYL test ZPL 1Z999VES014200042";
        assert_eq!(
            lp.argv,
            [
                "-d",
                "Zebra_ZD220",
                "-t",
                title,
                "-o",
                "raw",
                file.to_str().unwrap()
            ]
        );
        assert_no_trace(&d);
    }

    #[test]
    fn test_print_rejects_bad_requests_with_job_error_codes() {
        let td = tempfile::tempdir().unwrap();
        let (d, cups) = test_print_deps(td.path(), Ok(None));
        let missing = td
            .path()
            .join("assets/test-labels/vesyl-roadrunner-4x6.zpl");
        fs::remove_file(&missing).unwrap();
        let not_found = format!("file not found: {}", missing.display());
        for (queue, format, error, code) in [
            (
                "Zebra",
                "png",
                "unsupported test format: png",
                "invalid_job",
            ),
            // The format is checked first.
            ("", " EPL ", "unsupported test format:  EPL ", "invalid_job"),
            ("  ", "pdf", "missing cups_name", "invalid_job"),
            ("Zebra", "zpl", not_found.as_str(), "content_missing"),
        ] {
            let argv = ["test-print", "--queue", queue, "--format", format, "--json"];
            let (r, out) = run_args(&d, &argv);
            assert_eq!(r.unwrap(), 1, "{argv:?}: {out}");
            assert_eq!(
                json_line(&out),
                json!({"ok": false, "error": error, "code": code}),
                "{argv:?}"
            );
        }
        assert!(cups.calls().is_empty(), "nothing printed");
        assert_no_trace(&d);
    }

    #[test]
    fn test_print_reports_lp_failures() {
        let td = tempfile::tempdir().unwrap();
        let refused = JobError::new(
            "lp: Error - The printer or class does not exist.",
            "unknown_queue",
        );
        let (d, cups) = test_print_deps(td.path(), Err(refused));
        let (r, out) = run_args(
            &d,
            &["test-print", "--queue", "Gone", "--format", "pdf", "--json"],
        );
        assert_eq!(r.unwrap(), 1, "{out}");
        assert_eq!(
            json_line(&out),
            json!({"ok": false, "error": "lp: Error - The printer or class does not exist.",
                   "code": "unknown_queue"})
        );
        assert_eq!(cups.calls().len(), 1);
        // The failed job's queue file went with the private store.
        assert_no_trace(&d);
    }

    #[test]
    fn test_print_plain_output() {
        let td = tempfile::tempdir().unwrap();
        let (d, _) = test_print_deps(td.path(), Ok(Some("Brother_HL-8".into())));
        let (r, out) = run_args(
            &d,
            &["test-print", "--queue", "Brother_HL", "--format", "pdf"],
        );
        assert_eq!(r.unwrap(), 0, "{out}");
        for needle in [
            "job_id:     ",
            "vesyl-roadrunner-4x6.pdf\n",
            "cups_name:  Brother_HL\n",
            "format:     pdf\n",
            "result:     delivered\n",
        ] {
            assert!(out.contains(needle), "missing {needle:?} in\n{out}");
        }
        assert!(!out.contains('{'), "no JSON without --json");
        let (r, out) = run_args(
            &d,
            &["test-print", "--queue", "Brother_HL", "--format", "png"],
        );
        assert_eq!(
            r.unwrap_err().0,
            "test print failed: unsupported test format: png (invalid_job)"
        );
        assert_eq!(out, "");
        assert_no_trace(&d);
    }

    #[test]
    fn test_label_location() {
        let exe = PathBuf::from("/opt/vesyl-print/releases/0.4.0/vesyl-print");
        assert_eq!(
            assets_dir_from(Some("/srv/assets".into()), Some(exe.clone())),
            Path::new("/srv/assets")
        );
        assert_eq!(
            assets_dir_from(Some("".into()), Some(exe)),
            Path::new("/opt/vesyl-print/releases/0.4.0/assets")
        );
        assert_eq!(
            assets_dir_from(None, None),
            Path::new("/opt/vesyl-print/current/assets")
        );
        assert_eq!(
            test_label_path(Path::new("/a"), "zpl"),
            Path::new("/a/test-labels/vesyl-roadrunner-4x6.zpl")
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

    /// A slot holding the executable binary, as an install leaves it.
    fn runnable_slot(root: &Path, version: &str) -> PathBuf {
        let slot = root.join("releases").join(version);
        fs::create_dir_all(&slot).unwrap();
        fs::write(slot.join("vesyl-print"), b"bin").unwrap();
        crate::util::set_mode(&slot.join("vesyl-print"), 0o755).unwrap();
        slot
    }

    #[test]
    fn version_and_rollback() {
        let td = tempfile::tempdir().unwrap();
        let d = deps(td.path(), "http://127.0.0.1:9");
        let root = &d.update_env.install_root;
        for v in ["0.3.0", "0.4.0"] {
            runnable_slot(root, v);
        }
        update::flip_current(root, "0.4.0").unwrap();
        // A staging dir a crashed extract left is not a release, and a slot
        // without an executable binary is never rolled back to.
        runnable_slot(root, "0.5.0.staging");
        fs::create_dir_all(root.join("releases/0.3.5")).unwrap();
        fs::write(root.join("releases/0.3.5/vesyl-print"), b"bin").unwrap();
        let (r, out) = run_args(&d, &["version"]);
        assert_eq!(r.unwrap(), 0);
        assert!(
            out.contains("releases:       0.3.0, 0.3.5, 0.4.0\n"),
            "{out}"
        );
        let (r, out) = run_args(&d, &["update", "rollback"]);
        assert_eq!(r.unwrap(), 0);
        assert_eq!(
            out,
            "rolled back to 0.3.0\nholding 0.4.0: the agent will not install it again \
             until the server asks for another version (or `update apply` does)\n"
        );
        assert_eq!(
            update::current_release_version(root).as_deref(),
            Some("0.3.0")
        );
        let (r, out) = run_args(&d, &["update", "rollback", "--version", "0.3.5"]);
        assert_eq!(
            r.unwrap_err().0,
            "release 0.3.5 cannot run: no executable vesyl-print in its slot"
        );
        assert_eq!(out, "");
        assert_eq!(
            update::current_release_version(root).as_deref(),
            Some("0.3.0")
        );
    }

    /// Release tarball (the executable binary) and its unsigned manifest,
    /// whose `artifact_url` points at the tarball (`file://`).
    fn release(td: &Path, version: &str) -> (PathBuf, PathBuf) {
        release_with_mode(td, version, 0o755)
    }

    /// [`release`] with the binary's mode `mode`.
    fn release_with_mode(td: &Path, version: &str, mode: u32) -> (PathBuf, PathBuf) {
        let src = td.join(format!("src-{version}"));
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("vesyl-print"), b"bin").unwrap();
        crate::util::set_mode(&src.join("vesyl-print"), mode).unwrap();
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
        runnable_slot(root, version);
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
        let root = &d.update_env.install_root;
        assert_eq!(
            update::current_release_version(root).as_deref(),
            Some("0.9.0")
        );
        // The archive has no VERSION: the slot's comes from the manifest.
        assert_eq!(
            fs::read_to_string(root.join("releases/0.9.0/VERSION")).ok(),
            Some("0.9.0\n".into())
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

    fn apply_file(d: &Deps, tarball: &Path, manifest: &Path) -> (CmdResult, String) {
        run_args(
            d,
            &[
                "update",
                "apply",
                "--file",
                tarball.to_str().unwrap(),
                "--manifest",
                manifest.to_str().unwrap(),
            ],
        )
    }

    /// `update apply --file` refuses what an online apply refuses: a
    /// release this agent is too old for, and an archive without the
    /// executable binary the units exec.
    #[test]
    fn update_apply_file_checks_what_an_online_apply_checks() {
        let td = tempfile::tempdir().unwrap();
        let d = unsigned_deps(td.path());
        installed_slot(&d, "0.8.0");
        let root = &d.update_env.install_root;

        let (tarball, manifest) = release(td.path(), "0.9.0");
        let mut m: Value = serde_json::from_str(&fs::read_to_string(&manifest).unwrap()).unwrap();
        m["min_agent_version"] = json!("99.0.0");
        fs::write(&manifest, m.to_string()).unwrap();
        let (r, out) = apply_file(&d, &tarball, &manifest);
        assert_eq!(
            r.unwrap_err().0,
            format!("current {} < min_agent_version 99.0.0", agent_version())
        );
        assert_eq!(out, "");

        let (tarball, manifest) = release_with_mode(td.path(), "0.9.1", 0o644);
        let (r, out) = apply_file(&d, &tarball, &manifest);
        assert_eq!(
            r.unwrap_err().0,
            "archive missing an executable vesyl-print binary"
        );
        assert_eq!(out, "");

        assert_eq!(update::list_releases(root), ["0.8.0"]);
        assert_eq!(
            update::current_release_version(root).as_deref(),
            Some("0.8.0")
        );
    }

    /// A slot of the version being applied that the agent cannot delete
    /// (root unpacked it) is moved aside, as an online apply does, rather
    /// than failing the apply.
    #[test]
    fn update_apply_file_moves_aside_a_slot_it_cannot_delete() {
        let td = tempfile::tempdir().unwrap();
        let d = unsigned_deps(td.path());
        installed_slot(&d, "0.8.0");
        let releases = d.update_env.install_root.join("releases");
        let locked = releases.join("0.9.0/locked");
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join("vesyl-print"), b"old").unwrap();
        crate::util::set_mode(&locked, 0o555).unwrap();

        let (tarball, manifest) = release(td.path(), "0.9.0");
        let (r, out) = apply_file(&d, &tarball, &manifest);
        assert_eq!(r.unwrap(), 0, "{out}");
        assert_eq!(
            update::current_release_version(&d.update_env.install_root).as_deref(),
            Some("0.9.0")
        );
        assert!(update::slot_is_runnable(&releases.join("0.9.0")));
        assert!(!releases.join("0.9.0/locked").exists());
        // Root deletes it outright; anyone else moves it aside.
        let aside = releases.join(".0.9.0.stale-1/locked");
        if aside.exists() {
            crate::util::set_mode(&aside, 0o755).unwrap();
        }
    }

    /// With the apply-update helper installed, `update apply --file`
    /// activates through it, as an online apply does, never in-process:
    /// when it refuses, the apply fails and `current` stays.
    #[test]
    fn update_apply_file_activates_through_the_helper() {
        let td = tempfile::tempdir().unwrap();
        let mut d = unsigned_deps(td.path());
        installed_slot(&d, "0.8.0");
        let root = d.update_env.install_root.clone();
        let line = format!(
            "activate {} {}\n",
            root.join("releases/0.9.0").display(),
            root.join("current").display()
        );
        let (tarball, manifest) = release(td.path(), "0.9.0");
        for refuse in [true, false] {
            let dir = td.path().join(format!("helper-{refuse}"));
            fs::create_dir(&dir).unwrap();
            d.update_env.apply_helper = Some(update::fake_helper(&dir, refuse));
            let (r, out) = apply_file(&d, &tarball, &manifest);
            if refuse {
                assert_eq!(
                    r.unwrap_err().0,
                    "apply-update activate failed: apply-update: refused"
                );
            } else {
                assert_eq!(r.unwrap(), 0, "{out}");
                assert!(out.starts_with("activated 0.9.0"), "{out}");
            }
            assert_eq!(fs::read_to_string(dir.join("helper.calls")).unwrap(), line);
            // This stand-in flips nothing: nothing else did either.
            assert_eq!(
                update::current_release_version(&root).as_deref(),
                Some("0.8.0"),
                "refuse={refuse}"
            );
        }
    }

    const SERVICE_USER_TD: &str = "VESYL_TEST_CLI_SERVICE_USER_TD";

    /// Every path under `dir` whose owner (lstat) is not `uid`.
    fn not_owned_by(dir: &Path, uid: u32) -> Vec<PathBuf> {
        use std::os::unix::fs::MetadataExt;
        let mut wrong = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(path) = stack.pop() {
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.uid() != uid {
                wrong.push(path.clone());
            }
            if meta.is_dir() {
                stack.extend(fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
            }
        }
        wrong
    }

    /// `sudo vesyl-print update apply --file` into the service user's
    /// install root (as `setup.sh` leaves it) leaves nothing in `releases/`
    /// root's: the slot, `VERSION` included, is the service user's. So the
    /// agent, running as that user, later installs the same version over it
    /// by deleting it, not moving it aside for good. Needs root (or a user
    /// namespace): `unshare --map-root-user --map-auto <test binary>
    /// --include-ignored`.
    #[test]
    #[ignore = "needs root (or a user namespace) to chown and switch users"]
    fn root_update_apply_file_leaves_the_slot_to_the_service_user() {
        use std::os::unix::process::CommandExt;
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            return;
        }
        let td = tempfile::tempdir().unwrap();
        let d = unsigned_deps(td.path());
        installed_slot(&d, "0.8.0");
        std::os::unix::fs::chown(td.path(), Some(1000), Some(1000)).unwrap();
        crate::util::hand_tree_to_parent_owner(&d.update_env.install_root).unwrap();
        let (tarball, manifest) = release(td.path(), "0.9.0");

        let (r, out) = apply_file(&d, &tarball, &manifest);
        assert_eq!(r.unwrap(), 0, "{out}");
        let releases = d.update_env.install_root.join("releases");
        assert!(releases.join("0.9.0/VERSION").is_file());
        assert_eq!(not_owned_by(&releases, 1000), Vec::<PathBuf>::new());

        // Through /proc: the service user may not search the directories the
        // test binary sits in.
        let out = std::process::Command::new("/proc/self/exe")
            .args([
                "--exact",
                "cli::tests::root_update_apply_file_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(SERVICE_USER_TD, td.path())
            .uid(1000)
            .gid(1000)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "child failed: {stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mut names: Vec<String> = fs::read_dir(&releases)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["0.8.0", "0.9.0"], "moved aside instead of deleted");
    }

    /// The agent's OTA of the same version, as the service user.
    #[test]
    #[ignore = "child process of root_update_apply_file_leaves_the_slot_to_the_service_user"]
    fn root_update_apply_file_child() {
        let Some(td) = std::env::var_os(SERVICE_USER_TD).map(PathBuf::from) else {
            return;
        };
        let raw = fs::read_to_string(td.join("m-0.9.0.json")).unwrap();
        let manifest = ReleaseManifest::from_dict(
            serde_json::from_str::<Value>(&raw)
                .unwrap()
                .as_object()
                .unwrap(),
        )
        .unwrap();
        let env = UpdateEnv {
            install_root: td.join("install"),
            apply_helper: None,
            running_version: agent_version().into(),
            running_from_slot: false,
            restart: false,
        };
        update::apply_release(&manifest, &env, None, false, &AtomicBool::new(false)).unwrap();
        assert_eq!(
            update::current_release_version(&env.install_root).as_deref(),
            Some("0.9.0")
        );
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

    /// `auto_update_enabled: false` keeps the agent from installing a desired
    /// version on its own, not an operator: `update apply` installs the
    /// heartbeat's desired version, and `update apply --version` its own.
    /// The heartbeat's `update_url` is the manifest of the server's desired
    /// version, so `--version` takes it for exactly that version only.
    #[test]
    fn update_apply_online_installs_with_auto_update_disabled() {
        let td = tempfile::tempdir().unwrap();
        let (_, m090) = release(td.path(), "0.9.0");
        let (_, rc) = release(td.path(), "0.9.1-rc.1");
        // --version finds its manifest under releases_base_url, which has
        // no 0.9.0: that comes only from the heartbeat's update_url.
        let cdn = td.path().join("cdn");
        fs::create_dir_all(&cdn).unwrap();
        for v in ["0.9.1", "0.9.2"] {
            let (_, m) = release(td.path(), v);
            fs::copy(&m, cdn.join(format!("vesyl-print-{v}.manifest.json"))).unwrap();
        }
        // This node, with auto-update off, its heartbeats answered with `hb`.
        let node = |hb: Value| {
            let srv = http_stub::serve(move |_, s| respond(s, 200, &[], hb.to_string().as_bytes()));
            let d = deps(td.path(), &srv.base_url);
            Deps {
                cfg: Config {
                    auto_update_enabled: false,
                    update_require_signature: false,
                    releases_base_url: file_url(&cdn),
                    ..d.cfg
                },
                ..d
            }
        };
        let asking = node(json!({"desired_agent_version": "0.9.0", "update_url": file_url(&m090)}));
        // A pre-release of 0.9.1: its update_url is no manifest for 0.9.1.
        let canary =
            node(json!({"desired_agent_version": "0.9.1-rc.1", "update_url": file_url(&rc)}));
        let silent = node(json!({"ok": true}));
        installed_slot(&asking, "0.8.0");
        let creds = auth::credentials_from_pair_response(
            json!({"node_id": "n1", "device_token": "tok"})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        auth::save_credentials(&asking.cfg.credentials_path(), &creds).unwrap();
        let root = &asking.update_env.install_root;
        // Each apply moves `current`: 0.8.0, 0.9.0, 0.9.1, 0.9.0, 0.9.1, 0.9.2.
        for (d, argv, version) in [
            (&asking, &["update", "apply"][..], "0.9.0"),
            (&asking, &["update", "apply", "--version", "0.9.1"], "0.9.1"),
            (&asking, &["update", "apply", "--version", "0.9.0"], "0.9.0"),
            (&canary, &["update", "apply", "--version", "0.9.1"], "0.9.1"),
            (&silent, &["update", "apply", "--version", "0.9.2"], "0.9.2"),
        ] {
            let (r, out) = run_args(d, argv);
            assert_eq!(r.unwrap(), 0, "{argv:?}: {out}");
            let st: Value = serde_json::from_str(&out).unwrap();
            assert_eq!(
                st["status"],
                update::STATUS_PENDING_HEALTH,
                "{argv:?}: {out}"
            );
            assert_eq!(st["target_version"], version, "{argv:?}");
            assert_eq!(
                update::current_release_version(root).as_deref(),
                Some(version),
                "{argv:?}"
            );
        }
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
            &AtomicBool::new(false),
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
        let mut agent = Agent::new(d.cfg.clone());
        agent.inventory = inventory;
        agent.update_env = UpdateEnv {
            running_version: version.into(),
            running_from_slot: true,
            ..d.update_env.clone()
        };
        agent
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

    // --- update apply --version, update rollback, holds, the agent's stop -------

    /// Save credentials: `d`'s node is paired.
    fn pair(d: &Deps) {
        let creds = auth::credentials_from_pair_response(
            json!({"node_id": "n1", "device_token": "tok"})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        auth::save_credentials(&d.cfg.credentials_path(), &creds).unwrap();
    }

    /// The version `current` points at in `d`'s install root.
    fn current(d: &Deps) -> Option<String> {
        update::current_release_version(&d.update_env.install_root)
    }

    /// `update apply --version` takes a leading `v` (`v0.9.1` is 0.9.1) and
    /// refuses anything else that is not a version before it does anything
    /// (`v1.2.3` used to read as 0.2.3, and reinstall a running 1.2.3). A
    /// manifest of another version is refused before anything is installed;
    /// with no manifest URL, it says so rather than fetch a relative one.
    #[test]
    fn update_apply_version_is_checked() {
        use std::os::unix::fs::MetadataExt;
        let td = tempfile::tempdir().unwrap();
        let cdn = td.path().join("cdn");
        fs::create_dir_all(&cdn).unwrap();
        let (_, m091) = release(td.path(), "0.9.1");
        fs::copy(&m091, cdn.join("vesyl-print-0.9.1.manifest.json")).unwrap();
        // Published under 0.9.2's name: the manifest of 0.9.3.
        let (_, m093) = release(td.path(), "0.9.3");
        fs::copy(&m093, cdn.join("vesyl-print-0.9.2.manifest.json")).unwrap();
        let srv = http_stub::serve(|_, s| respond(s, 200, &[], br#"{"ok": true}"#));
        let node = |releases_base_url: String, running: &str| {
            let d = deps(td.path(), &srv.base_url);
            Deps {
                cfg: Config {
                    update_require_signature: false,
                    releases_base_url,
                    ..d.cfg
                },
                update_env: UpdateEnv {
                    running_version: running.into(),
                    ..d.update_env
                },
                ..d
            }
        };
        let d = node(file_url(&cdn), "0.8.0");
        installed_slot(&d, "0.8.0");
        pair(&d);

        for bad in ["latest", "0.9", "v", "1.2.3.staging", ""] {
            let (r, out) = run_args(&d, &["update", "apply", "--version", bad]);
            assert_eq!(
                r.unwrap_err().0,
                format!("invalid --version {bad:?}: expected a release version such as 0.5.0")
            );
            assert_eq!(out, "");
        }
        assert!(srv.requests().is_empty(), "a heartbeat was sent");

        let (r, out) = run_args(&d, &["update", "apply", "--version", "v0.9.1"]);
        assert_eq!(r.unwrap(), 0, "{out}");
        let st: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(st["status"], update::STATUS_PENDING_HEALTH, "{out}");
        assert_eq!(st["target_version"], "0.9.1");
        assert_eq!(current(&d).as_deref(), Some("0.9.1"));

        let (r, out) = run_args(&d, &["update", "apply", "--version", "0.9.2"]);
        assert_eq!(r.unwrap(), 1, "{out}");
        let st: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(st["status"], update::STATUS_FAILED);
        assert_eq!(st["last_error"], "manifest is for version 0.9.3, not 0.9.2");
        assert_eq!(st["last_error_code"], "version_mismatch");
        let releases = d.update_env.install_root.join("releases");
        assert!(!releases.join("0.9.2").exists() && !releases.join("0.9.3").exists());
        assert_eq!(current(&d).as_deref(), Some("0.9.1"));

        // Running 0.9.1: nothing to do, and the hold on 0.9.2 (it cannot
        // be installed) stays.
        let running = node(file_url(&cdn), "0.9.1");
        let slot = fs::metadata(releases.join("0.9.1")).unwrap().ino();
        let (r, out) = run_args(&running, &["update", "apply", "--version", "v0.9.1"]);
        assert_eq!(r.unwrap(), 0, "{out}");
        let st: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(st["status"], update::STATUS_FAILED, "{out}");
        assert_eq!(st["target_version"], "0.9.2", "{out}");
        assert_eq!(fs::metadata(releases.join("0.9.1")).unwrap().ino(), slot);
        // With nothing held, idle.
        let idle = update::UpdateStatus::default();
        update::write_update_status(&running.cfg.update_status_path(), &idle).unwrap();
        let (r, out) = run_args(&running, &["update", "apply", "--version", "v0.9.1"]);
        assert_eq!(r.unwrap(), 0, "{out}");
        let st: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(st["status"], update::STATUS_IDLE, "{out}");
        assert_eq!(st["target_version"], "0.9.1", "{out}");

        let nowhere = node(String::new(), "0.9.1");
        let (r, out) = run_args(&nowhere, &["update", "apply", "--version", "0.9.4"]);
        assert_eq!(
            r.unwrap_err().0,
            "no manifest URL for 0.9.4: releases_base_url is not set, and the server \
             does not offer 0.9.4 (pass --manifest-url)"
        );
        assert_eq!(out, "");
    }

    /// The node runs 0.9.0, the server's desired version, whose manifest the
    /// heartbeat's update_url is: `update apply --version 0.8.5` installs
    /// 0.8.5 from releases_base_url, with 0.9.0 to roll back to, and leaves
    /// the 0.9.0 slot alone. (It reinstalled 0.9.0 in place, with nothing to
    /// roll back to, before the update_url was kept for its own version.)
    #[test]
    fn update_apply_version_moves_off_the_servers_version() {
        use std::os::unix::fs::MetadataExt;
        let td = tempfile::tempdir().unwrap();
        let (_, m090) = release(td.path(), "0.9.0");
        let (_, m085) = release(td.path(), "0.8.5");
        let cdn = td.path().join("cdn");
        fs::create_dir_all(&cdn).unwrap();
        fs::copy(&m085, cdn.join("vesyl-print-0.8.5.manifest.json")).unwrap();
        let hb =
            json!({"ok": true, "desired_agent_version": "0.9.0", "update_url": file_url(&m090)})
                .to_string();
        let srv = http_stub::serve(move |_, s| respond(s, 200, &[], hb.as_bytes()));
        let base = deps(td.path(), &srv.base_url);
        let d = Deps {
            cfg: Config {
                update_require_signature: false,
                releases_base_url: file_url(&cdn),
                ..base.cfg
            },
            update_env: UpdateEnv {
                running_version: "0.9.0".into(),
                running_from_slot: true,
                ..base.update_env
            },
            ..base
        };
        installed_slot(&d, "0.8.0");
        installed_slot(&d, "0.9.0");
        pair(&d);
        let releases = d.update_env.install_root.join("releases");
        let slot = fs::metadata(releases.join("0.9.0")).unwrap().ino();
        let (r, out) = run_args(&d, &["update", "apply", "--version", "0.8.5"]);
        assert_eq!(r.unwrap(), 0, "{out}");
        assert_eq!(current(&d).as_deref(), Some("0.8.5"));
        let st = update::read_update_status(&d.cfg.update_status_path()).unwrap();
        assert_eq!(st.status, update::STATUS_PENDING_HEALTH);
        assert_eq!(st.target_version.as_deref(), Some("0.8.5"));
        assert_eq!(st.previous_version.as_deref(), Some("0.9.0"));
        assert_eq!(fs::metadata(releases.join("0.9.0")).unwrap().ino(), slot);
    }

    /// `update rollback` away from 0.9.0, which the server still asks for,
    /// sticks: the restarted agent's heartbeats do not install 0.9.0 again,
    /// nor even fetch its manifest, until the server asks for another
    /// version. (They did: the rollback recorded nothing.)
    #[test]
    fn update_rollback_is_not_undone_by_the_next_heartbeat() {
        let td = tempfile::tempdir().unwrap();
        let (_, m090) = release(td.path(), "0.9.0");
        let (_, m091) = release(td.path(), "0.9.1");
        let desired = Arc::new(Mutex::new("0.9.0"));
        let asked = desired.clone();
        let srv = http_stub::serve(move |req, s| match req.path.as_str() {
            "/print/v1/whoami" => respond(s, 200, &[], br#"{"node_id":"n1"}"#),
            "/print/v1/heartbeat" => {
                let v = *asked.lock().unwrap();
                let url = format!("http://127.0.0.1:{}/manifests/{v}.json", req.port());
                let hb = json!({"ok": true, "desired_agent_version": v, "update_url": url});
                respond(s, 200, &[], hb.to_string().as_bytes());
            }
            "/manifests/0.9.0.json" => respond(s, 200, &[], &fs::read(&m090).unwrap()),
            "/manifests/0.9.1.json" => respond(s, 200, &[], &fs::read(&m091).unwrap()),
            _ => respond(s, 404, &[], b"{}"),
        });
        let manifests = || {
            srv.requests()
                .iter()
                .filter(|r| r.path.starts_with("/manifests/"))
                .count()
        };
        let d = unsigned_deps(td.path());
        let d = Deps {
            cfg: Config {
                api_base_url: srv.base_url.clone(),
                ..d.cfg
            },
            ..d
        };
        d.cfg.ensure_dirs().unwrap();
        installed_slot(&d, "0.8.0");
        installed_slot(&d, "0.9.0");
        pair(&d);
        let path = d.cfg.update_status_path();
        // 0.9.0 passed its gate long ago.
        let idle = update::UpdateStatus {
            current_version: "0.9.0".into(),
            ..Default::default()
        };
        update::write_update_status(&path, &idle).unwrap();

        let (r, out) = run_args(&d, &["update", "rollback", "--version", "v0.8.0"]);
        assert_eq!(r.unwrap(), 0, "{out}");
        assert!(
            out.starts_with("rolled back to 0.8.0\nholding 0.9.0: "),
            "{out}"
        );
        let st = update::read_update_status(&path).unwrap();
        assert_eq!(st.status, update::STATUS_ROLLED_BACK);
        assert_eq!(st.target_version.as_deref(), Some("0.9.0"));
        assert_eq!(
            st.last_error.as_deref(),
            Some("manual rollback from 0.9.0 to 0.8.0")
        );
        assert_eq!((st.previous_version, st.health_deadline_at), (None, None));

        // The 0.8.0 agent the rollback restarts.
        let agent = slot_agent(&d, "0.8.0", Arc::new(|| Some(Vec::new())));
        for _ in 0..2 {
            assert_eq!(agent.run_once(false).last_error, None);
            assert_eq!(current(&d).as_deref(), Some("0.8.0"));
        }
        assert_eq!(manifests(), 0, "0.9.0 fetched again");
        let st = update::read_update_status(&path).unwrap();
        assert_eq!(st.status, update::STATUS_ROLLED_BACK);
        assert_eq!(st.target_version.as_deref(), Some("0.9.0"));

        // The server asks for another version: installed.
        *desired.lock().unwrap() = "0.9.1";
        agent.run_once(false);
        assert_eq!(current(&d).as_deref(), Some("0.9.1"));
        let st = update::read_update_status(&path).unwrap();
        assert_eq!(st.status, update::STATUS_PENDING_HEALTH, "{st:?}");
    }

    /// A node that rolled back from 0.9.0 to 0.8.0 by hand, the server still
    /// asking for 0.9.0 (it answers `hb`), and the 0.8.0 CLI and agent.
    fn rolled_back_by_hand(td: &Path) -> (Deps, Agent, http_stub::Stub) {
        let (_, m090) = release(td, "0.9.0");
        let srv = http_stub::serve(move |req, s| match req.path.as_str() {
            "/print/v1/whoami" => respond(s, 200, &[], br#"{"node_id":"n1"}"#),
            "/print/v1/heartbeat" => {
                let hb = json!({"ok": true, "desired_agent_version": "0.9.0",
                                "update_url": file_url(&m090)});
                respond(s, 200, &[], hb.to_string().as_bytes());
            }
            _ => respond(s, 404, &[], b"{}"),
        });
        let base = unsigned_deps(td);
        let d = Deps {
            cfg: Config {
                api_base_url: srv.base_url.clone(),
                cable_enabled: false,
                pull_jobs_enabled: false,
                ..base.cfg
            },
            update_env: UpdateEnv {
                running_version: "0.8.0".into(),
                ..base.update_env
            },
            ..base
        };
        d.cfg.ensure_dirs().unwrap();
        installed_slot(&d, "0.8.0");
        installed_slot(&d, "0.9.0");
        pair(&d);
        let (r, out) = run_args(&d, &["update", "rollback"]);
        assert_eq!(r.unwrap(), 0, "{out}");
        assert_eq!(current(&d).as_deref(), Some("0.8.0"));
        let agent = slot_agent(&d, "0.8.0", Arc::new(|| Some(Vec::new())));
        (d, agent, srv)
    }

    /// The hold `update rollback` records, as the 0.8.0 agent's next
    /// heartbeat leaves it: 0.9.0 not installed again.
    fn assert_still_held(d: &Deps, agent: &Agent, ctx: &str) {
        agent.run_once(false);
        assert_eq!(
            current(d).as_deref(),
            Some("0.8.0"),
            "{ctx}: 0.9.0 reinstalled"
        );
        let st = update::read_update_status(&d.cfg.update_status_path()).unwrap();
        assert_eq!(st.status, update::STATUS_ROLLED_BACK, "{ctx}: {st:?}");
        assert_eq!(st.target_version.as_deref(), Some("0.9.0"), "{ctx}");
        assert_eq!(
            st.last_error.as_deref(),
            Some("manual rollback from 0.9.0 to 0.8.0"),
            "{ctx}"
        );
    }

    /// An `update apply` that installs nothing (the version asked for runs
    /// already) leaves the hold of `update rollback` alone, and so does a
    /// reinstall of the running slot with --restart (a repair). They wrote
    /// over it (idle; a gate that then passed), and the agent installed the
    /// version rolled back from again at its next heartbeat.
    #[test]
    fn an_apply_that_installs_nothing_keeps_the_hold_of_a_rollback() {
        let td = tempfile::tempdir().unwrap();
        let (d, agent, _srv) = rolled_back_by_hand(td.path());
        assert_still_held(&d, &agent, "after the rollback");

        for argv in [
            &["update", "apply", "--version", "0.8.0"][..],
            &["update", "apply", "--version", "v0.8.0"],
        ] {
            let (r, out) = run_args(&d, argv);
            assert_eq!(r.unwrap(), 0, "{argv:?}: {out}");
            let st: Value = serde_json::from_str(&out).unwrap();
            assert_eq!(st["status"], update::STATUS_ROLLED_BACK, "{argv:?}: {out}");
            assert_eq!(st["target_version"], "0.9.0", "{argv:?}");
            assert_still_held(&d, &agent, &format!("{argv:?}"));
        }

        let (tarball, manifest) = release(td.path(), "0.8.0");
        let (t, m) = (tarball.to_str().unwrap(), manifest.to_str().unwrap());
        let (r, out) = run_args(
            &d,
            &["update", "apply", "--file", t, "--manifest", m, "--restart"],
        );
        assert_eq!(r.unwrap(), 0, "{out}");
        assert!(out.contains("still holding 0.9.0"), "{out}");
        assert!(!update::should_pause_jobs_from_path(
            &d.cfg.update_status_path()
        ));
        assert_still_held(&d, &agent, "--file 0.8.0 --restart");

        // Applying the version held is what releases it.
        let (r, out) = run_args(&d, &["update", "apply", "--version", "0.9.0"]);
        assert_eq!(r.unwrap(), 0, "{out}");
        assert_eq!(current(&d).as_deref(), Some("0.9.0"));
        let st = update::read_update_status(&d.cfg.update_status_path()).unwrap();
        assert_eq!(st.status, update::STATUS_PENDING_HEALTH, "{st:?}");
    }

    /// After `update rollback` from 0.9.0 to 0.8.0, 0.9.0 reinstalled by
    /// `update apply --file` without --restart and run at the next restart,
    /// the server asking for 0.9.0: the rollback is over. It stayed, and the
    /// LCD said "Rolled back" (and the heartbeat reported it) while 0.9.0 ran.
    #[test]
    fn a_rollback_is_over_once_the_desired_version_runs() {
        let td = tempfile::tempdir().unwrap();
        let (d, _, _srv) = rolled_back_by_hand(td.path());
        let (tarball, manifest) = release(td.path(), "0.9.0");
        let (t, m) = (tarball.to_str().unwrap(), manifest.to_str().unwrap());
        let (r, out) = run_args(&d, &["update", "apply", "--file", t, "--manifest", m]);
        assert_eq!(r.unwrap(), 0, "{out}");
        assert_eq!(current(&d).as_deref(), Some("0.9.0"));
        let path = d.cfg.update_status_path();
        assert_eq!(
            update::read_update_status(&path).unwrap().status,
            update::STATUS_ROLLED_BACK
        );

        let agent = slot_agent(&d, "0.9.0", Arc::new(|| Some(Vec::new())));
        agent.run_once(false);
        let st = update::read_update_status(&path).unwrap();
        assert_eq!(st.status, update::STATUS_IDLE, "{st:?}");
        assert_eq!(st.last_error, None);
        assert_eq!(st.target_version.as_deref(), Some("0.9.0"));
        assert_eq!(current(&d).as_deref(), Some("0.9.0"));
    }

    /// The process a rollback leaves running until its restart runs the
    /// version rolled back from, which the server still asks for: its
    /// heartbeats keep the hold, here after `update rollback` without
    /// --restart (for a gate's late restart, see
    /// [`the_process_a_gate_rolls_back_from_keeps_its_hold`]). They
    /// ended the rollback (idle, nothing held) as the version asked for was
    /// running, and the agent rolled back to installed it again.
    #[test]
    fn the_process_a_rollback_leaves_running_keeps_its_hold() {
        // `update rollback` without --restart: the 0.9.0 agent still runs.
        let td = tempfile::tempdir().unwrap();
        let (d, agent080, _srv) = rolled_back_by_hand(td.path());
        let old = slot_agent(&d, "0.9.0", Arc::new(|| Some(Vec::new())));
        for _ in 0..2 {
            old.run_once(false);
            let st = update::read_update_status(&d.cfg.update_status_path()).unwrap();
            assert_eq!(st.status, update::STATUS_ROLLED_BACK, "{st:?}");
            assert_eq!(st.target_version.as_deref(), Some("0.9.0"));
        }
        assert_still_held(&d, &agent080, "after the 0.9.0 agent's heartbeats");
    }

    /// [`the_process_a_rollback_leaves_running_keeps_its_hold`], for a gate's
    /// rollback: 0.9.0 fails its gate, and the restart into 0.8.0 has not
    /// come yet when the 0.9.0 agent heartbeats again. Ended there, the
    /// rollback looped: download, gate, rollback, for as long as the server
    /// asked for 0.9.0.
    #[test]
    fn the_process_a_gate_rolls_back_from_keeps_its_hold() {
        let td = tempfile::tempdir().unwrap();
        let (d, agent080, _srv) = rolled_back_by_hand(td.path());
        let root = &d.update_env.install_root;
        update::flip_current(root, "0.9.0").unwrap();
        let path = d.cfg.update_status_path();
        let gate = update::UpdateStatus {
            status: update::STATUS_PENDING_HEALTH.into(),
            current_version: "0.9.0".into(),
            target_version: Some("0.9.0".into()),
            previous_version: Some("0.8.0".into()),
            health_deadline_at: Some("2000-01-01T00:00:00+00:00".into()),
            ..Default::default()
        };
        update::write_update_status(&path, &gate).unwrap();
        // What fails the gate: the slot can no longer run.
        let binary = root.join("releases/0.9.0/vesyl-print");
        crate::util::set_mode(&binary, 0o644).unwrap();
        let old = slot_agent(&d, "0.9.0", Arc::new(|| Some(Vec::new())));
        old.run_once(false);
        assert_eq!(current(&d).as_deref(), Some("0.8.0"), "never rolled back");
        crate::util::set_mode(&binary, 0o755).unwrap();
        let gated = update::read_update_status(&path).unwrap();
        assert_eq!(gated.status, update::STATUS_ROLLED_BACK, "{gated:?}");
        for _ in 0..2 {
            old.run_once(false);
            let st = update::read_update_status(&path).unwrap();
            assert_eq!(st.status, update::STATUS_ROLLED_BACK, "{st:?}");
            assert_eq!(st.target_version.as_deref(), Some("0.9.0"));
            assert_eq!(st.last_error, gated.last_error);
        }
        agent080.run_once(false);
        assert_eq!(current(&d).as_deref(), Some("0.8.0"), "0.9.0 reinstalled");
        let st = update::read_update_status(&path).unwrap();
        assert_eq!(st.status, update::STATUS_ROLLED_BACK, "{st:?}");
        assert_eq!(st.target_version.as_deref(), Some("0.9.0"));
        assert_eq!(st.last_error, gated.last_error);
    }

    /// `update check` shows the manifest URL without the query a presigned
    /// URL carries its signature in.
    #[test]
    fn update_check_redacts_the_manifest_url() {
        let td = tempfile::tempdir().unwrap();
        let srv = http_stub::serve(|_, s| {
            let hb = json!({"ok": true, "desired_agent_version": "0.9.0",
                            "update_url": "https://cdn.example/m.json?X-Amz-Signature=s3cr3t"});
            respond(s, 200, &[], hb.to_string().as_bytes())
        });
        let d = deps(td.path(), &srv.base_url);
        pair(&d);
        let (r, out) = run_args(&d, &["update", "check"]);
        assert_eq!(r.unwrap(), 0, "{out}");
        assert!(
            out.contains("manifest: https://cdn.example/m.json\n"),
            "{out}"
        );
        assert!(!out.contains("s3cr3t"), "{out}");
    }

    /// `update rollback` while the gate of the version it leaves is open
    /// closes the gate at once: jobs resume, and the version is held, with
    /// no wait for the agent's next cycle to notice `current` moved.
    #[test]
    fn update_rollback_closes_an_open_gate_at_once() {
        let td = tempfile::tempdir().unwrap();
        let d = unsigned_deps(td.path());
        installed_slot(&d, "0.8.0");
        let (tarball, manifest) = release(td.path(), "0.9.0");
        let (t, m) = (tarball.to_str().unwrap(), manifest.to_str().unwrap());
        let (r, out) = run_args(
            &d,
            &["update", "apply", "--file", t, "--manifest", m, "--restart"],
        );
        assert_eq!(r.unwrap(), 0, "{out}");
        let path = d.cfg.update_status_path();
        assert!(update::should_pause_jobs_from_path(&path));

        let (r, out) = run_args(&d, &["update", "rollback"]);
        assert_eq!(r.unwrap(), 0, "{out}");
        assert_eq!(current(&d).as_deref(), Some("0.8.0"));
        let st = update::read_update_status(&path).unwrap();
        assert_eq!(st.status, update::STATUS_ROLLED_BACK);
        assert_eq!(st.target_version.as_deref(), Some("0.9.0"));
        assert_eq!(
            (st.health_deadline_at, st.previous_version),
            (None, None),
            "the gate is still armed"
        );
        assert!(!update::should_pause_jobs_from_path(&path));
    }

    /// `update rollback --restart` restarts the services as the apply arms
    /// do: only when the environment says so, which a test's never does.
    /// (It ran systemctl or sudo for real.)
    #[test]
    fn update_rollback_restart_honors_the_environment() {
        let td = tempfile::tempdir().unwrap();
        let mut d = deps(td.path(), "http://127.0.0.1:9");
        let root = d.update_env.install_root.clone();
        for v in ["0.3.0", "0.4.0"] {
            runnable_slot(&root, v);
        }
        update::flip_current(&root, "0.4.0").unwrap();
        let rollback = |d: &Deps, argv: &[&str]| update::restarts_during(|| run_args(d, argv));

        let ((r, out), restarts) = rollback(&d, &["update", "rollback", "--restart"]);
        assert_eq!(r.unwrap(), 0, "{out}");
        assert_eq!(restarts, 0, "restarted with UpdateEnv::restart off");
        assert!(!out.contains("services restarted"), "{out}");

        d.update_env.restart = true;
        let ((r, out), restarts) = rollback(&d, &["update", "rollback", "--restart"]);
        assert_eq!(r.unwrap(), 0, "{out}");
        assert_eq!(restarts, 1);
        assert!(out.ends_with("services restarted\n"), "{out}");
        let ((r, out), restarts) = rollback(&d, &["update", "rollback"]);
        assert_eq!((r.unwrap(), restarts), (0, 0), "{out}");
    }

    /// A version the agent holds (it cannot be installed) or waits to retry
    /// (after a failure that may pass) is installed all the same by `update
    /// apply`, which starts from a fresh status.
    #[test]
    fn update_apply_overrides_the_agents_holds_and_backoff() {
        let td = tempfile::tempdir().unwrap();
        let (_, m090) = release(td.path(), "0.9.0");
        let hb =
            json!({"ok": true, "desired_agent_version": "0.9.0", "update_url": file_url(&m090)})
                .to_string();
        let srv = http_stub::serve(move |_, s| respond(s, 200, &[], hb.as_bytes()));
        let base = unsigned_deps(td.path());
        let d = Deps {
            cfg: Config {
                api_base_url: srv.base_url.clone(),
                ..base.cfg
            },
            ..base
        };
        d.cfg.ensure_dirs().unwrap();
        installed_slot(&d, "0.8.0");
        pair(&d);
        let failed = |code: &str, attempts: i64, retry_at: Option<String>| update::UpdateStatus {
            status: update::STATUS_FAILED.into(),
            target_version: Some("0.9.0".into()),
            last_error: Some("an earlier attempt failed".into()),
            last_error_code: Some(code.into()),
            attempts,
            retry_at,
            ..Default::default()
        };
        for held in [
            failed("bad_archive", 1, None),
            failed("download_failed", 3, Some(update::utc_now_plus(3000))),
        ] {
            update::flip_current(&d.update_env.install_root, "0.8.0").unwrap();
            update::write_update_status(&d.cfg.update_status_path(), &held).unwrap();
            let (r, out) = run_args(&d, &["update", "apply"]);
            assert_eq!(r.unwrap(), 0, "{held:?}: {out}");
            let st = update::read_update_status(&d.cfg.update_status_path()).unwrap();
            assert_eq!(st.status, update::STATUS_PENDING_HEALTH, "{held:?}");
            assert_eq!(
                (st.last_error_code, st.attempts, st.retry_at),
                (None, 0, None)
            );
            assert_eq!(current(&d).as_deref(), Some("0.9.0"));
        }
    }

    const TILDE_TD: &str = "VESYL_TEST_CLI_TILDE_TD";

    /// `~/` in `print-test --file` and `update apply --file` / `--manifest`
    /// is the home directory, as in a job's own path: bash leaves it alone
    /// in `--file=~/…`, and in quotes. Run in a child process with its own
    /// $HOME.
    #[test]
    fn tilde_paths_are_the_home_directory() {
        let td = tempfile::tempdir().unwrap();
        let home = td.path().join("home");
        fs::create_dir_all(&home).unwrap();
        fs::write(home.join("label.pdf"), "%PDF-1.4\n").unwrap();
        let (tarball, manifest) = release(td.path(), "0.9.0");
        fs::rename(tarball, home.join("rel.tar.gz")).unwrap();
        fs::rename(manifest, home.join("m.json")).unwrap();
        let out = std::process::Command::new("/proc/self/exe")
            .args([
                "--exact",
                "cli::tests::tilde_paths_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(TILDE_TD, td.path())
            .env("HOME", &home)
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
    #[ignore = "child process of tilde_paths_are_the_home_directory"]
    fn tilde_paths_child() {
        let Some(td) = std::env::var_os(TILDE_TD).map(PathBuf::from) else {
            return;
        };
        let home = td.join("home");
        let (d, cups) = test_print_deps(&td, Ok(Some("Brother_HL-9".into())));
        let (r, out) = run_args(
            &d,
            &["print-test", "--file=~/label.pdf", "--queue", "Brother_HL"],
        );
        assert_eq!(r.unwrap(), 0, "{out}");
        assert!(
            out.contains(&format!(
                "file:       {}\n",
                home.join("label.pdf").display()
            )),
            "{out}"
        );
        let label = fs::canonicalize(home.join("label.pdf")).unwrap();
        assert_eq!(cups.calls()[0].file, label);

        let d = unsigned_deps(&td);
        installed_slot(&d, "0.8.0");
        let (r, out) = run_args(
            &d,
            &[
                "update",
                "apply",
                "--file=~/rel.tar.gz",
                "--manifest",
                "~/m.json",
            ],
        );
        assert_eq!(r.unwrap(), 0, "{out}");
        assert_eq!(current(&d).as_deref(), Some("0.9.0"));
    }

    /// The agent's stop reaches an update in progress: a `systemctl stop`
    /// mid-download ends `Agent::run` within a read, not when the download
    /// is done, with nothing activated and nothing that holds the retry at
    /// the next start.
    #[test]
    fn the_agents_stop_ends_an_update_download() {
        let td = tempfile::tempdir().unwrap();
        let (tarball, _) = release(td.path(), "0.9.0");
        let body = fs::read(&tarball).unwrap();
        let sha = update::sha256_file(&tarball).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        // The artifact trickles: 40 pieces 200 ms apart, 8 s in all; the
        // stop comes once the first is out.
        let srv = http_stub::serve(move |req, s| {
            let base = format!("http://127.0.0.1:{}", req.port());
            match req.path.as_str() {
                "/print/v1/whoami" => respond(s, 200, &[], br#"{"node_id":"n1"}"#),
                "/print/v1/heartbeat" => {
                    let hb = json!({"ok": true, "desired_agent_version": "0.9.0",
                                    "update_url": format!("{base}/m.json")});
                    respond(s, 200, &[], hb.to_string().as_bytes());
                }
                "/m.json" => {
                    let m = json!({"version": "0.9.0", "artifact_sha256": sha,
                                   "artifact_url": format!("{base}/a.tar.gz")});
                    respond(s, 200, &[], m.to_string().as_bytes());
                }
                "/a.tar.gz" => {
                    let _ = write!(
                        s,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let pieces: Vec<&[u8]> = body.chunks(body.len().div_ceil(40)).collect();
                    let _ = s.write_all(pieces[0]);
                    let _ = s.flush();
                    stopping.store(true, Ordering::SeqCst);
                    for piece in &pieces[1..] {
                        thread::sleep(Duration::from_millis(200));
                        if s.write_all(piece).and_then(|()| s.flush()).is_err() {
                            return;
                        }
                    }
                }
                _ => respond(s, 404, &[], b"{}"),
            }
        });
        let base = unsigned_deps(td.path());
        let d = Deps {
            cfg: Config {
                api_base_url: srv.base_url.clone(),
                cable_enabled: false,
                pull_jobs_enabled: false,
                ..base.cfg
            },
            ..base
        };
        d.cfg.ensure_dirs().unwrap();
        installed_slot(&d, "0.8.0");
        pair(&d);
        let mut agent = slot_agent(&d, "0.8.0", Arc::new(|| Some(Vec::new())));
        // Never real CUPS (lpinfo, lpadmin, a LAN scan) from a test.
        agent.provision_printers = Arc::new(Vec::new);

        let started = std::time::Instant::now();
        agent.run(stop);
        let took = started.elapsed();
        assert!(took < Duration::from_secs(5), "took {took:?}");
        let st = update::read_update_status(&d.cfg.update_status_path()).unwrap();
        assert_eq!(st.status, update::STATUS_IDLE, "{st:?}");
        assert_eq!(
            st.last_error.as_deref(),
            Some("update stopped while downloading: the agent is stopping")
        );
        assert_eq!(current(&d).as_deref(), Some("0.8.0"));
        let root = &d.update_env.install_root;
        assert_eq!(file_names(&root.join("update")), Vec::<String>::new());
        assert_eq!(file_names(&root.join("releases")), ["0.8.0"]);
    }

    /// `update apply --file 0.7.0 … --restart` while the 0.8.0 agent
    /// downloads 0.9.0: the CLI activates 0.7.0 and arms its gate, and its
    /// restart stops the agent mid-download. The status the agent writes
    /// as it stops keeps that gate. (It wrote idle over it, and the
    /// restarted 0.7.0 ran with no gate: nothing rolled a bad one back.)
    #[test]
    fn a_gate_armed_during_the_agents_download_survives_its_stop() {
        let td = tempfile::tempdir().unwrap();
        let (tarball, _) = release(td.path(), "0.9.0");
        let body = fs::read(&tarball).unwrap();
        let sha = update::sha256_file(&tarball).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        // What the CLI does, once the download is under way.
        type CliApply = Box<dyn Fn() + Send>;
        let cli: Arc<Mutex<Option<CliApply>>> = Arc::new(Mutex::new(None));
        let in_cli = cli.clone();
        let srv = http_stub::serve(move |req, s| {
            let base = format!("http://127.0.0.1:{}", req.port());
            match req.path.as_str() {
                "/print/v1/whoami" => respond(s, 200, &[], br#"{"node_id":"n1"}"#),
                "/print/v1/heartbeat" => {
                    let hb = json!({"ok": true, "desired_agent_version": "0.9.0",
                                    "update_url": format!("{base}/m.json")});
                    respond(s, 200, &[], hb.to_string().as_bytes());
                }
                "/m.json" => {
                    let m = json!({"version": "0.9.0", "artifact_sha256": sha,
                                   "artifact_url": format!("{base}/a.tar.gz")});
                    respond(s, 200, &[], m.to_string().as_bytes());
                }
                "/a.tar.gz" => {
                    let _ = write!(
                        s,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let pieces: Vec<&[u8]> = body.chunks(body.len().div_ceil(40)).collect();
                    let _ = s.write_all(pieces[0]);
                    let _ = s.flush();
                    if let Some(apply) = in_cli.lock().unwrap().as_ref() {
                        apply();
                    }
                    // The restart's SIGTERM.
                    stopping.store(true, Ordering::SeqCst);
                    for piece in &pieces[1..] {
                        thread::sleep(Duration::from_millis(200));
                        if s.write_all(piece).and_then(|()| s.flush()).is_err() {
                            return;
                        }
                    }
                }
                _ => respond(s, 404, &[], b"{}"),
            }
        });
        let base = unsigned_deps(td.path());
        let d = Deps {
            cfg: Config {
                api_base_url: srv.base_url.clone(),
                cable_enabled: false,
                pull_jobs_enabled: false,
                ..base.cfg
            },
            ..base
        };
        d.cfg.ensure_dirs().unwrap();
        installed_slot(&d, "0.7.0");
        installed_slot(&d, "0.8.0");
        pair(&d);
        let (cfg, root) = (d.cfg.clone(), d.update_env.install_root.clone());
        *cli.lock().unwrap() = Some(Box::new(move || {
            update::flip_current(&root, "0.7.0").unwrap();
            update::arm_health_gate(
                &cfg,
                &cfg.update_status_path(),
                "0.7.0",
                Some("0.8.0".into()),
            )
            .unwrap();
        }));
        let mut agent = slot_agent(&d, "0.8.0", Arc::new(|| Some(Vec::new())));
        // Never real CUPS (lpinfo, lpadmin, a LAN scan) from a test.
        agent.provision_printers = Arc::new(Vec::new);

        agent.run(stop);
        let st = update::read_update_status(&d.cfg.update_status_path()).unwrap();
        assert_eq!(st.status, update::STATUS_PENDING_HEALTH, "{st:?}");
        assert_eq!(st.target_version.as_deref(), Some("0.7.0"));
        assert_eq!(st.previous_version.as_deref(), Some("0.8.0"));
        assert_eq!(current(&d).as_deref(), Some("0.7.0"));
    }

    /// `Agent::run` of the 0.9.0 agent started after its update, which
    /// finds the health gate for 0.9.0 (over 0.8.0) past its deadline;
    /// whoami answers `whoami`. The agent is stopped while whoami is under
    /// way when `stop_in_whoami`, else once the gate's verdict is on disk.
    /// Returns the update status left, and the restarts asked for.
    fn gate_under_agent_run(whoami: u16, stop_in_whoami: bool) -> (update::UpdateStatus, usize) {
        let td = tempfile::tempdir().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let srv = http_stub::serve(move |req, s| match req.path.as_str() {
            "/print/v1/whoami" => {
                if stop_in_whoami {
                    stopping.store(true, Ordering::SeqCst);
                }
                respond(s, whoami, &[], b"{}");
            }
            _ => respond(s, 404, &[], b"{}"),
        });
        let base = unsigned_deps(td.path());
        let d = Deps {
            cfg: Config {
                api_base_url: srv.base_url.clone(),
                cable_enabled: false,
                pull_jobs_enabled: false,
                ..base.cfg
            },
            ..base
        };
        d.cfg.ensure_dirs().unwrap();
        installed_slot(&d, "0.8.0");
        installed_slot(&d, "0.9.0");
        pair(&d);
        if whoami == 401 {
            // Whoami reaching the API passes the gate: what fails it here
            // is the slot, which can no longer run.
            let binary = d.update_env.install_root.join("releases/0.9.0/vesyl-print");
            crate::util::set_mode(&binary, 0o644).unwrap();
        }
        let path = d.cfg.update_status_path();
        let gate = update::UpdateStatus {
            status: update::STATUS_PENDING_HEALTH.into(),
            current_version: "0.9.0".into(),
            target_version: Some("0.9.0".into()),
            previous_version: Some("0.8.0".into()),
            health_deadline_at: Some("2000-01-01T00:00:00+00:00".into()),
            ..Default::default()
        };
        update::write_update_status(&path, &gate).unwrap();
        let mut agent = slot_agent(&d, "0.9.0", Arc::new(|| Some(Vec::new())));
        // Never real CUPS (lpinfo, lpadmin, a LAN scan) from a test.
        agent.provision_printers = Arc::new(Vec::new);
        // Counted by restarts_during, never run.
        agent.update_env.restart = true;
        // Stops the agent once the gate's verdict is on disk, which it writes
        // after it has restarted the services or not; or after 20 s, so that
        // a gate that never decides fails the test rather than hangs it.
        let stopper = {
            let (stop, path) = (stop.clone(), path.clone());
            thread::spawn(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(20);
                while !stop.load(Ordering::SeqCst)
                    && update::read_update_status(&path)
                        .is_none_or(|st| st.status == update::STATUS_PENDING_HEALTH)
                    && std::time::Instant::now() < deadline
                {
                    thread::sleep(Duration::from_millis(10));
                }
                stop.store(true, Ordering::SeqCst);
            })
        };
        let ((), restarts) = update::restarts_during(|| agent.run(stop));
        stopper.join().unwrap();
        assert_eq!(current(&d).as_deref(), Some("0.8.0"), "never rolled back");
        (update::read_update_status(&path).unwrap(), restarts)
    }

    /// The agent's stop reaches its health gate: a gate that rolls back
    /// during a `systemctl stop` flips `current` but restarts nothing, as
    /// that restart would replace the stop job and bring the agent back. So
    /// whether whoami fails or the API turns the node away (the gate runs
    /// on either path of the agent's cycle). Not stopping, it restarts.
    #[test]
    fn the_agents_stop_reaches_its_health_gate() {
        for (whoami, why) in [
            (503, "health failed: "),
            (
                401,
                "health failed: current slot has no executable vesyl-print binary",
            ),
        ] {
            let (st, restarts) = gate_under_agent_run(whoami, true);
            assert_eq!(st.status, update::STATUS_ROLLED_BACK, "{whoami}: {st:?}");
            let error = st.last_error.unwrap_or_default();
            assert!(
                error.starts_with(why) && error.ends_with("; rolled back to 0.8.0"),
                "{whoami}: {error}"
            );
            assert_eq!(restarts, 0, "{whoami}: restarted while stopping");
        }
        let (st, restarts) = gate_under_agent_run(503, false);
        assert_eq!(st.status, update::STATUS_ROLLED_BACK, "{st:?}");
        assert_eq!(restarts, 1);
    }

    /// `--version` with `--manifest-url` or `--file` is the version the
    /// release must be: a manifest for another is refused before anything
    /// is downloaded, unpacked or activated, and a `--version` that is no
    /// version before the manifest is even fetched. (It was ignored: 0.9.3
    /// was installed for `--version 0.9.2`, and `--version latest` passed.)
    #[test]
    fn update_apply_version_holds_for_every_source() {
        let td = tempfile::tempdir().unwrap();
        let d = unsigned_deps(td.path());
        installed_slot(&d, "0.8.0");
        let (tarball, manifest) = release(td.path(), "0.9.3");
        let served = fs::read(&manifest).unwrap();
        let srv = http_stub::serve(move |_, s| respond(s, 200, &[], &served));
        let url = format!("{}/m.json", srv.base_url);
        let (t, m) = (tarball.to_str().unwrap(), manifest.to_str().unwrap());
        // `update apply --version <v>` from the manifest URL, and from the file.
        let from_url =
            |v: &'static str| vec!["update", "apply", "--manifest-url", &url, "--version", v];
        let from_file = |v: &'static str| {
            vec![
                "update",
                "apply",
                "--version",
                v,
                "--file",
                t,
                "--manifest",
                m,
            ]
        };
        let root = &d.update_env.install_root;
        let invalid = "invalid --version \"latest\": expected a release version such as 0.5.0";
        let another = "manifest is for version 0.9.3, not 0.9.2";
        for (argv, error) in [
            (from_url("latest"), invalid),
            (from_url("0.9.2"), another),
            (from_file("latest"), invalid),
            (from_file("0.9.2"), another),
        ] {
            let (r, out) = run_args(&d, &argv);
            assert_eq!(r.unwrap_err().0, error, "{argv:?}");
            assert_eq!(out, "", "{argv:?}");
            assert_eq!(current(&d).as_deref(), Some("0.8.0"), "{argv:?}");
            assert_eq!(file_names(&root.join("releases")), ["0.8.0"], "{argv:?}");
            assert_eq!(file_names(&root.join("update")), Vec::<String>::new());
        }
        // Only the --version that is one fetched the manifest.
        assert_eq!(srv.requests().len(), 1);

        // The version the release is: installed, from either source.
        for argv in [from_url("v0.9.3"), from_file("0.9.3")] {
            update::flip_current(root, "0.8.0").unwrap();
            let (r, out) = run_args(&d, &argv);
            assert_eq!(r.unwrap(), 0, "{argv:?}: {out}");
            assert_eq!(current(&d).as_deref(), Some("0.9.3"), "{argv:?}");
        }
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
            Command::Claim { code, name, json } => {
                assert_eq!(code, "AB7K2Q9M");
                assert_eq!(name.as_deref(), Some("Pack 2"));
                assert!(!json);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            cmd(&["claim", "--js", "AB7K2Q9M"]),
            Command::Claim { json: true, .. }
        ));
        match cmd(&["test-print", "--q", "Zebra", "--f", "zpl", "--j"]) {
            Command::TestPrint(a) => {
                assert_eq!(
                    (a.queue.as_str(), a.format.as_str(), a.json),
                    ("Zebra", "zpl", true)
                );
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
            // test-print needs both a queue and a format.
            &["test-print", "--format", "pdf"],
            &["test-print", "--queue", "Zebra"],
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
            vec!["claim", "AB7K2Q9M", "--name", "n", "--json"],
            vec![
                "test-print",
                "--queue",
                "Zebra",
                "--format",
                "pdf",
                "--json",
            ],
        ] {
            Cli::try_parse_from(std::iter::once("vesyl-print").chain(argv)).unwrap();
        }
    }
}
