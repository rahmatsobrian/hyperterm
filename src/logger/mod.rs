//! Logger module.
//!
//! Responsibilities (per spec):
//!   - TRACE/DEBUG/INFO/WARN/ERROR/FATAL levels
//!   - Every log line: timestamp, thread id, module, source file, line, function*
//!   - Writes to `logs/` folder (rotated by day)
//!   - Captures Rust panics into `logs/panic.log`
//!   - `logs/crash.log` written by the top-level crash handler in `main.rs`
//!   - `logs/diagnostic.log` written by `export_diagnostic()` (see crash module)
//!
//! *Rust's `tracing` does not expose a "function name" macro out of the box the way
//! C++/C# do, so we approximate it with the `target` (module path) + file + line,
//! which is what `tracing::instrument`/`#[tracing::instrument]` gives you on real
//! call sites throughout the codebase.

pub mod crash;
#[cfg(windows)]
pub mod minidump;

use std::fs;
use std::panic;
use std::path::{Path, PathBuf};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

/// Custom timestamp source using `chrono` instead of the `time` crate, so
/// this project stays buildable on older/locked-down Rust toolchains that
/// can't satisfy `time`'s newer edition requirements (see CONTRIBUTING.md).
struct ChronoLocalTime;

impl tracing_subscriber::fmt::time::FormatTime for ChronoLocalTime {
    fn format_time(&self, w: &mut tracing_subscriber::fmt::format::Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", chrono::Local::now().to_rfc3339())
    }
}

/// Must be kept alive for the lifetime of the program or the async log writer
/// thread will stop flushing to disk.
pub struct LoggerGuard {
    _file_guard: WorkerGuard,
}

/// Returns the `logs/` directory next to the executable (portable) if writable,
/// otherwise falls back to the OS-standard app-data directory.
pub fn logs_dir() -> PathBuf {
    let portable = Path::new("logs");
    if fs::create_dir_all(portable).is_ok() {
        return portable.to_path_buf();
    }
    if let Some(proj) = directories::ProjectDirs::from("dev", "HyperTerm", "HyperTerm") {
        let dir = proj.data_dir().join("logs");
        let _ = fs::create_dir_all(&dir);
        return dir;
    }
    portable.to_path_buf()
}

/// Initialize the global tracing subscriber:
///   - stdout (human readable, colored, for interactive/debug runs)
///   - `logs/hyperterm.YYYY-MM-DD.log` (daily rotation, full detail, file+line+thread)
/// Also installs a panic hook that writes `logs/panic.log`.
pub fn init() -> LoggerGuard {
    let dir = logs_dir();
    let file_appender = tracing_appender::rolling::daily(&dir, "hyperterm.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_env("HYPERTERM_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,hyperterm=trace"));

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_target(true)
        .with_file(true)
        .with_line_number(true)
        .with_timer(ChronoLocalTime);

    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .init();

    install_panic_hook(dir.clone());

    #[cfg(windows)]
    {
        // Registers the vectored exception handler for *native* crashes
        // (access violations, etc.) -- see `minidump` module docs for why
        // this is separate from the Rust panic path below.
        minidump::install_vectored_exception_handler(dir.join("crash.dmp"));
    }

    tracing::info!(target: "hyperterm::logger", "logger initialized, writing to {:?}", dir);

    LoggerGuard { _file_guard: guard }
}

/// Install a panic hook that:
///   1. Logs the panic through `tracing` (so it lands in hyperterm.log too, level=ERROR)
///   2. Writes a dedicated, human-readable `panic.log` with a best-effort backtrace
fn install_panic_hook(dir: PathBuf) {
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown location>".to_string());
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());

        tracing::error!(
            target: "hyperterm::panic",
            location = %location,
            "PANIC: {}",
            msg
        );

        let path = dir.join("panic.log");
        let content = format!(
            "==== HYPERTERM PANIC ====\ntime: {}\nlocation: {}\nmessage: {}\nthread: {:?}\nbacktrace:\n{}\n",
            chrono::Local::now().to_rfc3339(),
            location,
            msg,
            std::thread::current().name().unwrap_or("<unnamed>"),
            backtrace
        );
        // Append, never overwrite previous crash evidence.
        use std::io::Write;
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = f.write_all(content.as_bytes());
        }

        #[cfg(windows)]
        {
            let dump_path = dir.join("panic.dmp");
            let wrote = minidump::write_best_effort_dump(&dump_path);
            if wrote {
                eprintln!("hyperterm: wrote best-effort crash dump to {dump_path:?}");
            }
        }

        default_hook(info);
    }));
}
