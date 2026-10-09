//! Cloud agent: whoami + heartbeat + job pull + ActionCable push.

use std::collections::VecDeque;
use std::ffi::CStr;
use std::fs;
use std::io;
use std::mem::MaybeUninit;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
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
/// How long the agent takes no job once its health gate has asked for its
/// own restart (see [`requested_own_restart`]): `systemctl restart
/// --no-block` stops it within seconds. Past this the restart is taken to
/// have failed, and jobs are taken again rather than never.
const OWN_RESTART_GRACE: Duration = Duration::from_secs(120);

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

/// Printer setup: adds a CUPS queue for every printer it can find and
/// returns the display names of all configured printers
/// ([`printers::ensure_printers`] in production).
pub type ProvisionFn = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

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
    // Drops the queue file and writes the marker that prevents a late
    // redelivery/print, never while the job's own run rewrites its record.
    store.cancel(job_id)
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
    /// Set to have the refresher start its next pass now rather than at its
    /// next tick (printer setup added queues).
    refresh_now: AtomicBool,
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

    /// Ask the refresher for a pass now. Without a refresher this does
    /// nothing: inline queries are always current.
    fn request_refresh(&self) {
        self.refresh_now.store(true, Ordering::SeqCst);
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
    /// True while a printer setup pass runs, so two never overlap.
    printer_setup: AtomicBool,
    /// Starts the inventory refresher and printer setup threads (injectable
    /// for tests).
    spawn: SpawnFn,
    /// The stop flag of the [`Agent::run`] in progress (a fresh one between
    /// runs): job pipelines stop draining, printing and waiting on CUPS
    /// once it is set (see [`Pipeline::stop`]).
    stop: Mutex<Arc<AtomicBool>>,
    /// When this process asked for its own restart, while it waits for it.
    own_restart: Mutex<Option<Instant>>,
    /// [`OWN_RESTART_GRACE`] (tests shorten it).
    own_restart_grace: Duration,
}

impl Default for Shared {
    fn default() -> Self {
        Shared {
            inventory: InventoryCache::default(),
            wait_tick: Mutex::default(),
            printer_setup: AtomicBool::new(false),
            spawn: Arc::new(jobs::spawn_thread),
            stop: Mutex::default(),
            own_restart: Mutex::default(),
            own_restart_grace: OWN_RESTART_GRACE,
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
    /// Printer setup (adds CUPS queues for new USB and network printers).
    /// [`Agent::run`] runs it once per start, on a background thread.
    pub provision_printers: ProvisionFn,
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
            provision_printers: Arc::new(printers::ensure_printers),
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
    /// (or sooner, when [`InventoryCache::request_refresh`] asks) until
    /// `stop`. Returns its generation (to end it). If the thread cannot
    /// start, heartbeats fall back to querying inline.
    ///
    /// It runs paired or not: the LCD's printer rows and test-print overlay
    /// (open while unpaired too) read the printers.json it writes.
    fn start_inventory_refresher(&self, stop: &Arc<AtomicBool>) -> u64 {
        let generation = self.shared.inventory.begin();
        let (shared, source, stop) = (self.shared.clone(), self.inventory.clone(), stop.clone());
        let printers_path = self.cfg.printers_path();
        let body = Box::new(move || {
            let cache = &shared.inventory;
            while !stop.load(Ordering::SeqCst) && cache.is_current(generation) {
                // This pass answers every refresh asked for until now.
                cache.refresh_now.store(false, Ordering::SeqCst);
                let inventory = contained("printer inventory", || source());
                if let Some(Some(printers)) = &inventory {
                    write_printers_snapshot(&printers_path, printers);
                }
                cache.publish(generation, inventory);
                sleep_until_any(
                    Instant::now() + INVENTORY_REFRESH_EVERY,
                    &[&stop, &cache.refresh_now],
                );
            }
            cache.end(generation);
        });
        if let Err(e) = (self.shared.spawn)("vesyl-print-inventory", body) {
            log::error!(target: LOG, "inventory refresher failed to start ({e}) — querying printers inline");
            self.shared.inventory.end(generation);
        }
        generation
    }

    /// Start printer setup ([`Agent::provision_printers`]) on a background
    /// thread: it browses USB and the network and scans the LAN, which takes
    /// many seconds, so it never runs on the agent loop. Two passes never
    /// overlap: while one is still running (from an earlier [`Agent::run`]),
    /// none is started. When a pass ends, the inventory refresher is asked
    /// for a fresh pass, so new queues reach the cloud and the LCD without
    /// waiting for its next tick. Returns whether a pass was started.
    fn start_printer_setup(&self) -> bool {
        if self.shared.printer_setup.swap(true, Ordering::SeqCst) {
            log::info!(target: LOG, "printer setup is still running from an earlier start — not starting another");
            return false;
        }
        let (shared, provision) = (self.shared.clone(), self.provision_printers.clone());
        let body = Box::new(move || {
            log::info!(target: LOG, "printer setup: looking for USB and network printers");
            let started = Instant::now();
            // A panic is logged by `contained`; the agent carries on without it.
            if let Some(names) = contained("printer setup", || provision()) {
                log::info!(
                    target: LOG,
                    "printer setup finished in {:.1}s: {}",
                    started.elapsed().as_secs_f64(),
                    printer_setup_summary(&names)
                );
            }
            shared.printer_setup.store(false, Ordering::SeqCst);
            shared.inventory.request_refresh();
        });
        match (self.shared.spawn)("vesyl-print-printer-setup", body) {
            Ok(()) => true,
            Err(e) => {
                log::error!(target: LOG, "printer setup failed to start ({e}) — new printers get no CUPS queue until the agent restarts");
                self.shared.printer_setup.store(false, Ordering::SeqCst);
                false
            }
        }
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
    /// the loop reads it to decide whether to hold jobs. And when its health
    /// gate asked for this process's restart, jobs wait for that restart
    /// (see [`Agent::awaiting_own_restart`]).
    fn heartbeat_step(&self, run_once: impl FnOnce() -> AgentStatus) -> Option<AgentStatus> {
        let root = &self.update_env.install_root;
        let slot_before = update::current_release_version(root);
        let st = contained("heartbeat", run_once);
        self.recover_interrupted_update();
        let after = update::read_update_status(&self.cfg.update_status_path());
        if requested_own_restart(
            self.update_env.restart,
            slot_before.as_deref(),
            update::current_release_version(root).as_deref(),
            after.as_ref(),
        ) {
            log::warn!(target: LOG, "health gate rolled back and restarted the services — taking no job until this agent is stopped");
            *lock(&self.shared.own_restart) = Some(Instant::now());
        }
        st
    }

    /// True while this process waits for the restart its health gate asked
    /// for: a job started now could be cut short by the SIGTERM. One that
    /// has not come within [`OWN_RESTART_GRACE`] is taken to have failed,
    /// and jobs go on.
    fn awaiting_own_restart(&self) -> bool {
        let mut asked = lock(&self.shared.own_restart);
        match *asked {
            Some(t) if t.elapsed() < self.shared.own_restart_grace => true,
            Some(t) => {
                log::warn!(target: LOG, "the restart asked for {:.0}s ago has not come — taking jobs again", t.elapsed().as_secs_f64());
                *asked = None;
                false
            }
            None => false,
        }
    }

    /// True while jobs must wait: an OTA is downloading, installing or in
    /// its health gate, or this process waits for its own restart. The
    /// update status counts as the next heartbeat will find it: a `failed`
    /// one that heartbeat turns back into a health gate (an install cut off
    /// after its flip, see [`update::should_pause_jobs_once_recovered`],
    /// which writes and logs nothing) pauses jobs already.
    fn jobs_paused(&self) -> bool {
        let st = update::read_update_status(&self.cfg.update_status_path());
        update::should_pause_jobs_once_recovered(st.as_ref(), &self.update_env)
            || self.awaiting_own_restart()
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
    /// Once `stop` is set, a rollback restarts nothing (see
    /// [`update::process_pending_health`]).
    fn health_gate(
        &self,
        whoami: WhoamiResult,
        whoami_error: Option<&str>,
        stop: &AtomicBool,
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
            stop,
        );
        self.write_update_status(&out);
        (Some(out), true)
    }

    /// Single REST heartbeat cycle. Updates status file for the LCD.
    ///
    /// `jobs_busy`: when true, OTA download/install is deferred (job work in
    /// flight, e.g. buffered ActionCable jobs) so we never flip slots mid-print.
    pub fn run_once(&self, jobs_busy: bool) -> AgentStatus {
        self.run_once_with_stop(jobs_busy, &AtomicBool::new(false))
    }

    /// [`Agent::run_once`] for [`Agent::run`], which passes its `stop`: once
    /// that is set, the heartbeat's reply starts no update. An update runs
    /// on to its restart, and that restart replaces a `systemctl stop`
    /// still in progress: the agent would come back, on the new slot.
    fn run_once_with_stop(&self, jobs_busy: bool, stop: &AtomicBool) -> AgentStatus {
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
            self.health_gate(WhoamiResult::Skipped, None, stop);
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
                self.health_gate(WhoamiResult::Unauthorized, Some(&e.message), stop);
                self.handle_unauthorized(Some(&creds));
                return self.revoked_status();
            }
            Err(e) => {
                log::warn!(target: LOG, "whoami failed: {}", e.message);
                (WhoamiResult::Error, Some(e.message))
            }
        };

        // Post-update health gate: declare OTA success only after whoami.
        let (update_status, gate_ran) = self.health_gate(whoami, whoami_error.as_deref(), stop);
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
                // None once the agent is stopping: checked now that the reply
                // is in, as a stop lets the request in flight finish.
                if stop.load(Ordering::SeqCst) {
                    log::info!(target: LOG, "stopping — any update waits for the next start");
                    return st;
                }
                let ust = update::maybe_update_from_heartbeat(
                    &hb,
                    &self.cfg,
                    &self.update_env,
                    update_status,
                    Some(&self.cfg.update_status_path()),
                    jobs_busy,
                    stop,
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
    /// is held while sending. A REST heartbeat carries the update status as
    /// the loop's does; its reply starts no update (only the loop's does,
    /// between jobs).
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
                let update = update::read_update_status(&agent.cfg.update_status_path());
                let body = HeartbeatBody {
                    agent_version: Some(agent_version().into()),
                    hostname: Some(sysinfo::hostname()),
                    printers: inv,
                    platform: Some(default_platform()),
                    update: update.as_ref().map(UpdateStatus::to_dict),
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
        // A stop (SIGTERM) ends a drain or a CUPS wait instead of waiting for
        // systemd's SIGKILL, which would leave a printed job queued.
        p.stop = lock(&self.shared.stop).clone();
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
        // Job pipelines follow this run's stop from here on.
        *lock(&self.shared.stop) = stop.clone();
        // Once a stop begins, whatever ends this run is the stop, not the
        // job in flight: its attempt must not count as a death in it.
        let store = self.store.clone();
        let _stop_note = on_stop(
            &stop,
            Box::new(move || {
                if let Err(e) = store.note_stop_began() {
                    log::warn!(target: LOG, "could not record that the agent is stopping: {e}");
                }
            }),
        );

        self.recover_interrupted_update();
        // CUPS inventory runs off this loop from here on.
        let inventory_refresher = self.start_inventory_refresher(&stop);
        // So does printer setup (new CUPS queues), once per start.
        self.start_printer_setup();

        // --- ActionCable session (push) ------------------------------------
        let push_jobs: Arc<Mutex<VecDeque<JsonObject>>> = Arc::default();
        let revoke_flag = Arc::new(AtomicBool::new(false));
        // Set by the session after each pushed job or revoke: ends the
        // loop's sleep, so they are handled now rather than next cycle.
        let wake = Arc::new(AtomicBool::new(false));
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
                wake.clone(),
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
            if start_cable_session(&holder, &sess) {
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

        // Jobs left queued print now, unless jobs are paused: after an OTA
        // restart (or an install cut off after its flip, whose gate the
        // first heartbeat reopens) the health gate has not judged this slot
        // yet. Then the loop drains them once the pause is over, before any
        // new job.
        let mut drain_pending = self.jobs_paused();
        if drain_pending {
            log::info!(target: LOG, "update in progress — queued jobs wait until it is done");
        } else {
            let token = auth::load_credentials(&cfg.credentials_path()).map(|c| c.device_token);
            self.drain_local_queue(token.as_deref(), None);
            // After the drain: it relies on markers of jobs still queued.
            self.prune_processed_markers();
        }
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
            // Cleared before the revoke flag and the pushed jobs are read
            // below: one that arrives after this is either handled in this
            // cycle or cuts its sleep short.
            wake.store(false, Ordering::SeqCst);
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
            // And while the restart this process asked for is on its way.
            ota_pause |= self.awaiting_own_restart();

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
            // this thread, so that is the buffered push jobs, and the startup
            // drain still to run: the heartbeat that closes the health gate
            // must not start the next OTA before it. Leftover queue files
            // (retryable failures) must not hold off every OTA.
            let jobs_busy = !lock(&push_jobs).is_empty() || drain_pending;
            let st = if elapsed_since(last_hb, hb_interval) {
                let st = self.heartbeat_step(|| self.run_once_with_stop(jobs_busy, &stop));
                last_hb = Some(Instant::now());
                // Re-read: an OTA may have activated (pending_health) or failed.
                ota_pause = update::should_pause_jobs_from_path(&cfg.update_status_path());
                ota_pause |= self.awaiting_own_restart();
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

            // The startup drain the pause held back, once it is over: the
            // gate passed, or closed without restarting this process (a gate
            // that restarts it leaves the queue to the agent it starts).
            if drain_pending && !ota_pause && !stop.load(Ordering::SeqCst) {
                drain_pending = false;
                let token = creds.as_ref().map(|c| c.device_token.as_str());
                self.drain_local_queue(token, current_cable(&holder));
                self.prune_processed_markers();
                last_prune = Instant::now();
            }

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

            if !drain_pending && last_prune.elapsed() >= PRUNE_EVERY {
                self.prune_processed_markers();
                last_prune = Instant::now();
            }

            // Sleep (a pushed job or a revoke ends it early: see `wake`)
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
                    _ if cfg.pull_jobs_enabled && creds.is_some() => pull_interval,
                    _ => hb_interval,
                }
            };
            // While a session is subscribed, come round every second whatever
            // the pull or offline pacing says: its heartbeat is due every
            // 10-15 s, and a dropped session or a local change (credentials,
            // update status) is seen within a second.
            if subscribed {
                sleep_for = sleep_for.min(secs(1.0));
            }
            if let Some(t) = last_hb {
                sleep_for = sleep_for.min(hb_interval.saturating_sub(t.elapsed()));
            }
            sleep_until_any(cycle_start + sleep_for, &[&stop, &wake]);
        }

        stop_cable();
        self.shared.inventory.end(inventory_refresher);
        // Calls made after this run (one-off ones) are not stopping.
        *lock(&self.shared.stop) = Arc::default();
        log::info!(target: LOG, "agent stopped");
    }

    fn session_handlers(
        &self,
        push_jobs: Arc<Mutex<VecDeque<JsonObject>>>,
        revoke_flag: Arc<AtomicBool>,
        wake: Arc<AtomicBool>,
        holder: Arc<Mutex<Option<Arc<PrintCableSession>>>>,
        subscribed_flag: Arc<AtomicBool>,
    ) -> SessionHandlers {
        let store = self.store.clone();
        let cfg = self.cfg.clone();
        let (shared, inventory) = (self.shared.clone(), self.inventory.clone());
        let revoke_wake = wake.clone();
        SessionHandlers {
            // The job (or revoke) first, then the wake-up: the loop clears
            // `wake` before it looks, so it never misses one.
            on_print_job: Some(Arc::new(move |job| {
                lock(&push_jobs).push_back(job);
                wake.store(true, Ordering::SeqCst);
            })),
            on_revoke: Some(Arc::new(move || {
                revoke_flag.store(true, Ordering::SeqCst);
                revoke_wake.store(true, Ordering::SeqCst);
            })),
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

/// True when a heartbeat asked for this process's own restart: its health
/// gate rolled back (the update status is now `rolled_back`, and `current`
/// moved from `slot_before` to `slot_after`), and `restarts` (the update env
/// restarts the services after a rollback, as `update::close_failed_gate`
/// does). A gate closed because `current` was switched by hand is
/// `rolled_back` too, but flips and restarts nothing; an activation leaves
/// `pending_health`, which pauses jobs on its own.
fn requested_own_restart(
    restarts: bool,
    slot_before: Option<&str>,
    slot_after: Option<&str>,
    status: Option<&UpdateStatus>,
) -> bool {
    restarts
        && slot_before != slot_after
        && status.is_some_and(|s| s.status == update::STATUS_ROLLED_BACK)
}

/// Start `sess` as the agent's cable session. It goes into `holder` first:
/// its socket thread can subscribe before `start` returns, and the
/// on_subscribed handler looks it up there to report the printers. One that
/// does not start (no ticket) is taken out again.
fn start_cable_session(
    holder: &Mutex<Option<Arc<PrintCableSession>>>,
    sess: &Arc<PrintCableSession>,
) -> bool {
    *lock(holder) = Some(sess.clone());
    if sess.start() {
        return true;
    }
    lock(holder).take();
    false
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

/// Publish the latest printer inventory for the LCD display, which reads it
/// instead of querying CUPS itself.
fn write_printers_snapshot(path: &std::path::Path, printers: &[Value]) {
    let body = json!({ "updated_at": crate::util::utc_now_iso(), "printers": printers });
    let mut raw = serde_json::to_string_pretty(&body).unwrap_or_default();
    raw.push('\n');
    if let Err(e) = crate::util::write_durable(path, raw.as_bytes(), 0o644, false) {
        log::warn!(target: LOG, "write {}: {e}", path.display());
    }
}

/// The printer setup result for the log.
fn printer_setup_summary(names: &[String]) -> String {
    if names.is_empty() {
        "no printer queues".into()
    } else {
        format!("{} printer queue(s): {}", names.len(), names.join(", "))
    }
}

/// Sleep until `deadline`, waking early once any of `flags` is set.
fn sleep_until_any(deadline: Instant, flags: &[&AtomicBool]) {
    while !flags.iter().any(|f| f.load(Ordering::SeqCst)) {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        thread::sleep((deadline - now).min(Duration::from_millis(100)));
    }
}

// --- the agent process (`vesyl-print agent`) --------------------------------

/// What stops the agent: SIGTERM (`systemctl stop`) and SIGINT (Ctrl-C).
const STOP_SIGNALS: [libc::c_int; 2] = [libc::SIGINT, libc::SIGTERM];

/// Stop the agent on SIGINT / SIGTERM without interrupting what it is doing.
///
/// A signal handler runs on whichever thread the kernel picks, often the
/// main one, and Linux never restarts a recv() it interrupts on a socket
/// with a receive timeout, which every ureq request has (signal(7)): the
/// heartbeat, ack or download in flight failed with EINTR. So both signals
/// are blocked instead, here on the calling thread before it starts any
/// other (threads inherit the mask), and one thread takes them with
/// sigwait(). The first sets `stop`: the agent finishes its current step,
/// a request in flight included (though a heartbeat then starts no
/// update), and [`Agent::run`] returns. A second one ends the process at
/// once. Child processes (lp, lpstat) still start with nothing blocked: std
/// would pass the mask on, so [`printers::run_with_timeout`] empties it
/// before exec.
pub fn stop_on_signals(stop: Arc<AtomicBool>) -> io::Result<()> {
    let set = signal_set(&STOP_SIGNALS);
    let mut old = signal_set(&[]);
    // SAFETY: both are initialized signal sets.
    let rc = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old) };
    if rc != 0 {
        return Err(io::Error::from_raw_os_error(rc));
    }
    let taker = thread::Builder::new()
        .name("vesyl-print-signals".into())
        .spawn(move || take_stop_signals(&set, &stop));
    if let Err(e) = taker {
        // Nothing would ever take them: unblock them again.
        // SAFETY: as above.
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut()) };
        return Err(e);
    }
    Ok(())
}

/// Something to do the moment a stop signal sets a given stop flag.
type StopHook = Box<dyn Fn() + Send>;

/// The [`StopHook`]s, by the address of the flag they wait on.
static STOP_HOOKS: Mutex<Vec<(usize, StopHook)>> = Mutex::new(Vec::new());

fn flag_key(stop: &Arc<AtomicBool>) -> usize {
    Arc::as_ptr(stop) as usize
}

/// Run `hook` on the signal thread when a stop signal sets `stop`, until
/// the returned guard is dropped. [`Agent::run`] records with it that its
/// run began to stop, at once, while the job in flight may not look at the
/// flag again before systemd's SIGKILL.
fn on_stop(stop: &Arc<AtomicBool>, hook: StopHook) -> StopHookGuard {
    let key = flag_key(stop);
    lock(&STOP_HOOKS).push((key, hook));
    StopHookGuard(key)
}

/// Removes the hooks [`on_stop`] added for its flag.
struct StopHookGuard(usize);

impl Drop for StopHookGuard {
    fn drop(&mut self) {
        lock(&STOP_HOOKS).retain(|(key, _)| *key != self.0);
    }
}

/// A stop signal arrived: set `stop`, then run its hooks.
fn stop_began(stop: &Arc<AtomicBool>) {
    stop.store(true, Ordering::SeqCst);
    let key = flag_key(stop);
    for (_, hook) in lock(&STOP_HOOKS).iter().filter(|(k, _)| *k == key) {
        hook();
    }
}

/// The signal thread: the first stop signal sets `stop`, a second one ends
/// the process.
fn take_stop_signals(set: &libc::sigset_t, stop: &Arc<AtomicBool>) {
    let mut stopping = false;
    loop {
        let mut sig: libc::c_int = 0;
        // SAFETY: `set` is an initialized signal set and `sig` is writable.
        if unsafe { libc::sigwait(set, &mut sig) } != 0 {
            continue;
        }
        let name = signal_name(sig);
        if stopping {
            log::warn!(target: LOG, "second stop signal ({name}) — quitting at once");
            die_by(sig);
        }
        stopping = true;
        log::info!(target: LOG, "{name} received — stopping once the current step is done (a second signal quits at once)");
        stop_began(stop);
    }
}

/// End the process by `sig`'s default action, as if it had never been
/// blocked.
fn die_by(sig: libc::c_int) -> ! {
    let set = signal_set(&[sig]);
    // SAFETY: plain libc calls on an initialized set. Unblocked, with its
    // default action, the raised signal ends the process.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
        libc::raise(sig);
    }
    // Not reached for a stop signal.
    std::process::exit(128 + sig)
}

fn signal_set(signals: &[libc::c_int]) -> libc::sigset_t {
    let mut set = MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: sigemptyset initializes the set before sigaddset changes it.
    unsafe {
        libc::sigemptyset(set.as_mut_ptr());
        for &sig in signals {
            libc::sigaddset(set.as_mut_ptr(), sig);
        }
        set.assume_init()
    }
}

fn signal_name(sig: libc::c_int) -> String {
    match sig {
        libc::SIGINT => "SIGINT".into(),
        libc::SIGTERM => "SIGTERM".into(),
        other => format!("signal {other}"),
    }
}

/// `Err` (what to do instead) when this process runs as root; see
/// [`root_refusal`].
pub fn refuse_root(cfg: &Config) -> Result<(), String> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    match root_refusal(unsafe { libc::geteuid() }, &cfg.state_dir) {
        Some(why) => Err(why),
        None => Ok(()),
    }
}

/// Why the agent must not run with effective uid `euid`, if it must not.
///
/// The service user owns the state and config directories: setup.sh makes
/// them so (and refuses root as the service user), and the unit runs the
/// agent as that user. A root agent leaves root-owned files there, and it
/// follows symlinks that user can plant: with queue/ a link, a root drain
/// moved config.json into the link target's failed/, where pruning would
/// delete it.
fn root_refusal(euid: u32, state_dir: &Path) -> Option<String> {
    if euid != 0 {
        return None;
    }
    let user = match fs::metadata(state_dir).map(|m| m.uid()) {
        Ok(uid) if uid != 0 => user_name(uid).unwrap_or_else(|| format!("'#{uid}'")),
        _ => "<service user>".into(),
    };
    Some(format!(
        "vesyl-print agent must not run as root: it would leave root-owned files in {} \
         and follow links the service user can plant there.\n\
         Run it as the service account: `sudo systemctl start vesyl-print-agent`, \
         or in the foreground `sudo -u {user} vesyl-print agent`.",
        state_dir.display()
    ))
}

/// The login name of `uid`.
fn user_name(uid: u32) -> Option<String> {
    let mut pwd = MaybeUninit::<libc::passwd>::uninit();
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer is valid for the call and `buf` is as long as stated.
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            pwd.as_mut_ptr(),
            buf.as_mut_ptr(),
            buf.len(),
            &mut found,
        )
    };
    if rc != 0 || found.is_null() {
        return None;
    }
    // SAFETY: on success `found` is `pwd`, whose name is a NUL-terminated
    // string in `buf`.
    let name = unsafe { CStr::from_ptr((*found).pw_name) };
    Some(name.to_string_lossy().into_owned())
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
    use std::sync::{mpsc, OnceLock};
    use std::time::SystemTime;
    use tungstenite::Message;

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
            // Never real CUPS (lpinfo, lpadmin, a LAN scan) from a test.
            provision_printers: Arc::new(Vec::new),
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
                cups_lookup: Arc::new(|_| Err("no CUPS in tests".into())),
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
        crate::util::set_mode(&slot.join("vesyl-print"), 0o755).unwrap();
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

    /// The unpaired path's health gate gets the agent's `stop` too: an
    /// unpaired node whose new slot cannot run rolls back to the previous
    /// one, but restarts nothing once the agent is stopping (that restart
    /// would replace the `systemctl stop` under way); not stopping, it
    /// restarts into the previous slot.
    #[test]
    fn unpaired_health_gate_restarts_nothing_while_stopping() {
        for (stopping, restarts) in [(true, 0), (false, 1)] {
            let td = tempfile::tempdir().unwrap();
            let mut agent = test_agent(td.path(), "http://127.0.0.1:9");
            // Counted by restarts_during, never run.
            agent.update_env.restart = true;
            let root = agent.update_env.install_root.clone();
            let ver = agent_version();
            // The gate's slot lost its binary's exec bit: a local hard fail.
            for (version, mode) in [("0.0.1", 0o755), (ver, 0o644)] {
                let slot = root.join("releases").join(version);
                fs::create_dir_all(&slot).unwrap();
                fs::write(slot.join("vesyl-print"), b"bin").unwrap();
                crate::util::set_mode(&slot.join("vesyl-print"), mode).unwrap();
            }
            update::flip_current(&root, ver).unwrap();
            let mut pending = UpdateStatus::default();
            update::mark_pending_health(&mut pending, ver, Some("0.0.1".into()), 120, None);
            update::write_update_status(&agent.cfg.update_status_path(), &pending).unwrap();

            let stop = AtomicBool::new(stopping);
            let (st, seen) = update::restarts_during(|| agent.run_once_with_stop(false, &stop));
            assert_eq!(st.pairing, PairingState::Unpaired);
            let ust = update::read_update_status(&agent.cfg.update_status_path()).unwrap();
            assert_eq!(ust.status, update::STATUS_ROLLED_BACK, "{ust:?}");
            let error = ust.last_error.unwrap_or_default();
            assert!(
                error.starts_with("health failed: current slot has no executable")
                    && error.ends_with("; rolled back to 0.0.1"),
                "{error}"
            );
            let current = fs::read_link(root.join("current")).unwrap();
            assert!(current.ends_with("0.0.1"), "{}", current.display());
            assert_eq!(seen, restarts, "stopping: {stopping}");
        }
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

    /// A stop signal records, at once, that this run began to stop (the job
    /// in flight then does not count a SIGKILL as a death in it), through
    /// the hook registered for its flag alone, and only while registered.
    #[test]
    fn a_stop_signal_records_that_the_run_is_stopping() {
        let td = tempfile::tempdir().unwrap();
        let store = JobStore::new(td.path().join("q"), td.path().join("p"));
        let (stop, other) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let s = store.clone();
        let guard = on_stop(&stop, Box::new(move || s.note_stop_began().unwrap()));

        stop_began(&other);
        assert!(other.load(Ordering::SeqCst));
        assert!(!store.stop_marker_path().exists(), "another flag's stop");

        stop_began(&stop);
        assert!(stop.load(Ordering::SeqCst));
        let runs = fs::read_to_string(store.stop_marker_path()).unwrap();
        assert_eq!(runs.lines().count(), 1, "{runs}");

        drop(guard);
        fs::remove_file(store.stop_marker_path()).unwrap();
        stop_began(&stop);
        assert!(!store.stop_marker_path().exists(), "hook removed");
    }

    /// The unit sends the stop's SIGTERM to the agent alone (see
    /// vesyl-print-agent.service): lp and the renderers finish their step.
    #[test]
    fn the_unit_stops_the_agent_alone() {
        let unit = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../vesyl-print-agent.service"),
        )
        .unwrap();
        let kill_modes: Vec<_> = unit
            .lines()
            .filter(|l| l.trim_start().starts_with("KillMode="))
            .collect();
        assert_eq!(kill_modes, ["KillMode=mixed"]);
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

    // --- jobs around OTA restarts and stops ---------------------------------

    fn queue_png(agent: &Agent, id: &str) {
        let job = PrintJob::from_dict(&obj(json!({"id": id, "cups_name": "P",
            "content_type": "png_base64", "content": PNG_1X1_B64})))
        .unwrap();
        agent.store.write_queue(&job).unwrap();
    }

    /// A release slot for `version` the units could run.
    fn install_slot(agent: &Agent, version: &str) {
        let slot = agent.update_env.install_root.join("releases").join(version);
        fs::create_dir_all(&slot).unwrap();
        fs::write(slot.join("vesyl-print"), b"#!/bin/sh\n").unwrap();
        crate::util::set_mode(&slot.join("vesyl-print"), 0o755).unwrap();
    }

    /// Times `lp` ran, each with whether jobs were paused then.
    type Prints = Arc<Mutex<Vec<(Instant, bool)>>>;

    fn recording_lp(agent: &mut Agent) -> Prints {
        let prints: Prints = Arc::default();
        let (p, status) = (prints.clone(), agent.cfg.update_status_path());
        agent.pipeline.lp = Arc::new(move |_, _, _| {
            let paused = update::should_pause_jobs_from_path(&status);
            lock(&p).push((Instant::now(), paused));
            Ok(None)
        });
        prints
    }

    /// The health gate an activation of the running version arms.
    fn gate_for_this_version() -> UpdateStatus {
        let mut pending = UpdateStatus::default();
        update::mark_pending_health(
            &mut pending,
            agent_version(),
            Some("0.0.1".into()),
            120,
            None,
        );
        pending
    }

    /// An agent an OTA just restarted into the running version: paired,
    /// `current` on a runnable slot of that version, `status` in
    /// update_status.json and job `q1` left queued. Its loop runs without
    /// the cable.
    fn agent_after_an_ota_restart(td: &Path, base_url: &str, status: &UpdateStatus) -> Agent {
        let mut agent = test_agent(td, base_url);
        agent.cfg.cable_enabled = false;
        pair(&agent);
        let ver = agent_version();
        install_slot(&agent, ver);
        update::flip_current(&agent.update_env.install_root, ver).unwrap();
        update::write_update_status(&agent.cfg.update_status_path(), status).unwrap();
        queue_png(&agent, "q1");
        agent
    }

    /// J1: after an OTA restart the health gate is open (pending_health) and
    /// jobs wait for it, but the startup drain did not: queued jobs printed
    /// before the gate had judged the new slot. They now wait for the gate,
    /// then print once, before the gate's heartbeat can start another OTA.
    #[test]
    fn queued_jobs_wait_for_the_health_gate_then_print_once() {
        queued_job_waits_for_the_gate(&gate_for_this_version());
    }

    /// J1 after a power loss: an install cut off after its flip leaves
    /// `installing`, which the agent's start marks failed and its first
    /// heartbeat turns back into the health gate
    /// (`update::recover_false_update_failure`). The startup drain went by
    /// the `failed` on disk and printed before that gate had run.
    #[test]
    fn an_install_cut_off_after_its_flip_holds_queued_jobs_for_its_gate() {
        let mut installing = UpdateStatus::with_status(update::STATUS_INSTALLING);
        installing.target_version = Some(agent_version().into());
        installing.previous_version = Some("0.0.1".into());
        queued_job_waits_for_the_gate(&installing);
    }

    /// Run [`agent_after_an_ota_restart`] with `status` until `q1` prints:
    /// it prints once, after the health gate's whoami and with jobs no
    /// longer paused, and the gate passes.
    fn queued_job_waits_for_the_gate(status: &UpdateStatus) {
        let td = tempfile::tempdir().unwrap();
        let srv = stub(|_, path| match path {
            "/print/v1/whoami" => (200, WHOAMI.into()),
            "/print/v1/jobs/pending" => (200, r#"{"jobs":[]}"#.into()),
            _ => (200, "{}".into()),
        });
        let mut agent = agent_after_an_ota_restart(td.path(), &srv.base_url, status);
        let prints = recording_lp(&mut agent);

        let (stop, handle) = start_run(&agent);
        let printed = eventually(Duration::from_secs(10), || !lock(&prints).is_empty());
        stop.store(true, Ordering::SeqCst);
        handle.join().unwrap();

        assert!(printed, "the queued job never printed");
        let prints = lock(&prints).clone();
        assert_eq!(prints.len(), 1, "printed once");
        let (at, paused) = prints[0];
        assert!(!paused, "printed while the gate was open");
        let whoami = srv.times("/print/v1/whoami");
        assert!(
            whoami.first().is_some_and(|w| *w < at),
            "printed before the gate's whoami"
        );
        let ust = update::read_update_status(&agent.cfg.update_status_path()).unwrap();
        assert_eq!(ust.status, update::STATUS_IDLE);
        assert!(agent.store.is_processed("q1"));
        assert!(!agent.store.has_pending_work());
    }

    /// J1: the heartbeat that closes the health gate must not start the
    /// next OTA ahead of the drain the gate held back. While that drain is
    /// still to run the update its reply asks for is deferred (`jobs_busy`),
    /// as for buffered push jobs: the job prints first, and the update waits
    /// for a later heartbeat.
    #[test]
    fn the_gates_heartbeat_defers_an_update_until_the_held_back_drain_ran() {
        let td = tempfile::tempdir().unwrap();
        // Every heartbeat reply asks for 99.0.0, its manifest on this stub.
        let base: Arc<OnceLock<String>> = Arc::default();
        let b = base.clone();
        let srv = stub(move |_, path| match path {
            "/print/v1/whoami" => (200, WHOAMI.into()),
            "/print/v1/jobs/pending" => (200, r#"{"jobs":[]}"#.into()),
            "/print/v1/heartbeat" => {
                let url = format!("{}/m.json", b.get().unwrap());
                let reply =
                    json!({"ok": true, "desired_agent_version": "99.0.0", "update_url": url});
                (200, reply.to_string())
            }
            "/m.json" => (404, "{}".into()),
            _ => (200, "{}".into()),
        });
        base.set(srv.base_url.clone()).unwrap();
        let mut agent =
            agent_after_an_ota_restart(td.path(), &srv.base_url, &gate_for_this_version());
        assert!(agent.cfg.auto_update_enabled);
        // The update status when `lp` ran.
        type AtPrint = Arc<Mutex<Vec<(Instant, Option<UpdateStatus>)>>>;
        let at_print: AtPrint = Arc::default();
        let (p, status) = (at_print.clone(), agent.cfg.update_status_path());
        agent.pipeline.lp = Arc::new(move |_, _, _| {
            lock(&p).push((Instant::now(), update::read_update_status(&status)));
            Ok(None)
        });

        let (stop, handle) = start_run(&agent);
        let printed = eventually(Duration::from_secs(10), || !lock(&at_print).is_empty());
        stop.store(true, Ordering::SeqCst);
        handle.join().unwrap();

        assert!(printed, "the queued job never printed");
        let at_print = lock(&at_print).clone();
        assert_eq!(at_print.len(), 1, "printed once");
        let (at, ust) = at_print[0].clone();
        let ust = ust.expect("no update status when the job printed");
        assert_eq!(
            (ust.status.as_str(), ust.last_error.as_deref()),
            (update::STATUS_IDLE, None),
            "an update started before the drain: {ust:?}"
        );
        let heartbeats = srv.times("/print/v1/heartbeat");
        assert!(
            heartbeats.first().is_some_and(|h| *h < at),
            "printed before the gate's heartbeat"
        );
        assert!(
            srv.times("/m.json").iter().all(|m| *m > at),
            "the update's manifest was fetched before the job printed"
        );
    }

    /// J4: a health gate that rolls back restarts the services, this agent
    /// with them. `rolled_back` does not pause jobs, so the loop took pushed
    /// and pulled jobs until the SIGTERM came, which could end a print
    /// midway. Such a heartbeat now holds jobs until the restart; a gate
    /// closed because `current` was switched by hand restarts nothing.
    #[test]
    fn a_rollback_that_restarts_this_agent_holds_its_jobs() {
        let td = tempfile::tempdir().unwrap();
        let mut agent = test_agent(td.path(), "http://127.0.0.1:9");
        let root = agent.update_env.install_root.clone();
        let status = agent.cfg.update_status_path();
        for v in ["9.9.8", "9.9.9"] {
            install_slot(&agent, v);
        }
        // A heartbeat whose gate closed 9.9.9 as rolled back, switching
        // `current` back to 9.9.8 (`flip`) or finding it switched by hand.
        let gate = |agent: &Agent, flip: bool| {
            update::flip_current(&root, "9.9.9").unwrap();
            *lock(&agent.shared.own_restart) = None;
            agent.heartbeat_step(|| {
                if flip {
                    update::flip_current(&root, "9.9.8").unwrap();
                }
                let mut st = UpdateStatus::with_status(update::STATUS_ROLLED_BACK);
                st.target_version = Some("9.9.9".into());
                update::write_update_status(&status, &st).unwrap();
                agent.run_once(false)
            });
            agent.jobs_paused()
        };
        assert!(!gate(&agent, true), "no restart without env.restart");
        agent.update_env.restart = true;
        assert!(!gate(&agent, false), "switched by hand: nothing restarted");
        assert!(
            gate(&agent, true),
            "jobs taken while the restart is on its way"
        );
        assert!(!update::should_pause_jobs_from_path(&status));
    }

    /// J4 in the loop: while its restart is on its way the agent takes no
    /// job (no drain, no pull). One that does not come within the grace is
    /// taken to have failed: jobs go on rather than never.
    #[test]
    fn the_loop_takes_no_job_until_its_restart_or_the_grace_is_over() {
        let td = tempfile::tempdir().unwrap();
        let srv = stub(|_, path| match path {
            "/print/v1/whoami" => (200, WHOAMI.into()),
            "/print/v1/jobs/pending" => (200, r#"{"jobs":[]}"#.into()),
            _ => (200, "{}".into()),
        });
        let mut agent = test_agent(td.path(), &srv.base_url);
        agent.cfg.cable_enabled = false;
        agent.cfg.pull_interval_seconds = 1;
        let grace = Duration::from_millis(1500);
        agent.shared = Arc::new(Shared {
            own_restart_grace: grace,
            ..Shared::default()
        });
        pair(&agent);
        let prints = recording_lp(&mut agent);
        queue_png(&agent, "q1");
        let asked = Instant::now();
        *lock(&agent.shared.own_restart) = Some(asked);

        let (stop, handle) = start_run(&agent);
        let resumed = eventually(Duration::from_secs(10), || {
            !lock(&prints).is_empty() && srv.count("/print/v1/jobs/pending") > 0
        });
        stop.store(true, Ordering::SeqCst);
        handle.join().unwrap();

        assert!(resumed, "jobs never went on after the grace");
        let first_pull = srv.times("/print/v1/jobs/pending")[0];
        let (printed, _) = lock(&prints)[0];
        for (what, at) in [("printed", printed), ("pulled", first_pull)] {
            assert!(
                at.duration_since(asked) >= grace,
                "{what} while waiting for the restart"
            );
        }
        assert_eq!(lock(&prints).len(), 1);
    }

    /// The pipeline used in the stop tests: CUPS keeps `Q-1` printing (out
    /// of paper) until the agent stops, or 10 s have passed (as the real
    /// wait gives up after its timeout).
    fn out_of_paper_until_stopped(agent: &mut Agent) -> mpsc::Receiver<()> {
        let (waiting_tx, waiting) = mpsc::channel();
        let waiting_tx = Mutex::new(waiting_tx);
        agent.cfg.wait_cups = WaitCups::Sync;
        agent.pipeline.lp = Arc::new(|_, _, _| Ok(Some("Q-1".into())));
        agent.pipeline.wait_cups_job = Arc::new(move |_, ctx| {
            let _ = lock(&waiting_tx).send(());
            let gives_up = Instant::now() + Duration::from_secs(10);
            while !ctx.stop.load(Ordering::SeqCst) && Instant::now() < gives_up {
                thread::sleep(Duration::from_millis(10));
            }
            CupsOutcome::Unknown
        });
        waiting
    }

    /// J3: a stop (SIGTERM) during the startup drain's CUPS wait (printer out
    /// of paper) used to wait for CUPS for up to 24 h, so systemd killed the
    /// agent after 90 s and the next start printed the label again. The
    /// wait ends at once now, the rest of the queue waits, and the next
    /// start finishes the job without printing it again.
    #[test]
    fn a_stop_during_the_drain_leaves_the_rest_and_never_prints_twice() {
        let td = tempfile::tempdir().unwrap();
        let mut agent = test_agent(td.path(), "http://127.0.0.1:9");
        agent.cfg.cable_enabled = false;
        let waiting = out_of_paper_until_stopped(&mut agent);
        queue_png(&agent, "q1");
        queue_png(&agent, "q2");

        let (stop, handle) = start_run(&agent);
        let in_wait = waiting.recv_timeout(Duration::from_secs(10)).is_ok();
        let stopped_at = Instant::now();
        stop.store(true, Ordering::SeqCst);
        handle.join().unwrap();
        assert!(in_wait, "the drain never waited on CUPS");
        assert!(
            stopped_at.elapsed() < Duration::from_secs(3),
            "{:?}",
            stopped_at.elapsed()
        );
        assert_eq!(agent.store.list_queued_ids(), ["q1", "q2"]);
        assert!(!agent.store.is_processed("q1"));

        // The next start: q1 is finished from what CUPS says, q2 printed.
        let printed: Arc<Mutex<Vec<String>>> = Arc::default();
        let pr = printed.clone();
        agent.pipeline.lp = Arc::new(move |_, path, _| {
            let id = path.file_stem().unwrap().to_string_lossy().to_string();
            lock(&pr).push(id);
            Ok(None)
        });
        agent.pipeline.cups_lookup = Arc::new(|key| {
            assert_eq!(key, "Q-1");
            Ok(jobs::CupsJobState::Printed)
        });
        agent.drain_local_queue(None, None);
        assert_eq!(*lock(&printed), ["q2"], "q1 must not print again");
        assert!(agent.store.is_processed("q1") && agent.store.is_processed("q2"));
        assert!(!agent.store.has_pending_work());
    }

    /// J10: the REST heartbeats a long CUPS wait sends carried no update
    /// status (the loop's do), so the cloud saw none while a job waited.
    #[test]
    fn wait_tick_heartbeats_carry_the_update_status() {
        let td = tempfile::tempdir().unwrap();
        let srv = serve(vec![(200, "{}")]);
        let agent = test_agent(td.path(), &srv.base_url);
        let mut st = UpdateStatus::with_status(update::STATUS_FAILED);
        st.target_version = Some("9.9.9".into());
        st.last_error = Some("download failed".into());
        update::write_update_status(&agent.cfg.update_status_path(), &st).unwrap();
        agent.inventory_wait_tick(None, Some("tok"))();
        let body: Value = serde_json::from_slice(&srv.requests.lock().unwrap()[0].body).unwrap();
        assert_eq!(body["update"]["status"], "failed");
        assert_eq!(body["update"]["target_version"], "9.9.9");
        assert_eq!(body["update"]["last_error"], "download failed");
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

    /// A stop that lands while the heartbeat is in flight (the request now
    /// finishes) starts no update its reply announces: the update would run
    /// on to its restart, and that restart replaces the stop.
    #[test]
    fn a_stop_during_the_heartbeat_starts_no_update() {
        let td = tempfile::tempdir().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let base: Arc<OnceLock<String>> = Arc::default();
        let (b, s) = (base.clone(), stop.clone());
        let srv = stub(move |_, path| match path {
            "/print/v1/whoami" => (200, WHOAMI.into()),
            "/print/v1/heartbeat" => {
                // systemctl stop, while the request is in flight.
                s.store(true, Ordering::SeqCst);
                (
                    200,
                    json!({"desired_agent_version": "9.9.9",
                           "update_url": format!("{}/manifest.json", b.get().unwrap())})
                    .to_string(),
                )
            }
            _ => (404, r#"{"error":"not found"}"#.into()),
        });
        base.set(srv.base_url.clone()).unwrap();
        let mut agent = test_agent(td.path(), &srv.base_url);
        agent.cfg.cable_enabled = false;
        pair(&agent);

        agent.run(stop);

        assert_eq!(srv.count("/print/v1/heartbeat"), 1);
        assert_eq!(srv.count("/manifest.json"), 0, "an update started");
        let st = statusio::read_status(&agent.cfg.status_path()).unwrap();
        assert_eq!(st.cloud, CloudState::Online);
        // Nothing recorded about it either: the next start decides afresh.
        assert!(update::read_update_status(&agent.cfg.update_status_path()).is_none());
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

    #[test]
    fn inventory_snapshot_is_published_for_the_display() {
        let td = tempfile::tempdir().unwrap();
        let mut agent = test_agent(td.path(), "http://127.0.0.1:9");
        agent.inventory = Arc::new(|| {
            Some(vec![
                json!({"cups_name": "Zebra", "status": "idle", "supports_raw": true}),
            ])
        });
        let stop = Arc::new(AtomicBool::new(false));
        let generation = agent.start_inventory_refresher(&stop);
        let path = agent.cfg.printers_path();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.is_file() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        stop.store(true, Ordering::SeqCst);
        agent.shared.inventory.end(generation);
        let snap: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(snap["printers"][0]["cups_name"], "Zebra");
        assert!(snap["updated_at"].as_str().is_some_and(|t| !t.is_empty()));
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&path).unwrap().permissions(),
        );
        assert_eq!(
            mode & 0o777,
            0o644,
            "the display user must be able to read it"
        );
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

    // --- printer setup (formerly the LCD display's job) ---------------------

    /// Poll `cond` until it holds or `within` passes; returns whether it held.
    fn eventually(within: Duration, cond: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + within;
        while !cond() {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(10));
        }
        true
    }

    /// A gate printer setup waits at until the test opens it.
    #[derive(Default)]
    struct Gate {
        open: Mutex<bool>,
        opened: Condvar,
    }

    impl Gate {
        fn pass(&self) {
            let open = lock(&self.open);
            let _ = self
                .opened
                .wait_timeout_while(open, Duration::from_secs(20), |open| !*open);
        }

        fn open(&self) {
            *lock(&self.open) = true;
            self.opened.notify_all();
        }
    }

    /// Printer setup held at `gate`: counts its runs and records the thread
    /// each ran on.
    fn gated_setup(gate: &Arc<Gate>) -> (ProvisionFn, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
        let runs = Arc::new(AtomicUsize::new(0));
        let threads: Arc<Mutex<Vec<String>>> = Arc::default();
        let (gate, r, t) = (gate.clone(), runs.clone(), threads.clone());
        let setup: ProvisionFn = Arc::new(move || {
            r.fetch_add(1, Ordering::SeqCst);
            lock(&t).push(thread::current().name().unwrap_or_default().to_string());
            gate.pass();
            vec!["Zebra ZD421".to_string()]
        });
        (setup, runs, threads)
    }

    /// Start `agent.run` on a thread; returns its stop flag and handle.
    fn start_run(agent: &Agent) -> (Arc<AtomicBool>, thread::JoinHandle<()>) {
        let stop = Arc::new(AtomicBool::new(false));
        let (a, s) = (agent.clone(), stop.clone());
        (stop, thread::spawn(move || a.run(s)))
    }

    /// Printer setup runs on its own thread, once per start: the loop goes on
    /// heartbeating and pulling (and stops promptly) while it is still
    /// running, a start during that run does not begin a second one, and a
    /// start after it finished runs it again.
    #[test]
    fn printer_setup_runs_once_per_start_off_the_agent_loop() {
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
        let gate = Arc::new(Gate::default());
        let (setup, runs, threads) = gated_setup(&gate);
        agent.provision_printers = setup;

        let (stop, handle) = start_run(&agent);
        assert!(eventually(Duration::from_secs(5), || runs
            .load(Ordering::SeqCst)
            == 1));
        // Setup is held; the loop still heartbeats and pulls, every second.
        assert!(
            eventually(Duration::from_secs(5), || srv
                .count("/print/v1/jobs/pending")
                >= 2),
            "the agent loop waited for printer setup"
        );
        assert!(srv.count("/print/v1/heartbeat") >= 1);
        assert_eq!(runs.load(Ordering::SeqCst), 1, "one pass per start");
        assert_eq!(*lock(&threads), ["vesyl-print-printer-setup"]);
        let t = Instant::now();
        stop.store(true, Ordering::SeqCst);
        handle.join().unwrap();
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());

        // Started again while that pass is still running: no second pass.
        let (stop, handle) = start_run(&agent);
        let pulls = srv.count("/print/v1/jobs/pending");
        assert!(eventually(Duration::from_secs(5), || srv
            .count("/print/v1/jobs/pending")
            > pulls));
        stop.store(true, Ordering::SeqCst);
        handle.join().unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 1, "two passes overlapped");

        // Once it has finished, the next start runs it again.
        gate.open();
        assert!(eventually(Duration::from_secs(5), || !agent
            .shared
            .printer_setup
            .load(Ordering::SeqCst)));
        let (stop, handle) = start_run(&agent);
        assert!(eventually(Duration::from_secs(5), || runs
            .load(Ordering::SeqCst)
            == 2));
        stop.store(true, Ordering::SeqCst);
        handle.join().unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    /// Queues printer setup adds reach the inventory (heartbeats, the LCD's
    /// printers.json) as soon as it finishes, not at the next 15 s tick.
    #[test]
    fn printer_setup_refreshes_the_inventory_when_done() {
        let td = tempfile::tempdir().unwrap();
        let mut agent = test_agent(td.path(), "http://127.0.0.1:9");
        let passes = Arc::new(AtomicUsize::new(0));
        let p = passes.clone();
        agent.inventory = Arc::new(move || {
            p.fetch_add(1, Ordering::SeqCst);
            Some(Vec::new())
        });
        let gate = Arc::new(Gate::default());
        let (setup, runs, _) = gated_setup(&gate);
        agent.provision_printers = setup;
        let stop = Arc::new(AtomicBool::new(false));
        let generation = agent.start_inventory_refresher(&stop);
        assert!(agent.start_printer_setup());

        assert!(eventually(Duration::from_secs(5), || passes
            .load(Ordering::SeqCst)
            == 1
            && runs.load(Ordering::SeqCst) == 1));
        thread::sleep(Duration::from_millis(300));
        assert_eq!(passes.load(Ordering::SeqCst), 1, "next pass is 15 s out");
        gate.open();
        assert!(
            eventually(Duration::from_secs(2), || passes.load(Ordering::SeqCst)
                == 2),
            "no inventory pass after printer setup"
        );
        stop.store(true, Ordering::SeqCst);
        agent.shared.inventory.end(generation);
    }

    /// A printer setup that panics, or whose thread the OS refuses, is
    /// logged; the agent carries on and a later start tries again.
    #[test]
    fn printer_setup_failures_are_contained() {
        let td = tempfile::tempdir().unwrap();
        let mut agent = test_agent(td.path(), "http://127.0.0.1:9");
        let runs = Arc::new(AtomicUsize::new(0));
        let r = runs.clone();
        agent.provision_printers = Arc::new(move || {
            r.fetch_add(1, Ordering::SeqCst);
            panic!("lpinfo exploded");
        });
        for expected in [1, 2] {
            run_for(&agent, Duration::from_millis(200));
            assert!(eventually(Duration::from_secs(5), || {
                runs.load(Ordering::SeqCst) == expected
                    && !agent.shared.printer_setup.load(Ordering::SeqCst)
            }));
        }
        // The loop itself kept going: status.json was written.
        assert_eq!(
            statusio::read_status(&agent.cfg.status_path())
                .unwrap()
                .pairing,
            PairingState::Unpaired
        );

        agent.shared = Arc::new(Shared {
            spawn: Arc::new(|_, _| Err(std::io::Error::from_raw_os_error(libc::EAGAIN))),
            ..Shared::default()
        });
        assert!(!agent.start_printer_setup());
        assert!(!agent.shared.printer_setup.load(Ordering::SeqCst));
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    // --- cable pushes wake the loop -----------------------------------------

    /// A loopback ActionCable endpoint for one agent session: it welcomes
    /// the client, confirms the subscription `confirm_after` the subscribe
    /// arrives, sends `message` on the channel 300 ms later, then reads the
    /// client's frames until it hangs up.
    struct CableEndpoint {
        url: String,
        /// When `message` was sent.
        pushed: mpsc::Receiver<Instant>,
        /// The text frames the client sent once subscribed, in order.
        frames: mpsc::Receiver<String>,
        /// When the client hung up.
        closed: mpsc::Receiver<Instant>,
        server: thread::JoinHandle<()>,
    }

    fn cable_endpoint(message: Value, confirm_after: Duration) -> CableEndpoint {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}/print/cable", listener.local_addr().unwrap());
        let (pushed_tx, pushed) = mpsc::channel();
        let (frames_tx, frames) = mpsc::channel();
        let (closed_tx, closed) = mpsc::channel();
        let server = thread::spawn(move || {
            // Never wait forever: a test that failed early stops the agent.
            listener.set_nonblocking(true).unwrap();
            let deadline = Instant::now() + Duration::from_secs(20);
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                    Err(_) => return,
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(20)))
                .unwrap();
            let mut ws = tungstenite::accept(stream).unwrap();
            let identifier = json!({ "channel": "PrintNodeChannel" }).to_string();
            ws.send(Message::text(r#"{"type":"welcome"}"#)).unwrap();
            let subscribe: Value =
                serde_json::from_str(ws.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(subscribe["command"], "subscribe");
            thread::sleep(confirm_after);
            let confirm = json!({"identifier": identifier, "type": "confirm_subscription"});
            ws.send(Message::text(confirm.to_string())).unwrap();
            thread::sleep(Duration::from_millis(300));
            let push = json!({"identifier": identifier, "message": message});
            ws.send(Message::text(push.to_string())).unwrap();
            let _ = pushed_tx.send(Instant::now());
            // Acks, job statuses and heartbeats, until the agent hangs up.
            while let Ok(frame) = ws.read() {
                if let Message::Text(text) = frame {
                    let _ = frames_tx.send(text.to_string());
                }
            }
            let _ = closed_tx.send(Instant::now());
        });
        CableEndpoint {
            url,
            pushed,
            frames,
            closed,
            server,
        }
    }

    /// The channel action and data of a client frame.
    fn performed(frame: &str) -> (String, Value) {
        let frame: Value = serde_json::from_str(frame).unwrap();
        let data: Value = serde_json::from_str(frame["data"].as_str().unwrap_or("{}")).unwrap();
        (py_str(&data["action"]), data)
    }

    /// J2: the cable can subscribe before the session's `start` returns
    /// (here it lingers 300 ms once its socket thread runs, as if preempted
    /// there). The agent put the session where its on_subscribed handler
    /// looks for it only once `start` had returned, so the first
    /// report_printers was never sent.
    #[test]
    fn the_first_report_printers_goes_out_however_soon_the_cable_subscribes() {
        let td = tempfile::tempdir().unwrap();
        let cable = cable_endpoint(json!({"type": "noop"}), Duration::ZERO);
        let mut agent = test_agent(td.path(), "http://127.0.0.1:9");
        agent.inventory = Arc::new(|| Some(vec![json!({"cups_name": "Zebra"})]));
        let holder: Arc<Mutex<Option<Arc<PrintCableSession>>>> = Arc::default();
        let handlers = agent.session_handlers(
            Arc::default(),
            Arc::default(),
            Arc::default(),
            holder.clone(),
            Arc::default(),
        );
        let sess = Arc::new(
            PrintCableSession::new(
                &cable.url,
                Box::new(|| Ok(obj(json!({"ticket": "t"})))),
                handlers,
            )
            .with_start_pause(Duration::from_millis(300)),
        );
        assert!(start_cable_session(&holder, &sess));
        let report = cable
            .frames
            .recv_timeout(Duration::from_secs(5))
            .map(|f| performed(&f));
        sess.stop();
        lock(&holder).take();
        cable.server.join().unwrap();
        let (action, data) = report.expect("no frame after the subscription");
        assert_eq!(action, "report_printers");
        assert_eq!(data["printers"], json!([{"cups_name": "Zebra"}]));
    }

    fn pushed_job(id: &str) -> Value {
        json!({"type": "print_job", "job": {
            "id": id, "cups_name": "P", "content_type": "png_base64", "content": PNG_1X1_B64,
        }})
    }

    /// A paired agent taking cable pushes: REST answers whoami and the ws
    /// ticket, the job pull is off (heartbeat every 30 s), and its `lp`
    /// reports when it prints.
    fn push_agent(td: &Path, cable_url: &str) -> (Agent, Stub, mpsc::Receiver<Instant>) {
        let srv = stub(|_, path| match path {
            "/print/v1/whoami" => (200, WHOAMI.into()),
            "/print/v1/ws_ticket" => (200, r#"{"ticket":"t"}"#.into()),
            _ => (200, "{}".into()),
        });
        let mut agent = test_agent(td, &srv.base_url);
        agent.cfg.pull_jobs_enabled = false;
        agent.cfg.cable_url = cable_url.into();
        let (printed_tx, printed) = mpsc::channel();
        agent.pipeline.lp = Arc::new(move |_, _, _| {
            let _ = printed_tx.send(Instant::now());
            Ok(None)
        });
        pair(&agent);
        (agent, srv, printed)
    }

    /// N13: a job pushed over the cable while the loop sleeps (job pull
    /// off, 30 s heartbeat) is taken at once, not at the next cycle.
    #[test]
    fn a_pushed_job_wakes_the_loop() {
        let td = tempfile::tempdir().unwrap();
        // Confirmed well after the first cycle went to sleep, so only a
        // wake-up can cut that sleep short.
        let cable = cable_endpoint(pushed_job("push-1"), Duration::from_millis(500));
        let (agent, _srv, printed) = push_agent(td.path(), &cable.url);

        let (stop, handle) = start_run(&agent);
        let pushed = cable.pushed.recv_timeout(Duration::from_secs(10));
        let taken = pushed
            .is_ok()
            .then(|| printed.recv_timeout(Duration::from_secs(5)));
        stop.store(true, Ordering::SeqCst);
        handle.join().unwrap();
        cable.server.join().unwrap();

        let pushed = pushed.expect("no job was pushed");
        let taken = taken
            .unwrap()
            .expect("the pushed job waited for the next cycle");
        let waited = taken.saturating_duration_since(pushed);
        assert!(waited < Duration::from_secs(2), "taken after {waited:?}");
        assert!(agent.store.is_processed("push-1"));
    }

    /// A revoke pushed over the cable is acted on at once too.
    #[test]
    fn a_pushed_revoke_wakes_the_loop() {
        let td = tempfile::tempdir().unwrap();
        let cable = cable_endpoint(json!({"type": "revoke"}), Duration::from_millis(500));
        let (agent, _srv, _) = push_agent(td.path(), &cable.url);
        let creds = agent.cfg.credentials_path();

        let (stop, handle) = start_run(&agent);
        let pushed = cable.pushed.recv_timeout(Duration::from_secs(10));
        let revoked = pushed.is_ok() && eventually(Duration::from_secs(2), || !creds.exists());
        stop.store(true, Ordering::SeqCst);
        handle.join().unwrap();
        cable.server.join().unwrap();

        pushed.expect("no revoke was pushed");
        assert!(revoked, "the revoke waited for the next cycle");
        let st = statusio::read_status(&agent.cfg.status_path()).unwrap();
        assert_eq!(st.pairing, PairingState::Revoked);
    }

    /// While a session is subscribed the loop comes round every second,
    /// whatever its pacing says (here: no pull, 30 s heartbeat). So it sees
    /// local changes within a second, like credentials removed by `unpair`,
    /// on which it closes the session.
    #[test]
    fn a_subscribed_loop_comes_round_every_second() {
        let td = tempfile::tempdir().unwrap();
        let cable = cable_endpoint(pushed_job("push-2"), Duration::ZERO);
        let (agent, _srv, printed) = push_agent(td.path(), &cable.url);

        let (stop, handle) = start_run(&agent);
        // The job is taken in a cycle that ran with the session subscribed;
        // the sleep after it is the one under test.
        let taken = cable
            .pushed
            .recv_timeout(Duration::from_secs(10))
            .is_ok_and(|_| printed.recv_timeout(Duration::from_secs(5)).is_ok());
        auth::clear_credentials(&agent.cfg.credentials_path()).unwrap();
        let closed = taken && cable.closed.recv_timeout(Duration::from_secs(4)).is_ok();
        stop.store(true, Ordering::SeqCst);
        handle.join().unwrap();
        cable.server.join().unwrap();

        assert!(taken, "the pushed job was not taken");
        assert!(closed, "the session stayed open after the credentials went");
    }

    /// A3: as root the agent would leave root-owned files in the service
    /// user's state and follow links that user can plant there. It refuses,
    /// and says how to run it instead.
    #[test]
    fn agent_refuses_to_run_as_root() {
        let td = tempfile::tempdir().unwrap();
        let state = td.path();
        assert_eq!(root_refusal(1000, state), None);
        let why = root_refusal(0, state).unwrap();
        assert!(why.contains("must not run as root"), "{why}");
        assert!(why.contains(&state.display().to_string()), "{why}");
        assert!(
            why.contains("`sudo systemctl start vesyl-print-agent`"),
            "{why}"
        );
        // The foreground hint names the state directory's owner (setup.sh
        // makes it the service user); when root owns it, or it is missing,
        // it can only ask for the service user.
        let owner = fs::metadata(state).unwrap().uid();
        let name = if owner == 0 {
            "<service user>".to_string()
        } else {
            let id = std::process::Command::new("id")
                .args(["-nu", &owner.to_string()])
                .output()
                .unwrap();
            if id.status.success() {
                String::from_utf8(id.stdout).unwrap().trim().to_string()
            } else {
                format!("'#{owner}'")
            }
        };
        assert!(
            why.contains(&format!("`sudo -u {name} vesyl-print agent`")),
            "{why}"
        );
        let why = root_refusal(0, &state.join("missing")).unwrap();
        assert!(
            why.contains("`sudo -u <service user> vesyl-print agent`"),
            "{why}"
        );
        // This process: refused exactly when it is root.
        let cfg = Config {
            state_dir: state.to_path_buf(),
            ..Config::default()
        };
        // SAFETY: geteuid has no preconditions.
        let root = unsafe { libc::geteuid() } == 0;
        assert_eq!(refuse_root(&cfg).is_err(), root);
    }

    #[test]
    fn printer_setup_summary_for_the_log() {
        assert_eq!(printer_setup_summary(&[]), "no printer queues");
        assert_eq!(
            printer_setup_summary(&["Zebra ZD421".into(), "HL-L3280CDW".into()]),
            "2 printer queue(s): Zebra ZD421, HL-L3280CDW"
        );
    }
}
