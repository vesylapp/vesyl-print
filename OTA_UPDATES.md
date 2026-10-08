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
  current -> releases/0.4.0          # atomic symlink
  releases/
    0.4.0/                           # previous (rollback)
    0.4.1/                           # active tree (vesyl-print binary, LCD *.py, assets)
  update/                            # download staging
```

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
| `base.jpg` | auto-provision test page |
| `setup.sh`, `vesyl-print-*.service`, `scripts/` (not `build-release.sh`), `overlays/`, `keys/update_public.pem` | provisioning from the extracted tarball |
| `VERSION`, `README.md`, `OTA_UPDATES.md` | version, docs |

Nothing else ships: no `rust/`, `tests/`, `.github/`, `requirements.txt`,
private keys or Tailscale keys. `build-release.sh` packages an allowlist and
fails if a required file (binary, `main.py`, units, `scripts/apply-update`,
test labels, …) is missing.

**URLs (device default `releases_base_url`):**

```text
https://github.com/vesylapp/vesyl-print/releases/download/vX.Y.Z/vesyl-print-X.Y.Z.manifest.json
https://github.com/vesylapp/vesyl-print/releases/download/vX.Y.Z/vesyl-print-X.Y.Z-linux-aarch64.tar.gz
```

**Building:** `scripts/build-release.sh [VERSION]` cross-compiles the binary
with `cargo-zigbuild` (`aarch64-unknown-linux-gnu.2.31`), bakes the version in
(`VESYL_PRINT_VERSION`), checks it reports that version (under qemu-aarch64 on
x86 CI), packages the tarball and signs the manifest. It needs cargo-zigbuild,
jq, rsync and openssl; no Python. CI runs it in three jobs so the signing key
never shares a runner with build code:

| Mode | Job | Does | Tools |
|------|-----|------|-------|
| `BUILD_ONLY=1` | build | tarball only; never reads a key | cargo-zigbuild, rsync, tar, jq |
| `SIGN_ONLY=1` | sign | hash the tarball, write the signed manifest | jq, openssl, sha256sum, coreutils |
| `VERIFY_ONLY=1` | publish | refuse unless the manifest names this version, URL and sha256 and verifies with `keys/update_public.pem` | same as SIGN_ONLY |

With no mode set it builds and signs in one go (local use:
`UPDATE_PRIVATE_KEY_FILE=… ./scripts/build-release.sh 0.4.1`).

**Manifest fields (contract):**

```json
{
  "version": "0.4.1",
  "channel": "stable",
  "artifact_url": "https://github.com/vesylapp/vesyl-print/releases/download/v0.4.1/vesyl-print-0.4.1-linux-aarch64.tar.gz",
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

The agent and CLI are the `vesyl-print` binary; there is no Python agent and no
fallback to one. The LCD display stack (`main.py` and its modules) is still
Python and ships in the same slot. It reads the agent's state files
(`status.json`, `printers.json`, `update_status.json`) and calls the CLI
(`vesyl-print claim --json`, `vesyl-print test-print --json`).

A slot is runnable only with the `vesyl-print` binary, which the agent unit
execs from the slot root: the agent rejects archives without the binary, the
health gate fails a slot without it, and `apply-update` refuses to activate a
slot without an executable `vesyl-print` at its root.

Devices provisioned by an older `setup.sh` run `python3 …/agent.py` from
root-owned units, and OTA cannot rewrite those. They are **re-provisioned, not
migrated by OTA**: run `setup.sh` from an extracted release (over Tailscale or
SSH). It rewrites both units, the CLI wrapper, `apply-update`, `wifi-setup` and
sudoers, removes old release slots without the binary (so a rollback cannot
pick one), and keeps config, credentials and the queue. Until then the
`min_agent_version` floor makes a Python 0.3.x agent refuse new releases. Lab
devices that ran the 0.4 bridge (Python units handing off to the binary) report
0.4.x and are not covered by that floor: re-provision them before offering them
a newer release.

### 4.3 Device-side flow

Implemented in `rust/crates/vesyl-print/src/update.rs`, invoked from the agent
after a successful heartbeat and from the CLI.

```text
1. Heartbeat POST includes agent_version, platform, optional update status blob
2. Response may include desired_agent_version (+ update_channel, update_url)
3. If desired empty or == current → idle
4. If auto_update_enabled false → record target only, do not install
5. If jobs in flight (durable queue or buffered ActionCable jobs) →
     defer install (stay idle, keep target); retry next heartbeat
6. Resolve manifest URL:
     - heartbeat.update_url if set
     - else {releases_base_url}/vesyl-print-{desired}.manifest.json
7. Fetch manifest → verify Ed25519 (if require_signature)
8. Download tarball → verify SHA-256
   (while status is downloading|installing|pending_health: **pause**
    REST job pull and ActionCable print_job processing)
9. Extract to releases/<version>/ (path-escape rejected)
10. Write VERSION file; require the vesyl-print binary
11. Activate:
      - preferred: sudo -n apply-update activate <release> <current>
      - else: atomic symlink flip as the service user (lab install root)
12. Persist `update_status.json` with `status=pending_health`,
    `previous_version`, and `health_deadline_at` (default 120s)
13. Restart services (apply-update restart or systemctl)
14. New agent process runs the **health gate**:
      - local: `current` has the vesyl-print binary + VERSION matches target
      - if paired: `GET /print/v1/whoami` must reach the API (`ok` or
        `unauthorized` both count — proves the new code talks to cloud)
      - if unpaired: local checks only
15. Health OK → `status=idle` (OTA success); job pull/push resume
16. Health not ready → stay `pending_health` and retry each cycle
17. Hard local failure **or** deadline exceeded → auto-rollback to
    `previous_version`, restart services, `status=rolled_back`
18. Pre-activate failure: leave previous `current`; `status=failed`;
    no half-open symlink
```

**Rollback:** `vesyl-print update rollback` flips `current` to the previous
release directory (or an explicit version). Auto-rollback after a failed
health gate uses the same path.

### 4.4 Privileges (`setup.sh`)

| Path | Role |
|------|------|
| `/usr/local/lib/vesyl-print/apply-update` | Root helper: `activate`, `restart`, `rollback` |
| `/usr/local/lib/vesyl-print/wifi-setup` | Root helper for the LCD's Wi-Fi setup (NetworkManager hotspot / scan / connect) |
| `/etc/sudoers.d/vesyl-print` | `$RUN_USER ALL=(root) NOPASSWD:` those two helpers only |

Rules:

- Drop-in mode **0440**, validated with `visudo -cf` before install.
- Helpers owned by root, not writable by the service user.
- No shell wrappers or `NOPASSWD: ALL`.

`apply-update` trusts none of its arguments as a path, since sudoers lets the
service user pass anything:

- the install root is fixed in the installed file (`setup.sh` writes it);
  `activate` takes only `<root>/releases/<version>` and `<root>/current`,
  `rollback` only `<root>` and a version;
- the version must match the release pattern (never `.`, `..` or a `/`);
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
  "agent_version": "0.3.0",
  "hostname": "VESYL-PRINT-…",
  "platform": "linux-aarch64",
  "printers": [ … ],
  "update": {
    "status": "idle|downloading|installing|failed|rolled_back",
    "current_version": "0.3.0",
    "target_version": null,
    "last_error": null,
    "last_checked_at": "…"
  }
}
```

**Server → agent (response fields):**

```json
{
  "ok": true,
  "node_id": "…",
  "status": "online",
  "last_seen_at": "…",
  "desired_agent_version": "0.4.0",
  "update_channel": "stable",
  "update_url": "https://github.com/vesylapp/vesyl-print/releases/download/v0.4.0/vesyl-print-0.4.0.manifest.json"
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
  "update_public_key_path": "/etc/vesyl-print/keys/update_public.pem"
}
```

| Key | Default | Meaning |
|-----|---------|---------|
| `auto_update_enabled` | `true` | If false, log desired version but do not install |
| `update_channel` | `stable` | Informational / future channel latest index |
| `releases_base_url` | GitHub `…/releases/download` | Prefix; device appends `/vX.Y.Z/vesyl-print-X.Y.Z.manifest.json` |
| `update_require_signature` | `true` | Lab may set false only with care |
| `update_public_key_path` | empty | Explicit PEM path |

Env: `VESYL_PRINT_INSTALL_ROOT` overrides install root.

### 4.7 CLI

```bash
vesyl-print version
vesyl-print update check              # heartbeat; print desired if paired
vesyl-print update apply              # use cloud desired + update_url
vesyl-print update apply --version 0.4.0
vesyl-print update apply --manifest-url https://…
vesyl-print update apply --file ./rel.tar.gz --manifest ./rel.manifest.json
vesyl-print update rollback [--version X] [--restart]
```

### 4.8 Version source of truth

- Repo / release tree: `VERSION` file  
- The binary bakes its version in at build time (`VESYL_PRINT_VERSION`, set
  from the tag by `build-release.sh`; else `VERSION`), and the build checks the
  packaged binary reports it  
- Heartbeat, CLI and the LCD footer report that version  

Release process must bump `VERSION` (and tags) in the same commit as the ship.

### 4.9 Implementation map

| Component | Path |
|-----------|------|
| Core logic | `rust/crates/vesyl-print/src/update.rs` |
| Root helper | `scripts/apply-update` → `/usr/local/lib/vesyl-print/apply-update` |
| Heartbeat hook | `agent.rs` → `update::maybe_update_from_heartbeat` |
| HTTP client | `cloud.rs` `CloudClient::heartbeat` |
| Config | `config.rs` |
| CLI | `cli.rs` `version` / `update *` |
| Provisioning | `setup.sh` (units, CLI wrapper, helpers + sudoers, public key) |
| Build, sign, verify | `scripts/build-release.sh` |
| CI publish | `.github/workflows/release.yml` → GitHub Releases |
| Signing docs | `keys/README.md` |
| Tests | `update.rs` unit tests; `rust/crates/vesyl-print/tests/build_release.rs` and `apply_update.rs` (drive the scripts) |

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

### Open (must land for fleet OTA)

- [ ] Fleet metrics: version histogram, failure rate  
- [ ] Optional: mirror GitHub Release assets to `releases.vesyl.com` if customers block github.com  

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
| Bad signature / checksum | Do not flip `current`; `update.status=failed` |
| Download interrupted | No activate; retry on later heartbeat |
| Disk full | Fail before extract; report error |
| Activate succeeds, whoami never succeeds | Auto-rollback after `update_health_gate_seconds` (default 120) |
| Archive without the `vesyl-print` binary | Rejected before activate; `apply-update` also refuses such a slot |
| Activate succeeds, slot missing the binary | Immediate auto-rollback (hard local fail) |
| Agent self-restart SIGTERM during `apply-update restart` | **Not** a failure — restart is detached/`--no-block`; sticky false `failed` is recovered when version matches target |
| New binary never starts (crash-loop) | No process to roll back, and the CLI is the same binary. Support, as root: `/usr/local/lib/vesyl-print/apply-update rollback /opt/vesyl-print <previous>` then `apply-update restart` |
| Server omits desired version | No update attempt |
| `auto_update_enabled: false` | Log desired only; support can apply via CLI on-site |
| GitHub blocked, CDN allowed | Still works if artifacts on CDN |
| CDN blocked | No app OTA until IT allowlists release host |

**Support playbook (short):**

1. `vesyl-print version` / `update check`  
2. `journalctl -u vesyl-print-agent` for update errors  
3. `cat /var/lib/vesyl-print/update_status.json`  
4. `vesyl-print update rollback --restart` if new slot is bad (if the binary
   itself will not run: `sudo /usr/local/lib/vesyl-print/apply-update rollback
   /opt/vesyl-print <previous>` + `sudo /usr/local/lib/vesyl-print/apply-update restart`)  
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
