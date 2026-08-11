//! Native crash minidump capture (Windows only).
//!
//! Calls `MiniDumpWriteDump` (from `dbghelp.dll`, via the `windows-sys`
//! bindings) to produce a real `.dmp` file that can be opened in WinDbg or
//! Visual Studio, closing the "Export Diagnostic Report" gap noted in
//! `0.1.0-phase1`'s known limitations (which shipped a placeholder text
//! note instead of an actual dump).
//!
//! Two capture paths, matching the two ways this process can die:
//!
//! 1. **Native exceptions** (access violation, stack overflow, illegal
//!    instruction, etc.) -- these surface as real Win32 structured
//!    exceptions even in a Rust process. [`install_vectored_exception_handler`]
//!    registers a handler via `AddVectoredExceptionHandler` that writes a
//!    full dump *with* the faulting exception context/registers, then lets
//!    the exception continue propagating normally (`EXCEPTION_CONTINUE_SEARCH`)
//!    so this never suppresses or alters the actual crash behavior.
//! 2. **Rust panics** (`panic!`, unwrap on `None`, etc.) -- these are
//!    handled by Rust's own unwinder, not a Win32 SEH exception, so there's
//!    no exception context to attach. [`write_best_effort_dump`] is called
//!    from the panic hook (see `logger::mod`) and still produces a useful
//!    dump of the process's state *at the moment of the panic* (thread
//!    stacks, loaded modules, memory), just without exception-specific
//!    metadata.
//!
//! ## Honest disclosure
//! This module is compiled and linked only when targeting Windows
//! (`cfg(windows)`, and the `windows-sys` dependency itself is
//! `[target.'cfg(windows)'.dependencies]`-scoped so it isn't even fetched
//! on other platforms). It **cannot be exercised in the Linux sandbox this
//! project was developed in** -- there is no Windows loader, no
//! `dbghelp.dll`, nothing to call. The FFI signatures below are transcribed
//! carefully from the Win32 API documentation and the crate compiles
//! cleanly against `windows-sys`' bindings, but "compiles" is not the same
//! claim as "verified to produce a valid, loadable dump on real Windows" --
//! please treat this as needing a real-hardware smoke test before you rely
//! on it for a production crash report pipeline. See CONTRIBUTING.md.

#![cfg(windows)]

use std::path::Path;

use windows_sys::Win32::Foundation::{GetLastError, BOOL, GENERIC_WRITE, HANDLE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_ALWAYS,
};
use windows_sys::Win32::System::Diagnostics::Debug::{
    MiniDumpWriteDump, EXCEPTION_POINTERS, MINIDUMP_EXCEPTION_INFORMATION, MINIDUMP_TYPE,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId};

// MiniDumpNormal (0x0000) -- a compact dump (thread stacks, modules,
// exception context if any) rather than a full memory dump. Chosen because
// full-memory dumps of a terminal emulator holding potentially large
// scrollback buffers in RAM would produce enormous, slow-to-generate files;
// MiniDumpNormal is what's actually useful for "why did this crash",
// consistent with the "don't block the crash path" spirit of this project's
// performance goals.
const MINI_DUMP_NORMAL: MINIDUMP_TYPE = 0x0000_0000;

fn to_wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// Opens (creating if needed) the target `.dmp` file and calls
/// `MiniDumpWriteDump`. `exception_pointers` is `None` for the
/// best-effort/panic path and `Some` when called from the vectored
/// exception handler with real fault context.
fn write_dump(path: &Path, exception_pointers: Option<*mut EXCEPTION_POINTERS>) -> bool {
    let wide_path = to_wide(path);
    // SAFETY: `CreateFileW` is called with a valid, nul-terminated wide
    // string and standard flags; we check the returned handle for
    // INVALID_HANDLE_VALUE before use.
    let file_handle: HANDLE = unsafe {
        CreateFileW(
            wide_path.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            0,
        )
    };
    if file_handle == -1isize as HANDLE {
        // Can't use `tracing` safely from a crash/exception handler context
        // (allocating, taking locks) -- best-effort stderr only.
        eprintln!(
            "hyperterm: minidump: CreateFileW failed for {:?}, GetLastError={}",
            path,
            unsafe { GetLastError() }
        );
        return false;
    }

    let exception_info = MINIDUMP_EXCEPTION_INFORMATION {
        ThreadId: unsafe { GetCurrentThreadId() },
        ExceptionPointers: exception_pointers.unwrap_or(std::ptr::null_mut()),
        ClientPointers: 0,
    };
    let exception_param_ptr: *const MINIDUMP_EXCEPTION_INFORMATION = if exception_pointers.is_some() {
        &exception_info as *const MINIDUMP_EXCEPTION_INFORMATION
    } else {
        std::ptr::null()
    };

    // SAFETY: all pointers passed are either null or valid for the
    // duration of this call; `file_handle` was just successfully opened
    // above and is closed via `windows_sys`' `CloseHandle` afterward.
    let ok: BOOL = unsafe {
        MiniDumpWriteDump(
            GetCurrentProcess(),
            GetCurrentProcessId(),
            file_handle,
            MINI_DUMP_NORMAL,
            exception_param_ptr,
            std::ptr::null(),
            std::ptr::null(),
        )
    };

    unsafe {
        windows_sys::Win32::Foundation::CloseHandle(file_handle);
    }

    ok != 0
}

/// Best-effort dump with no exception context, called from the Rust panic
/// hook (`logger::install_panic_hook`) alongside `panic.log`.
pub fn write_best_effort_dump(path: &Path) -> bool {
    write_dump(path, None)
}

/// Registers a process-wide vectored exception handler that writes a full
/// minidump (with real exception context) to `logs/crash.dmp` the moment a
/// native structured exception occurs, then lets the exception continue
/// propagating as if this handler weren't here
/// (`EXCEPTION_CONTINUE_SEARCH`) -- this only observes and records, it
/// never changes crash behavior or attempts recovery.
///
/// Call once, early in `main()`, on Windows only.
pub fn install_vectored_exception_handler(dump_path: std::path::PathBuf) {
    use std::sync::OnceLock;
    use windows_sys::Win32::System::Diagnostics::Debug::AddVectoredExceptionHandler;

    static DUMP_PATH: OnceLock<std::path::PathBuf> = OnceLock::new();
    let _ = DUMP_PATH.set(dump_path);

    unsafe extern "system" fn handler(info: *mut EXCEPTION_POINTERS) -> i32 {
        if let Some(path) = DUMP_PATH.get() {
            write_dump(path, Some(info));
        }
        // EXCEPTION_CONTINUE_SEARCH: let normal (or default OS) crash
        // handling proceed exactly as if we weren't here.
        0
    }

    // SAFETY: `handler` matches the required `PVECTORED_EXCEPTION_HANDLER`
    // signature; `1` registers it to run first (call-first-added semantics
    // are best-effort per Win32 docs, fine for our "observe and dump"
    // use case).
    unsafe {
        AddVectoredExceptionHandler(1, Some(handler));
    }
}
