//! `tracing` setup: console + a rolling file under `server.log_dir` (SDD §2 "Logging").
//!
//! # Why a broken log directory does not stop the node
//!
//! Every other startup failure in [`crate::main`] is fatal, and deliberately so. This one is not:
//! a node that refuses to ingest the night's frames because its log volume filled up has failed
//! the operator worse than one that runs and says so on the console. The failure is
//! logged at `WARN` and reported by `/api/system/info` (`logging.file`), so it is visible rather
//! than silent.
//!
//! # Why the file writer is non-blocking
//!
//! `tracing-appender`'s non-blocking writer hands lines to a background thread. Without it, a
//! log line written from a runtime worker blocks that worker on the disk — precisely the
//! blocking-on-the-runtime failure SDD §2 exists to prevent.

use std::path::{Path, PathBuf};

use astroctl_core::config::LogLevel;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Layer as _};

/// Live logging state. Dropping it flushes and stops the file writer, so `main` holds it until
/// the process exits.
#[derive(Debug)]
pub struct Telemetry {
    /// Held, never read: dropping it is what flushes the background writer.
    _guard: Option<WorkerGuard>,
    file: Option<PathBuf>,
    error: Option<String>,
}

impl Telemetry {
    /// The directory the rolling log is being written to, if file logging came up.
    #[must_use]
    pub fn file_dir(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    /// Why file logging is not running, if it is not.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Emit whatever could only be said once the subscriber existed.
    pub fn report(&self) {
        match (&self.file, &self.error) {
            (Some(dir), _) => tracing::info!(dir = %dir.display(), "file logging active"),
            (None, Some(error)) => tracing::warn!(
                %error,
                "file logging is NOT active — this node logs to the console only"
            ),
            (None, None) => {}
        }
    }
}

/// Install the global subscriber.
///
/// `RUST_LOG`, when set, wins over `server.log_level`: an operator debugging a specific module at
/// 2 a.m. should not have to edit and reload a YAML file to do it.
///
/// `log_prefix` names the file (`astroctl-stack.log.2026-07-29`); daily rotation keeps one
/// night's logs in one file, which is the unit an operator asks for.
#[must_use]
pub fn init(level: LogLevel, log_dir: &Path, log_prefix: &str) -> Telemetry {
    let filter = || {
        EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(default_directive(level)))
    };

    let console = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_filter(filter());

    let (file_layer, guard, file, error) = match open_file_writer(log_dir, log_prefix) {
        Ok((writer, guard)) => {
            let layer = tracing_subscriber::fmt::layer()
                // No colour codes in a file: they turn `grep` output into escape soup.
                .with_ansi(false)
                .with_target(true)
                .with_writer(writer)
                .with_filter(filter());
            (Some(layer), Some(guard), Some(log_dir.to_path_buf()), None)
        }
        Err(error) => (None, None, None, Some(error)),
    };

    tracing_subscriber::registry()
        .with(console)
        .with(file_layer)
        .init();

    Telemetry {
        _guard: guard,
        file,
        error,
    }
}

type FileWriter = (tracing_appender::non_blocking::NonBlocking, WorkerGuard);

fn open_file_writer(log_dir: &Path, log_prefix: &str) -> Result<FileWriter, String> {
    std::fs::create_dir_all(log_dir)
        .map_err(|e| format!("cannot create log directory `{}`: {e}", log_dir.display()))?;

    // `rolling::daily` panics on a first write it cannot perform, which would be a panic on the
    // logging path rather than a startup error. Prove the directory is writable here instead.
    let probe = log_dir.join(format!(".{log_prefix}.write-test"));
    std::fs::write(&probe, b"")
        .map_err(|e| format!("log directory `{}` is not writable: {e}", log_dir.display()))?;
    let _ = std::fs::remove_file(&probe);

    let appender = tracing_appender::rolling::daily(log_dir, format!("{log_prefix}.log"));
    Ok(tracing_appender::non_blocking(appender))
}

/// The default filter for a configured level.
///
/// Third-party crates are held one level quieter than our own below `DEBUG`: at `INFO`, hyper and
/// tokio's own `INFO` lines say nothing an operator needs and would bury the ones that do.
fn default_directive(level: LogLevel) -> String {
    let ours = match level {
        LogLevel::Trace => "trace",
        LogLevel::Debug => "debug",
        LogLevel::Info => "info",
        LogLevel::Warn => "warn",
        LogLevel::Error => "error",
    };
    match level {
        LogLevel::Trace | LogLevel::Debug => ours.to_owned(),
        _ => format!("warn,astroctl_stack={ours},astroctl_core={ours},astroctl={ours}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_levels_keep_third_party_crates_at_warn() {
        let directive = default_directive(LogLevel::Info);
        assert!(directive.starts_with("warn,"), "{directive}");
        assert!(directive.contains("astroctl_stack=info"), "{directive}");
    }

    #[test]
    fn debug_and_trace_turn_everything_up() {
        assert_eq!(default_directive(LogLevel::Debug), "debug");
        assert_eq!(default_directive(LogLevel::Trace), "trace");
    }

    #[test]
    fn every_directive_parses_as_a_filter() {
        for level in [
            LogLevel::Trace,
            LogLevel::Debug,
            LogLevel::Info,
            LogLevel::Warn,
            LogLevel::Error,
        ] {
            let directive = default_directive(level);
            EnvFilter::try_new(&directive)
                .unwrap_or_else(|e| panic!("`{directive}` is not a valid filter: {e}"));
        }
    }

    #[test]
    fn an_unwritable_log_directory_is_an_error_not_a_panic() {
        // `/proc` exists and is not writable by anyone, root included.
        let error = open_file_writer(Path::new("/proc/astroctl-m0t05"), "astroctl-stack")
            .expect_err("cannot create a directory under /proc");
        assert!(error.contains("log directory"), "{error}");
    }

    #[test]
    fn a_usable_log_directory_opens() {
        let dir = std::env::temp_dir().join(format!("astroctl-m0t05-log-{}", std::process::id()));
        let (_writer, _guard) =
            open_file_writer(&dir, "astroctl-stack").expect("a temp directory is writable");
        std::fs::remove_dir_all(&dir).ok();
    }
}
