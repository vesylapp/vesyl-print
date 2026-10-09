# vesyl-print

Raspberry Pi **print node** for VESYL: LCD status display, CUPS printer discovery, and cloud pairing to a warehouse via wms-api.

## What it does

| Component | Role |
|-----------|------|
| **Agent** (`vesyl-print agent` / `vesyl-print-agent.service`) | Rust binary. Heartbeats + `whoami`, job pull and ActionCable push, CUPS printing, printer discovery, app OTA. Writes the state files the LCD reads |
| **CLI** (`vesyl-print`, the same binary) | `claim`, `enroll`, `status`, `queues`, `unpair`, `print-test`, `test-print`, `update`, `version` |
| **LCD** (`main.py` / `vesyl-print-display.service`) | Python, for now. Multi-page status (ops / network / system), touch cycle, MJPEG stream + claim page, Wi-Fi setup. Pairs and test-prints through the CLI |

The agent and CLI are Rust ([`rust/`](rust/README.md)); only the LCD display
stack is still Python, a thin client of the CLI and of the state files the
agent writes. There is no Python agent, bridge or fallback (git history
only): devices set up by the Python agent are re-provisioned with `setup.sh`
(see below).

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

This installs the packages (CUPS, poppler-utils, NetworkManager, rsync;
python3, Pillow, numpy and DejaVu fonts for the LCD), the display overlay,
config dirs, the root helpers + sudoers, the release, the CLI wrapper and both
systemd units, then deletes the extracted `vesyl-print-$V/`
(`SKIP_SOURCE_CLEANUP=1` keeps it; a git checkout or a directory with another
name is never deleted).
A required package that apt cannot install, and that is not installed
already, stops `setup.sh`, naming it, before any later step runs.
`python3-segno` (the Wi-Fi setup QR code) is installed on its own, best
effort: without it the LCD shows the network name and PIN as text.
Options go after `sudo`, which drops the caller's environment:
`sudo SKIP_TAILSCALE=1 ./setup.sh` (see the header of `setup.sh`).

Run it with `sudo` from the account the services should run as (e.g.
`vesyl`): that account (`SUDO_USER`) becomes the units' `User=`, the owner of
`/etc/vesyl-print`, `/var/lib/vesyl-print` and the install root, and the
account allowed to run the root helpers through sudo. `setup.sh` takes
`SUDO_USER` whenever it is set and not root, and a root shell opened with
`sudo -i` or `sudo -s` keeps it: `./setup.sh` run there makes the account
that ran `sudo` (e.g. `pi`) the service account. Only when `SUDO_USER` is
unset or root (a direct root login, `su -`, or `sudo` run from a root shell)
does `setup.sh` fall back to the owner of the extracted tree, and it stops,
before changing anything, if that owner is root; `chown -R` the tree to the
service account first. A custom install root
(`sudo INSTALL_ROOT=/srv/vesyl-print ./setup.sh`) is written into the units,
the CLI wrapper and both root helpers. It must be an absolute path whose
components are letters, digits and `._-` (no `.` or `..`); trailing slashes
are dropped.

A git checkout has no binary, so `setup.sh` stops before changing anything.
Build a release from a checkout (needs cargo-zigbuild, binutils, jq and
rsync; see [Publish a release](#publish-a-release)) with
`BUILD_ONLY=1 ./scripts/build-release.sh [VERSION]` and run the `setup.sh`
inside the extracted tarball; how to number a build that is not a release is
covered below.

```text
/opt/vesyl-print/current → releases/<VERSION>/   vesyl-print binary, LCD (*.py), assets
/usr/local/bin/vesyl-print                       wrapper: exec current/vesyl-print
```

Services and the CLI run from `current`, so OTA can flip the symlink without
rewriting unit files. Site config stays in `/etc/vesyl-print`; runtime state in
`/var/lib/vesyl-print`.

Either directory, or the install root, may be a symlink (to a data disk, say),
as in `sudo ln -s /data/vesyl-print /var/lib/vesyl-print`. Root must own the
link and the directory that holds it, and nobody else may write to that
directory unless it is sticky (`/var/lib`, `/etc` and `/opt` qualify by
default). Any further symlink on the way to the target must meet the same
rule, and the target must exist. Run as root (`sudo vesyl-print claim`,
`enroll`, `unpair`, `print-test` or `update …`), the CLI's writes into these
trees (and an update's unpack and hand-over) follow no other symlink. It refuses a symlink that root does not
own, and one in a directory that someone other than root owns or can write
to, such as `/var/lib/vesyl-print/queue` inside the service user's state dir.
Such a run fails with "Not a directory" and logs which symlink it refused;
link the top-level directory instead. The agent runs as the service user
(never as root, see [CLI](#cli)) and follows symlinks as usual.

### Re-provisioning a Python-era device

Devices set up before the Rust agent (units running `python3 agent.py`,
versions 0.3.x) cannot take the first Rust release over OTA: releases carry
`min_agent_version` 0.4.0, so a 0.3.x agent refuses them (`too_old`) instead
of installing a slot its units cannot run. Re-run `setup.sh` from an extracted
release tarball (over Tailscale or SSH). It removes the old release slots
without a `vesyl-print` binary (the Python-only ones), rewrites the units, the
CLI wrapper and the root helpers, and keeps `/etc/vesyl-print` (config and
credentials: the node stays paired) and `/var/lib/vesyl-print` (queue,
state). The lab Pi was moved this way.

Release tarballs never carry `keys/tailscale.key`, so on a device that is
already on the tailnet `setup.sh` reports "No Tailscale auth key" and leaves
Tailscale as it is. The lab Pi has run lab builds 0.4.0 through 0.4.3, signed
with a throwaway lab key, and it refuses production-signed releases: when the
dev server asked it for 0.3.17, it held that version as `bad_signature` on
the first attempt and downloaded nothing more. Re-provision it the same way,
from the published v0.5.0 tarball (or a later release). A tarball built
before that tag must not be numbered 0.5.0: a device running such a build
would take v0.5.0 as already installed and never update to it. (A suffix
makes another version: 0.5.0-rc.1 sorts below 0.5.0, as semver has it, see
[OTA_UPDATES.md §4.8](./OTA_UPDATES.md#48-version-source-of-truth).) Number an interim lab build below 0.5.0, at or above the 0.4.0
floor and above the lab builds so far, e.g.
`BUILD_ONLY=1 ./scripts/build-release.sh 0.4.4` (give the version: until the
release bumps it, the checkout's `VERSION` is 0.3.17, below the floor, and a
build of it is refused).

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

The agent, the CLI and the LCD find the config and state directories in this
order (config / state):

1. `$VESYL_PRINT_CONFIG_DIR` / `$VESYL_PRINT_STATE_DIR`, when set and not
   empty
2. `/etc/vesyl-print` / `/var/lib/vesyl-print`, when it exists
3. `$XDG_CONFIG_HOME/vesyl-print` / `$XDG_STATE_HOME/vesyl-print` (else
   `$XDG_DATA_HOME/vesyl-print`), when that variable is set and not empty
4. `~/.config/vesyl-print` / `~/.local/share/vesyl-print`, where `~` is
   `$HOME` or, when `HOME` is unset, the account's home from the passwd
   database

### Job delivery

**Push (preferred when cable subscribed):**

1. `POST /print/v1/ws_ticket` → connect `cable_url?token=…`  
2. Subscribe `PrintNodeChannel`  
3. On `{type: print_job, job: {…}}` → same durable pipeline  
4. Prefer channel `ack_job` / `job_status`; fall back to REST  

**Pull (safety net):**

1. `GET /print/v1/jobs/pending`  
2. Write `queue/<id>.json` (fsync)  
3. `POST …/ack` (or cable `ack_job`)  
4. content → `lp`  
5. `POST …/status` (or cable `job_status`): `printing`, `delivered` once `lp`
   took it, `printed` once CUPS finished it (not with `wait_cups: off`), or
   `error`  

Also handles cable `{type: revoke}` (re-pair) and `{type: job_canceled}`.

The cable handshake does not follow HTTP redirects (the Python agent's
websocket-client followed up to 3): a `cable_url` that redirects leaves push
off, while REST pull still delivers jobs. Point `cable_url` at the final
address.

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
| `update_status.json` | agent (OTA); CLI `update apply` / `rollback` | update banner / footer |
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

`vesyl-print agent` runs as the service account (the units' `User=`, the
owner of `/var/lib/vesyl-print`) and refuses to run as root, which would
leave root-owned files in the state dir and follow links that account can
plant there. To run it in the foreground, stop the service and use
`sudo -u <service user> vesyl-print agent` (e.g. `sudo -u vesyl`). On
SIGTERM or Ctrl-C it finishes an in-process step in flight, an HTTP request
included, then exits; a second signal quits at once. Child processes are not
spared: systemd's stop signals every process in the unit's cgroup, and Ctrl-C
the whole foreground process group, so a running `lp`, `lpstat`, `pdftoppm`
or `gs` is killed with the agent (see the drain notes under
[Local print test](#local-print-test-no-cloud)). Under systemd a stop waits for that
at most the default `TimeoutStopSec` (90 s) before the agent is killed. Jobs
not yet given to `lp` stay queued for the next start
([Local print test](#local-print-test-no-cloud)). A stop during an OTA
download ends it and removes the partial file; one after the new slot was
activated skips the restart, and the next start runs that slot and its health
gate ([OTA_UPDATES.md §4.3](./OTA_UPDATES.md#43-device-side-flow)).

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
2. Record the attempt in it, then materialize content → `lp -d <cups_name>`
   (add `-o raw` for `raw_*` / ZPL). Once `lp` accepts the job, record that,
   and its CUPS request id, in the queue file
3. Marker `processed/<job_id>`, delete queue file
4. Watch CUPS in the background (`wait_cups: async`) so the next job can
   `lp` immediately. Page order is the CUPS queue, not “wait for printed.”

`print-test` itself waits for CUPS to finish its job (as `wait_cups: sync`
does) before it prints the result.

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
the agent rasterizes the file (`pdftoppm` from poppler-utils, else
Ghostscript) to 1-bit and wraps it in a ZPL `^GFA` graphic (ASCII hex), then
`lp -o raw`. A multi-page PDF prints one label per page, in order (each page
its own `^XA`…`^XZ`), as CUPS does on a filtered queue; `copies` repeats the
whole document. At most **50 pages**: a longer PDF fails with the permanent
error `pdf_too_many_pages`, so its queue file is retired to `queue/failed/`
instead of being retried. Native ZPL (`raw_*` / files starting with `^XA`) is
sent unchanged.

Each page is rasterized at the head's resolution (`zpl_dpi`, below). A page
that would pass 50 million pixels there (A0 at 203 dpi; Tabloid or A3 at
600 dpi) is rasterized at the highest resolution within that limit when
`zpl_fit` scales it onto the label anyway (`contain`, the default, or
`width`), and prints at its usual size; a small label on such a page is
enlarged from that lower resolution, so its edges print a little softer. With
`zpl_fit` `none` such a page fails with the permanent error
`pdf_page_too_large`, as an image over 50 million dots fails with
`label_too_large`. Rasterizing a PDF may take 2 minutes, plus 10 s for each
further page, in all; a PDF still rasterizing then fails with the permanent
error `pdf_render`.

Options:

- `zpl_page`: print just that page.
- `zpl_max_width_dots` / `zpl_max_height_dots`, or `label_width_dots` /
  `label_height_dots` (these win when both are given): the label box.
  Default: a 4×6" label at the head's resolution, 812 × 1218 dots at
  203 dpi. At 300 / 600 dpi it is 1227 / 2454 dots wide for the 4" models
  the agent knows (ZD220, ZD230, ZD421, ZD621, ZT410, ZT411, GK420, GX430),
  else 1200 / 2400, and 1800 / 3600 tall. Values over ZPL's 32000 dots are
  clamped to 32000.
- `zpl_dpi`: the resolution PDFs are rasterized at. Default: the queue
  name's `200dpi` / `203dpi` (203), `300dpi` or `600dpi` token, else 203;
  clamped to 72–600. A dpi token in the queue name still sets the label box.
- `zpl_fit`: `contain` (the default: only scale down to fit the label),
  `width` (scale up or down to fill its width, capped by its length), or
  `none` (print at the rasterized size; any other value does the same).
- `zpl_x` / `zpl_y`: the graphic's `^FO` offset in dots (default 0 / 32, a
  top margin). A positive offset narrows the box by that much and widens
  `^PW` / lengthens `^LL` to match, so nothing is clipped; a negative offset
  only moves `^FO`. An offset over 32000 fails with `zpl_error`.
- `zpl_threshold` (128), `zpl_invert` (or `invert`), `no_zpl_convert`.

Size limits keep one document from exhausting the Pi's memory. Each fails
the job with a permanent error, so its queue file is retired to
`queue/failed/`:

- `pdf_page_too_large`: a PDF page over 50 million pixels at the render dpi
  that is printed at its own size (`zpl_fit` `none`), or that could not
  cover the label box within that limit. US Legal at 600 dpi is within it.
  The page is sized from its MediaBox with `pdfinfo` before rendering;
  without poppler-utils, Ghostscript renders it and it is checked before it
  is decoded.
- `pdf_render`: also when pdftoppm could not allocate a page (it exits 0
  with a 1×1 image): the job fails instead of printing a blank label.
- `image_bad`: also for an image over 178,956,970 pixels (Pillow's
  decompression-bomb limit), refused from its header.
- `label_too_large`: a label graphic over 50 million dots after fitting (an
  oversize image printed at its own size, say), or one whose resize would
  need more than 512 MiB.

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
After an OTA restart the drain waits until the health gate has passed. The
same holds after a power loss mid-install: the first heartbeat reopens the
gate of an install cut off after its slot flip.

- A job whose queue file says CUPS already has it (the agent stopped or died
  after `lp`) is never given to `lp` again: the agent asks CUPS and finishes
  it from there (printed, failed, or followed while it still prints), or
  reports it delivered when CUPS no longer knows it.
- A job the agent died in three times while converting or submitting it (out
  of memory, say) is retired to `queue/failed/` as `crash_loop` and reported
  `error`.
- Once the agent is stopping (SIGTERM), the drain takes no further job and no
  job goes to `lp`. An in-process step already running (a content fetch, an
  image conversion) finishes first. A `wait_cups: sync` wait ends at its next
  check: it naps 100 ms at a time between `lpstat` polls. A job cut short
  that way stays queued for the next start.
- Child processes do not finish first. The unit sets no `KillMode`, so
  `systemctl stop` or restart sends SIGTERM to the whole cgroup at once (and
  Ctrl-C reaches the whole foreground process group); the CUPS tools and
  renderers run in the agent's group with signals unblocked. A PDF
  conversion killed that way currently fails the job permanently as
  `pdf_render` (retired to `queue/failed/`, reported `error`). An `lp` killed
  mid-submit fails as `lp_error`, reported `error` but kept queued and sent
  again on the next start; if cupsd had already accepted it, it can print
  twice. This is a known gap (it needs `KillMode=mixed` in the unit, which
  reaches devices only through `setup.sh`, or the children in their own
  process group).

The queue file is the cloud payload plus the agent's notes under `_agent`
(`attempts`, `cups_job_id`, `submitted_at`). Agents from older releases ignore
that key: after a rollback to one, a job whose record says CUPS has it may
print again.

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
agent's log level. The other commands (`update`, `claim`, …) write their
warnings to stderr; `VESYL_PRINT_LOG=info` or `debug` shows more.

The agent unit sets `OOMPolicy=continue` and `MemoryMax=50%`: a PDF renderer
(`pdftoppm`, `gs`) the kernel kills for memory fails its print job while the
agent keeps running, and the agent with its renderers never takes more than
half the RAM, so a runaway render cannot push the LCD into the kernel's OOM
killer. Both need the kernel's memory cgroup controller, and Raspberry Pi OS
boots with `cgroup_disable=memory` on the kernel command line (the lab Pi,
Debian 13 trixie, does): there both settings are inert. Enabling the
controller means adding `cgroup_enable=memory` to `cmdline.txt` (beside
`config.txt` in `/boot/firmware`, or `/boot` on older images) and rebooting,
a provisioning decision `setup.sh` does not make. The real protection is the
agent's own PDF and image size limits (**PDF / PNG / JPEG → Zebra** under
[Local print test](#local-print-test-no-cloud)).
Units are installed by `setup.sh` only; OTA never rewrites them, so a unit
change reaches a device only when `setup.sh` runs there.

## OTA updates (app)

Long-term plan (app + OS layers, control plane, roadmap): **[OTA_UPDATES.md](./OTA_UPDATES.md)**.

Appliances update over **outbound HTTPS only** — no `git pull` on customer devices.
Artifacts ship on **GitHub Releases** (CDN). A release tarball holds the
`vesyl-print` binary (aarch64, glibc ≥ 2.31), the Python LCD and its assets,
and the provisioning files; never `rust/`, `tests/` or secrets.

### Publish a release

```bash
# 1) Set repo secret UPDATE_PRIVATE_KEY (Ed25519 PEM; public half = keys/update_public.pem)
# 2) Bump VERSION, commit only VERSION, tag, push (the tag must match VERSION):
echo 0.5.0 > VERSION && git commit -m "Release 0.5.0" VERSION
git tag v0.5.0
git push origin HEAD v0.5.0
# CI: build (BUILD_ONLY=1) → sign (SIGN_ONLY=1) → publish (VERIFY_ONLY=1 + gh release)
```

Name `VERSION` in the commit rather than using `git commit -a`, which also
commits every other modified tracked file, including a key copied over the
tracked `keys/tailscale.key` (see `keys/README.md`).

**The first Rust-only release is v0.5.0.** The lab Pi already ran lab builds
0.4.0 through 0.4.3, signed with a throwaway lab key. A device that already
reports the desired version does nothing, so a real 0.4.x would never
replace the lab build of the same number. Tag above them, with `VERSION`
bumped to 0.5.0 in the same commit. `MIN_AGENT_VERSION` keeps its default,
0.4.0: the Python-era cutoff, not the release version. The agent orders
versions as semver does (0.9.1-rc.1 is below 0.9.1, and no longer counts as
it), and a build made before the tag must not be numbered 0.5.0 either:
number an interim lab build below 0.5.0 and above the lab builds (e.g. 0.4.4), and re-provision the
lab Pi from the published v0.5.0 tarball
([Re-provisioning](#re-provisioning-a-python-era-device)).

Local build: `UPDATE_PRIVATE_KEY_FILE=… ./scripts/build-release.sh 0.5.0` builds
and signs in one go. It needs cargo-zigbuild and zig (the versions CI pins
are in `.github/zigbuild-requirements.txt`), binutils (`readelf`, for the
glibc 2.31 floor check), jq, rsync and openssl. `BUILD_ONLY=1`,
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
   ActionCable print processing pause**. So does the start-up drain of
   leftover queue files. After a health gate rolls back and restarts the
   services, the agent takes no job until its restart, or for 2 minutes if
   none comes. OTA is deferred while buffered push jobs, or a start-up drain
   held back by the gate, are still to run — never flip slots mid-print.
6. New agent runs the **health gate**: local slot checks + `whoami` when paired.
   On success → `idle` (jobs resume). On hard failure or deadline
   (`update_health_gate_seconds`, default 120s) → auto-rollback and restart.
   With no runnable slot to roll back to, the status is `failed`
   (`health failed: …`): the new slot stays active, jobs run, and it clears
   to `idle` by itself once that version runs from a runnable slot and
   `whoami` does not fail.
7. An install that fails before activation leaves `current` alone
   (`failed`). A release this node cannot install (bad manifest, signature,
   checksum or archive, `too_old`, another version than asked for,
   `no_exchange`) is held: not downloaded again until the desired version
   changes or someone runs `update apply`. Any other failure (network, HTTP
   error, disk) is retried after 1 min, doubling up to 1 h. See
   [OTA_UPDATES.md §4.3](./OTA_UPDATES.md#43-device-side-flow).

### CLI

```bash
vesyl-print version
vesyl-print update check
vesyl-print update apply [--version 0.5.0]
vesyl-print update apply --manifest-url https://github.com/vesylapp/vesyl-print/releases/download/v0.5.0/vesyl-print-0.5.0.manifest.json [--version 0.5.0] [--restart]
vesyl-print update apply --file ./release.tar.gz --manifest ./release.manifest.json [--version 0.5.0] [--restart]
vesyl-print update rollback [--version 0.5.0] [--restart]
```

`--version` accepts a leading `v` (`v0.5.0`); anything else that is not a
release version is refused before any network call. `--file` and
`--manifest` (as `print-test --file`) expand a leading `~/` from `$HOME`,
also in the `--file=~/…` form, where the shell does not.

`update apply` without a source takes the cloud's desired version (or
`--version`) and goes the heartbeat way: install, `pending_health`, restart,
health gate. It installs even with `auto_update_enabled: false`, which only
stops the agent from installing on its own (the Python CLI installed nothing
then), and even a version the agent holds or backs off (see How it works,
step 7). `--version X` fetches X's manifest from `releases_base_url`; the
heartbeat's `update_url` is used only when the cloud's desired version is
exactly `X`; when it is not and `releases_base_url` is empty, the command
fails ("no manifest URL for X … (pass --manifest-url)"). A manifest for
another version than asked for is refused before anything is installed.

`--manifest-url` and `--file` install and activate only, with the same checks
as an online install (`min_agent_version`, signature, SHA-256, an executable
`vesyl-print` in the slot) and through `apply-update` when it is installed.
Without `--version` they install whatever version their manifest names. With
it, the release must be that version: a manifest for another one is refused
("manifest is for version Y, not X", exit 1) before anything is downloaded,
unpacked or activated. Add `--restart` to also arm the health gate
(`pending_health` until `update_health_gate_seconds`, rollback to the slot
that was active) and restart the services. Without it nothing restarts and no
gate is armed: the running agent carries on, and the new slot starts,
unchecked, on the next service restart.

`update rollback` activates the newest other release whose slot holds an
executable `vesyl-print` (or `--version X`, refused if X's slot cannot run).
Every activation goes through `apply-update` where it is installed; if the
helper refuses, `current` stays where it was. It then records the version it
left as held (`update_status.json`: `rolled_back`, "manual rollback from X to
Y") and prints "holding X: …": the agent does not install X again while the
server still asks for it, only once the desired version changes or
`update apply` installs a version (one that installs nothing, the version
asked for running already, keeps the hold). An open health gate closes at
once. Once the server asks for the version running, the rollback is over:
`idle` again, and the LCD stops showing "Rolled back".

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

Unit tests mock HTTP; no network or real tokens required. The PDF rendering
tests run against `pdftoppm` (poppler-utils) and `gs` (ghostscript) when they
are installed, and skip without them, unless
`VESYL_PRINT_REQUIRE_RENDERERS=1` is set (CI's test job sets it): then a
missing `pdftoppm`, `pdfinfo` or `gs` fails them. The Rust integration
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

CI: `.github/workflows/rust.yml` (format, clippy, Rust and script tests, with
both PDF renderers installed; and an aarch64 job that runs clippy for
aarch64, builds the release tarball as the tag's build job does, glibc 2.31
floor and `--version` under qemu included, and runs the unit tests for
aarch64 under qemu) and `.github/workflows/lcd.yml` (the Python LCD tests,
with Pillow, numpy and the DejaVu fonts from apt as on devices; segno too
where the runner packages it). The LCD tests never touch the machine's
network setup: the Wi-Fi tests keep the captive-portal snippet, setup state
and portal pid in a temp dir, record iptables calls, and fail if anything
else would run.

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
| OTA failed | red `Update failed` (+ short error when present), also for a health gate that failed and could not roll back, and while a release that cannot be installed is held (until the desired version changes) |
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
