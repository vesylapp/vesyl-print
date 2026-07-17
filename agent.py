"""Cloud agent: whoami + heartbeat + job pull + ActionCable push."""

from __future__ import annotations

import logging
import queue
import signal
import threading
import time
from datetime import datetime, timezone
from typing import Any, Callable

import auth
import cable
import jobs
import printers
import statusio
import sysinfo
from cloud import CloudClient, CloudError
from config import AGENT_VERSION, Config, default_platform, load_config
from jobs import JobError, JobStore, PrintJob
import update as update_mod

log = logging.getLogger("vesyl-print.agent")


def _utc_now_iso() -> str:
    return datetime.now(timezone.utc).replace(microsecond=0).isoformat()


def _status_from_creds(
    creds: auth.Credentials | None,
    *,
    pairing: statusio.PairingState = "unpaired",
    cloud: statusio.CloudState = "unknown",
    last_error: str | None = None,
    last_heartbeat_at: str | None = None,
) -> statusio.AgentStatus:
    st = statusio.AgentStatus(
        pairing=pairing,
        cloud=cloud,
        last_error=last_error,
        last_heartbeat_at=last_heartbeat_at,
        agent_version=AGENT_VERSION,
    )
    if creds:
        st.node_id = creds.node_id
        st.name = creds.name
        st.organization_name = creds.organization_name
        # Multi-warehouse: comma-separated codes; single: name (see Credentials.warehouse_label).
        st.warehouse_name = creds.warehouse_label()
    return st


def _handle_unauthorized(cfg: Config, creds: auth.Credentials | None) -> None:
    """Revoke local pairing: clear credentials, write LCD status. No auto-reclaim."""
    log.warning("device token rejected (401) — re-pair required")
    st = _status_from_creds(creds, pairing="revoked", cloud="offline")
    st.last_error = "re-pair required"
    statusio.write_status(cfg.status_path, st)
    auth.clear_credentials(cfg.credentials_path)


def cloud_job_hooks(
    client: CloudClient,
    device_token: str,
    *,
    cable_session: cable.PrintCableSession | None = None,
) -> tuple[Callable[[PrintJob], None], Callable[[PrintJob, str, str | None], None]]:
    """Build ack / status callbacks — prefer ActionCable when subscribed, else REST."""

    def ack(job: PrintJob) -> None:
        if cable_session and cable_session.perform("ack_job", job_id=job.id):
            return
        client.ack_job(device_token, job.id)

    def report_state(job: PrintJob, status: str, detail: str | None = None) -> None:
        # printing | delivered (lp handoff) | printed (CUPS complete) | error
        if status not in ("printing", "delivered", "printed", "error"):
            return
        if cable_session and cable_session.perform(
            "job_status", job_id=job.id, status=status, message=detail
        ):
            return
        client.report_job_status(
            device_token, job.id, status, message=detail
        )

    return ack, report_state


def run_once(
    cfg: Config,
    client: CloudClient | None = None,
    *,
    jobs_busy: bool = False,
) -> statusio.AgentStatus:
    """Single REST heartbeat cycle. Updates status file for the LCD.

    ``jobs_busy``: when True, OTA download/install is deferred (queue non-empty
    or in-flight ActionCable jobs) so we never flip slots mid-print.
    """
    client = client or CloudClient(cfg.api_base_url)
    creds = auth.load_credentials(cfg.credentials_path)

    # Promote sticky false "failed" (self-restart SIGTERM) to pending_health
    # *before* whoami so the LCD never paints red "Update failed" mid-OTA.
    try:
        early = update_mod.read_update_status(cfg.update_status_path)
        if early and early.status == update_mod.STATUS_FAILED:
            recovered = update_mod.recover_false_update_failure(early, cfg=cfg)
            if recovered.status == update_mod.STATUS_PENDING_HEALTH:
                update_mod.write_update_status(cfg.update_status_path, recovered)
    except Exception:
        log.debug("early update-status recovery failed", exc_info=True)

    if not creds:
        # Unpaired: still complete post-update health (local slot checks only).
        update_status = update_mod.read_update_status(cfg.update_status_path)
        if update_status and update_status.status in (
            update_mod.STATUS_PENDING_HEALTH,
            update_mod.STATUS_FAILED,
        ):
            try:
                update_status = update_mod.process_pending_health(
                    update_status,
                    cfg=cfg,
                    whoami_result="skipped",
                )
                update_mod.write_update_status(cfg.update_status_path, update_status)
            except Exception:
                log.exception("post-update health gate failed")

        existing = statusio.read_status(cfg.status_path)
        if existing and existing.pairing == "revoked":
            st = existing
            st.cloud = "offline"
            st.agent_version = AGENT_VERSION
            statusio.write_status(cfg.status_path, st)
            return st
        st = _status_from_creds(None, pairing="unpaired", cloud="unknown")
        statusio.write_status(cfg.status_path, st)
        return st

    whoami_result = "skipped"
    whoami_error: str | None = None
    try:
        who = client.whoami(creds.device_token)
        creds = auth.merge_whoami(creds, who)
        auth.save_credentials(cfg.credentials_path, creds)
        whoami_result = "ok"
    except CloudError as e:
        if e.unauthorized:
            whoami_result = "unauthorized"
            # Still run health gate (API reached) before clearing credentials.
            update_status = update_mod.read_update_status(cfg.update_status_path)
            if update_status and update_status.status in (
                update_mod.STATUS_PENDING_HEALTH,
                update_mod.STATUS_FAILED,
            ):
                try:
                    ust = update_mod.process_pending_health(
                        update_status,
                        cfg=cfg,
                        whoami_result=whoami_result,
                        whoami_error=e.message,
                    )
                    update_mod.write_update_status(cfg.update_status_path, ust)
                except Exception:
                    log.exception("post-update health gate failed")
            _handle_unauthorized(cfg, creds)
            return statusio.read_status(cfg.status_path) or _status_from_creds(
                None, pairing="revoked", cloud="offline"
            )
        whoami_result = "error"
        whoami_error = e.message
        log.warning("whoami failed: %s", e.message)
    except Exception as e:
        whoami_result = "error"
        whoami_error = str(e)
        log.warning("whoami failed: %s", e)

    # Post-update health gate: declare OTA success only after whoami (or local
    # checks if unpaired). Also recover sticky "failed" after self-restart SIGTERM.
    update_status = update_mod.read_update_status(cfg.update_status_path)
    if update_status and update_status.status in (
        update_mod.STATUS_PENDING_HEALTH,
        update_mod.STATUS_FAILED,
    ):
        try:
            update_status = update_mod.process_pending_health(
                update_status,
                cfg=cfg,
                whoami_result=whoami_result,
                whoami_error=whoami_error,
            )
            update_mod.write_update_status(cfg.update_status_path, update_status)
            if update_status.status == update_mod.STATUS_ROLLED_BACK:
                log.warning(
                    "OTA health gate rolled back: %s", update_status.last_error
                )
                # Services restart after rollback; this process may be dying.
                return _status_from_creds(
                    creds,
                    pairing="paired",
                    cloud="online" if whoami_result == "ok" else "offline",
                    last_error=update_status.last_error,
                )
        except Exception:
            log.exception("post-update health gate failed")

    printers_payload = None
    try:
        printers_payload = printers.inventory_payload()
    except Exception:
        log.debug("printer inventory unavailable", exc_info=True)

    update_payload = None
    if update_status:
        update_payload = update_status.to_dict()

    try:
        hb = client.heartbeat(
            creds.device_token,
            agent_version=AGENT_VERSION,
            hostname=sysinfo.hostname(),
            printers=printers_payload,
            platform=default_platform(),
            update=update_payload,
        )
        last_hb = hb.get("last_seen_at") or _utc_now_iso()
        st = _status_from_creds(
            creds,
            pairing="paired",
            cloud="online",
            last_heartbeat_at=str(last_hb),
        )
        statusio.write_status(cfg.status_path, st)

        # OTA plan A: desired version + optional update_url on heartbeat response.
        try:
            ust = update_mod.maybe_update_from_heartbeat(
                hb,
                cfg=cfg,
                status=update_status,
                auto_apply=bool(getattr(cfg, "auto_update_enabled", True)),
                status_path=cfg.update_status_path,
                jobs_busy=jobs_busy,
            )
            update_mod.write_update_status(cfg.update_status_path, ust)
        except Exception:
            log.exception("update check failed")

        return st
    except CloudError as e:
        if e.unauthorized:
            _handle_unauthorized(cfg, creds)
            return statusio.read_status(cfg.status_path) or _status_from_creds(
                None, pairing="revoked", cloud="offline"
            )
        log.warning("heartbeat failed: %s", e.message)
        st = _status_from_creds(
            creds,
            pairing="paired",
            cloud="offline",
            last_error=e.message,
        )
        prev = statusio.read_status(cfg.status_path)
        if prev and prev.last_heartbeat_at:
            st.last_heartbeat_at = prev.last_heartbeat_at
        statusio.write_status(cfg.status_path, st)
        return st
    except Exception as e:
        log.warning("heartbeat failed: %s", e)
        st = _status_from_creds(
            creds, pairing="paired", cloud="offline", last_error=str(e)
        )
        statusio.write_status(cfg.status_path, st)
        return st


def _inventory_wait_tick(
    cable_session: cable.PrintCableSession | None,
    *,
    client: CloudClient | None = None,
    device_token: str | None = None,
) -> Callable[[], None]:
    """Build on_wait_tick that re-reports CUPS status while a job is held.

    Prefer ActionCable report_printers; fall back to REST heartbeat when the
    cable is down so admin still sees out-of-paper / jam during long waits.
    """
    last = {"t": 0.0}
    min_interval = 10.0

    def tick() -> None:
        now = time.monotonic()
        if now - last["t"] < min_interval:
            return
        last["t"] = now
        try:
            inv = printers.inventory_payload()
        except Exception:
            log.debug("wait-tick inventory failed", exc_info=True)
            return
        if cable_session is not None and cable_session.subscribed:
            if cable_session.perform("report_printers", printers=inv):
                return
        if client is not None and device_token:
            try:
                client.heartbeat(
                    device_token,
                    agent_version=AGENT_VERSION,
                    hostname=sysinfo.hostname(),
                    printers=inv,
                )
            except Exception:
                log.debug("wait-tick REST inventory failed", exc_info=True)

    return tick


def drain_local_queue(
    cfg: Config,
    *,
    client: CloudClient | None = None,
    device_token: str | None = None,
    cable_session: cable.PrintCableSession | None = None,
) -> None:
    """Crash recovery: finish any jobs left in queue/*.json."""
    store = jobs.store_from_config(cfg)
    pending = store.list_queued_ids()
    if not pending:
        return
    log.info("draining %d queued job(s)", len(pending))

    ack: Callable[[PrintJob], None] = jobs.noop_ack
    report_state: Callable[[PrintJob, str, str | None], None] = jobs.noop_state
    if client is not None and device_token:
        ack, report_state = cloud_job_hooks(
            client, device_token, cable_session=cable_session
        )

    results = jobs.drain_queue(
        store,
        ack=ack,
        report_state=report_state,
        on_wait_tick=_inventory_wait_tick(
            cable_session, client=client, device_token=device_token
        ),
    )
    for job_id, result in results:
        log.info("drain %s → %s", job_id, result)


def pull_and_process(
    cfg: Config,
    client: CloudClient,
    device_token: str,
    store: JobStore | None = None,
    *,
    cable_session: cable.PrintCableSession | None = None,
) -> str:
    """Pull pending jobs from the cloud and run the durable print pipeline."""
    store = store or jobs.store_from_config(cfg)
    try:
        payloads = client.pending_jobs(device_token)
    except CloudError as e:
        if e.unauthorized:
            return "unauthorized"
        if e.not_found or e.service_disabled:
            log.warning(
                "jobs/pending unavailable (HTTP %s): %s — pull will retry",
                e.status,
                e.message,
            )
            return "unavailable"
        log.warning("jobs/pending failed: %s", e.message)
        return "error"
    except Exception as e:
        log.warning("jobs/pending failed: %s", e)
        return "error"

    if not payloads:
        return "ok"

    log.info("pulled %d job(s)", len(payloads))
    ack, report_state = cloud_job_hooks(
        client, device_token, cable_session=cable_session
    )
    on_wait_tick = _inventory_wait_tick(
        cable_session, client=client, device_token=device_token
    )

    for payload in payloads:
        try:
            job = PrintJob.from_dict(payload)
        except JobError as e:
            log.error("skip invalid job payload: %s", e.message)
            continue
        try:
            jobs.receive_job(
                job,
                store,
                ack=ack,
                report_state=report_state,
                on_wait_tick=on_wait_tick,
            )
        except JobError as e:
            log.error("job %s failed: %s", job.id, e.message)
        except Exception:
            log.exception("job %s failed unexpectedly", getattr(job, "id", "?"))

    return "ok"


def process_job_payload(
    payload: dict[str, Any],
    cfg: Config,
    client: CloudClient,
    device_token: str,
    *,
    cable_session: cable.PrintCableSession | None = None,
    store: JobStore | None = None,
) -> None:
    """Handle one job dict (from pull or ActionCable print_job)."""
    store = store or jobs.store_from_config(cfg)
    try:
        job = PrintJob.from_dict(payload)
    except JobError as e:
        log.error("skip invalid job payload: %s", e.message)
        return
    ack, report_state = cloud_job_hooks(
        client, device_token, cable_session=cable_session
    )
    try:
        jobs.receive_job(
            job,
            store,
            ack=ack,
            report_state=report_state,
            on_wait_tick=_inventory_wait_tick(
                cable_session, client=client, device_token=device_token
            ),
        )
    except JobError as e:
        log.error("job %s failed: %s", job.id, e.message)
    except Exception:
        log.exception("job %s failed unexpectedly", job.id)


def handle_job_canceled(job_id: str, store: JobStore) -> None:
    """Stop local work for a canceled job; do not print if still queued."""
    if store.is_processed(job_id):
        store.delete_queue(job_id)
        return
    if store.has_queue_file(job_id):
        log.info("job %s canceled — dropping queue file", job_id)
        store.delete_queue(job_id)
    # Marker prevents a late redelivery/print of a canceled job.
    store.mark_processed(job_id)


def run_agent(cfg: Config | None = None) -> None:
    """Long-running agent: REST heartbeat/pull + optional ActionCable push."""
    cfg = cfg or load_config()
    cfg.ensure_dirs()
    client = CloudClient(cfg.api_base_url)
    store = jobs.store_from_config(cfg)

    running = {"go": True}
    signal.signal(signal.SIGINT, lambda *_: running.update(go=False))
    signal.signal(signal.SIGTERM, lambda *_: running.update(go=False))

    log.info(
        "agent starting api_base_url=%s cable=%s pull_jobs=%s "
        "heartbeat=%ss pull_interval=%ss ws_available=%s",
        cfg.api_base_url,
        cfg.cable_enabled,
        cfg.pull_jobs_enabled,
        cfg.heartbeat_seconds,
        cfg.pull_interval_seconds,
        cable.websocket_available(),
    )

    # --- ActionCable session (push) ----------------------------------------
    push_jobs: queue.Queue[dict[str, Any]] = queue.Queue()
    revoke_flag = threading.Event()
    cable_session_holder: dict[str, cable.PrintCableSession | None] = {"s": None}

    def on_print_job(job_payload: dict[str, Any]) -> None:
        push_jobs.put(job_payload)

    def on_revoke() -> None:
        revoke_flag.set()

    def on_job_canceled(job_id: str) -> None:
        try:
            handle_job_canceled(job_id, store)
        except Exception:
            log.exception("job_canceled handler failed")

    def apply_node_config(msg: dict[str, Any]) -> None:
        """Apply ActionCable node_config (warehouses/name) to creds + LCD status."""
        creds_now = auth.load_credentials(cfg.credentials_path)
        if not creds_now:
            return
        try:
            updated = auth.merge_whoami(creds_now, msg)
            auth.save_credentials(cfg.credentials_path, updated)
            st = _status_from_creds(
                updated,
                pairing="paired",
                cloud="online",
            )
            prev = statusio.read_status(cfg.status_path)
            if prev and prev.last_heartbeat_at:
                st.last_heartbeat_at = prev.last_heartbeat_at
            statusio.write_status(cfg.status_path, st)
            log.info(
                "node_config applied warehouses=%s name=%s",
                updated.warehouse_label(),
                updated.name,
            )
        except Exception:
            log.exception("failed to apply node_config")

    def report_printers_now(sess: cable.PrintCableSession) -> None:
        """Push CUPS inventory over the cable so admin sees printers promptly."""
        try:
            inv = printers.inventory_payload()
        except Exception:
            log.debug("printer inventory unavailable", exc_info=True)
            return
        if inv is None:
            return
        sess.perform("report_printers", printers=inv)

    def get_ticket() -> dict[str, Any]:
        creds_now = auth.load_credentials(cfg.credentials_path)
        if not creds_now:
            raise RuntimeError("no credentials")
        return client.ws_ticket(creds_now.device_token)

    def ensure_cable(creds: auth.Credentials) -> cable.PrintCableSession | None:
        """Start cable in the background. Never block on subscribe/stop."""
        if not cfg.cable_enabled or not cable.websocket_available():
            return None
        sess = cable_session_holder["s"]
        if sess and sess.subscribed:
            return sess
        if sess and sess.connected:
            # Handshake in progress — leave it alone.
            return sess
        if sess:
            # Dead/failed session — tear down without blocking the heartbeat loop.
            try:
                sess.stop()
            except Exception:
                pass
            cable_session_holder["s"] = None

        def on_subscribed() -> None:
            log.info("cable PrintNodeChannel ready")
            s = cable_session_holder["s"]
            if s:
                report_printers_now(s)

        sess = cable.PrintCableSession(
            cable_url=cfg.cable_url,
            get_ticket=get_ticket,
            on_print_job=on_print_job,
            on_revoke=on_revoke,
            on_job_canceled=on_job_canceled,
            on_node_config=apply_node_config,
            on_subscribed=on_subscribed,
            on_disconnected=lambda: log.info("cable disconnected"),
        )
        try:
            if sess.start():
                cable_session_holder["s"] = sess
                return sess
        except Exception:
            log.exception("cable start failed")
        return None

    creds = auth.load_credentials(cfg.credentials_path)
    token = creds.device_token if creds else None
    try:
        drain_local_queue(cfg, client=client, device_token=token)
    except Exception:
        log.exception("queue drain failed")

    hb_interval = max(5, int(cfg.heartbeat_seconds))
    pull_interval = max(1, int(cfg.pull_interval_seconds))
    # When cable is healthy, pull less often (safety net only).
    pull_interval_when_cabled = max(pull_interval * 6, 30)
    # Report printers / channel heartbeat more often than REST whoami so admin
    # inventory stays fresher while WS is up (REST still at hb_interval).
    cable_hb_interval = max(10, min(hb_interval, 15))
    last_hb = 0.0
    last_pull = 0.0
    last_cable_try = 0.0
    last_cable_hb = 0.0
    backoff = 1.0
    max_backoff = 60.0
    pull_disabled_until = 0.0
    cable_retry_after = 0.0

    try:
        while running["go"]:
            now = time.monotonic()
            cycle_start = now
            creds = auth.load_credentials(cfg.credentials_path)
            sess = cable_session_holder["s"]

            if revoke_flag.is_set():
                revoke_flag.clear()
                if sess:
                    sess.stop()
                    cable_session_holder["s"] = None
                if creds:
                    _handle_unauthorized(cfg, creds)
                creds = None
                sess = None

            # Pause pull/push while OTA is downloading, installing, or in health gate.
            ota_pause_jobs = update_mod.should_pause_jobs_from_path(
                cfg.update_status_path
            )

            # Drain push job queue on the main thread (lp must not run on WS thread).
            # Skip while OTA is active so we never start a print that restart would kill.
            if not ota_pause_jobs:
                while True:
                    try:
                        payload = push_jobs.get_nowait()
                    except queue.Empty:
                        break
                    if creds:
                        process_job_payload(
                            payload,
                            cfg,
                            client,
                            creds.device_token,
                            cable_session=sess,
                            store=store,
                        )
            elif push_jobs.qsize() > 0:
                log.debug(
                    "OTA in progress — holding %d ActionCable job(s)",
                    push_jobs.qsize(),
                )

            # REST heartbeat first — cable must never block liveness / LCD status.
            # Defer OTA if durable queue or buffered push jobs still have work.
            jobs_busy = store.has_pending_work() or push_jobs.qsize() > 0
            do_hb = (now - last_hb) >= hb_interval or last_hb == 0.0
            st: statusio.AgentStatus | None = None
            if do_hb:
                st = run_once(cfg, client, jobs_busy=jobs_busy)
                last_hb = time.monotonic()
                # Re-read after OTA may have entered downloading/installing/pending_health.
                ota_pause_jobs = update_mod.should_pause_jobs_from_path(
                    cfg.update_status_path
                )
                if st.cloud == "online":
                    backoff = 1.0
                elif st.pairing == "paired" and st.cloud == "offline":
                    backoff = min(max_backoff, max(backoff, 1.0) * 2)
            else:
                st = statusio.read_status(cfg.status_path)

            # Maintain cable in the background when paired (non-blocking).
            if creds and cfg.cable_enabled and cable.websocket_available():
                need = sess is None or not sess.connected
                if need and now >= last_cable_try + 5.0 and now >= cable_retry_after:
                    last_cable_try = now
                    try:
                        sess = ensure_cable(creds)
                        if sess is None:
                            cable_retry_after = now + min(60.0, max(10.0, backoff * 3))
                            backoff = min(max_backoff, backoff * 2)
                    except Exception:
                        log.exception("cable ensure failed")
                        cable_retry_after = now + 30.0
                        sess = cable_session_holder["s"]
                elif sess and sess.subscribed:
                    backoff = 1.0
                    # Channel heartbeat + printer inventory more often than REST.
                    if (now - last_cable_hb) >= cable_hb_interval:
                        try:
                            inv = printers.inventory_payload()
                        except Exception:
                            inv = None
                        if sess.perform(
                            "heartbeat",
                            agent_version=AGENT_VERSION,
                            hostname=sysinfo.hostname(),
                            printers=inv,
                        ):
                            last_cable_hb = time.monotonic()
            else:
                if sess:
                    try:
                        sess.stop()
                    except Exception:
                        pass
                    cable_session_holder["s"] = None
                    sess = None

            # REST pull safety net (paused during OTA install / health gate)
            pull_every = (
                pull_interval_when_cabled
                if (sess and sess.subscribed)
                else pull_interval
            )
            if ota_pause_jobs:
                if cfg.pull_jobs_enabled and creds:
                    log.debug("OTA in progress — pausing job pull")
            elif (
                cfg.pull_jobs_enabled
                and creds
                and (st is None or st.pairing == "paired")
                and time.monotonic() >= pull_disabled_until
                and (time.monotonic() - last_pull) >= pull_every
            ):
                result = pull_and_process(
                    cfg,
                    client,
                    creds.device_token,
                    store=store,
                    cable_session=sess,
                )
                last_pull = time.monotonic()
                if result == "unauthorized":
                    _handle_unauthorized(cfg, creds)
                    if sess:
                        sess.stop()
                        cable_session_holder["s"] = None
                elif result == "unavailable":
                    pull_disabled_until = time.monotonic() + min(
                        60.0, max(15.0, backoff * 5)
                    )
                elif result == "error":
                    pull_disabled_until = time.monotonic() + min(30.0, backoff)

            # Sleep
            if push_jobs.qsize() > 0:
                sleep_for = 0.05
            elif st and st.pairing == "unpaired":
                sleep_for = min(10.0, float(hb_interval))
            elif st and st.pairing == "revoked":
                sleep_for = min(30.0, float(hb_interval))
            elif st and st.cloud == "offline":
                # Reconnect quickly after failures; still respect backoff cap.
                sleep_for = min(max_backoff, max(5.0, backoff))
            elif cfg.pull_jobs_enabled and creds:
                sleep_for = min(
                    float(pull_interval),
                    1.0 if (sess and sess.subscribed) else float(pull_interval),
                )
            else:
                sleep_for = float(hb_interval)

            if last_hb > 0:
                until_hb = hb_interval - (time.monotonic() - last_hb)
                if until_hb > 0:
                    sleep_for = min(sleep_for, until_hb)

            elapsed = time.monotonic() - cycle_start
            time.sleep(max(0.0, sleep_for - elapsed))
    finally:
        sess = cable_session_holder["s"]
        if sess:
            sess.stop()


def main() -> None:
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    run_agent(load_config())


if __name__ == "__main__":
    main()
