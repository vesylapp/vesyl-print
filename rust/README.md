# vesyl-print (Rust)

**Status: Rust-only.** The agent and CLI that ship on devices are one
binary, `vesyl-print`: the `vesyl-print-agent.service` unit runs
`vesyl-print agent`, and operators and the LCD display use the same binary as
the CLI. There is no Python agent, no bridge and no Python fallback; they
are in git history only. Only the LCD display stack is still Python
(`main.py` and its modules at the repository root), a thin client of the CLI
and of the state files, `printers.json` among them (see below).

```bash
cd rust
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

## Modules

| Module | Role |
|---|---|
| `agent.rs` | `vesyl-print agent`: heartbeat loop, job pull, cable session, OTA hook; writes `printers.json` after each inventory refresh |
| `cli.rs` | subcommands: `claim`, `enroll`, `status`, `queues`, `unpair`, `agent`, `version`, `update check\|apply\|rollback`, `print-test`, `test-print` |
| `cloud.rs` | wms-api client: claim / enroll / whoami / heartbeat / ws_ticket (`ureq`, OS trust store) |
| `cable.rs` | ActionCable `PrintNodeChannel` client (`tungstenite`, sync thread) |
| `jobs.rs` | durable queue + print pipeline (`lp`, CUPS wait) |
| `printers.rs` | CUPS discovery, Zebra LAN scan, auto-provisioning, inventory |
| `zpl.rs` | PDF/PNG/JPEG → ZPL `^GFA` for raw thermal queues (`image` crate, `pdftoppm`) |
| `update.rs` | app OTA: manifest verify (`ed25519-dalek`), download, extract, activate, health gate, rollback |
| `config.rs`, `auth.rs`, `statusio.rs` | paths and config, `credentials.json` (0600), `status.json` for the LCD |
| `net.rs` | shared HTTP plumbing (`ureq` agents for the API, job content, OTA and LAN probes): urllib-like timeouts and proxies, redirects, URL redaction for errors and logs |
| `util.rs` | durable atomic writes; root-safe directory walks and hand-over to the service user |
| `sysinfo.rs`, `logging.rs` | helpers |

Run the agent locally (unpaired, temp dirs):

```bash
VESYL_PRINT_CONFIG_DIR=/tmp/vp/cfg VESYL_PRINT_STATE_DIR=/tmp/vp/state \
VESYL_PRINT_INSTALL_ROOT=/tmp/vp/install cargo run -- agent
```

`VESYL_PRINT_LOG=debug` raises log verbosity.

## Interfaces the Python LCD relies on

- State files under the state dir (`/var/lib/vesyl-print`): `status.json`,
  `update_status.json`, and `printers.json` (`{"updated_at", "printers": [...]}`,
  mode 0644, rewritten after every inventory refresh, about every 15 s). The
  LCD shows every printer status as unknown once `updated_at` is more than
  120 s old, so a stopped refresher never leaves a stale `idle` on screen.
- `vesyl-print claim CODE [--name NAME] --json` and
  `vesyl-print test-print --queue Q --format pdf|zpl [--json]`: one JSON object
  on stdout, exit 0 on success and 1 on failure (shapes in the top-level
  README). The test labels come from `assets/test-labels/` next to the running
  executable; `VESYL_PRINT_ASSETS_DIR` points at another `assets` directory
  (tests use it).
- The agent provisions printers (`printers::ensure_printers`) once at startup,
  in a background thread; the LCD no longer does.

## Tests

Unit tests sit next to the code and replace mocks with injectable hooks:
`jobs::Pipeline` takes `lp`, `ack`, `report_state`, `fetch_url`,
`wait_cups_job`, `cups_lookup` and `supports_raw` as closures (and `stop` as
a flag), and the cloud/cable tests run against local HTTP/WebSocket servers.
The PDF tests run against the `pdftoppm` and `gs` on PATH and skip without
them, unless `VESYL_PRINT_REQUIRE_RENDERERS=1` (CI's test job) makes a
missing `pdftoppm`, `pdfinfo` or `gs` fail them.

`crates/vesyl-print/tests/` drives the release tooling in temp dirs:
`build_release.rs` runs `scripts/build-release.sh` (all modes, with a fake
cargo/qemu/readelf and throwaway Ed25519 keys) and checks that its manifests
verify with `update::verify_manifest`, including a non-ASCII changelog, that
it refuses a binary above the glibc floor and takes exactly the versions
`update::is_version` takes, and that the workflows pin their actions and
tools; `apply_update.rs` runs a copy of the root helper against a fake
install root; `setup_sh.rs` runs `setup.sh`'s preflight unprivileged (with
`sudo` stubbed: the install root in plain form, versions as `is_version`
judges them, and `apply-update`'s version check too), and its `root_*` tests
run all of `setup.sh` in a chroot inside a private mount namespace (host
`/usr` read-only; apt-get, dpkg-query, systemctl, usermod, visudo,
tailscale, curl and sudo stubbed): a custom `INSTALL_ROOT` written into both
root helpers and activated through the installed `apply-update` (also with
trailing slashes, as the agent's OTA paths), the service account
(`SUDO_USER`, else the tree's owner, never root), required packages
installed without the optional one, re-provisioning without a Tailscale key,
and the source-tree cleanup.
They need bash, jq, rsync, openssl and GNU coreutils; when one is missing they
skip locally and fail in CI.

Tests named `root_*` are `#[ignore]`d: they chown (and chroot). Run each test
binary in a user namespace that maps uid 1000 too:
`unshare --map-root-user --map-auto <test binary> --include-ignored` (list the
binaries with `cargo test --locked --no-run --message-format=json`).

## Release builds

`scripts/build-release.sh` cross-compiles for `aarch64-unknown-linux-gnu.2.31`
with cargo-zigbuild (Debian bullseye and newer) and sets `VESYL_PRINT_VERSION`
from the release tag. It refuses a binary that needs a glibc symbol newer
than 2.31 (`readelf -V`), and in CI one that does not run under
qemu-aarch64. Wherever the Rust tests run in CI, the `aarch64` job of
`.github/workflows/rust.yml` runs that build too (`BUILD_ONLY=1`, a
throwaway version), with clippy for aarch64 and the unit tests under qemu,
so code that only breaks on the Pi fails before a tag. Both workflows
install cargo-zigbuild and zig from `.github/zigbuild-requirements.txt`
(pinned versions and hashes). See [OTA_UPDATES.md](../OTA_UPDATES.md) §4.2.

The first Rust-only release is tagged `v0.5.0`, with `VERSION` bumped to
0.5.0 in the same commit: the lab Pi already ran lab builds 0.4.0 through
0.4.3 (throwaway lab key), and an agent ignores a desired version equal to
its own. `update::version_cmp` ignores a `-` suffix (an open item: 0.9.1-rc.1
counts as 0.9.1), so releases are tagged `vX.Y.Z`, and no build made before
the tag may be numbered 0.5.0 or 0.5.0-anything (use e.g. 0.4.4).
`MIN_AGENT_VERSION` stays 0.4.0, the Python-era cutoff
([OTA_UPDATES.md](../OTA_UPDATES.md) §4.8).

## Behaviour notes

Contracts a change here must keep (several differ from the retired Python
agent on purpose):

- `VERSION` is baked in at compile time (`VESYL_PRINT_VERSION`, else the
  repository `VERSION` file), not read at runtime.
- `Pipeline::default()` waits on CUPS synchronously; the agent passes
  `config.wait_cups` (default `async`).
- Unparsable `options.copies` falls back to 1 instead of failing the job.
- ZPL resize uses Lanczos3 from the `image` crate; grayscale conversion uses
  Pillow's coefficients.
- A release slot is runnable only with the `vesyl-print` binary; Python-era
  slots never count.
- The OTA public key is compiled in from `keys/update_public.pem`; a key file
  set in config overrides it.
- `ReleaseManifest::canonical_bytes()` must stay byte-identical to the
  canonical JSON `scripts/build-release.sh` signs (`jq -S -c -a`: sorted keys,
  compact, `\uXXXX` for non-ASCII, nulls and `signature` dropped).
- `update_status.json`, `status.json` and `printers.json` are written
  atomically.
- Agent sleeps wake within 100 ms of SIGTERM. Once it is set, the queue drain
  takes no further job, no job goes to `lp`, and a synchronous CUPS wait ends
  at its next check (it naps 100 ms at a time between `lpstat` polls). A
  step already running (a content fetch, a conversion, `lp`, an `lpstat`
  poll of up to 15 s, a wait-tick heartbeat) finishes first.
- Every CUPS tool (lp, lpstat, lpinfo, lpoptions, ipptool, lpadmin) runs
  with LC_ALL and LC_MESSAGES set to C.UTF-8 (`printers::CUPS_ENV`): their
  output is parsed in English.
- Queue records keep the cloud payload as received; the agent's notes go
  under `_agent` (attempts, cups_job_id, submitted_at). A record that says
  CUPS has the job is never sent to `lp` again; three deaths in one job
  retire it as `crash_loop`. Agents of older releases ignore `_agent`, so
  after a rollback to one such a record may print again.
- Release tarballs keep shipping `base.jpg`: it is the sample image for
  `vesyl-print print-test --file /opt/vesyl-print/current/base.jpg` (top-level
  README).
- `status --check` and `queues --json` print JSON keys sorted.
- ZPL conversion has size limits Python lacked. A PDF page over 50 MP fails
  with `pdf_page_too_large` (Python rasterized it, so A0 at 203 dpi printed
  scaled down), unless `zpl_fit` scales it onto the label, when it is drawn
  at the highest dpi within 50 MP instead. A page pdftoppm could not allocate
  fails with `pdf_render` (Python printed a one-dot label). A label graphic
  over 50 M dots, or a resize needing over 512 MiB, fails with
  `label_too_large` (Python printed such labels; on an absurd box, such as
  200000 dots square, it ran out of memory, and the Rust agent, before this
  limit, aborted on the allocation). `zpl_x`/`zpl_y` over 32000 fail with
  `zpl_error` (Python emitted out-of-range `^FO`/`^LL`). A positive `zpl_x`
  narrows the fit box and widens `^PW` by that much (Python clipped the right
  edge). Images over 178,956,970 pixels fail with `image_bad`, as under
  Pillow.
- The cable WebSocket handshake follows no HTTP redirect (the Python agent's
  websocket-client followed up to 3): a redirecting `cable_url` leaves push
  off, while REST pull still works.
- Every activation (an OTA install, `update apply --file` /
  `--manifest-url`, `update rollback`, the health gate's rollback) goes
  through the installed `apply-update` sudo helper when present, and its
  refusal is final; `update::flip_current` runs only without a helper (lab,
  tests). A rollback takes the newest other slot that can run and refuses an
  explicit one that cannot.
- `vesyl-print agent` refuses to run as root: run it as the service user
  (`sudo -u <service user> vesyl-print agent`). On SIGTERM or SIGINT it
  finishes the step in flight, an HTTP request included, so a stop takes up
  to systemd's `TimeoutStopSec` (default 90 s); a second signal exits at
  once. A stop during an OTA download removes the partial file; one after
  activation skips the restart, and the next start runs the health gate.
- Run as root (an operator's CLI), `util::write_durable`,
  `create_dir_all_owned`, `open_dir_owned` and `hand_tree_to_parent_owner`
  follow no symlink on the path except a root-owned link in a root-owned
  directory that is not group/other-writable or is sticky; anything else
  fails with ENOTDIR and a warning naming the link. A state dir that is a
  symlink to another disk must therefore be root's link. Root's writes go to
  the service user that owns the tree.
- `update apply` (online, or `--version`) installs even with
  `auto_update_enabled: false`, and even a version the agent holds or backs
  off; `--version X` (a leading `v` allowed, validated before any network
  call) uses the heartbeat's `update_url` only when the server wants exactly
  X, and any source's manifest for another version is refused.
- An install that fails for good (`update::fails_for_good`: bad_manifest,
  bad_signature, bad_checksum, bad_archive, too_old, version_mismatch,
  no_exchange) holds the desired version until the server asks for another
  or `update apply` runs; any other failure backs off 1 min, doubling to a
  1 h cap. `update rollback` holds the version it left. `update_status.json`
  carries `last_error_code`, `attempts` and `retry_at` (like `armed_at`) only
  while set, so the file is unchanged for every status that does not need
  them.
- Reinstalling the active version swaps the slot in one step with
  renameat2(RENAME_EXCHANGE); without exchange support that install is
  refused (`no_exchange`), and the active slot is never replaced another way.
