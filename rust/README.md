# vesyl-print (Rust)

The agent and CLI that ship on devices: one binary, `vesyl-print`. The
`vesyl-print-agent.service` unit runs `vesyl-print agent`; operators and the
LCD display use the same binary as the CLI. The LCD display stack is still
Python (`main.py` and its modules at the repository root) and talks to the
binary only through state files and CLI calls.

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
| `sysinfo.rs`, `net.rs`, `util.rs`, `logging.rs` | helpers |

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
`wait_cups_job` and `supports_raw` as closures, and the cloud/cable tests run
against local HTTP/WebSocket servers.

`crates/vesyl-print/tests/` drives the release tooling in temp dirs:
`build_release.rs` runs `scripts/build-release.sh` (all modes, with a fake
cargo/qemu and throwaway Ed25519 keys) and checks that its manifests verify
with `update::verify_manifest`, including a non-ASCII changelog;
`apply_update.rs` runs a copy of the root helper against a fake install root;
`setup_sh.rs` runs `setup.sh`'s preflight unprivileged (with `sudo` stubbed),
and its `root_*` tests run all of `setup.sh` in a chroot inside a private
mount namespace (host `/usr` read-only; apt-get, systemctl, usermod, visudo,
tailscale, curl and sudo stubbed): a custom `INSTALL_ROOT` written into both
root helpers and activated through the installed `apply-update`,
re-provisioning without a Tailscale key, and the source-tree cleanup.
They need bash, jq, rsync, openssl and GNU coreutils; when one is missing they
skip locally and fail in CI.

Tests named `root_*` are `#[ignore]`d: they chown (and chroot). Run each test
binary in a user namespace that maps uid 1000 too:
`unshare --map-root-user --map-auto <test binary> --include-ignored` (list the
binaries with `cargo test --locked --no-run --message-format=json`).

## Release builds

`scripts/build-release.sh` cross-compiles for `aarch64-unknown-linux-gnu.2.31`
with cargo-zigbuild (Debian bullseye and newer) and sets `VESYL_PRINT_VERSION`
from the release tag. See [OTA_UPDATES.md](../OTA_UPDATES.md) §4.2.

The first Rust-only release is tagged `v0.5.0`, with `VERSION` bumped to
0.5.0 in the same commit: the lab Pi already ran lab builds 0.4.0 and 0.4.1
(throwaway lab key), and an agent ignores a desired version equal to its own.
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
- Agent sleeps wake within 100 ms of SIGTERM.
- `printers::test_image()` looks for `base.jpg` next to the executable, then
  `/opt/vesyl-print/current/base.jpg`, so release tarballs keep shipping it.
- `status --check` and `queues --json` print JSON keys sorted.
- `update apply --manifest-url` and `update rollback` use the installed
  `apply-update` sudo helper when present.
