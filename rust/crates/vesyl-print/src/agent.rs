//! Cloud agent: whoami + heartbeat + job pull + ActionCable push.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::auth::{self, Credentials};
use crate::cable::{PrintCableSession, SessionHandlers};
use crate::cloud::{CloudClient, CloudError, HeartbeatBody};
use crate::config::{agent_version, default_platform, Config, WaitCups};
use crate::jobs::{
    self, AckFn, JobError, JobState, JobStore, Pipeline, PrintJob, SpawnFn, StateFn, TickFn,
};
use crate::statusio::{self, AgentStatus, CloudState, PairingState};
use crate::update::{self, UpdateEnv, UpdateStatus, WhoamiResult};
use crate::util::{py_str, truthy};
use crate::{printers, sysinfo, JsonObject};

const LOG: &str = "vesyl-print.agent";

/// How often the background thread re-reads the printer inventory.
const INVENTORY_REFRESH_EVERY: Duration = Duration::from_secs(15);
/// How long the first REST heartbeat waits for the first inventory.
const INVENTORY_FIRST_WAIT: Duration = Duration::from_secs(30);
/// Processed markers older than this are pruned (far past any redelivery).
pub const PROCESSED_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const PRUNE_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// Cable reconnect pacing: the gap after an attempt that never subscribed
/// doubles from MIN up to MAX, and drops back to MIN once a session subscribes.
const CABLE_RETRY_MIN: Duration = Duration::from_secs(5);
const CABLE_RETRY_MAX: Duration = Duration::from_secs(60);
/// `last_error` for an OTA download / install that never finished: the
/// previous agent process died during it, or the heartbeat running it panicked.
pub const UPDATE_INTERRUPTED: &str = "update interrupted before it finished";

/// The bits of a cable session the job hooks need (mockable in tests).
pub trait CableChannel: Send + Sync {
    /// Perform a channel action; false if not subscribed or the send failed.
    fn perform(&self, action: &str, data: JsonObject) -> bool;
    fn subscribed(&self) -> bool;
}

impl CableChannel for PrintCableSession {
    fn perform(&self, action: &str, data: JsonObject) -> bool {
        PrintCableSession::perform(self, action, data)
    }

    fn subscribed(&self) -> bool {
        PrintCableSession::subscribed(self)
    }
}

pub type Cable = Option<Arc<dyn CableChannel>>;

/// Printer inventory source (CUPS in production; `None` when unavailable).
pub type InventoryFn = Arc<dyn Fn() -> Option<Vec<Value>> + Send + Sync>;

fn obj(v: Value) -> JsonObject {
    match v {
        Value::Object(m) => m,
        _ => JsonObject::new(),
    }
}

/// Mutex lock that survives a panicked holder (the data here stays valid).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Run `f`; a panic (e.g. the OS refusing a thread deep inside a library) is
/// logged and comes back as `None`, so one bad request or job cannot take
/// down the agent along with every job queued behind it.
fn contained<T>(what: &str, f: impl FnOnce() -> T) -> Option<T> {
    match jobs::catch_panic(f) {
        Ok(v) => Some(v),
        Err(msg) => {
            log::error!(target: LOG, "{what} panicked: {msg} — continuing");
            None
        }
    }
}

/// Result of one REST pull.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullResult {
    Ok,
    Unauthorized,
    /// 404/503: server lacks the endpoint or print service is off.
    Unavailable,
    Error,
}

pub fn status_from_creds(
    creds: Option<&Credentials>,
    pairing: PairingState,
    cloud: CloudState,
    last_error: Option<String>,
    last_heartbeat_at: Option<String>,
) -> AgentStatus {
    let mut st = AgentStatus {
        pairing,
        cloud,
        last_error,
        last_heartbeat_at,
        agent_version: Some(agent_version().into()),
        ..Default::default()
    };
    if let Some(c) = creds {
        st.node_id = Some(c.node_id.clone());
        st.name = c.name.clone();
        st.organization_name = c.organization_name.clone();
        // Multi-warehouse: comma-separated codes; single: name.
        st.warehouse_name = Some(c.warehouse_label());
    }
    st
}

/// Stop local work for a canceled job; do not print if still queued.
///
/// An id that is not a [`jobs::valid_job_id`] names no job of ours and is
/// never joined onto a path: the message is ignored.
pub fn handle_job_canceled(job_id: &str, store: &JobStore) -> std::io::Result<()> {
    if !jobs::valid_job_id(job_id) {
        log::warn!(target: LOG, "ignoring job_canceled with invalid job id {}", jobs::shown_id(job_id));
        return Ok(());
    }
    if store.is_processed(job_id) {
        store.delete_queue(job_id);
        return Ok(());
    }
    if store.has_queue_file(job_id) {
        log::info!(target: LOG, "job {job_id} canceled — dropping queue file");
        store.delete_queue(job_id);
    }
    // Marker prevents a late redelivery/print of a canceled job.
    store.mark_processed(job_id)
}

/// A stand-in job for reporting a cloud payload that failed validation, when
/// its id is usable (the status hooks only need the id).
fn rejected_job(payload: &JsonObject) -> Option<PrintJob> {
    let id = ["id", "job_id"]
        .iter()
        .find_map(|k| payload.get(*k).filter(|v| truthy(v)))
        .map(py_str)?;
    jobs::valid_job_id(&id).then(|| PrintJob {
        id,
        cups_name: String::new(),
        content_type: String::new(),
        content: String::new(),
        title: None,
        printer_id: None,
        options: JsonObject::new(),
        raw: JsonObject::new(),
    })
}

/// Tell the cloud a job will never print, so it does not sit at "sent".
fn report_rejected(pipeline: &Pipeline, job: &PrintJob, err: &JobError) {
    if let Err(e) = (pipeline.report_state)(job, JobState::Error, Some(&err.message)) {
        log::warn!(target: LOG, "could not report rejected job {}: {e}", job.id);
    }
}

/// Latest printer inventory, kept fresh by a background thread while
/// [`Agent::run`] is active. A full inventory takes ~16 s on a Pi (about 1 s
/// per `lpoptions`, up to 5 s for an ipps:// probe): far too long to run
/// inline in the loop that also takes and prints jobs.
#[derive(Default)]
struct InventoryCache {
    state: Mutex<InventoryState>,
    updated: Condvar,
}

#[derive(Default)]
struct InventoryState {
    /// Generation of the refresher keeping `latest` current (0: none).
    refresher: u64,
    generations: u64,
    /// Latest result; `None` until the refresher's first pass finishes.
    latest: Option<Option<Vec<Value>>>,
}

impl InventoryCache {
    /// The latest snapshot while a refresher runs (waiting at most
    /// `wait_first` for its first pass); with no refresher, query `source`.
    fn get(&self, source: &InventoryFn, wait_first: Duration) -> Option<Vec<Value>> {
        let st = lock(&self.state);
        if st.refresher == 0 {
            drop(st);
            return source();
        }
        let (st, _) = self
            .updated
            .wait_timeout_while(st, wait_first, |s| s.refresher != 0 && s.latest.is_none())
            .unwrap_or_else(|e| e.into_inner());
        st.latest.clone().flatten()
    }

    /// Register a new refresher; returns its generation.
    fn begin(&self) -> u64 {
        let mut st = lock(&self.state);
        st.generations += 1;
        st.refresher = st.generations;
        st.latest = None;
        st.refresher
    }

    fn is_current(&self, generation: u64) -> bool {
        lock(&self.state).refresher == generation
    }

    /// Store one refresh (`None`: it panicked — keep the previous snapshot).
    fn publish(&self, generation: u64, inventory: Option<Option<Vec<Value>>>) {
        let mut st = lock(&self.state);
        if st.refresher != generation {
            return;
        }
        match inventory {
            Some(inv) => st.latest = Some(inv),
            None => {
                st.latest.get_or_insert(None);
            }
        }
        drop(st);
        self.updated.notify_all();
    }

    /// The refresher stopped: callers query inline again.
    fn end(&self, generation: u64) {
        let mut st = lock(&self.state);
        if st.refresher == generation {
            st.refresher = 0;
            st.latest = None;
        }
        drop(st);
        self.updated.notify_all();
    }
}

#[derive(Default)]
struct TickTimes {
    rest: Option<Instant>,
    cable: Option<Instant>,
}

/// Runtime state shared by every clone of an [`Agent`].
struct Shared {
    inventory: InventoryCache,
    /// When the CUPS wait tick last sent each heartbeat: one schedule per
    /// agent however many jobs wait.
    wait_tick: Mutex<TickTimes>,
    /// Starts the inventory refresher thread (injectable for tests).
    spawn: SpawnFn,
}

impl Default for Shared {
    fn default() -> Self {
        Shared {
            inventory: InventoryCache::default(),
            wait_tick: Mutex::default(),
            spawn: Arc::new(jobs::spawn_thread),
        }
    }
}

/// Pacing for cable (re)connect attempts, kept apart from the REST backoff
/// (which resets on every online heartbeat, so an upgrade that keeps failing
/// while REST works would otherwise mint a ticket every loop).
#[derive(Debug)]
struct CableRetry {
    delay: Duration,
    next_try: Option<Instant>,
}

impl Default for CableRetry {
    fn default() -> Self {
        CableRetry {
            delay: CABLE_RETRY_MIN,
            next_try: None,
        }
    }
}

impl CableRetry {
    fn due(&self, now: Instant) -> bool {
        self.next_try.is_none_or(|t| now >= t)
    }

    /// A new session was started (or its ticket failed) at `now`.
    fn attempted(&mut self, now: Instant) {
        self.next_try = Some(now + self.delay);
        self.delay = (self.delay * 2).min(CABLE_RETRY_MAX);
    }

    /// A session reached `subscribed`: a later drop is retried promptly.
    fn subscribed(&mut self, now: Instant) {
        self.delay = CABLE_RETRY_MIN;
        self.next_try = self.next_try.map(|t| t.min(now + CABLE_RETRY_MIN));
    }
}

#[derive(Clone)]
pub struct Agent {
    pub cfg: Config,
    pub client: CloudClient,
    pub store: JobStore,
    /// Printer inventory source. Inside [`Agent::run`] it runs on a background
    /// thread and heartbeats send the latest snapshot.
    pub inventory: InventoryFn,
    pub update_env: UpdateEnv,
    /// Base job pipeline (lp, fetch, CUPS wait, raw probe). Cloud hooks and
    /// `wait_cups` are filled in per job; every clone shares its async CUPS
    /// watcher.
    pub pipeline: Pipeline,
    shared: Arc<Shared>,
}

impl Agent {
    pub fn new(cfg: Config) -> Self {
        let client = CloudClient::new(&cfg.api_base_url);
        let store = JobStore::from_config(&cfg);
        let update_env = UpdateEnv::detect(&cfg);
        Agent {
            client,
            store,
            inventory: Arc::new(|| Some(printers::inventory_payload())),
            update_env,
            pipeline: Pipeline::default(),
            shared: Arc::default(),
            cfg,
        }
    }

    fn write_status(&self, st: &mut AgentStatus) {
        if let Err(e) = statusio::write_status(&self.cfg.status_path(), st) {
            log::warn!(target: LOG, "write status: {e}");
        }
    }

    fn write_update_status(&self, st: &UpdateStatus) {
        if let Err(e) = update::write_update_status(&self.cfg.update_status_path(), st) {
            log::warn!(target: LOG, "write update status: {e}");
        }
    }

    /// Printer inventory for a heartbeat: inside [`Agent::run`] the background
    /// snapshot (waiting at most `wait_first` for the first one), otherwise an
    /// inline query (one-off callers of [`Agent::run_once`]).
    fn printers(&self, wait_first: Duration) -> Option<Vec<Value>> {
        self.shared.inventory.get(&self.inventory, wait_first)
    }

    /// Start the inventory refresher: first pass right away, then every 15 s
    /// until `stop`. Returns its generation (to end it). If the thread cannot
    /// start, heartbeats fall back to querying inline.
    fn start_inventory_refresher(&self, stop: &Arc<AtomicBool>) -> u64 {
        let generation = self.shared.inventory.begin();
        let (shared, source, stop) = (self.shared.clone(), self.inventory.clone(), stop.clone());
        let body = Box::new(move || {
            let cache = &shared.inventory;
            while !stop.load(Ordering::SeqCst) && cache.is_current(generation) {
                let inventory = contained("printer inventory", || source());
                cache.publish(generation, inventory);
                sleep_until(Instant::now() + INVENTORY_REFRESH_EVERY, &stop);
            }
            cache.end(generation);
        });
        if let Err(e) = (self.shared.spawn)("vesyl-print-inventory", body) {
            log::error!(target: LOG, "inventory refresher failed to start ({e}) — querying printers inline");
            self.shared.inventory.end(generation);
        }
        generation
    }

    /// Only [`Agent::run_once`] downloads and installs updates, synchronously.
    /// So at agent start, and whenever run_once has returned or unwound, an
    /// update status of `downloading` / `installing` is stale: the previous
    /// process died mid-update, or the heartbeat running it panicked. Left
    /// alone it would pause jobs, and the held push jobs would keep the
    /// update deferred forever. Mark it failed instead; `run_once` still
    /// promotes it to `pending_health` when the interrupted install had
    /// already switched slots.
    fn recover_interrupted_update(&self) {
        let Some(mut st) = update::read_update_status(&self.cfg.update_status_path()) else {
            return;
        };
        if st.status != update::STATUS_DOWNLOADING && st.status != update::STATUS_INSTALLING {
            return;
        }
        log::warn!(
            target: LOG,
            "update to {} stopped while {} and is not running — marking it failed",
            st.target_version.as_deref().unwrap_or("?"),
            st.status
        );
        st.status = update::STATUS_FAILED.into();
        st.last_error = Some(UPDATE_INTERRUPTED.into());
        self.write_update_status(&st);
    }

    /// The loop's REST heartbeat: `run_once` ([`Agent::run_once`], passed in
    /// so tests can make it unwind) with panics contained. Once it has
    /// returned or unwound no update is downloading or installing, so a
    /// status still saying so (a panic mid-download, say) is cleared before
    /// the loop reads it to decide whether to hold jobs.
    fn heartbeat_step(&self, run_once: impl FnOnce() -> AgentStatus) -> Option<AgentStatus> {
        let st = contained("heartbeat", run_once);
        self.recover_interrupted_update();
        st
    }

    fn prune_processed_markers(&self) {
        let removed = self.store.prune_processed(PROCESSED_RETENTION);
        if removed > 0 {
            log::info!(target: LOG, "pruned {removed} processed marker(s) older than 30 days");
        }
    }

    /// Revoke local pairing: clear credentials, write LCD status. No auto-reclaim.
    pub fn handle_unauthorized(&self, creds: Option<&Credentials>) {
        log::warn!(target: LOG, "device token rejected (401) — re-pair required");
        let mut st = status_from_creds(
            creds,
            PairingState::Revoked,
            CloudState::Offline,
            None,
            None,
        );
        st.last_error = Some("re-pair required".into());
        self.write_status(&mut st);
        if let Err(e) = auth::clear_credentials(&self.cfg.credentials_path()) {
            log::warn!(target: LOG, "clear credentials: {e}");
        }
    }

    fn revoked_status(&self) -> AgentStatus {
        statusio::read_status(&self.cfg.status_path()).unwrap_or_else(|| {
            status_from_creds(None, PairingState::Revoked, CloudState::Offline, None, None)
        })
    }

    /// Build ack / status callbacks — prefer ActionCable when subscribed, else REST.
    pub fn cloud_job_hooks(&self, device_token: &str, cable: Cable) -> (AckFn, StateFn) {
        let (client, token, cable2) =
            (self.client.clone(), device_token.to_string(), cable.clone());
        let ack: AckFn = Arc::new(move |job: &PrintJob| {
            if let Some(c) = &cable {
                if c.perform("ack_job", obj(json!({ "job_id": job.id }))) {
                    return Ok(());
                }
            }
            client.ack_job(&token, &job.id)?;
            Ok(())
        });
        let (client, token) = (self.client.clone(), device_token.to_string());
        let report: StateFn = Arc::new(
            move |job: &PrintJob, state: JobState, detail: Option<&str>| {
                // printing | delivered (lp handoff) | printed (CUPS complete) | error
                if let Some(c) = &cable2 {
                    let data = obj(
                        json!({ "job_id": job.id, "status": state.as_str(), "message": detail }),
                    );
                    if c.perform("job_status", data) {
                        return Ok(());
                    }
                }
                client.report_job_status(&token, &job.id, state.as_str(), detail)?;
                Ok(())
            },
        );
        (ack, report)
    }

    /// Run the post-update health gate if an OTA is pending (or stuck "failed").
    /// Returns the (possibly updated) update status and whether the gate ran.
    fn health_gate(
        &self,
        whoami: WhoamiResult,
        whoami_error: Option<&str>,
    ) -> (Option<UpdateStatus>, bool) {
        let Some(st) = update::read_update_status(&self.cfg.update_status_path()) else {
            return (None, false);
        };
        if st.status != update::STATUS_PENDING_HEALTH && st.status != update::STATUS_FAILED {
            return (Some(st), false);
        }
        let out = update::process_pending_health(
            st,
            &self.cfg,
            &self.update_env,
            whoami,
            whoami_error,
            None,
        );
        self.write_update_status(&out);
        (Some(out), true)
    }

    /// Single REST heartbeat cycle. Updates status file for the LCD.
    ///
    /// `jobs_busy`: when true, OTA download/install is deferred (job work in
    /// flight, e.g. buffered ActionCable jobs) so we never flip slots mid-print.
    pub fn run_once(&self, jobs_busy: bool) -> AgentStatus {
        let creds = auth::load_credentials(&self.cfg.credentials_path());

        // Promote sticky false "failed" (self-restart SIGTERM) to pending_health
        // *before* whoami so the LCD never paints red "Update failed" mid-OTA.
        if let Some(early) = update::read_update_status(&self.cfg.update_status_path()) {
            if early.status == update::STATUS_FAILED {
                let recovered =
                    update::recover_false_update_failure(early, &self.cfg, &self.update_env);
                if recovered.status == update::STATUS_PENDING_HEALTH {
                    self.write_update_status(&recovered);
                }
            }
        }

        let Some(mut creds) = creds else {
            // Unpaired: still complete post-update health (local slot checks only).
            self.health_gate(WhoamiResult::Skipped, None);
            if let Some(mut existing) = statusio::read_status(&self.cfg.status_path()) {
                if existing.pairing == PairingState::Revoked {
                    existing.cloud = CloudState::Offline;
                    existing.agent_version = Some(agent_version().into());
                    self.write_status(&mut existing);
                    return existing;
                }
            }
            let mut st = status_from_creds(
                None,
                PairingState::Unpaired,
                CloudState::Unknown,
                None,
                None,
            );
            self.write_status(&mut st);
            return st;
        };

        let (whoami, whoami_error) = match self.client.whoami(&creds.device_token) {
            Ok(who) => match auth::merge_whoami(&creds, &who) {
                Ok(merged) => {
                    creds = merged;
                    if let Err(e) = auth::save_credentials(&self.cfg.credentials_path(), &creds) {
                        log::warn!(target: LOG, "save credentials: {e}");
                    }
                    (WhoamiResult::Ok, None)
                }
                Err(e) => (WhoamiResult::Error, Some(e.to_string())),
            },
            Err(e) if e.unauthorized() => {
                // Still run health gate (API reached) before clearing credentials.
                self.health_gate(WhoamiResult::Unauthorized, Some(&e.message));
                self.handle_unauthorized(Some(&creds));
                return self.revoked_status();
            }
            Err(e) => {
                log::warn!(target: LOG, "whoami failed: {}", e.message);
                (WhoamiResult::Error, Some(e.message))
            }
        };

        // Post-update health gate: declare OTA success only after whoami.
        let (update_status, gate_ran) = self.health_gate(whoami, whoami_error.as_deref());
        if let Some(us) = update_status.as_ref().filter(|_| gate_ran) {
            if us.status == update::STATUS_ROLLED_BACK {
                log::warn!(target: LOG, "OTA health gate rolled back: {}", us.last_error.as_deref().unwrap_or(""));
                // Services restart after rollback; this process may be dying.
                let cloud = if whoami == WhoamiResult::Ok {
                    CloudState::Online
                } else {
                    CloudState::Offline
                };
                return status_from_creds(
                    Some(&creds),
                    PairingState::Paired,
                    cloud,
                    us.last_error.clone(),
                    None,
                );
            }
        }

        let body = HeartbeatBody {
            agent_version: Some(agent_version().into()),
            hostname: Some(sysinfo::hostname()),
            printers: self.printers(INVENTORY_FIRST_WAIT),
            platform: Some(default_platform()),
            update: update_status.as_ref().map(UpdateStatus::to_dict),
        };
        match self.client.heartbeat(&creds.device_token, &body) {
            Ok(hb) => {
                let last_hb = hb
                    .get("last_seen_at")
                    .filter(|v| crate::util::truthy(v))
                    .map(crate::util::py_str)
                    .unwrap_or_else(crate::util::utc_now_iso);
                let mut st = status_from_creds(
                    Some(&creds),
                    PairingState::Paired,
                    CloudState::Online,
                    None,
                    Some(last_hb),
                );
                self.write_status(&mut st);

                // OTA: desired version + optional update_url on heartbeat response.
                let ust = update::maybe_update_from_heartbeat(
                    &hb,
                    &self.cfg,
                    &self.update_env,
                    update_status,
                    Some(&self.cfg.update_status_path()),
                    jobs_busy,
                );
                self.write_update_status(&ust);
                st
            }
            Err(e) if e.unauthorized() => {
                self.handle_unauthorized(Some(&creds));
                self.revoked_status()
            }
            Err(e) => {
                log::warn!(target: LOG, "heartbeat failed: {}", e.message);
                let mut st = status_from_creds(
                    Some(&creds),
                    PairingState::Paired,
                    CloudState::Offline,
                    Some(e.message),
                    None,
                );
                if let Some(prev) = statusio::read_status(&self.cfg.status_path()) {
                    st.last_heartbeat_at = prev.last_heartbeat_at;
                }
                self.write_status(&mut st);
                st
            }
        }
    }

    /// on_wait_tick for long synchronous CUPS waits (out of paper, jam, etc.).
    ///
    /// A `WaitCups::Sync` job blocks the agent loop, so this keeps **REST
    /// heartbeats** flowing (what the web uses for last_seen / offline
    /// detection) and refreshes printer inventory over the cable when
    /// available. Every tick of this agent shares one schedule, inventory is
    /// the background snapshot (read only when something is due), and no lock
    /// is held while sending.
    pub fn inventory_wait_tick(&self, cable: Cable, device_token: Option<&str>) -> TickFn {
        // Match configured heartbeat (default 30s); never slower than 10s for liveness.
        let rest_interval = Duration::from_secs_f64((self.cfg.heartbeat_seconds as f64).max(10.0));
        let cable_interval = rest_interval.min(Duration::from_secs(15));
        let agent = self.clone();
        let token = device_token.filter(|t| !t.is_empty()).map(String::from);
        Arc::new(move || {
            let now = Instant::now();
            let due = |t: Option<Instant>, every: Duration| {
                t.is_none_or(|t| now.saturating_duration_since(t) >= every)
            };
            // Optional: keep ActionCable path warm (does not replace REST last_seen).
            let cable = cable.as_ref().filter(|c| c.subscribed());
            let (send_cable, send_rest) = {
                let mut last = lock(&agent.shared.wait_tick);
                let send_cable = cable.is_some() && due(last.cable, cable_interval);
                if send_cable {
                    last.cable = Some(now);
                }
                // Always REST-heartbeat on the normal schedule while blocked on CUPS.
                let send_rest = token.is_some() && due(last.rest, rest_interval);
                if send_rest {
                    last.rest = Some(now);
                }
                (send_cable, send_rest)
            };
            if !send_cable && !send_rest {
                return;
            }
            let inv = agent.printers(Duration::ZERO);
            if let Some(c) = cable.filter(|_| send_cable) {
                if let Some(inv) = &inv {
                    c.perform("report_printers", obj(json!({ "printers": inv })));
                }
                c.perform(
                    "heartbeat",
                    obj(json!({
                        "agent_version": agent_version(),
                        "hostname": sysinfo::hostname(),
                        "printers": inv,
                    })),
                );
            }
            if let Some(token) = token.as_deref().filter(|_| send_rest) {
                let body = HeartbeatBody {
                    agent_version: Some(agent_version().into()),
                    hostname: Some(sysinfo::hostname()),
                    printers: inv,
                    platform: Some(default_platform()),
                    update: None,
                };
                if let Err(e) = agent.client.heartbeat(token, &body) {
                    log::debug!(target: LOG, "wait-tick REST heartbeat failed: {e}");
                }
            }
        })
    }

    fn job_pipeline(&self, device_token: Option<&str>, cable: Cable) -> Pipeline {
        let mut p = self.pipeline.clone();
        if let Some(token) = device_token.filter(|t| !t.is_empty()) {
            let (ack, report) = self.cloud_job_hooks(token, cable.clone());
            p.ack = ack;
            p.report_state = report;
        }
        p.wait_cups = self.cfg.wait_cups;
        // Only a synchronous CUPS wait blocks this loop and needs the tick;
        // async jobs go to the pipeline's shared watcher while the loop runs on.
        p.on_wait_tick =
            (p.wait_cups == WaitCups::Sync).then(|| self.inventory_wait_tick(cable, device_token));
        p
    }

    /// Crash recovery: finish any jobs left in queue/*.json.
    pub fn drain_local_queue(&self, device_token: Option<&str>, cable: Cable) {
        let pending = self.store.list_queued_ids();
        if pending.is_empty() {
            return;
        }
        log::info!(target: LOG, "draining {} queued job(s)", pending.len());
        for (job_id, result) in self.job_pipeline(device_token, cable).drain(&self.store) {
            log::info!(target: LOG, "drain {job_id} → {result}");
        }
    }

    /// Pull pending jobs from the cloud and run the durable print pipeline.
    pub fn pull_and_process(&self, device_token: &str, cable: Cable) -> PullResult {
        let payloads = match self.client.pending_jobs(device_token) {
            Ok(p) => p,
            Err(e) => return classify_pull_error(&e),
        };
        if payloads.is_empty() {
            return PullResult::Ok;
        }
        log::info!(target: LOG, "pulled {} job(s)", payloads.len());
        let pipeline = self.job_pipeline(Some(device_token), cable);
        for payload in payloads {
            self.run_payload(&pipeline, &payload);
        }
        PullResult::Ok
    }

    /// Handle one job dict (from pull or ActionCable print_job).
    pub fn process_job_payload(&self, payload: &JsonObject, device_token: &str, cable: Cable) {
        let pipeline = self.job_pipeline(Some(device_token), cable);
        self.run_payload(&pipeline, payload);
    }

    fn run_payload(&self, pipeline: &Pipeline, payload: &JsonObject) {
        // A panic fails this job (a queue file stays for the next start), not
        // the agent and the rest of the batch.
        contained("print job", || self.handle_payload(pipeline, payload));
    }

    fn handle_payload(&self, pipeline: &Pipeline, payload: &JsonObject) {
        let job = match PrintJob::from_dict(payload) {
            Ok(j) => j,
            Err(e) => {
                log::error!(target: LOG, "skip invalid job payload: {}", e.message);
                if let Some(job) = rejected_job(payload) {
                    report_rejected(pipeline, &job, &e);
                }
                return;
            }
        };
        // Cloud jobs carry their content; a local file path is CLI-only.
        if let Err(e) = jobs::check_remote_job(&job) {
            log::error!(target: LOG, "job {} rejected: {}", job.id, e.message);
            report_rejected(pipeline, &job, &e);
            return;
        }
        if let Err(e) = pipeline.process(&job, &self.store) {
            log::error!(target: LOG, "job {} failed: {}", job.id, e.message);
        }
    }

    /// Long-running agent: REST heartbeat/pull + optional ActionCable push.
    /// Returns when `stop` becomes true.
    pub fn run(&self, stop: Arc<AtomicBool>) {
        let cfg = &self.cfg;
        if let Err(e) = cfg.ensure_dirs() {
            log::warn!(target: LOG, "ensure dirs: {e}");
        }
        log::info!(
            target: LOG,
            "agent {} starting api_base_url={} cable={} pull_jobs={} heartbeat={}s pull_interval={}s",
            agent_version(),
            cfg.api_base_url,
            cfg.cable_enabled,
            cfg.pull_jobs_enabled,
            cfg.heartbeat_seconds,
            cfg.pull_interval_seconds
        );

        self.recover_interrupted_update();
        // CUPS inventory runs off this loop from here on.
        let inventory_refresher = self.start_inventory_refresher(&stop);

        // --- ActionCable session (push) ------------------------------------
        let push_jobs: Arc<Mutex<VecDeque<JsonObject>>> = Arc::default();
        let revoke_flag = Arc::new(AtomicBool::new(false));
        // Set by on_subscribed; resets the cable retry pacing.
        let cable_subscribed = Arc::new(AtomicBool::new(false));
        let holder: Arc<Mutex<Option<Arc<PrintCableSession>>>> = Arc::default();
        let current_cable = |h: &Mutex<Option<Arc<PrintCableSession>>>| -> Cable {
            lock(h).clone().map(|s| s as Arc<dyn CableChannel>)
        };

        // The session to use, and whether a new one was started (an attempt,
        // for the retry pacing).
        let ensure_cable = |agent: &Agent| -> (Option<Arc<PrintCableSession>>, bool) {
            let dead = {
                let mut slot = lock(&holder);
                if let Some(s) = slot.as_ref() {
                    // Subscribed, or handshake in progress — leave it alone.
                    // (Python only checked `connected`, which is set on `welcome`,
                    // so a slow loop iteration could tear down a session that
                    // was still connecting.)
                    if s.subscribed() || s.connected() || s.connecting() {
                        return (Some(s.clone()), false);
                    }
                }
                slot.take()
            };
            // Dead/failed session — tear down (outside the lock its handlers use).
            if let Some(old) = dead {
                old.stop();
            }
            let handlers = agent.session_handlers(
                push_jobs.clone(),
                revoke_flag.clone(),
                holder.clone(),
                cable_subscribed.clone(),
            );
            let (client, cred_path) = (agent.client.clone(), agent.cfg.credentials_path());
            let sess = Arc::new(PrintCableSession::new(
                &agent.cfg.cable_url,
                Box::new(move || {
                    let creds = auth::load_credentials(&cred_path).ok_or("no credentials")?;
                    Ok(client.ws_ticket(&creds.device_token)?)
                }),
                handlers,
            ));
            if sess.start() {
                *lock(&holder) = Some(sess.clone());
                return (Some(sess), true);
            }
            (None, true)
        };
        let stop_cable = || {
            let old = lock(&holder).take();
            if let Some(s) = old {
                s.stop();
            }
        };

        let token = auth::load_credentials(&cfg.credentials_path()).map(|c| c.device_token);
        self.drain_local_queue(token.as_deref(), None);
        // After the drain: it relies on markers of jobs still queued.
        self.prune_processed_markers();
        let mut last_prune = Instant::now();

        let secs = |s: f64| Duration::from_secs_f64(s.max(0.0));
        let hb_interval = secs(cfg.heartbeat_seconds.max(5) as f64);
        let pull_interval = secs(cfg.pull_interval_seconds.max(1) as f64);
        // When cable is healthy, pull less often (safety net only).
        let pull_interval_when_cabled = (pull_interval * 6).max(secs(30.0));
        // Channel heartbeat + inventory more often than REST so admin stays fresh.
        let cable_hb_interval = hb_interval.min(secs(15.0)).max(secs(10.0));
        let max_backoff = 60.0f64;
        let mut backoff = 1.0f64;
        let mut last_hb: Option<Instant> = None;
        let mut last_pull: Option<Instant> = None;
        let mut last_cable_hb: Option<Instant> = None;
        let mut pull_disabled_until: Option<Instant> = None;
        let mut cable_retry = CableRetry::default();
        let elapsed_since =
            |t: Option<Instant>, every: Duration| t.is_none_or(|t| t.elapsed() >= every);

        while !stop.load(Ordering::SeqCst) {
            let cycle_start = Instant::now();
            let mut creds = auth::load_credentials(&cfg.credentials_path());
            let mut sess = lock(&holder).clone();

            if revoke_flag.swap(false, Ordering::SeqCst) {
                stop_cable();
                if creds.is_some() {
                    self.handle_unauthorized(creds.as_ref());
                }
                creds = None;
                sess = None;
            }

            // Pause pull/push while OTA is downloading, installing, or in health gate.
            let mut ota_pause = update::should_pause_jobs_from_path(&cfg.update_status_path());

            // Drain push jobs on the main thread (lp must not run on the WS thread).
            // Skip while OTA is active so we never start a print a restart would kill.
            if !ota_pause {
                loop {
                    let next = lock(&push_jobs).pop_front();
                    let Some(payload) = next else { break };
                    if let Some(c) = &creds {
                        self.process_job_payload(&payload, &c.device_token, current_cable(&holder));
                    }
                }
            } else {
                let held = lock(&push_jobs).len();
                if held > 0 {
                    log::debug!(target: LOG, "OTA in progress — holding {held} ActionCable job(s)");
                }
            }

            // REST heartbeat first — cable must never block liveness / LCD status.
            // Defer OTA only for job work in flight. Jobs run synchronously on
            // this thread, so that is the buffered push jobs; leftover queue
            // files (retryable failures) must not hold off every OTA.
            let jobs_busy = !lock(&push_jobs).is_empty();
            let st = if elapsed_since(last_hb, hb_interval) {
                let st = self.heartbeat_step(|| self.run_once(jobs_busy));
                last_hb = Some(Instant::now());
                // Re-read: an OTA may have activated (pending_health) or failed.
                ota_pause = update::should_pause_jobs_from_path(&cfg.update_status_path());
                match &st {
                    Some(s) if s.cloud == CloudState::Online => backoff = 1.0,
                    Some(s)
                        if s.pairing == PairingState::Paired && s.cloud == CloudState::Offline =>
                    {
                        backoff = (backoff.max(1.0) * 2.0).min(max_backoff);
                    }
                    _ => {}
                }
                st.or_else(|| statusio::read_status(&cfg.status_path()))
            } else {
                statusio::read_status(&cfg.status_path())
            };

            // Maintain cable in the background when paired (non-blocking).
            if creds.is_some() && cfg.cable_enabled {
                if cable_subscribed.swap(false, Ordering::SeqCst) {
                    cable_retry.subscribed(Instant::now());
                }
                let need = sess.as_ref().is_none_or(|s| !s.connected());
                if need && cable_retry.due(Instant::now()) {
                    // (A panic in the ticket request counts as a failed attempt.)
                    let (s, attempted) =
                        contained("cable connect", || ensure_cable(self)).unwrap_or((None, true));
                    sess = s;
                    if attempted {
                        // Pace from the attempt itself, not the cycle start:
                        // run_once can take a while on a Pi.
                        cable_retry.attempted(Instant::now());
                    }
                } else if let Some(s) = sess.as_ref().filter(|s| s.subscribed()) {
                    backoff = 1.0;
                    if elapsed_since(last_cable_hb, cable_hb_interval) {
                        let data = obj(json!({
                            "agent_version": agent_version(),
                            "hostname": sysinfo::hostname(),
                            "printers": self.printers(Duration::ZERO),
                        }));
                        if s.perform("heartbeat", data) {
                            last_cable_hb = Some(Instant::now());
                        }
                    }
                }
            } else if sess.take().is_some() {
                stop_cable();
            }

            // REST pull safety net (paused during OTA install / health gate).
            let subscribed = sess.as_ref().is_some_and(|s| s.subscribed());
            let pull_every = if subscribed {
                pull_interval_when_cabled
            } else {
                pull_interval
            };
            if ota_pause {
                if cfg.pull_jobs_enabled && creds.is_some() {
                    log::debug!(target: LOG, "OTA in progress — pausing job pull");
                }
            } else if let Some(c) = creds.as_ref().filter(|_| cfg.pull_jobs_enabled) {
                let paired = st
                    .as_ref()
                    .is_none_or(|s| s.pairing == PairingState::Paired);
                let enabled = pull_disabled_until.is_none_or(|t| Instant::now() >= t);
                if paired && enabled && elapsed_since(last_pull, pull_every) {
                    // Stamp before the pull, at the cycle start: cycles start
                    // pull_interval apart, so the next pull is due next cycle
                    // (stamping after the pull skipped every other cycle).
                    // After a long cycle (the first inventory, a slow
                    // heartbeat) still keep half an interval between pulls.
                    let started = Instant::now();
                    last_pull = Some(
                        started
                            .checked_sub(pull_every / 2)
                            .map_or(cycle_start, |t| t.max(cycle_start)),
                    );
                    let result = contained("job pull", || {
                        self.pull_and_process(&c.device_token, current_cable(&holder))
                    })
                    .unwrap_or(PullResult::Error);
                    match result {
                        PullResult::Unauthorized => {
                            self.handle_unauthorized(Some(c));
                            stop_cable();
                        }
                        PullResult::Unavailable => {
                            pull_disabled_until =
                                Some(Instant::now() + secs((backoff * 5.0).clamp(15.0, 60.0)));
                        }
                        PullResult::Error => {
                            pull_disabled_until = Some(Instant::now() + secs(backoff.min(30.0)));
                        }
                        PullResult::Ok => {}
                    }
                }
            }

            if last_prune.elapsed() >= PRUNE_EVERY {
                self.prune_processed_markers();
                last_prune = Instant::now();
            }

            // Sleep
            let mut sleep_for = if !lock(&push_jobs).is_empty() {
                secs(0.05)
            } else {
                match &st {
                    Some(s) if s.pairing == PairingState::Unpaired => hb_interval.min(secs(10.0)),
                    Some(s) if s.pairing == PairingState::Revoked => hb_interval.min(secs(30.0)),
                    // Reconnect quickly after failures; still respect backoff cap.
                    Some(s) if s.cloud == CloudState::Offline => {
                        secs(backoff.max(5.0).min(max_backoff))
                    }
                    _ if cfg.pull_jobs_enabled && creds.is_some() => {
                        if subscribed {
                            pull_interval.min(secs(1.0))
                        } else {
                            pull_interval
                        }
                    }
                    _ => hb_interval,
                }
            };
            if let Some(t) = last_hb {
                sleep_for = sleep_for.min(hb_interval.saturating_sub(t.elapsed()));
            }
            sleep_until(cycle_start + sleep_for, &stop);
        }

        stop_cable();
        self.shared.inventory.end(inventory_refresher);
        log::info!(target: LOG, "agent stopped");
    }

    fn session_handlers(
        &self,
        push_jobs: Arc<Mutex<VecDeque<JsonObject>>>,
        revoke_flag: Arc<AtomicBool>,
        holder: Arc<Mutex<Option<Arc<PrintCableSession>>>>,
        subscribed_flag: Arc<AtomicBool>,
    ) -> SessionHandlers {
        let store = self.store.clone();
        let cfg = self.cfg.clone();
        let (shared, inventory) = (self.shared.clone(), self.inventory.clone());
        SessionHandlers {
            on_print_job: Some(Arc::new(move |job| lock(&push_jobs).push_back(job))),
            on_revoke: Some(Arc::new(move || revoke_flag.store(true, Ordering::SeqCst))),
            on_job_canceled: Some(Arc::new(move |job_id| {
                if let Err(e) = handle_job_canceled(&job_id, &store) {
                    log::error!(target: LOG, "job_canceled handler failed: {e}");
                }
            })),
            on_node_config: Some(Arc::new(move |msg| apply_node_config(&cfg, &msg))),
            on_subscribed: Some(Arc::new(move || {
                subscribed_flag.store(true, Ordering::SeqCst);
                log::info!(target: LOG, "cable PrintNodeChannel ready");
                // Push CUPS inventory so admin sees printers promptly (the
                // snapshot: never run CUPS queries on the socket thread).
                let sess = lock(&holder).clone();
                if let (Some(s), Some(inv)) =
                    (sess, shared.inventory.get(&inventory, Duration::ZERO))
                {
                    s.perform("report_printers", obj(json!({ "printers": inv })));
                }
            })),
            on_disconnected: Some(Arc::new(|| log::info!(target: LOG, "cable disconnected"))),
        }
    }
}

fn classify_pull_error(e: &CloudError) -> PullResult {
    if e.unauthorized() {
        return PullResult::Unauthorized;
    }
    if e.not_found() || e.service_disabled() {
        log::warn!(
            target: LOG,
            "jobs/pending unavailable (HTTP {}): {} — pull will retry",
            e.status,
            e.message
        );
        return PullResult::Unavailable;
    }
    log::warn!(target: LOG, "jobs/pending failed: {}", e.message);
    PullResult::Error
}

/// Apply ActionCable node_config (warehouses/name) to creds + LCD status.
fn apply_node_config(cfg: &Config, msg: &JsonObject) {
    let Some(creds) = auth::load_credentials(&cfg.credentials_path()) else {
        return;
    };
    let updated = match auth::merge_whoami(&creds, msg) {
        Ok(u) => u,
        Err(e) => {
            log::error!(target: LOG, "failed to apply node_config: {e}");
            return;
        }
    };
    if let Err(e) = auth::save_credentials(&cfg.credentials_path(), &updated) {
        log::error!(target: LOG, "failed to apply node_config: {e}");
        return;
    }
    let mut st = status_from_creds(
        Some(&updated),
        PairingState::Paired,
        CloudState::Online,
        None,
        None,
    );
    if let Some(prev) = statusio::read_status(&cfg.status_path()) {
        st.last_heartbeat_at = prev.last_heartbeat_at;
    }
    if let Err(e) = statusio::write_status(&cfg.status_path(), &mut st) {
        log::warn!(target: LOG, "write status: {e}");
    }
    log::info!(
        target: LOG,
        "node_config applied warehouses={} name={}",
        updated.warehouse_label(),
        updated.name.as_deref().unwrap_or("")
    );
}

/// Sleep until `deadline`, waking early if `stop` is set.
fn sleep_until(deadline: Instant, stop: &AtomicBool) {
    while !stop.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        thread::sleep((deadline - now).min(Duration::from_millis(100)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::CupsOutcome;
    use crate::testutil::serve;
    use std::fs;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::path::Path;
    use std::sync::atomic::AtomicUsize;
    use std::sync::OnceLock;
    use std::time::SystemTime;

    const CLAIM: &str = r#"{
        "node_id": "node-uuid-1",
        "device_token": "secret-device-token-do-not-log",
        "name": "Pack station 1",
        "warehouse": {"id": "wh-1", "name": "Main Warehouse", "code": "MAIN"},
        "organization": {"id": "org-1", "name": "Acme Corp", "slug": "acme"}
    }"#;

    const WHOAMI: &str = r#"{"node_id":"node-uuid-1","name":"Pack station 1"}"#;

    const PNG_1X1_B64: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

    fn test_agent(td: &Path, base_url: &str) -> Agent {
        let cfg = Config {
            api_base_url: base_url.into(),
            config_dir: td.join("cfg"),
            state_dir: td.join("state"),
            heartbeat_seconds: 30,
            ..Config::default()
        }
        .normalized();
        cfg.ensure_dirs().unwrap();
        Agent {
            client: CloudClient::new(base_url),
            store: JobStore::from_config(&cfg),
            inventory: Arc::new(|| Some(Vec::new())),
            update_env: UpdateEnv {
                install_root: td.join("install"),
                apply_helper: None,
                running_version: agent_version().into(),
                running_from_slot: false,
                restart: false,
            },
            pipeline: Pipeline {
                lp: Arc::new(|_, _, _| Ok(None)),
                supports_raw: Arc::new(|_| Ok(false)),
                wait_cups_job: Arc::new(|_, _| CupsOutcome::Printed),
                ..Pipeline::default()
            },
            shared: Arc::default(),
            cfg,
        }
    }

    fn pair(agent: &Agent) -> Credentials {
        let creds =
            auth::credentials_from_pair_response(&serde_json::from_str(CLAIM).unwrap()).unwrap();
        auth::save_credentials(&agent.cfg.credentials_path(), &creds).unwrap();
        creds
    }

    fn pending_job() -> String {
        json!({"jobs": [{
            "id": "job-uuid-1", "printer_id": "printer-1", "cups_name": "Label_1",
            "content_type": "png_base64", "content": PNG_1X1_B64, "title": "Ship label",
            "options": {"copies": 1}, "status": "sent",
        }]})
        .to_string()
    }

    /// Run the agent loop on a thread for `how_long`, then stop it.
    fn run_for(agent: &Agent, how_long: Duration) {
        let stop = Arc::new(AtomicBool::new(false));
        let (a, s) = (agent.clone(), stop.clone());
        let handle = thread::spawn(move || a.run(s));
        thread::sleep(how_long);
        stop.store(true, Ordering::SeqCst);
        handle.join().unwrap();
    }

    type Hits = Arc<Mutex<Vec<(String, Instant)>>>;

    /// Loopback HTTP stub answering any number of requests through
    /// `handler(method, path) -> (status, body)`; records each path and when
    /// it arrived.
    struct Stub {
        base_url: String,
        hits: Hits,
        stop: Arc<AtomicBool>,
    }

    impl Stub {
        fn count(&self, path: &str) -> usize {
            self.times(path).len()
        }

        fn times(&self, path: &str) -> Vec<Instant> {
            let hits = self.hits.lock().unwrap();
            hits.iter()
                .filter(|(p, _)| p == path)
                .map(|(_, t)| *t)
                .collect()
        }
    }

    impl Drop for Stub {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
        }
    }

    fn stub(handler: impl Fn(&str, &str) -> (u16, String) + Send + 'static) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let hits: Hits = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (h, s) = (hits.clone(), stop.clone());
        thread::spawn(move || {
            while !s.load(Ordering::SeqCst) {
                let Ok((stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap_or_default().to_string();
                let path = parts.next().unwrap_or_default().to_string();
                let mut len = 0usize;
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 {
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
                let _ = reader.read_exact(&mut body);
                h.lock().unwrap().push((path.clone(), Instant::now()));
                let (status, resp) = handler(&method, &path);
                let mut stream = stream;
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp}",
                    resp.len()
                );
            }
        });
        Stub {
            base_url,
            hits,
            stop,
        }
    }

    #[test]
    fn unpaired_writes_status() {
        let td = tempfile::tempdir().unwrap();
        let agent = test_agent(td.path(), "http://127.0.0.1:9");
        let st = agent.run_once(false);
        assert_eq!(st.pairing, PairingState::Unpaired);
        assert_eq!(
            statusio::read_status(&agent.cfg.status_path())
                .unwrap()
                .pairing,
            PairingState::Unpaired
        );
    }

    #[test]
    fn heartbeat_online() {
        let td = tempfile::tempdir().unwrap();
        let srv = serve(vec![
            (
                200,
                r#"{"node_id":"node-uuid-1","name":"Pack station 1","organization":{"name":"Acme Corp"},"warehouse":{"name":"Main Warehouse"}}"#,
            ),
            (
                200,
                r#"{"ok":true,"status":"online","last_seen_at":"2026-07-15T12:00:00Z"}"#,
            ),
        ]);
        let agent = test_agent(td.path(), &srv.base_url);
        pair(&agent);
        let st = agent.run_once(false);
        assert_eq!(st.pairing, PairingState::Paired);
        assert_eq!(st.cloud, CloudState::Online);
        assert_eq!(st.organization_name.as_deref(), Some("Acme Corp"));
        assert_eq!(
            st.last_heartbeat_at.as_deref(),
            Some("2026-07-15T12:00:00Z")
        );
        let reqs = srv.requests.lock().unwrap();
        assert_eq!(reqs[1].path, "/print/v1/heartbeat");
        let body: Value = serde_json::from_slice(&reqs[1].body).unwrap();
        assert_eq!(body["agent_version"], agent_version());
        assert_eq!(body["printers"], json!([]));
        // No OTA directive → update status written as idle.
        let ust = update::read_update_status(&agent.cfg.update_status_path()).unwrap();
        assert_eq!(ust.status, update::STATUS_IDLE);
    }

    #[test]
    fn health_gate_clears_pending_update_after_whoami() {
        let td = tempfile::tempdir().unwrap();
        let srv = serve(vec![
            (200, r#"{"node_id":"node-uuid-1"}"#),
            (200, r#"{"ok":true}"#),
        ]);
        let agent = test_agent(td.path(), &srv.base_url);
        pair(&agent);
        // Active slot for the running version, as after an OTA restart.
        let ver = agent_version();
        let slot = agent.update_env.install_root.join("releases").join(ver);
        std::fs::create_dir_all(&slot).unwrap();
        std::fs::write(slot.join("vesyl-print"), b"bin").unwrap();
        update::flip_current(&agent.update_env.install_root, ver).unwrap();
        let mut pending = UpdateStatus::default();
        update::mark_pending_health(&mut pending, ver, Some("0.0.1".into()), 120, None);
        update::write_update_status(&agent.cfg.update_status_path(), &pending).unwrap();
        assert!(update::should_pause_jobs_from_path(
            &agent.cfg.update_status_path()
        ));

        let st = agent.run_once(false);
        assert_eq!(st.cloud, CloudState::Online);
        let ust = update::read_update_status(&agent.cfg.update_status_path()).unwrap();
        assert_eq!(ust.status, update::STATUS_IDLE);
        assert!(ust.previous_version.is_none());
        // Heartbeat carried the update block.
        let body: Value = serde_json::from_slice(&srv.requests.lock().unwrap()[1].body).unwrap();
        assert_eq!(body["update"]["status"], "idle");
    }

    #[test]
    fn heartbeat_failure_keeps_last_seen() {
        let td = tempfile::tempdir().unwrap();
        let srv = serve(vec![
            (200, r#"{"node_id":"node-uuid-1"}"#),
            (500, r#"{"error":"boom"}"#),
        ]);
        let agent = test_agent(td.path(), &srv.base_url);
        pair(&agent);
        let mut prev = AgentStatus {
            last_heartbeat_at: Some("2026-07-15T11:00:00+00:00".into()),
            ..Default::default()
        };
        statusio::write_status(&agent.cfg.status_path(), &mut prev).unwrap();
        let st = agent.run_once(false);
        assert_eq!(st.cloud, CloudState::Offline);
        assert_eq!(st.last_error.as_deref(), Some("boom"));
        assert_eq!(
            st.last_heartbeat_at.as_deref(),
            Some("2026-07-15T11:00:00+00:00")
        );
    }

    #[test]
    fn unauthorized_clears_credentials_and_marks_revoked() {
        let td = tempfile::tempdir().unwrap();
        let srv = serve(vec![(
            401,
            r#"{"error":{"code":"unauthorized","message":"invalid"}}"#,
        )]);
        let agent = test_agent(td.path(), &srv.base_url);
        pair(&agent);
        let st = agent.run_once(false);
        assert_eq!(st.pairing, PairingState::Revoked);
        assert!(auth::load_credentials(&agent.cfg.credentials_path()).is_none());
        assert_eq!(
            statusio::read_status(&agent.cfg.status_path())
                .unwrap()
                .pairing,
            PairingState::Revoked
        );
        // Next cycle stays revoked (no auto-reclaim).
        assert_eq!(agent.run_once(false).pairing, PairingState::Revoked);
    }

    #[test]
    fn pull_ordering_through_rest_hooks() {
        let td = tempfile::tempdir().unwrap();
        let srv = serve(vec![
            (200, Box::leak(pending_job().into_boxed_str())),
            (200, "{}"),
            (200, "{}"),
            (200, "{}"),
        ]);
        let agent = test_agent(td.path(), &srv.base_url);
        assert_eq!(agent.pull_and_process("tok", None), PullResult::Ok);
        let reqs = srv.requests.lock().unwrap();
        let paths: Vec<&str> = reqs.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "/print/v1/jobs/pending",
                "/print/v1/jobs/job-uuid-1/ack",
                "/print/v1/jobs/job-uuid-1/status",
                "/print/v1/jobs/job-uuid-1/status",
            ]
        );
        let statuses: Vec<String> = reqs[2..]
            .iter()
            .map(|r| {
                serde_json::from_slice::<Value>(&r.body).unwrap()["status"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(statuses, ["printing", "delivered"]);
        assert!(agent.store.is_processed("job-uuid-1"));
        assert!(!agent.store.has_queue_file("job-uuid-1"));
    }

    #[test]
    fn pull_error_classification() {
        let td = tempfile::tempdir().unwrap();
        let srv = serve(vec![
            (404, r#"{"error":"not found"}"#),
            (503, "{}"),
            (401, r#"{"error":{"code":"unauthorized"}}"#),
            (500, "{}"),
        ]);
        let agent = test_agent(td.path(), &srv.base_url);
        assert_eq!(agent.pull_and_process("tok", None), PullResult::Unavailable);
        assert_eq!(agent.pull_and_process("tok", None), PullResult::Unavailable);
        assert_eq!(
            agent.pull_and_process("tok", None),
            PullResult::Unauthorized
        );
        assert_eq!(agent.pull_and_process("tok", None), PullResult::Error);
    }

    #[test]
    fn already_processed_reports_printed_without_reprint() {
        let td = tempfile::tempdir().unwrap();
        let srv = serve(vec![
            (200, Box::leak(pending_job().into_boxed_str())),
            (200, "{}"),
        ]);
        let mut agent = test_agent(td.path(), &srv.base_url);
        agent.pipeline.lp = Arc::new(|_, _, _| panic!("must not reprint"));
        agent.store.mark_processed("job-uuid-1").unwrap();
        assert_eq!(agent.pull_and_process("tok", None), PullResult::Ok);
        let reqs = srv.requests.lock().unwrap();
        assert_eq!(reqs.len(), 2, "no ack, one status");
        let body: Value = serde_json::from_slice(&reqs[1].body).unwrap();
        assert_eq!(body["status"], "printed");
    }

    /// A cloud job may not point at a local file (credentials.json would print):
    /// it is reported failed and never queued, acked or printed.
    #[test]
    fn cloud_local_path_jobs_are_rejected_before_queueing() {
        let td = tempfile::tempdir().unwrap();
        let pending = json!({"jobs": [{
            "id": "x1", "cups_name": "Label_1", "content_type": "local_path",
            "content": "/etc/vesyl-print/credentials.json",
        }]})
        .to_string();
        let srv = serve(vec![
            (200, Box::leak(pending.into_boxed_str())),
            (200, "{}"),
        ]);
        let mut agent = test_agent(td.path(), &srv.base_url);
        agent.pipeline.lp = Arc::new(|_, _, _| panic!("must not print"));
        assert_eq!(agent.pull_and_process("tok", None), PullResult::Ok);
        let reqs = srv.requests.lock().unwrap();
        let paths: Vec<&str> = reqs.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(
            paths,
            ["/print/v1/jobs/pending", "/print/v1/jobs/x1/status"]
        );
        let body: Value = serde_json::from_slice(&reqs[1].body).unwrap();
        assert_eq!(body["status"], "error");
        assert!(body["message"].as_str().unwrap().contains("local_path"));
        assert!(agent.store.list_queued_ids().is_empty());
        assert!(!agent.store.is_processed("x1"));

        // Same over ActionCable push.
        let cable = Arc::new(FakeCable {
            ok: true,
            calls: Mutex::default(),
        });
        let payload = obj(
            json!({"id": "x2", "cups_name": "P", "content_type": "LOCAL_PATH",
                                 "content": "/etc/vesyl-print/credentials.json"}),
        );
        agent.process_job_payload(&payload, "tok", Some(cable.clone()));
        let calls = cable.calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "no ack_job, one job_status");
        assert_eq!(calls[0].0, "job_status");
        assert_eq!(
            (calls[0].1["job_id"].as_str(), calls[0].1["status"].as_str()),
            (Some("x2"), Some("error"))
        );
        assert!(agent.store.list_queued_ids().is_empty());
    }

    /// Invalid payloads never reach the queue; ones we can name are reported
    /// failed so the cloud doesn't wait on them, path-like ids are dropped.
    #[test]
    fn invalid_cloud_payloads_never_reach_the_queue() {
        let td = tempfile::tempdir().unwrap();
        let mut agent = test_agent(td.path(), "http://127.0.0.1:9");
        agent.pipeline.lp = Arc::new(|_, _, _| panic!("must not print"));
        let cable = Arc::new(FakeCable {
            ok: true,
            calls: Mutex::default(),
        });
        for id in ["c/d", "../../etc/victim"] {
            let payload = obj(
                json!({"id": id, "cups_name": "P", "content_type": "png_base64",
                                     "content": PNG_1X1_B64}),
            );
            agent.process_job_payload(&payload, "tok", Some(cable.clone()));
        }
        assert!(cable.calls.lock().unwrap().is_empty(), "no ack, no status");
        assert!(!agent.store.queue_dir.join("c").exists());
        assert!(agent.store.list_queued_ids().is_empty());

        let payload = obj(json!({"id": "j-empty", "cups_name": "P", "content_type": "png_base64"}));
        agent.process_job_payload(&payload, "tok", Some(cable.clone()));
        let calls = cable.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "job_status");
        assert_eq!(calls[0].1["status"], "error");
        assert_eq!(calls[0].1["message"], "job missing content");
    }

    /// A panic in one job (e.g. the OS refusing a thread) must not take the
    /// rest of a pulled batch down with it.
    #[test]
    fn panicking_job_does_not_stop_the_batch() {
        let td = tempfile::tempdir().unwrap();
        let pending = json!({"jobs": [
            {"id": "p1", "cups_name": "P", "content_type": "png_base64", "content": PNG_1X1_B64},
            {"id": "p2", "cups_name": "P", "content_type": "png_base64", "content": PNG_1X1_B64},
        ]})
        .to_string();
        let srv = stub(move |_, path| {
            if path == "/print/v1/jobs/pending" {
                (200, pending.clone())
            } else {
                (200, "{}".into())
            }
        });
        let mut agent = test_agent(td.path(), &srv.base_url);
        let content: Arc<Mutex<Option<std::path::PathBuf>>> = Arc::default();
        let c = content.clone();
        agent.pipeline.lp = Arc::new(move |_, path, _| {
            if path.file_stem().is_some_and(|s| s == "p1") {
                *c.lock().unwrap() = Some(path.to_path_buf());
                panic!("failed to spawn thread");
            }
            Ok(None)
        });
        let cable = Arc::new(FakeCable {
            ok: true,
            calls: Mutex::default(),
        });
        assert_eq!(
            agent.pull_and_process("tok", Some(cable.clone())),
            PullResult::Ok
        );
        assert!(agent.store.is_processed("p2"));
        // p1 is reported failed, not left at printing...
        let p1: Vec<Value> = cable
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(action, data)| action == "job_status" && data["job_id"] == "p1")
            .map(|(_, data)| json!([data["status"], data["message"]]))
            .collect();
        assert_eq!(
            p1,
            [
                json!(["printing", null]),
                json!(["error", "job panicked: failed to spawn thread"]),
            ]
        );
        // ...its temp content and vesyl-print-* dir are gone...
        let content = content.lock().unwrap().clone().expect("lp saw p1");
        let dir = content.parent().unwrap();
        assert!(dir
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("vesyl-print-")));
        assert!(!content.exists() && !dir.exists(), "{}", dir.display());
        // ...and, its outcome unknown, it stays queued for the next start.
        assert_eq!(agent.store.list_queued_ids(), ["p1"]);
    }

    struct FakeCable {
        ok: bool,
        calls: Mutex<Vec<(String, JsonObject)>>,
    }

    impl CableChannel for FakeCable {
        fn perform(&self, action: &str, data: JsonObject) -> bool {
            self.calls.lock().unwrap().push((action.into(), data));
            self.ok
        }

        fn subscribed(&self) -> bool {
            self.ok
        }
    }

    fn sample_job() -> PrintJob {
        PrintJob::from_dict(&obj(
            json!({"id": "j1", "cups_name": "P", "content_type": "png_base64", "content": "AA=="}),
        ))
        .unwrap()
    }

    #[test]
    fn hooks_prefer_cable_when_subscribed() {
        let td = tempfile::tempdir().unwrap();
        // No server: any REST call would fail the hook.
        let agent = test_agent(td.path(), "http://127.0.0.1:9");
        let cable = Arc::new(FakeCable {
            ok: true,
            calls: Mutex::default(),
        });
        let (ack, report) = agent.cloud_job_hooks("tok", Some(cable.clone()));
        let job = sample_job();
        ack(&job).unwrap();
        report(&job, JobState::Delivered, None).unwrap();
        let calls = cable.calls.lock().unwrap();
        assert_eq!(calls[0].0, "ack_job");
        assert_eq!(calls[0].1["job_id"], "j1");
        assert_eq!(calls[1].0, "job_status");
        assert_eq!(calls[1].1["status"], "delivered");
    }

    #[test]
    fn hooks_fall_back_to_rest() {
        let td = tempfile::tempdir().unwrap();
        let srv = serve(vec![(200, "{}"), (200, "{}")]);
        let agent = test_agent(td.path(), &srv.base_url);
        let cable = Arc::new(FakeCable {
            ok: false,
            calls: Mutex::default(),
        });
        let (ack, report) = agent.cloud_job_hooks("tok", Some(cable));
        let job = sample_job();
        ack(&job).unwrap();
        report(&job, JobState::Error, Some("nope")).unwrap();
        let reqs = srv.requests.lock().unwrap();
        assert_eq!(reqs[0].path, "/print/v1/jobs/j1/ack");
        assert_eq!(reqs[0].header("Authorization"), Some("Bearer tok"));
        let body: Value = serde_json::from_slice(&reqs[1].body).unwrap();
        assert_eq!(body, json!({"status": "error", "message": "nope"}));
    }

    #[test]
    fn job_canceled_drops_queue_and_marks_processed() {
        let td = tempfile::tempdir().unwrap();
        let store = JobStore::new(td.path().join("q"), td.path().join("p"));
        store.write_queue(&PrintJob::from_dict(&obj(json!({"id": "c1", "cups_name": "P", "content_type": "local_path", "content": "/tmp/x"}))).unwrap()).unwrap();
        handle_job_canceled("c1", &store).unwrap();
        assert!(!store.has_queue_file("c1"));
        assert!(store.is_processed("c1"));
    }

    /// A job_canceled id must never reach outside queue/ or processed/
    /// (it used to delete /etc/vesyl-print/credentials.json).
    #[test]
    fn job_canceled_ignores_path_like_ids() {
        let td = tempfile::tempdir().unwrap();
        let state = td.path().join("var/lib/vesyl-print");
        let store = JobStore::new(state.join("queue"), state.join("processed"));
        store.ensure().unwrap();
        let etc = td.path().join("etc/vesyl-print");
        fs::create_dir_all(&etc).unwrap();
        fs::write(etc.join("credentials.json"), "{}").unwrap();
        handle_job_canceled("../../../../etc/vesyl-print/credentials", &store).unwrap();
        assert!(etc.join("credentials.json").is_file());
        assert!(!etc.join("credentials").exists(), "no stray marker");
        let mut names: Vec<_> = fs::read_dir(&etc)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, ["credentials.json"]);
    }

    #[test]
    fn node_config_updates_credentials_and_status() {
        let td = tempfile::tempdir().unwrap();
        let agent = test_agent(td.path(), "http://127.0.0.1:9");
        let creds = pair(&agent);
        apply_node_config(
            &agent.cfg,
            &obj(
                json!({"type": "node_config", "node_id": "node-uuid-1", "name": "Pack 2",
                        "warehouses": [{"code": "DFD"}, {"code": "MAIN"}]}),
            ),
        );
        let updated = auth::load_credentials(&agent.cfg.credentials_path()).unwrap();
        assert_eq!(updated.device_token, creds.device_token);
        assert_eq!(updated.name.as_deref(), Some("Pack 2"));
        let st = statusio::read_status(&agent.cfg.status_path()).unwrap();
        assert_eq!(st.warehouse_name.as_deref(), Some("DFD, MAIN"));
    }

    #[test]
    fn drain_recovers_queue_on_start() {
        let td = tempfile::tempdir().unwrap();
        let agent = test_agent(td.path(), "http://127.0.0.1:9");
        let job = PrintJob::from_dict(&obj(json!({"id": "q1", "cups_name": "P", "content_type": "png_base64", "content": PNG_1X1_B64}))).unwrap();
        agent.store.write_queue(&job).unwrap();
        agent.drain_local_queue(None, None);
        assert!(agent.store.is_processed("q1"));
        assert!(!agent.store.has_pending_work());
    }

    #[test]
    fn run_loop_stops_promptly() {
        let td = tempfile::tempdir().unwrap();
        let agent = test_agent(td.path(), "http://127.0.0.1:9");
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let handle = thread::spawn(move || agent.run(s));
        thread::sleep(Duration::from_millis(300));
        let t = Instant::now();
        stop.store(true, Ordering::SeqCst);
        handle.join().unwrap();
        assert!(t.elapsed() < Duration::from_secs(1));
    }

    /// A queue file kept for retry (transient failure) is not work in
    /// flight: it must not defer OTA (it used to, on every heartbeat, forever).
    #[test]
    fn leftover_queue_file_does_not_defer_ota() {
        let td = tempfile::tempdir().unwrap();
        let base: Arc<OnceLock<String>> = Arc::default();
        let b = base.clone();
        let srv = stub(move |_, path| match path {
            "/print/v1/whoami" => (200, WHOAMI.into()),
            "/print/v1/heartbeat" => (
                200,
                json!({"desired_agent_version": "9.9.9",
                       "update_url": format!("{}/manifest.json", b.get().unwrap())})
                .to_string(),
            ),
            "/print/v1/jobs/pending" => (200, r#"{"jobs":[]}"#.into()),
            _ => (404, r#"{"error":"not found"}"#.into()),
        });
        base.set(srv.base_url.clone()).unwrap();
        let mut agent = test_agent(td.path(), &srv.base_url);
        agent.cfg.cable_enabled = false;
        agent.pipeline.lp = Arc::new(|_, _, _| Err(JobError::new("printer offline", "lp_error")));
        pair(&agent);
        let job = PrintJob::from_dict(&obj(json!({"id": "q1", "cups_name": "P",
            "content_type": "png_base64", "content": PNG_1X1_B64})))
        .unwrap();
        agent.store.write_queue(&job).unwrap();

        run_for(&agent, Duration::from_millis(800));

        // The startup drain kept it (retryable), yet the update was attempted.
        assert!(agent.store.has_pending_work());
        assert!(srv.count("/manifest.json") >= 1, "OTA download attempted");
        let ust = update::read_update_status(&agent.cfg.update_status_path()).unwrap();
        assert_eq!(ust.status, update::STATUS_FAILED);
        assert_eq!(ust.target_version.as_deref(), Some("9.9.9"));
    }

    /// A fresh process cannot be mid-download: a status left at downloading
    /// / installing becomes failed at start, so it neither pauses jobs nor
    /// keeps held push jobs and the deferred update waiting on each other.
    #[test]
    fn interrupted_update_is_marked_failed_at_start() {
        for status in [update::STATUS_DOWNLOADING, update::STATUS_INSTALLING] {
            let td = tempfile::tempdir().unwrap();
            let agent = test_agent(td.path(), "http://127.0.0.1:9");
            let path = agent.cfg.update_status_path();
            let st = UpdateStatus {
                target_version: Some("9.9.9".into()),
                previous_version: Some("0.3.17".into()),
                ..UpdateStatus::with_status(status)
            };
            update::write_update_status(&path, &st).unwrap();
            assert!(update::should_pause_jobs_from_path(&path));

            run_for(&agent, Duration::from_millis(300));

            let after = update::read_update_status(&path).unwrap();
            assert_eq!(after.status, update::STATUS_FAILED, "{status}");
            assert_eq!(after.last_error.as_deref(), Some(UPDATE_INTERRUPTED));
            assert_eq!(after.target_version.as_deref(), Some("9.9.9"));
            assert_eq!(after.previous_version.as_deref(), Some("0.3.17"));
            assert!(!update::should_pause_jobs_from_path(&path));
        }
        // Other states are left alone.
        let td = tempfile::tempdir().unwrap();
        let agent = test_agent(td.path(), "http://127.0.0.1:9");
        let path = agent.cfg.update_status_path();
        for status in [
            update::STATUS_IDLE,
            update::STATUS_PENDING_HEALTH,
            update::STATUS_FAILED,
        ] {
            update::write_update_status(&path, &UpdateStatus::with_status(status)).unwrap();
            agent.recover_interrupted_update();
            let after = update::read_update_status(&path).unwrap();
            assert_eq!((after.status.as_str(), after.last_error), (status, None));
        }
    }

    /// What maybe_update_from_heartbeat persists before fetching the manifest.
    fn downloading_9_9_9() -> UpdateStatus {
        UpdateStatus {
            target_version: Some("9.9.9".into()),
            previous_version: Some("0.3.17".into()),
            ..UpdateStatus::with_status(update::STATUS_DOWNLOADING)
        }
    }

    /// A heartbeat that unwinds mid-download (the OS refusing a thread the
    /// manifest fetch needs) must not leave `downloading` behind in a live
    /// process: the loop would hold push jobs and pause pulls, and the held
    /// jobs would keep the update deferred, with no restart to clear it.
    #[test]
    fn heartbeat_that_unwinds_mid_update_releases_the_pause() {
        let td = tempfile::tempdir().unwrap();
        let agent = test_agent(td.path(), "http://127.0.0.1:9");
        let path = agent.cfg.update_status_path();
        let st = agent.heartbeat_step(|| {
            update::write_update_status(&path, &downloading_9_9_9()).unwrap();
            assert!(update::should_pause_jobs_from_path(&path));
            panic!("failed to spawn thread: Os {{ code: 11 }}");
        });
        assert!(st.is_none());
        let after = update::read_update_status(&path).unwrap();
        assert_eq!(after.status, update::STATUS_FAILED);
        assert_eq!(after.last_error.as_deref(), Some(UPDATE_INTERRUPTED));
        assert_eq!(after.target_version.as_deref(), Some("9.9.9"));
        assert_eq!(after.previous_version.as_deref(), Some("0.3.17"));
        assert!(!update::should_pause_jobs_from_path(&path));

        // A heartbeat that returns normally is checked the same way; any
        // other state it leaves is untouched.
        let st = agent.heartbeat_step(|| {
            update::write_update_status(&path, &downloading_9_9_9()).unwrap();
            agent.run_once(false)
        });
        assert_eq!(st.unwrap().pairing, PairingState::Unpaired);
        let after = update::read_update_status(&path).unwrap();
        assert_eq!(after.status, update::STATUS_FAILED);
        assert_eq!(after.last_error.as_deref(), Some(UPDATE_INTERRUPTED));
        let mut pending = UpdateStatus::default();
        update::mark_pending_health(&mut pending, "9.9.9", Some("0.3.17".into()), 120, None);
        let st = agent.heartbeat_step(|| {
            update::write_update_status(&path, &pending).unwrap();
            panic!("restart helper refused a thread");
        });
        assert!(st.is_none());
        let after = update::read_update_status(&path).unwrap();
        assert_eq!(
            (after.status.as_str(), after.last_error),
            (update::STATUS_PENDING_HEALTH, None)
        );
    }

    /// The same inside `Agent::run`: after the heartbeat unwinds mid-download
    /// the loop goes on pulling jobs instead of holding them.
    #[test]
    fn run_loop_recovers_from_a_heartbeat_that_unwinds_mid_update() {
        let td = tempfile::tempdir().unwrap();
        let srv = stub(|_, path| match path {
            "/print/v1/whoami" => (200, WHOAMI.into()),
            "/print/v1/jobs/pending" => (200, r#"{"jobs":[]}"#.into()),
            _ => (200, "{}".into()),
        });
        let mut agent = test_agent(td.path(), &srv.base_url);
        agent.cfg.cable_enabled = false;
        agent.cfg.pull_interval_seconds = 1;
        pair(&agent);
        // No refresher thread, so run_once queries the inventory inline: the
        // one injectable step inside it. Its first call plays the OTA
        // download: persist `downloading`, then panic as a refused thread does.
        agent.shared = Arc::new(Shared {
            spawn: Arc::new(|_, _| Err(std::io::Error::from_raw_os_error(libc::EAGAIN))),
            ..Shared::default()
        });
        let path = agent.cfg.update_status_path();
        let first = Arc::new(AtomicBool::new(true));
        agent.inventory = Arc::new(move || {
            if first.swap(false, Ordering::SeqCst) {
                update::write_update_status(&path, &downloading_9_9_9()).unwrap();
                panic!("failed to spawn thread: Os {{ code: 11 }}");
            }
            Some(Vec::new())
        });

        run_for(&agent, Duration::from_millis(1500));

        assert_eq!(srv.count("/print/v1/whoami"), 1, "one heartbeat ran");
        assert!(srv.count("/print/v1/jobs/pending") >= 1, "pull not paused");
        let after = update::read_update_status(&agent.cfg.update_status_path()).unwrap();
        assert_eq!(after.status, update::STATUS_FAILED);
        assert_eq!(after.last_error.as_deref(), Some(UPDATE_INTERRUPTED));
    }

    /// The pull is stamped when it starts, so with a 1 s pull interval it
    /// runs every second, not every other cycle.
    #[test]
    fn pull_runs_every_pull_interval() {
        let td = tempfile::tempdir().unwrap();
        let srv = stub(|_, path| match path {
            "/print/v1/whoami" => (200, WHOAMI.into()),
            "/print/v1/jobs/pending" => {
                // A real pull takes a while.
                thread::sleep(Duration::from_millis(300));
                (200, r#"{"jobs":[]}"#.into())
            }
            _ => (200, "{}".into()),
        });
        let mut agent = test_agent(td.path(), &srv.base_url);
        agent.cfg.cable_enabled = false;
        agent.cfg.pull_interval_seconds = 1;
        pair(&agent);

        run_for(&agent, Duration::from_millis(3600));

        let pulls = srv.times("/print/v1/jobs/pending");
        assert!(pulls.len() >= 3, "pulled {} times in 3.6 s", pulls.len());
        for gap in pulls.windows(2).map(|w| w[1] - w[0]) {
            assert!(gap < Duration::from_millis(1600), "pull gap {gap:?}");
        }
    }

    /// A long first cycle (the first heartbeat waits for the first inventory,
    /// ~16 s on a Pi) must not be followed by a second pull right away.
    #[test]
    fn no_back_to_back_pull_after_a_long_cycle() {
        let td = tempfile::tempdir().unwrap();
        let first = Arc::new(AtomicBool::new(true));
        let srv = stub(move |_, path| match path {
            "/print/v1/whoami" => {
                if first.swap(false, Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(1500));
                }
                (200, WHOAMI.into())
            }
            "/print/v1/jobs/pending" => (200, r#"{"jobs":[]}"#.into()),
            _ => (200, "{}".into()),
        });
        let mut agent = test_agent(td.path(), &srv.base_url);
        agent.cfg.cable_enabled = false;
        agent.cfg.pull_interval_seconds = 1;
        pair(&agent);

        run_for(&agent, Duration::from_millis(3300));

        let pulls = srv.times("/print/v1/jobs/pending");
        assert!(pulls.len() >= 2, "pulled {} times", pulls.len());
        for gap in pulls.windows(2).map(|w| w[1] - w[0]) {
            assert!(gap >= Duration::from_millis(400), "pull gap {gap:?}");
        }
    }

    #[test]
    fn processed_markers_are_pruned_at_start() {
        let td = tempfile::tempdir().unwrap();
        let agent = test_agent(td.path(), "http://127.0.0.1:9");
        for id in ["old", "recent"] {
            agent.store.mark_processed(id).unwrap();
        }
        fs::File::options()
            .write(true)
            .open(agent.store.processed_path("old"))
            .unwrap()
            .set_modified(SystemTime::now() - PROCESSED_RETENTION - Duration::from_secs(60))
            .unwrap();
        run_for(&agent, Duration::from_millis(200));
        assert!(!agent.store.is_processed("old"));
        assert!(agent.store.is_processed("recent"));
    }

    #[test]
    fn cable_retry_backs_off_until_a_session_subscribes() {
        let s = Duration::from_secs;
        let t0 = Instant::now();
        let mut r = CableRetry::default();
        assert!(r.due(t0));
        // Sessions that never subscribe: retried 5, 10, 20, 40, then 60 s apart.
        let mut t = t0;
        for gap in [5, 10, 20, 40, 60, 60] {
            r.attempted(t);
            assert!(!r.due(t + s(gap) - Duration::from_millis(1)), "{gap}");
            assert!(r.due(t + s(gap)), "{gap}");
            t += s(gap);
        }
        // A session subscribes: the next drop is retried within 5 s, and the
        // pacing starts over.
        r.attempted(t);
        r.subscribed(t + s(1));
        assert!(!r.due(t + s(5)));
        assert!(r.due(t + s(6)));
        r.attempted(t + s(6));
        assert!(!r.due(t + s(10)));
        assert!(r.due(t + s(11)));
    }

    #[test]
    fn async_jobs_get_no_wait_tick() {
        let td = tempfile::tempdir().unwrap();
        let mut agent = test_agent(td.path(), "http://127.0.0.1:9");
        assert_eq!(agent.cfg.wait_cups, WaitCups::Async);
        assert!(agent.job_pipeline(Some("tok"), None).on_wait_tick.is_none());
        agent.cfg.wait_cups = WaitCups::Sync;
        assert!(agent.job_pipeline(Some("tok"), None).on_wait_tick.is_some());
    }

    /// Jobs waiting on CUPS one after another share one heartbeat schedule,
    /// and inventory is only read when a heartbeat is due.
    #[test]
    fn wait_ticks_share_one_schedule() {
        let td = tempfile::tempdir().unwrap();
        let srv = stub(|_, _| (200, "{}".into()));
        let mut agent = test_agent(td.path(), &srv.base_url);
        let inventory_calls = Arc::new(AtomicUsize::new(0));
        let calls = inventory_calls.clone();
        agent.inventory = Arc::new(move || {
            calls.fetch_add(1, Ordering::SeqCst);
            Some(vec![json!({"cups_name": "Q"})])
        });
        let cable = Arc::new(FakeCable {
            ok: true,
            calls: Mutex::default(),
        });
        let first_job = agent.inventory_wait_tick(Some(cable.clone()), Some("tok"));
        let second_job = agent.inventory_wait_tick(Some(cable.clone()), Some("tok"));
        first_job();
        second_job();
        first_job();
        assert_eq!(srv.count("/print/v1/heartbeat"), 1);
        assert_eq!(inventory_calls.load(Ordering::SeqCst), 1);
        let actions: Vec<String> = cable
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|(a, _)| a.clone())
            .collect();
        assert_eq!(actions, ["report_printers", "heartbeat"]);
    }

    /// Inside `run`, inventory comes from the background snapshot: a slow
    /// CUPS inventory no longer blocks heartbeats (or the jobs behind them).
    #[test]
    fn heartbeats_use_the_inventory_snapshot() {
        let td = tempfile::tempdir().unwrap();
        let srv = stub(|_, path| match path {
            "/print/v1/whoami" => (200, WHOAMI.into()),
            _ => (200, "{}".into()),
        });
        let mut agent = test_agent(td.path(), &srv.base_url);
        let inventory_calls = Arc::new(AtomicUsize::new(0));
        let calls = inventory_calls.clone();
        agent.inventory = Arc::new(move || {
            calls.fetch_add(1, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(500));
            Some(vec![json!({"cups_name": "Q"})])
        });
        pair(&agent);
        let stop = Arc::new(AtomicBool::new(false));
        let refresher = agent.start_inventory_refresher(&stop);

        // The first heartbeat waits for the first snapshot...
        assert_eq!(agent.run_once(false).cloud, CloudState::Online);
        // ...later ones don't run the inventory at all.
        for _ in 0..3 {
            let t = Instant::now();
            assert_eq!(agent.run_once(false).cloud, CloudState::Online);
            assert!(
                t.elapsed() < Duration::from_millis(400),
                "{:?}",
                t.elapsed()
            );
        }
        assert_eq!(inventory_calls.load(Ordering::SeqCst), 1);
        stop.store(true, Ordering::SeqCst);
        agent.shared.inventory.end(refresher);
        // Without a refresher (one-off callers) inventory is queried inline.
        assert_eq!(
            agent.printers(Duration::ZERO),
            Some(vec![json!({"cups_name": "Q"})])
        );
        assert_eq!(inventory_calls.load(Ordering::SeqCst), 2);
    }

    /// The OS refusing the refresher thread must not panic: heartbeats fall
    /// back to querying the inventory inline.
    #[test]
    fn inventory_refresher_spawn_failure_falls_back_inline() {
        let td = tempfile::tempdir().unwrap();
        let mut agent = test_agent(td.path(), "http://127.0.0.1:9");
        agent.shared = Arc::new(Shared {
            spawn: Arc::new(|_, _| Err(std::io::Error::from_raw_os_error(libc::EAGAIN))),
            ..Shared::default()
        });
        let inventory_calls = Arc::new(AtomicUsize::new(0));
        let calls = inventory_calls.clone();
        agent.inventory = Arc::new(move || {
            calls.fetch_add(1, Ordering::SeqCst);
            Some(Vec::new())
        });
        let stop = Arc::new(AtomicBool::new(false));
        agent.start_inventory_refresher(&stop);
        assert_eq!(agent.printers(Duration::from_secs(5)), Some(Vec::new()));
        assert_eq!(agent.printers(Duration::ZERO), Some(Vec::new()));
        assert_eq!(inventory_calls.load(Ordering::SeqCst), 2);
    }

    /// The async CUPS watcher is shared by every job pipeline of an agent.
    #[test]
    fn job_pipelines_share_the_cups_watcher() {
        let td = tempfile::tempdir().unwrap();
        let agent = test_agent(td.path(), "http://127.0.0.1:9");
        let a = agent.job_pipeline(None, None);
        let b = agent.job_pipeline(Some("tok"), None);
        assert!(Arc::ptr_eq(&a.cups_watcher, &b.cups_watcher));
        assert!(Arc::ptr_eq(&a.cups_watcher, &agent.pipeline.cups_watcher));
    }
}
