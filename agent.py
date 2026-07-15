"""Cloud agent: whoami + heartbeat loop, status file for LCD."""

from __future__ import annotations

import logging
import signal
import time
from datetime import datetime, timezone

import auth
import printers
import statusio
import sysinfo
from cloud import CloudClient, CloudError
from config import AGENT_VERSION, Config, load_config

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
    # Preserve last known names on the status file for the LCD.
    st = _status_from_creds(creds, pairing="revoked", cloud="offline")
    st.last_error = "re-pair required"
    statusio.write_status(cfg.status_path, st)
    auth.clear_credentials(cfg.credentials_path)


def run_once(cfg: Config, client: CloudClient | None = None) -> statusio.AgentStatus:
    """Single agent cycle: whoami (if needed) + heartbeat. Updates status file."""
    client = client or CloudClient(cfg.api_base_url)
    creds = auth.load_credentials(cfg.credentials_path)

    if not creds:
        # Keep revoked status sticky until a successful claim rewrites it.
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
        # Refresh public fields on disk (token unchanged).
        auth.save_credentials(cfg.credentials_path, creds)
    except CloudError as e:
        if e.unauthorized:
            _handle_unauthorized(cfg, creds)
            return statusio.read_status(cfg.status_path) or _status_from_creds(
                None, pairing="revoked", cloud="offline"
            )
        log.warning("whoami failed: %s", e.message)
        # Continue to heartbeat with last-known creds; may still work.
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
        # Preserve last successful heartbeat timestamp if present.
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


def run_agent(cfg: Config | None = None) -> None:
    """Long-running heartbeat loop with reconnect backoff."""
    cfg = cfg or load_config()
    cfg.ensure_dirs()
    client = CloudClient(cfg.api_base_url)

    running = {"go": True}
    signal.signal(signal.SIGINT, lambda *_: running.update(go=False))
    signal.signal(signal.SIGTERM, lambda *_: running.update(go=False))

    log.info(
        "agent starting api_base_url=%s heartbeat=%ss",
        cfg.api_base_url,
        cfg.heartbeat_seconds,
    )

    backoff = 1.0
    max_backoff = 60.0
    interval = max(5, int(cfg.heartbeat_seconds))

    while running["go"]:
        start = time.monotonic()
        st = run_once(cfg, client)
        if st.cloud == "online":
            backoff = 1.0
            sleep_for = interval
        elif st.pairing == "unpaired":
            # Poll for credentials after CLI claim without thrashing the API.
            backoff = 1.0
            sleep_for = min(10.0, float(interval))
        elif st.pairing == "revoked":
            sleep_for = min(30.0, float(interval))
        else:
            # Paired but cloud offline — exponential backoff.
            sleep_for = min(max_backoff, max(float(interval), backoff))
            backoff = min(max_backoff, backoff * 2)

        # Account for work time so nominal period is ~heartbeat_seconds when healthy.
        elapsed = time.monotonic() - start
        time.sleep(max(0.0, sleep_for - elapsed))


def main() -> None:
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    # Ensure logs never accidentally include secrets via default record factory.
    run_agent(load_config())


if __name__ == "__main__":
    main()
