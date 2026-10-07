//! Python-style log lines on stderr (journald under systemd):
//! `%(asctime)s %(levelname)s %(name)s: %(message)s`.

use std::io::Write;

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
        // Third-party crates log under their module path; only surface their warnings.
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

/// Install the logger. `verbose` (or `VESYL_PRINT_LOG=debug`) enables debug.
pub fn init(verbose: bool) {
    let level = match std::env::var("VESYL_PRINT_LOG").as_deref() {
        _ if verbose => log::LevelFilter::Debug,
        Ok("debug") => log::LevelFilter::Debug,
        Ok("warning") | Ok("warn") => log::LevelFilter::Warn,
        _ => log::LevelFilter::Info,
    };
    if log::set_boxed_logger(Box::new(Logger { level })).is_ok() {
        log::set_max_level(level);
    }
}
