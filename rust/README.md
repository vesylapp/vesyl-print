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
| `printers.py` | `printers.rs` | `queue_supports_raw` only |
| `agent.py`, `cli.py`, `update.py`, rest of `printers.py` | — | next |
| display: `main.py`, `touch.py`, `framebuffer.py`, `stream_lcd.py`, … | — | later |

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
