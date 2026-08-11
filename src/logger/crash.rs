//! Crash handling + "Export Diagnostic Report" feature.
//!
//! `write_crash_log` is called from `main.rs`'s top-level error boundary
//! (i.e. when `main()` returns an `Err`, which is a controlled failure,
//! as opposed to `panic.log` which is written by the panic hook for
//! uncontrolled failures).
//!
//! `export_diagnostic_zip` bundles:
//!   - logs/*.log  (hyperterm.log, panic.log, crash.log)
//!   - config (redacted: passwords / private key paths noted but not key contents)
//!   - Windows / OS info
//!   - application version
//!   - crash dump, if one exists at `logs/crash.dmp` (Phase 2: minidump via `dbghelp`)
//! into `diagnostic-report-<timestamp>.zip`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::logger::logs_dir;

pub fn write_crash_log(context: &str, err: &anyhow::Error) -> Result<PathBuf> {
    let dir = logs_dir();
    let path = dir.join("crash.log");
    let content = format!(
        "==== HYPERTERM CRASH ====\ntime: {}\ncontext: {}\nerror: {:#}\n\ncaused by chain:\n{}\n",
        chrono::Local::now().to_rfc3339(),
        context,
        err,
        err.chain()
            .enumerate()
            .map(|(i, e)| format!("  [{i}] {e}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let mut f = fs::OpenOptions::new().create(true).append(true).open(&path)?;
    f.write_all(content.as_bytes())?;
    tracing::error!(target: "hyperterm::crash", "wrote crash log to {:?}", path);
    Ok(path)
}

/// Collects environment info that's useful for bug reports without needing
/// the user to describe their machine manually.
pub fn system_info_report() -> String {
    use sysinfo::System;
    let mut sys = System::new_all();
    sys.refresh_all();

    format!(
        "app_version: {}\nos: {} {}\nkernel: {}\nhostname: {}\ncpu_count: {}\ntotal_mem_mb: {}\nused_mem_mb: {}\nbuild_target: {}\nrustc_channel: phase1-debug\n",
        env!("CARGO_PKG_VERSION"),
        System::name().unwrap_or_else(|| "unknown".into()),
        System::os_version().unwrap_or_else(|| "unknown".into()),
        System::kernel_version().unwrap_or_else(|| "unknown".into()),
        System::host_name().unwrap_or_else(|| "unknown".into()),
        sys.cpus().len(),
        sys.total_memory() / 1024 / 1024,
        sys.used_memory() / 1024 / 1024,
        std::env::consts::ARCH,
    )
}

/// Builds `diagnostic-report-<timestamp>.zip` next to the executable and
/// returns its path. This is what the UI's "Export Diagnostic Report" button
/// (Phase 2 UI) calls into.
pub fn export_diagnostic_zip(config_toml_redacted: &str) -> Result<PathBuf> {
    let dir = logs_dir();
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let out_path = PathBuf::from(format!("diagnostic-report-{ts}.zip"));
    let file = fs::File::create(&out_path)
        .with_context(|| format!("creating {out_path:?}"))?;
    let mut zip = zip::ZipWriter::new(file);
    let opts = zip::write::FileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    // logs/*
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().map(|e| e == "log").unwrap_or(false) {
                add_file_to_zip(&mut zip, &p, opts)?;
            }
        }
    }

    // redacted config snapshot
    zip.start_file("config.redacted.toml", opts)?;
    zip.write_all(config_toml_redacted.as_bytes())?;

    // system info
    zip.start_file("system_info.txt", opts)?;
    zip.write_all(system_info_report().as_bytes())?;

    // crash dump, if present (native minidump on Windows, see
    // `logger::minidump`; written either by the vectored exception handler
    // on a real crash or best-effort from the panic hook).
    let dump_candidates = [dir.join("crash.dmp"), dir.join("panic.dmp")];
    let mut any_dump = false;
    for dump_path in &dump_candidates {
        if dump_path.exists() {
            add_file_to_zip(&mut zip, dump_path, opts)?;
            any_dump = true;
        }
    }
    if !any_dump {
        zip.start_file("crash_dump.txt", opts)?;
        #[cfg(windows)]
        let note = "No minidump found. This means the process hasn't crashed/panicked since \
                     logs/ was last cleared -- minidump capture is active (see logger::minidump) \
                     but only writes a file when there's actually something to capture.\n";
        #[cfg(not(windows))]
        let note = "Native minidump capture (MiniDumpWriteDump) is Windows-only; this platform \
                     doesn't produce .dmp files. See ROADMAP.md.\n";
        zip.write_all(note.as_bytes())?;
    }

    zip.finish()?;
    tracing::info!(target: "hyperterm::crash", "diagnostic report written to {:?}", out_path);
    Ok(out_path)
}

fn add_file_to_zip(
    zip: &mut zip::ZipWriter<fs::File>,
    path: &Path,
    opts: zip::write::FileOptions,
) -> Result<()> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file.log");
    zip.start_file(name, opts)?;
    let data = fs::read(path)?;
    zip.write_all(&data)?;
    Ok(())
}
