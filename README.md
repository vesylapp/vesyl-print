# vesyl-print

Raspberry Pi **print node** for VESYL: LCD status display, CUPS printer discovery, and cloud pairing to a warehouse via wms-api.

## What it does

| Component | Role |
|-----------|------|
| **LCD** (`main.py` / `vesyl-print-display.service`) | Clock, IP, CPU temp, CUPS printers, cloud pairing, agent version, OTA progress |
| **Agent** (`agent.py` / `vesyl-print-agent.service`) | Heartbeats + `whoami`; writes status for the LCD |
| **CLI** (`vesyl-print`) | `claim`, `enroll`, `status`, `unpair` |

**Local print (Phase B)** + **cloud job pull (Phase C)** + **ActionCable push (Phase D)** are implemented.

- Pull: `pull_jobs_enabled` (default `true`) — always-on safety net  
- Push: `cable_enabled` (default `true`) — `PrintNodeChannel` on `/print/cable`  
- Push requires `websocket-client` (`python3-websocket` or `pip install -r requirements.txt`)

## Hardware

- Raspberry Pi with **MHS-3.5" (ILI9486)** SPI LCD (`/dev/fb1`)
- Network printers discovered via CUPS (IPP Everywhere)

## Install

### Fresh Pi (one-time)

On a new board (network + root), identity + MHS35 LCD vendor setup:

```bash
# from a copy of this repo, or curl the raw script
sudo ./scripts/bootstrap-fresh-pi.sh
```

That script: generates SSH host keys, writes `/etc/appliance-id`, sets
`hostname` to `VESYL-PRINT-<last 6 hex of UUID>`, clones
[goodtft/LCD-show](https://github.com/goodtft/LCD-show) and runs `MHS35-show`,
then reboots.

### App stack

On the Pi (from this repo), after the board is up:

```bash
sudo ./setup.sh
```

This installs dependencies, display overlay, config dirs, CLI, OTA helper, and
both systemd units. The app is copied into the dual-slot layout:

```text
/opt/vesyl-print/current → releases/<VERSION>/
```

Services and `vesyl-print` CLI run from `current` so OTA can flip the symlink
without rewriting unit files. Site config stays in `/etc/vesyl-print`; runtime
state in `/var/lib/vesyl-print`.

## Config

**`/etc/vesyl-print/config.json`** (created by setup):

```json
{
  "api_base_url": "https://wms-api.vesyl.dev",
  "cable_url": "wss://wms-api.vesyl.dev/print/cable",
  "heartbeat_seconds": 30,
  "pull_interval_seconds": 5,
  "pull_jobs_enabled": true,
  "cable_enabled": true
}
```

| Key | Meaning |
|-----|---------|
| `heartbeat_seconds` | REST + cable heartbeat interval |
| `pull_interval_seconds` | REST pull when cable is down (slower when cable is up) |
| `pull_jobs_enabled` | REST pull safety net |
| `cable_enabled` | ActionCable push via `cable_url` |
| `cable_url` | e.g. `wss://wms-api.vesyl.dev/print/cable` |

### Job delivery

**Push (preferred when cable subscribed):**

1. `POST /print/v1/ws_ticket` → connect `cable_url?token=…`  
2. Subscribe `PrintNodeChannel`  
3. On `{type: print_job, job: {…}}` → same durable pipeline  
4. Prefer channel `ack_job` / `job_state`; fall back to REST  

**Pull (safety net):**

1. `GET /print/v1/jobs/pending`  
2. Write `queue/<id>.json` (fsync)  
3. `POST …/ack` (or cable `ack_job`)  
4. content → `lp`  
5. `POST …/state` `done`|`error`  

Also handles cable `{type: revoke}` (re-pair) and `{type: job_canceled}`.

| Topology | `api_base_url` |
|----------|----------------|
| Direct API (preferred) | `https://wms-api.vesyl.dev` or `https://wms.api.vesyl.com` |
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
# or: export VESYL_PRINT_API_URL=https://wms-api.vesyl.dev

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
vesyl-print print-test --file ./label.pdf --queue Brother_HL-L3280CDW_series
```

### Local print test (no cloud)

```bash
# uses first CUPS network queue if --queue omitted
vesyl-print print-test --file /home/vesyl/vesyl-print/base.jpg
vesyl-print print-test -f label.pdf -q My_CUPS_Queue --copies 1
```

Jobs go through the durable pipeline:

1. Write `queue/<job_id>.json` (fsync)
2. Materialize content → `lp -d <cups_name>`
3. Marker `processed/<job_id>`, delete queue file

On agent start, any leftover `queue/*.json` is drained (crash recovery).

Paths (on a provisioned Pi):

```
/var/lib/vesyl-print/queue/
/var/lib/vesyl-print/processed/
```

## Services

```bash
sudo systemctl status vesyl-print-display
sudo systemctl status vesyl-print-agent
journalctl -u vesyl-print-agent -f
```

Agent logs never include `device_token`.

## OTA updates (app)

Long-term plan (app + OS layers, control plane, roadmap): **[OTA_UPDATES.md](./OTA_UPDATES.md)**.

Appliances update over **outbound HTTPS only** — no `git pull` on customer devices.
Artifacts ship on **GitHub Releases** (CDN).

### Publish a release

```bash
# 1) Set repo secret UPDATE_PRIVATE_KEY (Ed25519 PEM; public half = keys/update_public.pem)
# 2) Bump VERSION, commit, tag, push:
git tag v0.4.0
git push origin v0.4.0
# CI builds + signs + uploads tarball + manifest to the GitHub Release
```

Local build (optional): `UPDATE_PRIVATE_KEY_FILE=… ./scripts/build-release.sh 0.4.0`

### How it works

1. Tag `vX.Y.Z` → CI uploads signed tarball + manifest to GitHub Releases.
2. Agent heartbeats report `agent_version` (+ optional `update` status).
3. Heartbeat **response** may include (plan A):

```json
{
  "ok": true,
  "desired_agent_version": "0.4.0",
  "update_channel": "stable",
  "update_url": "https://github.com/benwyrosdick/vesyl-print/releases/download/v0.4.0/vesyl-print-0.4.0.manifest.json"
}
```

4. Agent downloads, verifies **SHA-256 + Ed25519**, installs under `/opt/vesyl-print/releases/<ver>/`, flips `current`.
5. Status becomes `pending_health` (not success yet); services restart.
   While `downloading` / `installing` / `pending_health`, **job pull and
   ActionCable print processing pause**. OTA is deferred if the durable queue
   (or buffered push jobs) still has work — never flip slots mid-print.
6. New agent runs the **health gate**: local slot checks + `whoami` when paired.
   On success → `idle` (jobs resume). On hard failure or deadline
   (`update_health_gate_seconds`, default 120s) → auto-rollback and restart.

### CLI

```bash
vesyl-print version
vesyl-print update check
vesyl-print update apply --manifest-url https://github.com/benwyrosdick/vesyl-print/releases/download/v0.4.0/vesyl-print-0.4.0.manifest.json
vesyl-print update apply --file ./release.tar.gz --manifest ./release.manifest.json
vesyl-print update rollback [--version 0.3.0] --restart
```

### Config (`/etc/vesyl-print/config.json`)

```json
{
  "auto_update_enabled": true,
  "update_channel": "stable",
  "releases_base_url": "https://github.com/benwyrosdick/vesyl-print/releases/download",
  "update_require_signature": true,
  "update_public_key_path": "/etc/vesyl-print/keys/update_public.pem",
  "update_health_gate_seconds": 120
}
```

Install layout: `/opt/vesyl-print/current` → `releases/<version>` (lab: `$state_dir/app`).  
Credentials and `/var/lib/vesyl-print` are never part of the tarball.

`setup.sh` installs apply-update, sudoers, and `keys/update_public.pem` when present.
Requires `python3-cryptography` for signature verify. See `keys/README.md`.

### Customer firewall

```text
HTTPS out → wms.api.* / wms-api.* (API + pairing)
HTTPS out → github.com (GitHub Releases assets)
```

## Stream the LCD (demo)

The **display service** streams the live UI as MJPEG on port **8765** (same
frames it paints to the panel). On your laptop:

```text
http://10.0.0.28:8765/
```

| URL | Purpose |
|-----|---------|
| `/` | Full-page live view |
| `/stream.mjpg` | Raw MJPEG |
| `/snapshot.jpg` | Single frame |

Options on `main.py` / the unit’s `ExecStart`:

```bash
python3 main.py                  # stream on (default)
python3 main.py --no-stream      # LCD only
python3 main.py --stream-port 8765 --stream-scale 2 --stream-fps 2
```

Standalone (polls `/dev/fb1` without embedding in the display loop):

```bash
python3 stream_lcd.py --port 8765
```

Only use on a trusted network (binds all interfaces by default).

## Development / tests

```bash
python3 -m unittest discover -s tests -v
```

Unit tests mock HTTP; no network or real tokens required.

## LCD pairing states

Footer shows **agent version** just left of the status dot (e.g. `v0.3.0 ● cloud`).

| State | Footer / message |
|-------|------------------|
| Unpaired | `unpaired` + `vesyl-print claim <CODE>` |
| Paired + cloud OK | green `cloud` + org / warehouse |
| Paired + cloud down | red `cloud offline` (last org/warehouse kept) |
| Revoked (401) | `revoked` + re-pair hint |
| OTA downloading | amber `Updating X.Y.Z…` banner + footer |
| OTA installing / health | amber `Installing…` / `Verifying…` |
| OTA failed | red `Update failed` (+ short error when present) |
| OTA rolled back | amber `Rolled back` |

OTA labels come from `/var/lib/vesyl-print/update_status.json` (written by the agent).

## Repo layout

```
config.py         # paths, api_base_url, env
auth.py           # credentials 0600
cloud.py          # claim / enroll / whoami / heartbeat / ws_ticket
agent.py          # heartbeat + pull + cable session
cable.py          # ActionCable PrintNodeChannel client
jobs.py           # durable queue + print pipeline
statusio.py       # status.json for LCD
display_status.py # OTA/version labels for LCD
cli.py            # vesyl-print entry
main.py           # LCD
printers.py       # CUPS discovery + inventory_payload()
requirements.txt  # websocket-client for cable
```

## Non-goals (this phase)

- ZPL/EPL raw thermal (`raw_*` content types rejected for now)
- GPIO claim keypad
- Label generation on the Pi
