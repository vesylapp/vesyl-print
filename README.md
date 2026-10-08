# vesyl-print

Raspberry Pi **print node** for VESYL: LCD status display, CUPS printer discovery, and cloud pairing to a warehouse via wms-api.

## What it does

| Component | Role |
|-----------|------|
| **Agent** (`vesyl-print agent` / `vesyl-print-agent.service`) | Rust binary. Heartbeats + `whoami`, job pull and ActionCable push, CUPS printing, printer discovery, app OTA. Writes the state files the LCD reads |
| **CLI** (`vesyl-print`, the same binary) | `claim`, `enroll`, `status`, `queues`, `unpair`, `print-test`, `test-print`, `update`, `version` |
| **LCD** (`main.py` / `vesyl-print-display.service`) | Python, for now. Multi-page status (ops / network / system), touch cycle, MJPEG stream + claim page, Wi-Fi setup. Pairs and test-prints through the CLI |

The agent and CLI are Rust ([`rust/`](rust/README.md)); only the LCD display
stack is still Python. There is no Python agent or fallback: devices set up by
the Python agent are re-provisioned with `setup.sh` (see below).

**Local print** + **cloud job pull** + **ActionCable push** are implemented.

- Pull: `pull_jobs_enabled` (default `true`) — always-on safety net  
- Push: `cable_enabled` (default `true`) — `PrintNodeChannel` on `/print/cable`

## Hardware

- Raspberry Pi with **MHS-3.5" (ILI9486)** SPI LCD (`/dev/fb1`)
- Network printers discovered via CUPS (IPP Everywhere), plus a LAN scan for
  Zebra thermal printers on port **9100** (AppSocket) identified over HTTP

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

On the Pi, run `setup.sh` from an **extracted release tarball** (it carries the
`vesyl-print` binary):

```bash
V=0.5.0
curl -fLO https://github.com/vesylapp/vesyl-print/releases/download/v$V/vesyl-print-$V-linux-aarch64.tar.gz
tar -xzf vesyl-print-$V-linux-aarch64.tar.gz
cp ~/tailscale.key vesyl-print-$V/keys/tailscale.key   # optional, one-time auth key
sudo ./vesyl-print-$V/setup.sh
```

This installs the packages (CUPS, poppler-utils, NetworkManager; python3,
Pillow, numpy, segno and DejaVu fonts for the LCD), the display overlay, config
dirs, the root helpers + sudoers, the release, the CLI wrapper and both systemd
units, then deletes the extracted `vesyl-print-$V/` (`SKIP_SOURCE_CLEANUP=1`
keeps it; a git checkout or a directory with another name is never deleted).
Options go after `sudo`, which drops the caller's environment:
`sudo SKIP_TAILSCALE=1 ./setup.sh` (see the header of `setup.sh`).

Run it with `sudo` from the account the services should run as (e.g.
`vesyl`): that account (`SUDO_USER`) becomes the units' `User=`. Run from a
root shell, `setup.sh` falls back to the owner of the extracted tree and
stops, before changing anything, if that is root; `chown -R` the tree to the
service account first. A custom install root (`sudo
INSTALL_ROOT=/srv/vesyl-print ./setup.sh`) is written into the units, the CLI
wrapper and both root helpers.

A git checkout has no binary, so `setup.sh` stops before changing anything.
Build a release from a checkout with `BUILD_ONLY=1 ./scripts/build-release.sh`
(needs cargo-zigbuild) and run the `setup.sh` inside the extracted tarball.

```text
/opt/vesyl-print/current → releases/<VERSION>/   vesyl-print binary, LCD (*.py), assets
/usr/local/bin/vesyl-print                       wrapper: exec current/vesyl-print
```

Services and the CLI run from `current`, so OTA can flip the symlink without
rewriting unit files. Site config stays in `/etc/vesyl-print`; runtime state in
`/var/lib/vesyl-print`.

### Re-provisioning a Python-era device

Devices set up before the Rust agent (units running `python3 agent.py`) are not
migrated by OTA. Re-run `setup.sh` from an extracted release (over Tailscale or
SSH). It rewrites the units, CLI wrapper and root helpers, removes old release
slots without a `vesyl-print` binary, and keeps `/etc/vesyl-print` (config,
credentials) and `/var/lib/vesyl-print` (queue, state). Releases carry
`min_agent_version` 0.4.0, so a Python 0.3.x agent refuses them instead of
installing a slot its units cannot run.

Release tarballs never carry `keys/tailscale.key`, so on a device that is
already on the tailnet `setup.sh` reports "No Tailscale auth key" and leaves
Tailscale as it is. Lab devices that ran the 0.4.0 / 0.4.1 lab builds are
re-provisioned the same way, from 0.5.0 or later.

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
| `wait_cups` | After `lp`: `async` (default — next job spools immediately; CUPS FIFO keeps order), `sync` (wait for printer), `off` |

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

Never commit credentials or device tokens.

### State files the LCD reads

The display never talks to the cloud or CUPS itself. It reads what the agent
writes under `/var/lib/vesyl-print/`:

| File | Written by | LCD use |
|------|------------|---------|
| `status.json` | agent; CLI `claim` / `unpair` | pairing + cloud state, warehouse, last error |
| `printers.json` | agent, after each inventory refresh (about every 15 s), mode 0644 | printer rows and status dots; re-read every 8 s |
| `update_status.json` | agent (OTA) | update banner / footer |
| `queue/`, `processed/` | agent | local queue depth |

A `printers.json` older than 120 s (by its `updated_at`, else the file's
mtime) means the agent stopped refreshing it: the LCD still lists the printers
but shows every status as `unknown`, never a stale `idle`. A missing or
unreadable file keeps the rows it already shows.

`printers.json`:

```json
{
  "updated_at": "2026-10-08T16:05:00+00:00",
  "printers": [
    {
      "cups_name": "Zebra_ZD421",
      "uri": "socket://10.0.0.172:9100",
      "display_name": "Zebra ZD421",
      "status": "idle",
      "status_reasons": [],
      "status_message": null,
      "supports_raw": true
    }
  ]
}
```

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
vesyl-print claim <CODE> [--name NAME] [--json]
vesyl-print enroll <TOKEN> [--name NAME]
vesyl-print status [--check]
vesyl-print queues [--json]
vesyl-print unpair
vesyl-print agent          # what vesyl-print-agent.service runs
vesyl-print print-test --file ./label.pdf --queue Brother_HL-L3280CDW_series
vesyl-print test-print --queue Zebra_ZD421 --format zpl [--json]
vesyl-print version
vesyl-print update check|apply|rollback
```

The LCD and its stream page use the `--json` forms (the stream page's claim
form runs `vesyl-print claim CODE [--name N] --json`, the LCD's Test button
`vesyl-print test-print --queue Q --format F --json`):

- `claim CODE [--name N] --json` prints one object: `{"ok": true, "node_id",
  "name", "organization_name", "warehouse_name"}`, or `{"ok": false, "error",
  "status", "code"}` with exit 1 (`status` is the HTTP status: 400 for a code
  too short to send, 0 for transport and local errors; `code` is the cloud's
  error code or null).
- `test-print --queue Q --format pdf|zpl [--json]` sends the built-in 4x6 test
  label (`assets/test-labels/vesyl-roadrunner-4x6.pdf|.zpl`) through a
  private, temporary job store, not the agent's queue, and returns once `lp`
  has accepted it: `{"ok": true, "state": "delivered", "job_id", "queue",
  "format"}`, or `{"ok": false, "error", "code"}` with exit 1 (`code` is the
  job error code, e.g. `unknown_queue`). `zpl` is native ZPL for raw / Zebra
  queues; `pdf` works on any queue (rasterized to ZPL on a raw one). Without
  `--json` it prints the same fields as text.
- The test labels come from `assets/` next to the running `vesyl-print`
  binary, i.e. the active release slot; `VESYL_PRINT_ASSETS_DIR` points
  `test-print` at another directory holding `test-labels/` (tests, dev).

### Local print test (no cloud)

```bash
# uses first CUPS network queue if --queue omitted
vesyl-print print-test --file /opt/vesyl-print/current/base.jpg
vesyl-print print-test -f label.pdf -q My_CUPS_Queue --copies 1
```

Jobs go through the durable pipeline:

1. Write `queue/<job_id>.json` (fsync)
2. Materialize content → `lp -d <cups_name>` (add `-o raw` for `raw_*` / ZPL)
3. Marker `processed/<job_id>`, delete queue file
4. Watch CUPS in the background (`wait_cups: async`) so the next job can
   `lp` immediately. Page order is the CUPS queue, not “wait for printed.”

Cloud job content types: `pdf_*`, `png_*`, `jpeg_*`/`jpg_*` and `raw_*`
(ZPL/EPL), each as `*_uri` (fetched) or `*_base64` (inline). `local_path` (a
file on the Pi) is CLI-only: `print-test` and `test-print` use it, and the
agent rejects a cloud job carrying it before queueing, since it would print
any file the agent can read. Such a job is reported `error` ("content_type
local_path is only accepted from the local CLI") and never acked or printed.
Raw payloads are written as `.zpl`/`.raw` **without** PDF/PNG magic sniffing and
submitted with `lp -o raw`. Thermal printers usually need a **raw** CUPS queue
(`lpadmin -m raw` or `socket://host:9100`); driverless IPP Everywhere often will
not honor raw. Inventory reports `supports_raw` per queue for WMS.

**PDF / PNG / JPEG → Zebra:** if the queue is raw (USB ZD220, `socket://…:9100`),
the agent rasterizes the file (`pdftoppm`, from poppler-utils) to 1-bit and
wraps it in a ZPL `^GFA` graphic (ASCII hex), then `lp -o raw`. A multi-page
PDF prints one label per page, in order (each page its own `^XA`…`^XZ`), as
CUPS does on a filtered queue; `copies` repeats the whole document. At most
**50 pages**: a longer PDF fails with the permanent error
`pdf_too_many_pages`, so its queue file is retired to `queue/failed/` instead
of being retried. Options: `zpl_page` (print just that page),
`zpl_max_width_dots` / `zpl_max_height_dots` (default: a 4×6" label at the
head's resolution, 812 × 1218 dots at 203 dpi), `zpl_dpi` (default: the
queue name's `203dpi` / `300dpi` / `600dpi` token, else 203; clamped to
72–600), `zpl_threshold` (128), `zpl_invert`, `no_zpl_convert`. Native ZPL
(`raw_*` / files starting with `^XA`) is sent unchanged.

**Printer discovery:** when the agent starts it provisions printers once, in
the background (the LCD no longer does): USB and IPP devices from `lpinfo`,
then a scan of local `/24` LAN segments for TCP
**9100** that skips IPs already known from IPP/CUPS, GETs `http://IP/` to
confirm a Zebra print server (e.g. “ZTC ZD421-203dpi ZPL”), and adds an
AppSocket queue:

```bash
lpadmin -p Zebra_ZD421-203dpi_ZPL -v socket://10.0.0.172:9100 -m raw -E
```

Local ZPL smoke test:

```bash
vesyl-print print-test -f label.zpl -q Zebra_Raw --raw
```

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

Agent logs never include `device_token`. `VESYL_PRINT_LOG=debug` raises the
agent's log level.

## OTA updates (app)

Long-term plan (app + OS layers, control plane, roadmap): **[OTA_UPDATES.md](./OTA_UPDATES.md)**.

Appliances update over **outbound HTTPS only** — no `git pull` on customer devices.
Artifacts ship on **GitHub Releases** (CDN). A release tarball holds the
`vesyl-print` binary (aarch64, glibc ≥ 2.31), the Python LCD and its assets,
and the provisioning files; never `rust/`, `tests/` or secrets.

### Publish a release

```bash
# 1) Set repo secret UPDATE_PRIVATE_KEY (Ed25519 PEM; public half = keys/update_public.pem)
# 2) Bump VERSION, commit, tag, push (the tag must match VERSION):
echo 0.5.0 > VERSION && git commit -am "Release 0.5.0"
git tag v0.5.0
git push origin HEAD v0.5.0
# CI: build (BUILD_ONLY=1) → sign (SIGN_ONLY=1) → publish (VERIFY_ONLY=1 + gh release)
```

**The first Rust-only release is v0.5.0.** The lab Pi already ran two lab
builds, 0.4.0 and 0.4.1, signed with a throwaway lab key. A device that
already reports the desired version does nothing, so a real 0.4.0 or 0.4.1
would never replace the lab build of the same number. Tag above them, with
`VERSION` bumped to 0.5.0 in the same commit. `MIN_AGENT_VERSION` keeps its
default, 0.4.0: the Python-era cutoff, not the release version.

Local build: `UPDATE_PRIVATE_KEY_FILE=… ./scripts/build-release.sh 0.5.0` builds
and signs in one go (needs cargo-zigbuild, jq, rsync, openssl). `BUILD_ONLY=1`,
`SIGN_ONLY=1` and `VERIFY_ONLY=1` run one step each; see
[OTA_UPDATES.md §4.2](./OTA_UPDATES.md#42-artifact-format).

### How it works

1. Tag `vX.Y.Z` → CI uploads signed tarball + manifest to GitHub Releases.
2. Agent heartbeats report `agent_version` (+ optional `update` status).
3. Heartbeat **response** may include (plan A):

```json
{
  "ok": true,
  "desired_agent_version": "0.5.0",
  "update_channel": "stable",
  "update_url": "https://github.com/vesylapp/vesyl-print/releases/download/v0.5.0/vesyl-print-0.5.0.manifest.json"
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
vesyl-print update apply [--version 0.5.0]
vesyl-print update apply --manifest-url https://github.com/vesylapp/vesyl-print/releases/download/v0.5.0/vesyl-print-0.5.0.manifest.json [--restart]
vesyl-print update apply --file ./release.tar.gz --manifest ./release.manifest.json [--restart]
vesyl-print update rollback [--version 0.5.0] --restart
```

`update apply` without a source takes the cloud's desired version (or
`--version`) and goes the heartbeat way: install, `pending_health`, restart,
health gate. `--manifest-url` and `--file` install and activate only. Add
`--restart` to also arm the health gate (`pending_health` until
`update_health_gate_seconds`, rollback to the slot that was active) and
restart the services. Without it nothing restarts and no gate is armed: the
running agent carries on, and the new slot starts, unchecked, on the next
service restart.

### Config (`/etc/vesyl-print/config.json`)

```json
{
  "auto_update_enabled": true,
  "update_channel": "stable",
  "releases_base_url": "https://github.com/vesylapp/vesyl-print/releases/download",
  "update_require_signature": true,
  "update_public_key_path": "/etc/vesyl-print/keys/update_public.pem",
  "update_health_gate_seconds": 120
}
```

Install layout: `/opt/vesyl-print/current` → `releases/<version>` (lab: `$state_dir/app`).  
Credentials and `/var/lib/vesyl-print` are never part of the tarball.

`setup.sh` installs apply-update, sudoers, and `keys/update_public.pem`. The
binary verifies signatures itself (the public key is also compiled in). See
`keys/README.md`.

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
| `/` | LCD at top + claim form (when unpaired) + live stats tables |
| `/stream.mjpg` | Raw MJPEG |
| `/snapshot.jpg` | Single frame |
| `/api/stats` | JSON snapshot of the same stats (polls ~2s cache) |
| `/api/claim` | `POST {"code":"AB7K2Q9M","name":"optional"}` — pair node (trusted LAN) |

When the node is **unpaired** or **revoked**, the page shows a VESYL-branded claim
form: **8 large character boxes** with a dash between the two groups of four.
Typing auto-advances; paste fills all boxes and **ignores dashes/spaces**.
Optional node name field. It pairs through `vesyl-print claim --json`.

Options on `main.py` / the unit’s `ExecStart`:

```bash
python3 main.py                  # stream on (default)
python3 main.py --no-stream      # LCD only
python3 main.py --stream-port 8765 --stream-scale 1 --stream-fps 2
```

Standalone (polls `/dev/fb1` without embedding in the display loop):

```bash
python3 stream_lcd.py --port 8765
```

Only use on a trusted network (binds all interfaces by default).

## Development / tests

```bash
cd rust
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked                        # agent, CLI, and the release scripts
cd .. && python3 -m unittest discover -s tests   # LCD display (Python)
```

Unit tests mock HTTP; no network or real tokens required. The Rust integration
tests (`rust/crates/vesyl-print/tests/`) run `scripts/build-release.sh`,
`scripts/apply-update` and `setup.sh`'s preflight in temp dirs with a fake
cargo and throwaway keys; they need bash, jq, rsync, openssl and GNU
coreutils. Tests named `root_*` are ignored by default: they chown, and the
`setup.sh` ones run all of it in a chroot (host `/usr` read-only, apt-get,
systemctl, tailscale and the like stubbed). Run them in a user namespace,
each test binary on its own:

```bash
cargo test --locked --no-run --message-format=json \
  | jq -r 'select(.reason=="compiler-artifact" and .profile.test==true) | .executable'
unshare --map-root-user --map-auto <test binary> --include-ignored
```

CI: `.github/workflows/rust.yml` (format, clippy, Rust and script tests) and
`.github/workflows/lcd.yml` (the Python LCD tests, with Pillow, numpy and the
DejaVu fonts from apt as on devices; segno too where the runner packages it).

## LCD views

The display service paints the MHS-3.5" panel (and optional MJPEG stream).

### Unpaired / revoked (engineer)

Single **network** screen: hostname, LAN IP, Tailscale IP, claim hint. No page
cycle — always ready for install/support.

### Paired (multi-page)

| Page | Content |
|------|---------|
| **Ops** (home) | Warehouse · node, printer status dots, local job queue depth |
| **Network** | Hostname, LAN IP, Tailscale |
| **System** | Version, last heartbeat age, CPU temp, cloud, last error |

**Touch:** tap the resistive panel to advance Ops → Network → System → Ops.
After **10 seconds** without a tap, the UI returns to Ops.

```bash
python3 main.py --page network          # start on network (paired)
python3 main.py --no-touch              # disable page cycle
python3 main.py --touch-device /dev/input/event0
python3 main.py --idle-home 10
```

Touch uses the ADS7846/XPT2046 event node under `/dev/input` (auto-detected).
If no device is found, the LCD stays on the default page (Ops when paired).
`setup.sh` adds the service user to the `input` group so it can read the device
(re-login / restart the display unit after setup if touch was previously denied).

### Pairing / footer states

Footer shows **agent version** just left of the status dot (e.g. `v0.5.0 ● cloud`).

| State | Footer / message |
|-------|------------------|
| Unpaired | `unpaired` + `vesyl-print claim <CODE>` |
| Paired + cloud OK | green `cloud` |
| Paired + cloud down | red `cloud offline` |
| Revoked (401) | `revoked` + re-pair hint |
| OTA downloading | amber `Updating X.Y.Z…` banner + footer |
| OTA installing / health | amber `Installing…` / `Verifying…` |
| OTA failed | red `Update failed` (+ short error when present) |
| OTA rolled back | amber `Rolled back` |

OTA labels come from `/var/lib/vesyl-print/update_status.json` (written by the agent).

## Repo layout

```text
rust/                         vesyl-print binary: agent + CLI (rust/README.md)
main.py, stream_lcd.py, …     LCD display (Python, for now): pages, MJPEG stream,
                              touch, framebuffer, Wi-Fi setup + captive portal
setup.sh                      provisioning; run it from an extracted release
vesyl-print-*.service         systemd units (setup.sh installs them)
scripts/build-release.sh      release tarball + signed manifest (CI and local)
scripts/apply-update          root OTA helper: activate / restart / rollback
scripts/wifi-setup            root Wi-Fi helper (runs wifi_setup.py; see OTA_UPDATES.md §4.4)
scripts/bootstrap-fresh-pi.sh first boot: appliance id, hostname, LCD driver
assets/                       logo, boot splash, 4x6 test labels
keys/                         OTA public key (keys/README.md)
tests/                        LCD tests (Python) + ActionCable fixtures (Rust)
```

## Non-goals (this phase)

- GPIO claim keypad
- Label generation on the Pi
