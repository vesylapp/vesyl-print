//! VESYL print node: the cloud agent and the `vesyl-print` CLI. The LCD
//! display is Python; it reads the state files the agent writes
//! (`status.json`, `printers.json`, `update_status.json`) and calls the CLI.
//!
//! | Module       | What it holds                                              |
//! |--------------|------------------------------------------------------------|
//! | [`agent`]    | heartbeat loop: whoami, job pull, ActionCable push, OTA    |
//! | [`cli`]      | `vesyl-print` subcommands                                  |
//! | [`config`]   | `config.json`, env overrides, config/state dirs            |
//! | [`auth`]     | device credentials (`credentials.json`, mode 0600)         |
//! | [`cloud`]    | print/v1 REST client                                       |
//! | [`cable`]    | ActionCable `PrintNodeChannel` client                      |
//! | [`net`]      | HTTP transport: timeouts, proxies, redirects               |
//! | [`jobs`]     | durable job queue, content fetch, CUPS submit + completion |
//! | [`printers`] | CUPS discovery and provisioning                            |
//! | [`zpl`]      | PDF / raster to ZPL for raw thermal queues                 |
//! | [`update`]   | app OTA: verify, install, activate, roll back, health gate |
//! | [`statusio`] | `status.json`, pairing and cloud state for the LCD         |
//! | [`sysinfo`]  | host facts (hostname)                                      |
//! | [`logging`]  | log lines on stderr (journald)                             |
//! | [`util`]     | JSON coercion, home dirs, durable writes safe for root     |

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

/// A JSON object: API bodies, `config.json`, the release manifest.
pub type JsonObject = serde_json::Map<String, serde_json::Value>;
