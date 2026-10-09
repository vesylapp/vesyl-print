//! Python-style log lines on stderr (journald under systemd):
//! `%(asctime)s %(levelname)s %(name)s: %(message)s`.

use std::io::Write;

struct Logger {
    level: log::LevelFilter,
}

/// True for this crate's own log targets: `vesyl-print.<module>`, which
/// every log call in the crate names, or the module path a call without a
/// target gets (`vesyl_print::<module>`).
fn own_target(target: &str) -> bool {
    ["vesyl-print", "vesyl_print"].iter().any(|name| {
        target
            .strip_prefix(name)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('.') || rest.starts_with("::"))
    })
}

impl log::Log for Logger {
    /// Up to the configured level for our own targets; third-party crates
    /// (they log under their module path, `ureq::run`) only for warnings and
    /// errors. A crate that asks first (`log_enabled!`) skips building a
    /// line nobody would see.
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= self.level
            && (metadata.level() <= log::Level::Warn || own_target(metadata.target()))
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let name = record.target();
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

/// The level for `verbose` and `VESYL_PRINT_LOG` (`env`), else `default`.
fn level(verbose: bool, env: Option<&str>, default: log::LevelFilter) -> log::LevelFilter {
    match env {
        _ if verbose => log::LevelFilter::Debug,
        Some("debug") => log::LevelFilter::Debug,
        Some("info") => log::LevelFilter::Info,
        Some("warning") | Some("warn") => log::LevelFilter::Warn,
        _ => default,
    }
}

fn install(verbose: bool, default: log::LevelFilter) {
    let env = std::env::var("VESYL_PRINT_LOG").ok();
    let level = level(verbose, env.as_deref(), default);
    if log::set_boxed_logger(Box::new(Logger { level })).is_ok() {
        log::set_max_level(level);
    }
}

/// Install the agent's logger. `verbose` (or `VESYL_PRINT_LOG=debug`)
/// enables debug.
pub fn init(verbose: bool) {
    install(verbose, log::LevelFilter::Info);
}

/// Install the logger of a one-off command (`update`, `claim`, …): warnings
/// and errors only, unless `VESYL_PRINT_LOG` asks for more. Without one,
/// their warnings were lost: a symlink root would not follow, a file it
/// could not hand to the service user, a release passed over.
pub fn init_command() {
    install(false, log::LevelFilter::Warn);
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::{Level, Log};
    use std::fmt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn enabled(logger: &Logger, target: &str, level: Level) -> bool {
        logger.enabled(&log::Metadata::builder().target(target).level(level).build())
    }

    /// Third-party lines below a warning were dropped in `log()` only, so
    /// `enabled()` said yes to them (and `log_enabled!` in ureq built debug
    /// lines nobody saw). The target filter now lives in `enabled()`.
    #[test]
    fn third_party_lines_below_warn_are_not_enabled() {
        let debug = Logger {
            level: log::LevelFilter::Debug,
        };
        for level in [Level::Info, Level::Debug, Level::Trace] {
            assert!(!enabled(&debug, "ureq::run", level), "{level}");
            // A prefix of ours is not ours.
            assert!(!enabled(&debug, "vesyl-printer", level), "{level}");
        }
        assert!(enabled(&debug, "ureq::run", Level::Warn));
        assert!(enabled(&debug, "rustls::conn", Level::Error));
        for target in [
            "vesyl-print",
            "vesyl-print.agent",
            "vesyl-print::jobs",
            "vesyl_print::agent",
        ] {
            assert!(enabled(&debug, target, Level::Debug), "{target}");
        }
        assert!(!enabled(&debug, "vesyl-print.agent", Level::Trace));

        let info = Logger {
            level: log::LevelFilter::Info,
        };
        assert!(!enabled(&info, "vesyl-print.agent", Level::Debug));
        assert!(enabled(&info, "vesyl-print.agent", Level::Info));
        assert!(!enabled(&info, "ureq::run", Level::Info));
    }

    /// The agent logs from info, a one-off command only warnings, unless
    /// VESYL_PRINT_LOG (or --verbose) asks for another level.
    #[test]
    fn levels() {
        use log::LevelFilter::*;
        assert_eq!(level(false, None, Info), Info);
        assert_eq!(level(false, None, Warn), Warn);
        assert_eq!(level(true, None, Warn), Debug);
        assert_eq!(level(false, Some("debug"), Warn), Debug);
        assert_eq!(level(false, Some("info"), Warn), Info);
        assert_eq!(level(false, Some("warning"), Info), Warn);
        assert_eq!(level(false, Some("warn"), Info), Warn);
        assert_eq!(level(false, Some("loud"), Warn), Warn);
    }

    /// Counts how often it is formatted.
    struct Probe<'a>(&'a AtomicUsize);

    impl fmt::Display for Probe<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.0.fetch_add(1, Ordering::SeqCst);
            f.write_str("probe")
        }
    }

    /// A record `enabled()` refuses is neither formatted nor written.
    #[test]
    fn a_disabled_record_is_never_formatted() {
        let logger = Logger {
            level: log::LevelFilter::Debug,
        };
        let formatted = AtomicUsize::new(0);
        let probe = Probe(&formatted);
        for (target, level) in [
            ("ureq::run", Level::Debug),
            ("vesyl-print.agent", Level::Trace),
        ] {
            logger.log(
                &log::Record::builder()
                    .target(target)
                    .level(level)
                    .args(format_args!("{probe}"))
                    .build(),
            );
        }
        assert_eq!(formatted.load(Ordering::SeqCst), 0);
    }
}
