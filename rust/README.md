# vesyl-print (Rust port)

In-progress port of the Python agent. The Python app is still what ships;
this tree builds alongside it until the agent service can switch over.

```bash
cd rust
cargo test
cargo clippy --all-targets
```

## Status

| Python | Rust | State |
|---|---|---|
| `config.py` | `config.rs` | ported |
| `auth.py` | `auth.rs` | ported (reads existing `credentials.json`) |
| `statusio.py` | `statusio.rs` | ported (same `status.json` shape for the LCD) |
| `cloud.py` | `cloud.rs` | ported (`ureq`, OS trust store) |
| `cable.py` | `cable.rs` | ported (`tungstenite`, sync thread) |
| `jobs.py` | `jobs.rs` | ported |
| `zpl.py` | `zpl.rs` | ported (`image` crate) |
| `printers.py` | `printers.rs` | ported |
| `update.py` | `update.rs` | ported (`ed25519-dalek`, `tar`) |
| `agent.py` | `agent.rs` + `vesyl-print agent` | ported |
| `sysinfo.py` | `sysinfo.rs` | `hostname` only |
| `cli.py` | — | next |
| display: `main.py`, `touch.py`, `framebuffer.py`, `stream_lcd.py`, … | — | later |

Run the agent locally (unpaired, temp dirs):

```bash
VESYL_PRINT_CONFIG_DIR=/tmp/vp/cfg VESYL_PRINT_STATE_DIR=/tmp/vp/state \
VESYL_PRINT_INSTALL_ROOT=/tmp/vp/install cargo run -- agent
```

`VESYL_PRINT_LOG=debug` raises log verbosity.

Python tests that used `mock.patch` map to injectable hooks: `jobs::Pipeline`
takes `lp`, `ack`, `report_state`, `fetch_url`, `wait_cups_job` and
`supports_raw` as closures, and the cloud/cable tests run against local
HTTP/WebSocket servers.

## Intentional differences from Python

- `VERSION` is baked in at compile time (`include_str!`), not read at runtime.
- No "websocket library missing" mode: push is always available.
- `Pipeline::default()` waits on CUPS synchronously like `process_job`'s
  default; the agent passes `config.wait_cups` (default `async`).
- Unparsable `options.copies` falls back to 1 instead of failing the job.
- ZPL resize uses Lanczos3 from the `image` crate. Its output can differ from
  Pillow's LANCZOS by a few edge pixels. Grayscale conversion matches Pillow's
  coefficients exactly.
- OTA slots count as healthy with either the Python entrypoints
  (`agent.py`/`main.py`) or the Rust binary (`vesyl-print` / `bin/vesyl-print`),
  so rollback works across the migration in both directions.
- The OTA public key is compiled in from `keys/update_public.pem`; a key file
  set in config still overrides it.
- `update_status.json` is written atomically (Python wrote it in place).
- Agent sleeps wake within 100 ms of SIGTERM (Python finished its sleep).
- `printers::test_image()` looks for `base.jpg` next to the executable, then
  `/opt/vesyl-print/current/base.jpg`, so release tarballs must keep shipping it.
