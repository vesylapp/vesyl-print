# OTA updates — long-term plan and mechanics

Living document for **vesyl-print** appliances (Raspberry Pi print nodes on
customer networks). Update this file when the control plane, artifact format,
install layout, or OS strategy changes.

| | |
|--|--|
| **Owner** | Print / device platform |
| **Last reviewed** | 2026-10-08 |
| **Status** | App OTA client (Rust) + GitHub Releases CI + wms-api heartbeat OTA directives (fleet/node pin) |
| **Related code** | `rust/crates/vesyl-print/src/` (`update.rs`, `agent.rs`, `cli.rs`, `config.rs`), `scripts/build-release.sh`, `scripts/apply-update`, `.github/workflows/release.yml`, `setup.sh`, `keys/` |

---

## 1. Goals

1. **Fleet updates without per-site SSH** — appliances initiate all traffic (outbound HTTPS).
2. **Integrity** — TLS plus **Ed25519-signed manifests**; never trust “just a URL.”
3. **Atomic install + rollback** — dual-slot layout; failed health → previous slot.
4. **Preserve site state** — credentials, job queue, CUPS printers, config stay put.
5. **Separate app vs OS** — ship agent/display features weekly; patch OS/kernel on a slower, safer track.
6. **Observable** — cloud always knows `agent_version` and update status.

### Non-goals (for now)

- Full PrintNode-compatible remote shell / arbitrary package install from cloud.
- Customer-hosted update mirrors (may add later for air-gapped sites).
- Auto `apt full-upgrade` of the entire OS from the agent (too easy to brick SPI/LCD stacks).

---

## 2. Environment constraints

| Constraint | Design implication |
|------------|-------------------|
| Customer LAN, often locked down | Outbound HTTPS only; no inbound OTA listener |
| May block GitHub | Prefer allowlisting `github.com` object storage for releases; future: mirror to `releases.vesyl.com` |
| Possible TLS inspection | Signature verification is mandatory even with HTTPS |
| Pi + SPI LCD + CUPS | Kernel/driver updates are high-risk; image A/B later |
| Multi-tenant cloud | Desired version / channel from **wms-api**, not a public “latest” free-for-all |

**Firewall allowlist (customer IT handout):**

```text
HTTPS egress → API host(s)     e.g. wms.api.vesyl.com, wms-api.vesyl.dev (lab)
HTTPS egress → github.com      (GitHub Releases assets — current CDN)
# optional later:
# HTTPS egress → releases.vesyl.com
```

Demo LCD stream (`:8765`) is **not** part of OTA; keep off production images or document separately.

---

## 3. Two layers of update

```text
┌─────────────────────────────────────────────────────────────┐
│  Layer A — App OTA (vesyl-print agent / display)            │
│  Signed tarball → dual slot under /opt/vesyl-print          │
│  Trigger: heartbeat desired_agent_version (plan A)          │
│  Cadence: features / fixes (days–weeks)                     │
└─────────────────────────────────────────────────────────────┘
┌─────────────────────────────────────────────────────────────┐
│  Layer B — OS / image                                       │
│  Security packages + eventual A/B rootfs (RAUC / Mender)    │
│  Cadence: months; factory flash for major base moves        │
└─────────────────────────────────────────────────────────────┘
```

App OTA does **not** replace OS security patching. OS image OTA does **not**
replace fast app releases.

---

## 4. App OTA — mechanics (current design)

### 4.1 Install layout

Production (preferred):

```text
/opt/vesyl-print/
  current -> releases/0.5.1          # atomic symlink
  releases/
    0.5.0/                           # previous (rollback)
    0.5.1/                           # active tree (vesyl-print binary, LCD *.py, assets)
    0.5.2.staging/                   # an install, put together before it is swapped in
  update/                            # downloads
```

An install unpacks into a hidden `releases/.<version>.unpack/` first, then
moves the release to `<version>.staging` (§4.3 steps 9-10). Leftovers it
cannot delete (trees root unpacked) are renamed `.<name>.stale-<n>` and
deleted by a later install. None of these names is a version: rollback, the
`releases:` list of `vesyl-print version` and `apply-update` never take one.

Lab/dev without root:

```text
{state_dir}/app/   # same shape; override with VESYL_PRINT_INSTALL_ROOT
```

**Never** packaged into the tarball:

- `/etc/vesyl-print/credentials.json`
- `/etc/vesyl-print/config.json` (site-specific)
- `/var/lib/vesyl-print/**` (queue, processed, status)

Systemd units run from `current` after `setup.sh` (factory path):
`vesyl-print-agent.service` runs `current/vesyl-print agent`, and
`vesyl-print-display.service` runs `python3 current/main.py` (the LCD stays
Python for now). The source tree for `setup.sh` is an extracted release tarball
(a git checkout has no binary); setup copies it into
`/opt/vesyl-print/releases/<VERSION>` and points `current` there. OTA stages
new versions beside it under the same install root.

### 4.2 Artifact format

**CDN:** [GitHub Releases](https://github.com/vesylapp/vesyl-print/releases) (for now).

CI (`.github/workflows/release.yml`) runs on tag `vX.Y.Z` and uploads:

| Asset | Purpose |
|-------|---------|
| `vesyl-print-X.Y.Z-linux-aarch64.tar.gz` | App tree (below) |
| `vesyl-print-X.Y.Z.manifest.json` | Metadata + sha256 + Ed25519 signature |

**Tarball** (`vesyl-print-X.Y.Z/` at its root; entries owned by root):

| Path | Purpose |
|------|---------|
| `vesyl-print` | Rust agent + CLI, aarch64, glibc ≥ 2.31 (Debian bullseye and newer) |
| `*.py` | LCD display (Python, for now) |
| `assets/` | logo, boot splash, 4x6 test labels (`vesyl-print test-print`) |
| `base.jpg` | sample image (`vesyl-print print-test --file …/current/base.jpg`) |
| `setup.sh`, `vesyl-print-*.service`, `scripts/` (not `build-release.sh`), `overlays/`, `keys/update_public.pem` | provisioning from the extracted tarball |
| `VERSION`, `README.md`, `OTA_UPDATES.md` | version, docs |

Nothing else ships: no `rust/`, `tests/`, `.github/`, `requirements.txt`,
private keys or Tailscale keys. `build-release.sh` packages an allowlist and
fails if a required file (binary, `main.py`, units, `scripts/apply-update`,
test labels, …) is missing.

The tarball holds no hard links (`build-release.sh` copies the tree with
`rsync -a`, without `-H`): installed as root, an archive with one is refused
(`bad_archive`).

**URLs (device default `releases_base_url`):**

```text
https://github.com/vesylapp/vesyl-print/releases/download/vX.Y.Z/vesyl-print-X.Y.Z.manifest.json
https://github.com/vesylapp/vesyl-print/releases/download/vX.Y.Z/vesyl-print-X.Y.Z-linux-aarch64.tar.gz
```

**Building:** `scripts/build-release.sh [VERSION]` cross-compiles the binary
with `cargo-zigbuild` (`aarch64-unknown-linux-gnu.2.31`), bakes the version in
(`VESYL_PRINT_VERSION`), checks the binary needs no glibc symbol newer than
2.31 (`readelf -V`) and reports that version (under qemu-aarch64 on x86 CI,
where it must run), packages the tarball and signs the manifest. It needs
cargo-zigbuild (CI pins it and zig in `.github/zigbuild-requirements.txt`),
binutils, jq, rsync and openssl; no Python. The `aarch64` job of
`.github/workflows/rust.yml` runs the same `BUILD_ONLY=1` build for pull
requests and pushes to main (whenever the Rust tests run), with clippy and
the unit tests for aarch64 (under qemu), so a change that breaks the Pi
build fails there and not at the tag. CI runs the release in three jobs so
the signing key never shares a runner with build code:

| Mode | Job | Does | Tools |
|------|-----|------|-------|
| `BUILD_ONLY=1` | build | tarball only; never reads a key | cargo-zigbuild, readelf, rsync, tar, jq |
| `SIGN_ONLY=1` | sign | hash the tarball, write the signed manifest | jq, openssl, sha256sum, coreutils |
| `VERIFY_ONLY=1` | publish | refuse unless the manifest names this version, URL and sha256 and verifies with `keys/update_public.pem` | same as SIGN_ONLY |

With no mode set it builds and signs in one go (local use:
`UPDATE_PRIVATE_KEY_FILE=… ./scripts/build-release.sh 0.5.0`).

**Manifest fields (contract):**

```json
{
  "version": "0.5.0",
  "channel": "stable",
  "artifact_url": "https://github.com/vesylapp/vesyl-print/releases/download/v0.5.0/vesyl-print-0.5.0-linux-aarch64.tar.gz",
  "artifact_sha256": "<64 hex>",
  "min_agent_version": "0.4.0",
  "released_at": "2026-10-08T16:05:00+00:00",
  "signature": "<base64 Ed25519>"
}
```

Optional: `changelog` (`RELEASE_CHANGELOG`). `channel` comes from
`RELEASE_CHANNEL`. `min_agent_version` (`MIN_AGENT_VERSION`, default `0.4.0`) is
the oldest agent that may install the release; older agents fail with
`too_old`. The default keeps Python-era 0.3.x agents from installing a release
whose binary their units cannot run (§4.2.1). The build refuses a `VERSION`
below it, since devices on that release could never update again.

**Signature:** Ed25519 over the manifest's **canonical JSON**: every field
except `signature` and nulls, keys sorted, compact separators, non-ASCII
escaped as `\uXXXX` (Python `json.dumps(sort_keys=True, separators=(",", ":"))`
form). The scripts produce it with `jq -S -c -a`; devices rebuild the same
bytes in `update.rs` `ReleaseManifest::canonical_bytes()`. The Rust tests sign
with the script and verify with the device code (including a non-ASCII
changelog), and the other way round. See `keys/README.md`.

**Public key:** `config.update_public_key_path` when set, else the key compiled
into the binary from `keys/update_public.pem` (rotate by shipping a release).
`setup.sh` also installs `/etc/vesyl-print/keys/update_public.pem` for configs
that point at it.

Private key: CI secrets / HSM only — **never** on devices.

### 4.2.1 Runtime: Rust agent + Python LCD; migrating Python-era devices

The agent and CLI are the `vesyl-print` binary; there is no Python agent, no
bridge and no fallback to one (they are in git history only). The LCD display
stack (`main.py` and its modules) is still Python and ships in the same slot.
It reads the agent's state files (`status.json`, `printers.json`,
`update_status.json`) and calls the CLI
(`vesyl-print claim CODE [--name N] --json`,
`vesyl-print test-print --queue Q --format pdf|zpl --json`). Printer setup
(new CUPS queues) runs in the agent, once per start; the LCD only reads
`printers.json`, and shows every status as unknown once that file is older
than 120 s.

A slot is runnable only with the `vesyl-print` binary, which the agent unit
execs from the slot root: the agent rejects archives without an executable
one, rollback passes over slots without it, the health gate fails a slot
without it, and `apply-update` refuses to activate a slot without an
executable `vesyl-print` at its root.

Devices provisioned by an older `setup.sh` (versions 0.3.x) run
`python3 …/agent.py` from root-owned units, and OTA cannot rewrite those. Nor
can they take the first Rust release over OTA: its `min_agent_version` 0.4.0
makes a 0.3.x agent refuse it (`too_old`). They are **re-provisioned, not
migrated by OTA**: run `setup.sh` from an extracted release tarball (over
Tailscale or SSH). It removes the old release slots without the binary (the
Python-only ones, so a rollback cannot pick one), rewrites both units, the
CLI wrapper, `apply-update`, `wifi-setup` and sudoers, and keeps config,
credentials (the pairing) and the queue. The lab Pi was moved this way.

The lab Pi has run lab builds 0.4.0 through 0.4.3, signed with a throwaway
lab key, so the first real release is numbered above them: 0.5.0 (§4.8).
Those builds report 0.4.x, which the `min_agent_version` floor lets through,
but the lab Pi refuses production-signed releases: when the dev server asked
it for 0.3.17, it held that version as `bad_signature` on the first attempt
(§4.3 step 18) and downloaded nothing more, and its LCD shows `Update failed`
until the desired version changes. So the lab Pi is re-provisioned from the
published 0.5.0 tarball, never from a build made before the tag and numbered
0.5.0 or 0.5.0-anything.

A release tarball never holds `keys/tailscale.key`, so re-provisioning a
device that is already on the tailnet leaves Tailscale alone ("No Tailscale
auth key — skip Tailscale"). Afterwards `setup.sh` deletes the extracted
`vesyl-print-X.Y.Z/` it ran from (`SKIP_SOURCE_CLEANUP=1` keeps it); it never
deletes the install root, a slot, a git checkout or a directory with another
name.

### 4.3 Device-side flow

Implemented in `rust/crates/vesyl-print/src/update.rs`, invoked from the agent
after a successful heartbeat and from the CLI.

```text
1. Heartbeat POST includes agent_version, platform, optional update status blob
2. Response may include desired_agent_version (+ update_channel, update_url);
   the desired version is normalized: a leading `v` is dropped
3. If desired empty or == current → idle. A desired version held here, or
   still backing off (step 18), is left alone: nothing is fetched
4. If auto_update_enabled false → record target only, do not install
   (the agent; a manual `vesyl-print update apply` installs anyway, §4.7)
5. If buffered ActionCable jobs, or a start-up drain held back by the health
     gate, are still to run → defer install (stay idle, keep target); retry
     next heartbeat
6. Resolve manifest URL:
     - heartbeat.update_url if set
     - else {releases_base_url}/v{desired}/vesyl-print-{desired}.manifest.json
       (GitHub Releases; any other base gets no /v{desired}/)
     - neither → failed; checked again on each heartbeat (nothing fetched,
       no failed attempt counted)
7. Fetch manifest → its `version` must equal the desired version, else the
   update is refused before any download (version_mismatch); check
   min_agent_version (too_old) and verify Ed25519 (if require_signature)
8. Download tarball → verify SHA-256, into `update/`, opened once: the
   `.part` file is made, renamed and removed, and the artifact removed after
   the install, relative to that directory (as root too, a symlink swapped
   in for `update/` meanwhile takes nothing elsewhere). A stop ends the
   download after the read in progress, and the `.part` is removed
   (while status is downloading|installing|pending_health: **pause** REST
    job pull, ActionCable print_job processing and the start-up drain of
    queue/*.json; after a health gate rolls back and restarts the services,
    the agent also takes no job until its restart, or for 120 s if none
    comes)
9. Extract to releases/<version>.staging (path-escape rejected): the archive
   unpacks into a hidden `.<version>.unpack` beside the slot, and the release
   (its single top-level directory, else all of it) moves to
   `<version>.staging`. An archive whose top-level directory is
   `vesyl-print-<another version>` is refused (version_mismatch). As root
   (`sudo vesyl-print update apply`), the archive is unpacked through
   descriptors into a root-owned 0700 `.<version>.unpack` (a directory at
   that name that is not root's own and closed to others is refused), with
   no hard links. Before the release leaves it, group and others lose write
   on every directory and file, whatever modes the archive or the operator's
   umask gave. The slot is handed to the owner of `releases/` only once it
   is in place (step 10)
10. In staging, before the slot is touched: require an executable
    vesyl-print at the slot root (else bad_archive, and an installed slot
    of that version stays as it was), write VERSION, and syncfs. Then put
    it in place:
      - no slot of that version yet: the new slot is renamed in
      - a slot of that version, including the one `current` points at (a
        re-apply or repair): swapped with renameat2(RENAME_EXCHANGE) and
        the old tree removed, so `current` never dangles
      - on a filesystem without RENAME_EXCHANGE the active slot is never
        replaced (the install fails with no_exchange); any other slot is
        cleared and replaced
    As root, the slot (VERSION included) then goes to the owner of releases/
11. Activate:
      - where apply-update is installed (appliances), only through
        sudo -n apply-update activate <release> <current>; if it refuses,
        the update fails and `current` is not flipped some other way
      - with no helper (lab install root, tests): atomic symlink flip as
        the service user
12. Persist `update_status.json` with `status=pending_health`,
    `previous_version`, `health_deadline_at` (default 120s) and `armed_at`
13. Restart services (apply-update restart or systemctl), unless the agent
    is stopping (see Stopping during an OTA, below)
14. New agent process runs the **health gate**:
      - local: `current` has the vesyl-print binary + VERSION matches target
      - if paired: `GET /print/v1/whoami` must reach the API (`ok` or
        `unauthorized` both count — proves the new code talks to cloud)
      - if unpaired: local checks only
15. Health OK → `status=idle` (OTA success); job pull/push resume, and jobs
    left queued from before the restart print before any new job
16. Health not ready → stay `pending_health` and retry each cycle
17. Hard local failure **or** deadline exceeded → auto-rollback to
    `previous_version`, restart services, `status=rolled_back`
      - no previous slot, or it cannot run → `status=failed`, last_error
        "health failed: …" ("… (no previous slot to roll back to)" or
        "…; rollback error: …"). The slot stays active, jobs are not
        paused, and the gate is not re-armed. It clears to `idle`, without
        a new gate, on the first later agent cycle where that version runs
        from a runnable slot and whoami does not fail (`ok`,
        `unauthorized`, or unpaired)
      - a gate rollback during a `systemctl stop` flips `current` but
        restarts nothing
18. Pre-activate failure: leave `current` alone (no half-open symlink);
    `status=failed` with `last_error` and `last_error_code`
      - failures another try would only repeat (bad_manifest,
        bad_signature, bad_checksum, bad_archive, too_old,
        version_mismatch, no_exchange) hold that version like a failed
        health gate: no new download until the desired version changes or
        an operator runs `update apply`
      - any other failure (network, any HTTP status including 404/429/5xx,
        disk full or permissions, the helper, the key file) is retried
        after 1 min, doubling to 1 h (`attempts`, `retry_at` in
        update_status.json)
      - the counter resets when the desired version changes or an install
        succeeds
```

Errors (`last_error`, which the heartbeat and the LCD carry) and log lines
name manifest and artifact URLs, and any redirect target the HTTP client
could not follow, without their userinfo, query or fragment, cut to 200
characters, so a presigned URL's signature never reaches them.

**Stopping during an OTA:** a `systemctl stop` that lands before the new
slot is put in place abandons the update. That covers mid-download, before
the extract, and while the unpacked release is checked in staging. The
status becomes `idle` with last_error "update stopped …: the agent is
stopping"; failed-attempt counters are cleared, and the download and
staging are removed. A reinstall of the active slot leaves that slot exactly
as it was. The update is retried on the next start, not held.

- After activation the services are not restarted, because that restart
  would replace the stop job: `pending_health` stays, and the next start
  runs the new slot and its gate. The gate deadline keeps running while
  stopped; if it has passed, a failing first whoami at that start rolls back
  at once. The display service keeps running the old slot's code until it is
  itself restarted.
- A gate rollback during a stop flips `current` but restarts nothing.
- A connection that stalls entirely still holds a read for up to the
  artifact idle timeout (300 s), so systemd's 90 s stop timeout can SIGKILL
  such a download; only a slow download is ended promptly. The next start
  marks that update `failed` ("update interrupted before it finished") and
  tries it again.

**Rollback:** `vesyl-print update rollback` points `current` at the newest
other release whose slot can run (an executable `vesyl-print` at its root),
passing over and logging slots that cannot; `--version X` picks X, and is
refused (`not_runnable`) when its slot cannot run. It activates as step 11
does: through `apply-update` when that is installed, whose refusal is final,
else with the in-process symlink flip. Auto-rollback after a failed health
gate uses the same path.

`update rollback` then records `update_status.json` as `rolled_back`, with
`target_version` the version it left (X) and last_error "manual rollback from
X to Y", and prints "holding X: …". The agent then does not install X again
while the server still desires it, only once the desired version changes or
`update apply` runs. An open health gate is closed at once. A gate rollback
(step 17) holds the version it left the same way, unless the restart into
that version never happened (it never ran).

### 4.4 Privileges (`setup.sh`)

| Path | Role |
|------|------|
| `/usr/local/lib/vesyl-print/apply-update` | Root helper: `activate`, `restart`, `rollback` |
| `/usr/local/lib/vesyl-print/wifi-setup` | Root helper for the LCD's Wi-Fi setup (NetworkManager hotspot / scan / connect) |
| `/etc/sudoers.d/vesyl-print` | `$RUN_USER ALL=(root) NOPASSWD:` those two helpers only |

Rules:

- Drop-in mode **0440**, validated with `visudo -cf` before install.
- Helpers owned by root, not writable by the service user. `setup.sh` writes
  the install root into each one (`INSTALL_ROOT=` in `apply-update`,
  `INSTALL_ROOT = Path(…)` in `wifi-setup`) and stops before changing
  anything if that line is not there exactly once. It writes it in plain
  form, the same in the units: absolute, trailing slashes dropped, no `.`,
  `..` or empty component (it refuses those), since `apply-update` compares
  it as a string with the paths the agent builds from the units' value.
- No shell wrappers or `NOPASSWD: ALL`.
- **Known issue: `wifi-setup` is a root-escalation path for the service
  user.** The helper file is root-owned, but it runs as root through
  NOPASSWD sudo and imports `wifi_setup.py` (and `sysinfo.py`) from
  `<install root>/current`, a tree the service user owns, and
  `wifi_setup.py` spawns the `wifi_portal.py` beside it (so the active
  slot's) as root. Whatever the service user writes into the active slot
  runs as root. The helper no longer writes Python bytecode into the slot
  (`sys.dont_write_bytecode`, and `PYTHONDONTWRITEBYTECODE=1` for the portal
  it spawns), so it leaves no root-owned `__pycache__` there that the agent
  could never delete; the imports from the slot remain. Planned fix, not
  implemented yet: `setup.sh` installs root-owned copies of
  `wifi_setup.py`, `sysinfo.py` and `wifi_portal.py` next to the helper in
  `/usr/local/lib/vesyl-print/` and the helper imports only from its own
  directory; the portal, spawned from beside `wifi_setup.py`, then comes
  from there too. Those copies would change only when `setup.sh` runs, not
  with OTA: a trade-off for the maintainers to decide. Until provisioning
  changes, the image's cloud-init also gives the service user
  `NOPASSWD: ALL` sudo, so these helper restrictions do not limit what that
  account can do as root.

`apply-update` trusts none of its arguments as a path, since sudoers lets the
service user pass anything:

- the install root is fixed in the installed file (`setup.sh` writes it);
  `activate` takes only `<root>/releases/<version>` and `<root>/current`,
  `rollback` only `<root>` and a version;
- the version must be a release version, as `update.rs` `is_version`
  judges it (never `.`, `..`, a `/`, or an interrupted extract's
  `<version>.staging`);
- the slot must be a real directory, not a symlink, holding an executable
  `vesyl-print`, so a Python-era or broken slot is never activated;
- the only write is the `current` symlink, swapped atomically through
  `current.new` (`ln -T` / `mv -T`, never followed);
- it runs with a fixed `PATH` and `LC_ALL=C`; `restart` restarts the display,
  then the agent, with `--no-block`.

A rejected call changes nothing. `rust/crates/vesyl-print/tests/apply_update.rs`
covers each rule.

### 4.5 Control plane (wms-api) — plan A

**Chosen contract:** embed OTA directives on the **heartbeat response** (not a
separate `GET /print/v1/update` for v1).

**Agent → server (request body, partial):**

```json
{
  "agent_version": "0.5.0",
  "hostname": "VESYL-PRINT-…",
  "platform": "linux-aarch64",
  "printers": [ … ],
  "update": {
    "status": "idle|checking|downloading|installing|pending_health|failed|rolled_back",
    "current_version": "0.5.0",
    "target_version": null,
    "last_error": null,
    "last_checked_at": "…"
  }
}
```

The `update` blob is the agent's `update_status.json`, which also always
carries `channel`, `previous_version`, `health_deadline_at` and
`health_attempts`. The optional keys `last_error_code`, `attempts` and
`retry_at` (§4.3 step 18) are sent only while set, as is `armed_at` (an open
health gate).

**Server → agent (response fields):**

```json
{
  "ok": true,
  "node_id": "…",
  "status": "online",
  "last_seen_at": "…",
  "desired_agent_version": "0.5.1",
  "update_channel": "stable",
  "update_url": "https://github.com/vesylapp/vesyl-print/releases/download/v0.5.1/vesyl-print-0.5.1.manifest.json"
}
```

| Field | Required | Notes |
|-------|----------|--------|
| `desired_agent_version` | no | Omit or null → no update |
| `update_channel` | no | Default `stable` on device |
| `update_url` | no | Full manifest URL; if omitted, device builds GitHub Releases URL from `releases_base_url` + version |

**Policy sources (server-side, to implement / keep in sync):**

- Global default channel / version  
- Org or warehouse override  
- Per-node pin (“hold”, “force”)  
- Optional staged rollout (% canaries)

When the server does not yet send these fields, the agent stays idle (safe).

### 4.6 Device config

`/etc/vesyl-print/config.json` (relevant keys):

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

| Key | Default | Meaning |
|-----|---------|---------|
| `auto_update_enabled` | `true` | If false, the agent logs the desired version but does not install it (at info when the desired version changes or the agent starts, at debug on the heartbeats between). A manual `vesyl-print update apply` (or `--version`) still installs (§4.7); the Python CLI did not |
| `update_channel` | `stable` | Informational / future channel latest index |
| `releases_base_url` | GitHub `…/releases/download` | Prefix; device appends `/vX.Y.Z/vesyl-print-X.Y.Z.manifest.json` (GitHub Releases; any other base gets `/vesyl-print-X.Y.Z.manifest.json`) |
| `update_require_signature` | `true` | Lab may set false only with care |
| `update_public_key_path` | empty | Explicit PEM path |
| `update_health_gate_seconds` | `120` | Health gate deadline after an activation (§4.3 step 12); at least 15 |

Env: `VESYL_PRINT_INSTALL_ROOT` overrides install root.

### 4.7 CLI

```bash
vesyl-print version
vesyl-print update check              # heartbeat; print desired if paired
vesyl-print update apply              # use cloud desired + update_url
vesyl-print update apply --version 0.5.0
vesyl-print update apply --manifest-url https://… [--version 0.5.0] [--restart]
vesyl-print update apply --file ./rel.tar.gz --manifest ./rel.manifest.json [--version 0.5.0] [--restart]
vesyl-print update rollback [--version X] [--restart]
```

`--version` accepts a leading `v` (`v0.5.0`); anything else that is not a
release version is refused before any network call. `update apply
--file/--manifest` (like `print-test --file`) expands a leading `~/` from
`$HOME`, also in the `--file=~/…` form the shell leaves alone.

`update apply` with no source (the cloud's desired version, or `--version`)
runs the heartbeat path of §4.3: install, `pending_health`, restart the
services, and the new agent runs the health gate. It installs even with
`auto_update_enabled: false`, which only keeps the agent from installing on
its own (the Python CLI installed nothing then), and even a version the agent
holds or backs off (§4.3 step 18): it starts from a fresh status.
`--version X` installs X from `releases_base_url` (§4.6); it uses the
heartbeat's `update_url` only when the server desires exactly `X` (a leading
`v` aside), since that URL is the manifest of the server's version. When the
server does not desire `X` and `releases_base_url` is empty, it fails ("no
manifest URL for X … (pass --manifest-url)"). Any online manifest for
another version than the one asked for is refused before installing
(version_mismatch).

`--manifest-url` and `--file` only install and activate the slot. `--file`
runs the same checks as an online install: the manifest's
`min_agent_version` and signature, the tarball's SHA-256 against the
manifest, and an executable `vesyl-print` at the root of the unpacked slot
(§4.3 step 10); it activates as step 11 does, through `apply-update` when
that is installed. With them `--version` is optional and is the version the
release must be. It is validated first: a leading `v` is allowed, anything
else is refused before the manifest is fetched or read. A manifest for
another version is refused ("manifest is for version Y, not X", exit 1)
before anything is downloaded, unpacked or activated. Without `--version`
they install whatever version their manifest names. With `--restart` they
also arm the health gate (`pending_health`, deadline
`update_health_gate_seconds`, rollback to the slot that was active) and then
restart the services. Without `--restart` nothing is restarted and no gate
is armed (the running agent could only let it expire and roll back): the new
slot starts on the next service restart, and no health gate checks it.

`update rollback [--version X] [--restart]` records the hold described under
**Rollback** in §4.3 and prints "holding X: …"; its `--version` also accepts
a leading `v`.

Run as root (`sudo vesyl-print update …`), the CLI hands what it writes to
the service user that owns the tree it writes in. That means the download
and the unpacked slot (with its `VERSION`) under the install root, and
`update_status.json` in the state dir. On its way there it follows a symlink
only when root owns the link and the directory that holds it, and nobody
else can write to that directory (README, App stack). `/opt/vesyl-print`
itself may be such a link in `/opt`.

### 4.8 Version source of truth

- Repo / release tree: `VERSION` file  
- The binary bakes its version in at build time (`VESYL_PRINT_VERSION`, set
  from the tag by `build-release.sh`; else `VERSION`), and the build checks the
  packaged binary reports it  
- Heartbeat, CLI and the LCD footer report that version  

Release process must bump `VERSION` (and tags) in the same commit as the ship.

**First Rust-only release: v0.5.0.** The lab Pi has run lab builds 0.4.0
through 0.4.3, signed with a throwaway lab key. A device whose running
version equals the desired one stays idle (§4.3 step 3), so a real 0.4.x
would never replace the lab build of the same number. Tag `v0.5.0` with
`VERSION` bumped to 0.5.0 in the same commit. `MIN_AGENT_VERSION` keeps its
default of 0.4.0, the Python-era cutoff (§4.2).

That equality ignores suffixes, an open item (§5): `update.rs` `version_cmp`
compares only the numbers before any `-` or `+` (a part that is not a
number counts as 0), so 0.9.1-rc.1 counts as 0.9.1, and 0.5.0-rc.1 and
0.5.0.lab are both 0.5.0. Tag releases `vX.Y.Z`, without a suffix. A device
running a build numbered like that stays idle when the cloud asks for the
real v0.5.0, as it does for `update apply --version 0.5.0`, and
`update check` calls it up to date. Moving it takes a manual
`update apply --manifest-url …` (or `--file`) or another re-provision. So:

- Re-provision the lab Pi from the published v0.5.0 tarball (§4.2.1); its
  lab key refuses the production-signed release over OTA anyway.
- A tarball built before the tag must not be numbered 0.5.0 or anything that
  reads as 0.5.0. Number an interim lab build below 0.5.0, at or above
  `MIN_AGENT_VERSION` 0.4.0 and above the lab builds so far, e.g. 0.4.4:
  `BUILD_ONLY=1 ./scripts/build-release.sh 0.4.4`. Give the version: until
  the release bumps it, `VERSION` is 0.3.17, below the floor, and
  `build-release.sh` refuses it.

### 4.9 Implementation map

| Component | Path |
|-----------|------|
| Core logic | `rust/crates/vesyl-print/src/update.rs` |
| Root helper | `scripts/apply-update` → `/usr/local/lib/vesyl-print/apply-update` |
| Heartbeat hook | `agent.rs` → `update::maybe_update_from_heartbeat` |
| HTTP client | `cloud.rs` `CloudClient::heartbeat`; downloads through `net.rs` |
| Root-safe writes (CLI run as root) | `util.rs` `write_durable`, `create_dir_all_owned`, `open_dir_owned`, `hand_tree_to_parent_owner` |
| Config | `config.rs` |
| CLI | `cli.rs` `version` / `update *` |
| Provisioning | `setup.sh` (units, CLI wrapper, helpers + sudoers, public key) |
| Build, sign, verify | `scripts/build-release.sh` |
| CI publish | `.github/workflows/release.yml` → GitHub Releases |
| CI checks | `.github/workflows/rust.yml`: tests, and the aarch64 release build (`BUILD_ONLY=1`, glibc floor, `--version` under qemu); `lcd.yml`: LCD tests |
| Signing docs | `keys/README.md` |
| Tests | `update.rs` unit tests; `rust/crates/vesyl-print/tests/build_release.rs`, `apply_update.rs` and `setup_sh.rs` (drive the scripts; the `root_*` ones run `setup.sh` in a chroot) |

---

## 5. What is done vs open

### Done (device + publish)

- [x] Dual-slot install + rollback APIs  
- [x] Manifest parse, sha256 download verify  
- [x] Ed25519 verify in the binary (`ed25519-dalek`; key compiled in, config can override)  
- [x] Heartbeat request carries update status; response drives desired version (**plan A**)  
- [x] CLI check / apply / rollback  
- [x] `setup.sh` installs apply-update + sudoers  
- [x] Unit tests for extract, flip, rollback, checksum, heartbeat idle paths  
- [x] CI: build tarball + signed manifest; upload to **GitHub Releases** (CDN)  
- [x] Public key at `keys/update_public.pem` (private key = repo secret `UPDATE_PRIVATE_KEY`)  
- [x] wms-api: accept `update` on heartbeat; return `desired_agent_version` / channel / url  
  (`Print::UpdateDirective`, `print_nodes.desired_agent_version`, settings `PRINT_DESIRED_AGENT_VERSION`)  
- [x] Migrate production units to `/opt/vesyl-print/current` (factory `setup.sh`)  
- [x] Post-update health gate (whoami / local checks; auto-rollback on failure)  
- [x] Pause job pull during install; avoid updating mid-job  
- [x] LCD “Updating…” / failed update messaging (+ agent version on footer)  
- [x] Hold a release that cannot be installed, back off other failures (§4.3 step 18)  
- [x] Reinstall of the active version swapped in one step (`renameat2` `RENAME_EXCHANGE`)  

### Open (must land for fleet OTA)

- [ ] Fleet metrics: version histogram, failure rate  
- [ ] Optional: mirror GitHub Release assets to `releases.vesyl.com` if customers block github.com  
- [ ] Version comparison ignores pre-release suffixes (0.9.1-rc.1 counts as 0.9.1): until fixed, tag releases `vX.Y.Z` (§4.8)  

### Explicitly deferred

- [ ] Policy: org pin + admin UI / GraphQL (node column exists for per-node pin)  
- [ ] OS A/B image OTA (RAUC/Mender)  
- [ ] Offline USB update workflow for air-gapped sites  
- [ ] Cosign/Sigstore keyless signing  

---

## 6. OS upgrades

App OTA will not patch the kernel, Mesa, CUPS, OpenSSL, or the SPI display
stack reliably. Plan OS work as a **second track**.

### 6.1 Near term (every appliance image)

1. **Golden image** — Raspberry Pi OS (or derivative) with:
   - `setup.sh` already applied  
   - display overlay, CUPS, groups (`video`, `lpadmin`)  
   - unattended-upgrades for **security** pockets only (test on lab fleet first)  
2. **Document** which packages are frozen (e.g. kernel / firmware) if SPI
   regressions appear.  
3. **No** agent-driven `apt full-upgrade`.  
4. **Inventory:** report OS version / kernel in heartbeat later (field TBD) for
   support.
5. **Memory cgroup:** Raspberry Pi OS boots with `cgroup_disable=memory`, so
   the agent unit's `OOMPolicy=continue` and `MemoryMax=50%` are inert.
   Enabling them means `cgroup_enable=memory` in `cmdline.txt` and a reboot:
   decide it for the image (README, Services). Unit changes reach a device
   only through `setup.sh`; OTA never rewrites units.

### 6.2 Medium term

- Periodic **re-image** or USB/netboot refresh for major Debian/Pi OS jumps.
- Signed checklist: claim flow, print path, cable, OTA app update after reflash.

### 6.3 Long term — image A/B OTA

When truck-rolls become too expensive:

| Option | Pros | Cons |
|--------|------|------|
| **RAUC** | Embedded-friendly, dual partition, rollback | Image pipeline investment |
| **Mender** | Hosted/open, fleet UI | Extra agent/service |
| **balena / similar** | Full stack | Vendor lock / model fit |

**Principles if adopted:**

- Dual rootfs (or dual superblocks); bootloader flips only after successful boot
  mark.  
- App slot (`/opt/vesyl-print`) may still OTA independently **or** be baked into
  the image — pick one primary story and document it here.  
- Credentials on a **data partition** that image updates never wipe.  
- Same outbound-only constraint: pull images from VESYL HTTPS, signed.

Until A/B image OTA exists, treat “broken base OS” as **RMA / re-flash**, and
keep app OTA as the daily driver.

### 6.4 Security updates matrix

| Layer | Mechanism | Owner |
|-------|-----------|--------|
| App (Rust agent/CLI + Python LCD) | Signed OTA tarball | This repo + CI + wms-api |
| Debian security packages | unattended-upgrades (curated) | Image / platform |
| Kernel / firmware / SPI | Image rebuild or A/B OTA | Platform (later) |
| CUPS / printer drivers | Image or careful apt policy | Platform |

---

## 7. Failure modes and operations

| Failure | Expected behavior |
|---------|-------------------|
| Bad signature / checksum | Do not flip `current`; `update.status=failed`; held for that desired version: no new download until it changes or `update apply` |
| Download interrupted | No activate; retry after 1 min, doubling to 1 h (`retry_at`) |
| Disk full | Fail before activation; retried with the same backoff |
| Activate succeeds, whoami never succeeds | Auto-rollback after `update_health_gate_seconds` (default 120) |
| Gate fails and cannot roll back (no previous slot, or it cannot run) | Status `failed` "health failed: …", not re-armed on later cycles, and jobs are not paused. It clears to `idle` by itself on the first cycle where that version runs from a runnable slot and whoami does not fail. Otherwise (the slot cannot run, or another version runs) it needs an operator |
| Archive without the `vesyl-print` binary | Rejected before activate, and held; `apply-update` also refuses such a slot |
| Manifest or archive of another version than desired (or than `--version`) | Refused before download (manifest) or install (archive), `version_mismatch`; held when the agent fetched it |
| Reinstall of the version `current` points at | Swapped in one step; a bad artifact, or a stop before it is put in place, leaves the slot intact; refused (`no_exchange`, held) on a filesystem without RENAME_EXCHANGE |
| Activate succeeds, slot missing the binary | Immediate auto-rollback (hard local fail) |
| Agent self-restart SIGTERM during `apply-update restart` | **Not** a failure — restart is detached/`--no-block`; sticky false `failed` is recovered when version matches target |
| Power loss after `current` flipped, before `pending_health` was written | The next start marks the leftover `installing` failed; its first heartbeat turns that back into the health gate (version matches target, slot healthy), and jobs left queued wait for that gate |
| `systemctl stop` during an OTA | See **Stopping during an OTA** in §4.3 |
| New binary never starts (crash-loop) | No process to roll back, and the CLI is the same binary. Support, as root: `/usr/local/lib/vesyl-print/apply-update rollback /opt/vesyl-print <previous>` then `apply-update restart` |
| Server omits desired version | No update attempt |
| `auto_update_enabled: false` | The agent logs the desired version only, at info when it changes or the agent starts (debug in between); on site, `vesyl-print update apply` installs it anyway (the Python CLI installed nothing with the setting off) |
| GitHub blocked, CDN allowed | Still works if artifacts on CDN |
| CDN blocked | No app OTA until IT allowlists release host |

While a release that cannot be installed is held, the LCD footer shows red
`Update failed` (with the error) until the desired version changes. The lab
Pi shows it for the dev server's 0.3.17 (§4.2.1).

**Support playbook (short):**

1. `vesyl-print version` / `update check`  
2. `journalctl -u vesyl-print-agent` for update errors  
3. `cat /var/lib/vesyl-print/update_status.json`  
4. `vesyl-print update rollback --restart` if new slot is bad (if the binary
   itself will not run: `sudo /usr/local/lib/vesyl-print/apply-update rollback
   /opt/vesyl-print <previous>` + `sudo /usr/local/lib/vesyl-print/apply-update restart`).
   The manual rollback sticks: the agent holds the version it left. To
   resume, change the desired version or run `update apply`  
5. Confirm credentials still present under `/etc/vesyl-print/`  

---

## 8. Anti-patterns (do not reintroduce)

| Anti-pattern | Why |
|--------------|-----|
| `git pull` on customer Pis | Auth, non-atomic, dirty trees, branch drift |
| Unsigned zip from arbitrary URL | Supply-chain trivial |
| Overwrite running tree in place | Mid-write crash bricks until truck roll |
| Unrestricted sudo for service user | Lateral movement / ransomware path |
| Silent force-reboot every night | Print jobs / operator trust |
| Coupling every app fix to a full OS image | Too slow; too risky |

---

## 9. Roadmap (keep this section current)

### Phase 0 — Hygiene

- [x] Single version file  
- [x] Heartbeat reports `agent_version`  
- [x] Tag `v*.*.*` → GitHub Actions release workflow  
- [ ] Customer firewall one-pager linked from onboarding  

### Phase 1 — Device app OTA (this repo)

- [x] OTA client (`update.rs`) + CLI + agent heartbeat hook (plan A)  
- [x] Agent + CLI in Rust; Python only for the LCD; Python-era devices re-provisioned with `setup.sh`  
- [x] apply-update + sudoers via `setup.sh`  
- [x] CI publish to GitHub Releases  
- [x] Factory path always uses `/opt/vesyl-print/current`  
- [x] Health-check + automatic rollback (`pending_health` → whoami/local)  

### Phase 2 — Cloud control plane (wms-api)

- [x] Heartbeat accepts `update` status; returns OTA plan-A fields  
- [x] Fleet default via `PRINT_DESIRED_AGENT_VERSION` + per-node `desired_agent_version`  
- [ ] GraphQL / admin UI to set pins and view update_status  
- [ ] Org-level policy + staged rollout  

### Phase 3 — Fleet polish

- [x] LCD update state (`Updating…` / `Update failed` / `Rolled back` + version)  
- [ ] Maintenance windows / rate limits  
- [ ] Metrics dashboards  

### Phase 4 — OS image OTA

- [ ] Golden image pipeline  
- [ ] Choose RAUC vs Mender vs re-flash SOP  
- [ ] Data partition for credentials/queue  
- [ ] First production image OTA pilot  

---

## 10. Maintaining this document

When you change any of the following, **update this file in the same PR**:

1. Manifest schema or signing method  
2. Heartbeat OTA fields (request or response)  
3. Install paths or systemd unit paths  
4. Sudoers / apply-update interface  
5. Default channels or CDN hostnames  
6. OS image or A/B strategy  

Also bump **Last reviewed** at the top.

Short README pointer: see main `README.md` § OTA updates for operator-facing
commands; **this file** is the design source of truth.
