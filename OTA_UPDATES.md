# OTA updates — long-term plan and mechanics

Living document for **vesyl-print** appliances (Raspberry Pi print nodes on
customer networks). Update this file when the control plane, artifact format,
install layout, or OS strategy changes.

| | |
|--|--|
| **Owner** | Print / device platform |
| **Last reviewed** | 2026-07-16 |
| **Status** | App OTA client + GitHub Releases CI + wms-api heartbeat OTA directives (fleet/node pin) |
| **Related code** | `update.py`, `scripts/build-release.sh`, `scripts/apply-update`, `.github/workflows/release.yml`, `agent.py`, `cli.py`, `config.py`, `setup.sh`, `keys/` |

---

## 1. Goals

1. **Fleet updates without per-site SSH** — appliances initiate all traffic (outbound HTTPS).
2. **Integrity** — TLS plus **Ed25519-signed manifests**; never trust “just a URL.”
3. **Atomic install + rollback** — dual-slot layout; failed health → previous slot.
4. **Preserve site state** — credentials, job queue, CUPS printers, config stay put.
5. **Separate app vs OS** — ship Python/agent features weekly; patch OS/kernel on a slower, safer track.
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
    0.3.0/                           # previous (rollback)
    0.4.0/                           # active tree (agent.py, main.py, …)
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

Systemd units run from `current` after `setup.sh` (factory path). Lab can still
use a git checkout as the *source* tree; setup copies it into
`/opt/vesyl-print/releases/<VERSION>` and points `current` there. OTA stages
new versions beside it under the same install root.

### 4.2 Artifact format

**CDN:** [GitHub Releases](https://github.com/benwyrosdick/vesyl-print/releases) (for now).

CI (`.github/workflows/release.yml`) runs on tag `vX.Y.Z` and uploads:

| Asset | Purpose |
|-------|---------|
| `vesyl-print-X.Y.Z-linux-aarch64.tar.gz` | App tree |
| `vesyl-print-X.Y.Z.manifest.json` | Metadata + sha256 + Ed25519 signature |

**URLs (device default `releases_base_url`):**

```text
https://github.com/benwyrosdick/vesyl-print/releases/download/vX.Y.Z/vesyl-print-X.Y.Z.manifest.json
https://github.com/benwyrosdick/vesyl-print/releases/download/vX.Y.Z/vesyl-print-X.Y.Z-linux-aarch64.tar.gz
```

Build locally: `./scripts/build-release.sh [VERSION]` with
`UPDATE_PRIVATE_KEY` or `UPDATE_PRIVATE_KEY_FILE` set.

**Manifest fields (contract):**

```json
{
  "version": "0.4.0",
  "channel": "stable",
  "min_agent_version": "0.3.0",
  "artifact_url": "https://github.com/benwyrosdick/vesyl-print/releases/download/v0.4.0/vesyl-print-0.4.0-linux-aarch64.tar.gz",
  "artifact_sha256": "<64 hex>",
  "signature": "<base64 Ed25519>",
  "released_at": "2026-07-16T00:00:00Z"
}
```

**Signature:** Ed25519 over **canonical JSON** of the manifest **excluding**
`signature` (sorted keys, compact separators). See `keys/README.md`.

**Public key locations (first match wins):**

1. `config.update_public_key_path`
2. `/etc/vesyl-print/keys/update_public.pem` (installed by `setup.sh` if present in repo)
3. Bundled `keys/update_public.pem` next to the app
4. Optional baked-in PEM in `update.py` (empty by default)

Private key: CI secrets / HSM only — **never** on devices.

### 4.3 Device-side flow

Implemented primarily in `update.py`, invoked from the agent after a successful
heartbeat and from the CLI.

```text
1. Heartbeat POST includes agent_version, platform, optional update status blob
2. Response may include desired_agent_version (+ update_channel, update_url)
3. If desired empty or == current → idle
4. If auto_update_enabled false → record target only, do not install
5. Resolve manifest URL:
     - heartbeat.update_url if set
     - else {releases_base_url}/vesyl-print-{desired}.manifest.json
6. Fetch manifest → verify Ed25519 (if require_signature)
7. Download tarball → verify SHA-256
8. Extract to releases/<version>/ (path-escape rejected)
9. Write VERSION file; ensure agent.py or main.py present
10. Activate:
      - preferred: sudo -n apply-update activate <release> <current>
      - else: atomic symlink flip as the service user (lab install root)
11. Restart services (apply-update restart or systemctl)
12. Persist update_status.json; next heartbeats report status
13. On failure: leave previous current; status=failed; no half-open symlink
```

**Rollback:** `vesyl-print update rollback` flips `current` to the previous
release directory (or an explicit version).

### 4.4 Privileges (`setup.sh`)

| Path | Role |
|------|------|
| `/usr/local/lib/vesyl-print/apply-update` | Root helper: `activate`, `restart`, `rollback` |
| `/etc/sudoers.d/vesyl-print` | `$RUN_USER ALL=(root) NOPASSWD: /usr/local/lib/vesyl-print/apply-update` only |

Rules:

- Drop-in mode **0440**, validated with `visudo -cf` before install.
- Helper owned by root, not writable by the service user.
- No shell wrappers or `NOPASSWD: ALL`.

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
  "update_url": "https://github.com/benwyrosdick/vesyl-print/releases/download/v0.4.0/vesyl-print-0.4.0.manifest.json"
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
  "releases_base_url": "https://github.com/benwyrosdick/vesyl-print/releases/download",
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
- `config.AGENT_VERSION` reads `VERSION` at import  
- Heartbeat and CLI report that version  

Release process must bump `VERSION` (and tags) in the same commit as the ship.

### 4.9 Implementation map

| Component | Path |
|-----------|------|
| Core logic | `update.py` |
| Root helper | `scripts/apply-update` → `/usr/local/lib/vesyl-print/apply-update` |
| Heartbeat hook | `agent.py` → `maybe_update_from_heartbeat` |
| HTTP client | `cloud.py` `heartbeat(..., update=)` |
| Config | `config.py` |
| CLI | `cli.py` `version` / `update *` |
| Provisioning | `setup.sh` (helper + sudoers + optional public key) |
| Build + sign | `scripts/build-release.sh` |
| CI publish | `.github/workflows/release.yml` → GitHub Releases |
| Signing docs | `keys/README.md` |
| Tests | `tests/test_update.py` |

---

## 5. What is done vs open

### Done (device + publish)

- [x] Dual-slot install + rollback APIs  
- [x] Manifest parse, sha256 download verify  
- [x] Ed25519 verify via `cryptography` (when key present)  
- [x] Heartbeat request carries update status; response drives desired version (**plan A**)  
- [x] CLI check / apply / rollback  
- [x] `setup.sh` installs apply-update + sudoers  
- [x] Unit tests for extract, flip, rollback, checksum, heartbeat idle paths  
- [x] CI: build tarball + signed manifest; upload to **GitHub Releases** (CDN)  
- [x] Public key at `keys/update_public.pem` (private key = repo secret `UPDATE_PRIVATE_KEY`)  
- [x] wms-api: accept `update` on heartbeat; return `desired_agent_version` / channel / url  
  (`Print::UpdateDirective`, `print_nodes.desired_agent_version`, settings `PRINT_DESIRED_AGENT_VERSION`)  

### Open (must land for fleet OTA)

- [x] Migrate production units to `/opt/vesyl-print/current` (factory `setup.sh`)  
- [ ] Post-update health gate (whoami success before declaring success; auto-rollback on failure)  
- [ ] Pause job pull during install; avoid updating mid-job  
- [ ] LCD “Updating…” / failed update messaging  
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
| App (Python agent/display) | Signed OTA tarball | This repo + CI + wms-api |
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
| Activate succeeds, agent crash-loops | Manual/auto rollback to previous slot (auto health-gate still open) |
| Server omits desired version | No update attempt |
| `auto_update_enabled: false` | Log desired only; support can apply via CLI on-site |
| GitHub blocked, CDN allowed | Still works if artifacts on CDN |
| CDN blocked | No app OTA until IT allowlists release host |

**Support playbook (short):**

1. `vesyl-print version` / `update check`  
2. `journalctl -u vesyl-print-agent` for update errors  
3. `cat /var/lib/vesyl-print/update_status.json`  
4. `vesyl-print update rollback --restart` if new slot is bad  
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

- [x] `update.py` + CLI + agent heartbeat hook (plan A)  
- [x] apply-update + sudoers via `setup.sh`  
- [x] CI publish to GitHub Releases  
- [x] Factory path always uses `/opt/vesyl-print/current`  
- [ ] Health-check + automatic rollback  

### Phase 2 — Cloud control plane (wms-api)

- [x] Heartbeat accepts `update` status; returns OTA plan-A fields  
- [x] Fleet default via `PRINT_DESIRED_AGENT_VERSION` + per-node `desired_agent_version`  
- [ ] GraphQL / admin UI to set pins and view update_status  
- [ ] Org-level policy + staged rollout  

### Phase 3 — Fleet polish

- [ ] LCD update state  
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
