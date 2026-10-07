//! `vesyl-print` binary. Only the `agent` subcommand is ported so far; the
//! claim/enroll/status/queues/unpair CLI and the LCD display still run from
//! the Python app.

use std::io::Write;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use vesyl_print::agent::Agent;
use vesyl_print::config::{agent_version, load_config};

/// `%(asctime)s %(levelname)s %(name)s: %(message)s` to stderr (journald).
struct Logger {
    level: log::LevelFilter,
}

impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // Third-party crates log under their module path; keep ours readable.
        let name = record.target();
        if record.level() > log::Level::Warn && !name.starts_with("vesyl-print") {
            return;
        }
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S,%3f");
        let level = match record.level() {
            log::Level::Error => "ERROR",
            log::Level::Warn => "WARNING",
            log::Level::Info => "INFO",
            log::Level::Debug | log::Level::Trace => "DEBUG",
        };
        let _ = writeln!(
            std::io::stderr().lock(),
            "{ts} {level} {name}: {}",
            record.args()
        );
    }

    fn flush(&self) {}
}

fn init_logging() {
    let level = match std::env::var("VESYL_PRINT_LOG").as_deref() {
        Ok("debug") => log::LevelFilter::Debug,
        Ok("warning") | Ok("warn") => log::LevelFilter::Warn,
        _ => log::LevelFilter::Info,
    };
    let _ = log::set_boxed_logger(Box::new(Logger { level }));
    log::set_max_level(level);
}

fn usage() -> ExitCode {
    eprintln!("usage: vesyl-print <agent|--version>");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("agent") => {
            init_logging();
            let stop = Arc::new(AtomicBool::new(false));
            for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
                if let Err(e) = signal_hook::flag::register(sig, stop.clone()) {
                    eprintln!("signal handler: {e}");
                    return ExitCode::FAILURE;
                }
            }
            Agent::new(load_config(None, None)).run(stop);
            ExitCode::SUCCESS
        }
        Some("--version") | Some("version") => {
            println!("{}", agent_version());
            ExitCode::SUCCESS
        }
        _ => usage(),
    }
}
