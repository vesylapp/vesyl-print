//! Cloud agent: whoami + heartbeat + job pull + ActionCable push.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::auth::{self, Credentials};
use crate::cable::{PrintCableSession, SessionHandlers};
use crate::cloud::{CloudClient, CloudError, HeartbeatBody};
use crate::config::{agent_version, default_platform, Config};
use crate::jobs::{AckFn, JobState, JobStore, Pipeline, PrintJob, StateFn, TickFn};
use crate::statusio::{self, AgentStatus, CloudState, PairingState};
use crate::update::{self, UpdateEnv, UpdateStatus, WhoamiResult};
use crate::{printers, sysinfo, JsonObject};

const LOG: &str = "vesyl-print.agent";

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
pub fn handle_job_canceled(job_id: &str, store: &JobStore) -> std::io::Result<()> {
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

#[derive(Clone)]
pub struct Agent {
    pub cfg: Config,
    pub client: CloudClient,
    pub store: JobStore,
    pub inventory: InventoryFn,
    pub update_env: UpdateEnv,
    /// Base job pipeline (lp, fetch, CUPS wait, raw probe). Cloud hooks and
    /// `wait_cups` are filled in per job.
    pub pipeline: Pipeline,
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
    /// `jobs_busy`: when true, OTA download/install is deferred (queue
    /// non-empty or in-flight ActionCable jobs) so we never flip slots mid-print.
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
            printers: (self.inventory)(),
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

    /// on_wait_tick for long CUPS waits (out of paper, jam, etc.).
    ///
    /// Job processing blocks the agent loop, so this keeps **REST heartbeats**
    /// flowing (what the web uses for last_seen / offline detection) and
    /// refreshes printer inventory over the cable when available.
    pub fn inventory_wait_tick(&self, cable: Cable, device_token: Option<&str>) -> TickFn {
        // Match configured heartbeat (default 30s); never slower than 10s for liveness.
        let rest_interval = Duration::from_secs_f64((self.cfg.heartbeat_seconds as f64).max(10.0));
        let cable_interval = rest_interval.min(Duration::from_secs(15));
        let last: Mutex<(Option<Instant>, Option<Instant>)> = Mutex::new((None, None));
        let (client, inventory) = (self.client.clone(), self.inventory.clone());
        let token = device_token.map(String::from);
        Arc::new(move || {
            let now = Instant::now();
            let due = |t: Option<Instant>, every: Duration| t.is_none_or(|t| now - t >= every);
            let inv = inventory();
            let mut last = last.lock().unwrap();

            // Optional: keep ActionCable path warm (does not replace REST last_seen).
            if let Some(c) = cable.as_ref().filter(|c| c.subscribed()) {
                if due(last.1, cable_interval) {
                    last.1 = Some(now);
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
            }

            // Always REST-heartbeat on the normal schedule while blocked on CUPS.
            if let Some(token) = token.as_deref().filter(|t| !t.is_empty()) {
                if due(last.0, rest_interval) {
                    last.0 = Some(now);
                    let body = HeartbeatBody {
                        agent_version: Some(agent_version().into()),
                        hostname: Some(sysinfo::hostname()),
                        printers: inv,
                        platform: Some(default_platform()),
                        update: None,
                    };
                    if let Err(e) = client.heartbeat(token, &body) {
                        log::debug!(target: LOG, "wait-tick REST heartbeat failed: {e}");
                    }
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
        p.on_wait_tick = Some(self.inventory_wait_tick(cable, device_token));
        p.wait_cups = self.cfg.wait_cups;
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
        let job = match PrintJob::from_dict(payload) {
            Ok(j) => j,
            Err(e) => {
                log::error!(target: LOG, "skip invalid job payload: {}", e.message);
                return;
            }
        };
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

        // --- ActionCable session (push) ------------------------------------
        let push_jobs: Arc<Mutex<VecDeque<JsonObject>>> = Arc::default();
        let revoke_flag = Arc::new(AtomicBool::new(false));
        let holder: Arc<Mutex<Option<Arc<PrintCableSession>>>> = Arc::default();
        let current_cable = |h: &Mutex<Option<Arc<PrintCableSession>>>| -> Cable {
            h.lock()
                .unwrap()
                .clone()
                .map(|s| s as Arc<dyn CableChannel>)
        };

        let ensure_cable = |agent: &Agent| -> Option<Arc<PrintCableSession>> {
            {
                let mut slot = holder.lock().unwrap();
                if let Some(s) = slot.as_ref() {
                    // Subscribed, or handshake in progress — leave it alone.
                    // (Python only checked `connected`, which is set on `welcome`,
                    // so a slow loop iteration could tear down a session that
                    // was still connecting.)
                    if s.subscribed() || s.connected() || s.connecting() {
                        return Some(s.clone());
                    }
                }
                // Dead/failed session — tear down.
                if let Some(old) = slot.take() {
                    old.stop();
                }
            }
            let handlers =
                agent.session_handlers(push_jobs.clone(), revoke_flag.clone(), holder.clone());
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
                *holder.lock().unwrap() = Some(sess.clone());
                return Some(sess);
            }
            None
        };
        let stop_cable = || {
            let old = holder.lock().unwrap().take();
            if let Some(s) = old {
                s.stop();
            }
        };

        let token = auth::load_credentials(&cfg.credentials_path()).map(|c| c.device_token);
        self.drain_local_queue(token.as_deref(), None);

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
        let mut last_cable_try: Option<Instant> = None;
        let mut last_cable_hb: Option<Instant> = None;
        let mut pull_disabled_until: Option<Instant> = None;
        let mut cable_retry_after: Option<Instant> = None;
        let elapsed_since =
            |t: Option<Instant>, every: Duration| t.is_none_or(|t| t.elapsed() >= every);

        while !stop.load(Ordering::SeqCst) {
            let cycle_start = Instant::now();
            let now = cycle_start;
            let mut creds = auth::load_credentials(&cfg.credentials_path());
            let mut sess = holder.lock().unwrap().clone();

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
                    let next = push_jobs.lock().unwrap().pop_front();
                    let Some(payload) = next else { break };
                    if let Some(c) = &creds {
                        self.process_job_payload(&payload, &c.device_token, current_cable(&holder));
                    }
                }
            } else if !push_jobs.lock().unwrap().is_empty() {
                log::debug!(target: LOG, "OTA in progress — holding {} ActionCable job(s)", push_jobs.lock().unwrap().len());
            }

            // REST heartbeat first — cable must never block liveness / LCD status.
            // Defer OTA if durable queue or buffered push jobs still have work.
            let jobs_busy = self.store.has_pending_work() || !push_jobs.lock().unwrap().is_empty();
            let st = if elapsed_since(last_hb, hb_interval) {
                let st = self.run_once(jobs_busy);
                last_hb = Some(Instant::now());
                // Re-read: OTA may have entered downloading/installing/pending_health.
                ota_pause = update::should_pause_jobs_from_path(&cfg.update_status_path());
                if st.cloud == CloudState::Online {
                    backoff = 1.0;
                } else if st.pairing == PairingState::Paired && st.cloud == CloudState::Offline {
                    backoff = (backoff.max(1.0) * 2.0).min(max_backoff);
                }
                Some(st)
            } else {
                statusio::read_status(&cfg.status_path())
            };

            // Maintain cable in the background when paired (non-blocking).
            if creds.is_some() && cfg.cable_enabled {
                let need = sess.as_ref().is_none_or(|s| !s.connected());
                let try_due = elapsed_since(last_cable_try, secs(5.0))
                    && cable_retry_after.is_none_or(|t| now >= t);
                if need && try_due {
                    sess = ensure_cable(self);
                    // Measure the retry gap from the attempt itself, not the
                    // cycle start: run_once can take ~15 s on a Pi (CUPS inventory).
                    last_cable_try = Some(Instant::now());
                    if sess.is_none() {
                        cable_retry_after = Some(now + secs((backoff * 3.0).clamp(10.0, 60.0)));
                        backoff = (backoff * 2.0).min(max_backoff);
                    }
                } else if let Some(s) = sess.as_ref().filter(|s| s.subscribed()) {
                    backoff = 1.0;
                    if elapsed_since(last_cable_hb, cable_hb_interval) {
                        let inv = (self.inventory)();
                        let data = obj(json!({
                            "agent_version": agent_version(),
                            "hostname": sysinfo::hostname(),
                            "printers": inv,
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
                    let result = self.pull_and_process(&c.device_token, current_cable(&holder));
                    last_pull = Some(Instant::now());
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

            // Sleep
            let mut sleep_for = if !push_jobs.lock().unwrap().is_empty() {
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
        log::info!(target: LOG, "agent stopped");
    }

    fn session_handlers(
        &self,
        push_jobs: Arc<Mutex<VecDeque<JsonObject>>>,
        revoke_flag: Arc<AtomicBool>,
        holder: Arc<Mutex<Option<Arc<PrintCableSession>>>>,
    ) -> SessionHandlers {
        let store = self.store.clone();
        let cfg = self.cfg.clone();
        let inventory = self.inventory.clone();
        SessionHandlers {
            on_print_job: Some(Arc::new(move |job| {
                push_jobs.lock().unwrap().push_back(job)
            })),
            on_revoke: Some(Arc::new(move || revoke_flag.store(true, Ordering::SeqCst))),
            on_job_canceled: Some(Arc::new(move |job_id| {
                if let Err(e) = handle_job_canceled(&job_id, &store) {
                    log::error!(target: LOG, "job_canceled handler failed: {e}");
                }
            })),
            on_node_config: Some(Arc::new(move |msg| apply_node_config(&cfg, &msg))),
            on_subscribed: Some(Arc::new(move || {
                log::info!(target: LOG, "cable PrintNodeChannel ready");
                // Push CUPS inventory so admin sees printers promptly.
                let sess = holder.lock().unwrap().clone();
                if let (Some(s), Some(inv)) = (sess, inventory()) {
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
    use std::path::Path;

    const CLAIM: &str = r#"{
        "node_id": "node-uuid-1",
        "device_token": "secret-device-token-do-not-log",
        "name": "Pack station 1",
        "warehouse": {"id": "wh-1", "name": "Main Warehouse", "code": "MAIN"},
        "organization": {"id": "org-1", "name": "Acme Corp", "slug": "acme"}
    }"#;

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
}
