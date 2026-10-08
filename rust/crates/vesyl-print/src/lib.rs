//! VESYL print node agent — Rust port of the Python agent.
//!
//! Module map (Python → Rust):
//!
//! | Python        | Rust            |
//! |---------------|-----------------|
//! | `config.py`   | [`config`]      |
//! | `auth.py`     | [`auth`]        |
//! | `statusio.py` | [`statusio`]    |
//! | `cloud.py`    | [`cloud`]       |
//! | `cable.py`    | [`cable`]       |
//! | `jobs.py`     | [`jobs`]        |
//! | `zpl.py`      | [`zpl`]         |
//! | `printers.py` | [`printers`]    |
//! | `agent.py`    | [`agent`]       |
//! | `cli.py`      | [`cli`]         |
//! | `update.py`   | [`update`]      |
//! | `sysinfo.py`  | [`sysinfo`] (hostname only so far) |

pub mod agent;
pub mod auth;
pub mod cable;
pub mod cli;
pub mod cloud;
pub mod config;
pub mod jobs;
pub mod logging;
pub mod net;
pub mod printers;
pub mod statusio;
pub mod sysinfo;
pub mod update;
pub mod util;
pub mod zpl;

#[cfg(test)]
mod testutil;

/// Generic boxed error for injectable hooks (ack, report_state, fetch, …).
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A JSON object (`dict[str, Any]` in the Python agent).
pub type JsonObject = serde_json::Map<String, serde_json::Value>;
