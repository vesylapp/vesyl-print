# vesyl-print

Raspberry Pi **print node** for VESYL: LCD status display, CUPS printer discovery, and cloud pairing to a warehouse via wms-api.

## What it does

| Component | Role |
|-----------|------|
| **LCD** (`main.py` / `printserve-display.service`) | Clock, IP, CPU temp, CUPS printers, cloud pairing state |
| **Agent** (`agent.py` / `vesyl-print-agent.service`) | Heartbeats + `whoami`; writes status for the LCD |
| **CLI** (`vesyl-print`) | `claim`, `enroll`, `status`, `unpair` |

Job pull/print is prepared for later; `pull_jobs_enabled` stays `false` until wms-api job APIs ship.

## Hardware

- Raspberry Pi with **MHS-3.5" (ILI9486)** SPI LCD (`/dev/fb1`)
- Network printers discovered via CUPS (IPP Everywhere)

## Install

On the Pi (from this repo):

```bash
sudo ./setup.sh
```

This installs dependencies, display overlay, config dirs, CLI, and both systemd units.

## Config

**`/etc/vesyl-print/config.json`** (created by setup):

```json
{
  "api_base_url": "https://wms.api.staging.vesyl.com",
  "cable_url": "wss://wms.api.staging.vesyl.com/print/cable",
  "heartbeat_seconds": 30,
  "pull_interval_seconds": 5,
  "pull_jobs_enabled": false
}
```

| Topology | `api_base_url` |
|----------|----------------|
| Direct API (preferred) | `https://wms.api.staging.vesyl.com` or `https://wms.api.vesyl.com` |
| Edge + `/api` prefix | `https://wms.staging.vesyl.com/api` |

**Env override:** `VESYL_PRINT_API_URL` → `api_base_url`.

Credentials (mode **0600**):

```
/etc/vesyl-print/credentials.json
```

Status file for the LCD:

```
/var/lib/vesyl-print/status.json
```

Never commit credentials or device tokens.

## Staging claim flow

1. Ensure **print service is enabled** on the target wms-api env (`print_service_enabled`).
2. In WMS UI (or API), create a **claim code** for the warehouse.
3. On the Pi:

```bash
# optional: point at staging
sudo edit /etc/vesyl-print/config.json   # set api_base_url
# or: export VESYL_PRINT_API_URL=https://wms.api.staging.vesyl.com

vesyl-print claim AB7K2Q9M
# optional name:
vesyl-print claim AB7K2Q9M --name "Pack station 1"

sudo systemctl restart vesyl-print-agent
vesyl-print status --check
```

4. Confirm in WMS that the node is **online** after ~30s heartbeats.
5. LCD should show **organization**, **warehouse**, green **cloud** status, and printers.

### Headless enroll

```bash
vesyl-print enroll <enrollment_token>
```

### Unpair (local only)

```bash
vesyl-print unpair
```

Deletes local credentials only; does not delete the cloud node record. Re-pair with a new claim code.

### 401 / revoked

If the device token is revoked, the agent clears local credentials and the LCD shows **Revoked — re-pair required**. Claim again with a new code (no auto-reclaim).

## CLI

```bash
vesyl-print claim <CODE> [--name NAME]
vesyl-print enroll <TOKEN> [--name NAME]
vesyl-print status [--check]
vesyl-print unpair
vesyl-print agent          # same as agent.py service
```

## Services

```bash
sudo systemctl status printserve-display
sudo systemctl status vesyl-print-agent
journalctl -u vesyl-print-agent -f
```

Agent logs never include `device_token`.

## Development / tests

```bash
python3 -m unittest discover -s tests -v
```

Unit tests mock HTTP; no network or real tokens required.

## LCD pairing states

| State | Footer / message |
|-------|------------------|
| Unpaired | `unpaired` + `vesyl-print claim <CODE>` |
| Paired + cloud OK | green `cloud` + org / warehouse |
| Paired + cloud down | red `cloud offline` (last org/warehouse kept) |
| Revoked (401) | `revoked` + re-pair hint |

## Repo layout

```
config.py      # paths, api_base_url, env
auth.py        # credentials 0600
cloud.py       # claim / enroll / whoami / heartbeat / ws_ticket
agent.py       # heartbeat loop
statusio.py    # status.json for LCD
cli.py         # vesyl-print entry
main.py        # LCD
printers.py    # CUPS discovery + inventory_payload()
```

## Non-goals (this phase)

- Job pull/print (enable later with `pull_jobs_enabled`)
- ActionCable push
- GPIO claim keypad
- Label generation on the Pi
