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
from config import AGENT_VERSION, Config, load_config
from jobs import JobError, JobStore, PrintJob

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
        st.warehouse_name = creds.warehouse_name
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
    """Build ack / state callbacks — prefer ActionCable when subscribed, else REST."""

    def ack(job: PrintJob) -> None:
        if cable_session and cable_session.perform("ack_job", job_id=job.id):
            return
        client.ack_job(device_token, job.id)

    def report_state(job: PrintJob, state: str, detail: str | None = None) -> None:
        # Server accepts only done|error from agents (not "printing").
        if state not in ("done", "error"):
            return
        if cable_session and cable_session.perform(
            "job_state", job_id=job.id, state=state, message=detail
        ):
            return
        client.report_job_state(
            device_token, job.id, state, message=detail
        )

    return ack, report_state


def run_once(cfg: Config, client: CloudClient | None = None) -> statusio.AgentStatus:
    """Single REST heartbeat cycle. Updates status file for the LCD."""
    client = client or CloudClient(cfg.api_base_url)
    creds = auth.load_credentials(cfg.credentials_path)

    if not creds:
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

    try:
        who = client.whoami(creds.device_token)
        creds = auth.merge_whoami(creds, who)
        auth.save_credentials(cfg.credentials_path, creds)
    except CloudError as e:
        if e.unauthorized:
            _handle_unauthorized(cfg, creds)
            return statusio.read_status(cfg.status_path) or _status_from_creds(
                None, pairing="revoked", cloud="offline"
            )
        log.warning("whoami failed: %s", e.message)
    except Exception as e:
        log.warning("whoami failed: %s", e)

    printers_payload = None
    try:
        printers_payload = printers.inventory_payload()
    except Exception:
        log.debug("printer inventory unavailable", exc_info=True)

    try:
        hb = client.heartbeat(
            creds.device_token,
            agent_version=AGENT_VERSION,
            hostname=sysinfo.hostname(),
            printers=printers_payload,
        )
        last_hb = hb.get("last_seen_at") or _utc_now_iso()
        st = _status_from_creds(
            creds,
            pairing="paired",
            cloud="online",
            last_heartbeat_at=str(last_hb),
        )
        statusio.write_status(cfg.status_path, st)
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

    results = jobs.drain_queue(store, ack=ack, report_state=report_state)
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
        jobs.receive_job(job, store, ack=ack, report_state=report_state)
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

        sess = cable.PrintCableSession(
            cable_url=cfg.cable_url,
            get_ticket=get_ticket,
            on_print_job=on_print_job,
            on_revoke=on_revoke,
            on_job_canceled=on_job_canceled,
            on_subscribed=lambda: log.info("cable PrintNodeChannel ready"),
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

            # Drain push job queue on the main thread (lp must not run on WS thread).
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

            # REST heartbeat first — cable must never block liveness / LCD status.
            do_hb = (now - last_hb) >= hb_interval or last_hb == 0.0
            st: statusio.AgentStatus | None = None
            if do_hb:
                st = run_once(cfg, client)
                last_hb = time.monotonic()
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
                    # Channel heartbeat (flush pending) alongside REST.
                    if (now - last_cable_hb) >= hb_interval:
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

            # REST pull safety net
            pull_every = (
                pull_interval_when_cabled
                if (sess and sess.subscribed)
                else pull_interval
            )
            if (
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
