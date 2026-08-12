//! Pageant transport (Windows only).
//!
//! PuTTY's Pageant doesn't speak the standard OpenSSH agent named-pipe
//! protocol that `windows_pipe_agent` (the sibling module) talks to --
//! it uses its own IPC: a hidden window named `"Pageant"`, a
//! `PAGE_READWRITE` file mapping shared via `WM_COPYDATA`, one
//! request/response round trip per SSH-agent-protocol message. This is a
//! well-documented, stable protocol (used by every Pageant-compatible
//! tool -- WinSCP, KiTTY, WSL's `wsl-ssh-pageant`, etc.), transcribed here
//! from Win32 API references and verified field-by-field against the
//! `windows-sys` crate's actual generated bindings (see git history / PR
//! description for the verification pass), not from a live test against
//! a running Pageant instance.
//!
//! ## Design: bridge into the existing SSH-agent-protocol codec
//! Rather than hand-rolling the SSH agent *wire protocol* (identity list
//! / sign request encoding -- real, security-sensitive parsing code)
//! ourselves, this module only implements the *transport*: a
//! [`tokio::io::DuplexStream`] whose other end is fed by a small bridge
//! task that does one blocking Pageant round trip per length-prefixed
//! frame. The actual protocol encoding/decoding is still handled by
//! `russh_keys::agent::client::AgentClient`, the same well-exercised code
//! path the Unix and named-pipe transports use -- this module only needed
//! to get bytes to and from Pageant correctly, not to re-implement agent
//! protocol parsing.
//!
//! ## Honest disclosure
//! Same caveat as `logger::minidump` and `windows_pipe_agent`: this code
//! compiles only when targeting Windows and could not be exercised
//! against a real, running Pageant process in this project's Linux
//! sandbox. Please validate against real Pageant before depending on it.

#![cfg(windows)]

use std::ffi::CString;
use std::io;

use russh::keys::agent::client::AgentClient;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HWND, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
use windows_sys::Win32::System::Memory::{
    CreateFileMappingA, MapViewOfFile, UnmapViewOfFile, FILE_MAP_WRITE, PAGE_READWRITE,
};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowA, SendMessageA, WM_COPYDATA};

/// Magic value Pageant expects in `COPYDATASTRUCT::dwData` to recognize an
/// agent request (fixed by the protocol, not something we chose).
const AGENT_COPYDATA_ID: usize = 0x804e_50ba;

/// Connects to a running Pageant instance and returns an `AgentClient`
/// talking to it, matching the return type shape of the Unix/named-pipe
/// transports so `ssh_engine::agent::authenticate` can treat all three
/// uniformly.
pub async fn connect() -> io::Result<AgentClient<DuplexStream>> {
    if find_pageant_window().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no running Pageant window found (is Pageant running?)",
        ));
    }

    let (client_side, server_side) = tokio::io::duplex(16 * 1024);
    tokio::spawn(bridge_task(server_side));
    Ok(AgentClient::connect(client_side))
}

fn find_pageant_window() -> Option<HWND> {
    let class_name = CString::new("Pageant").ok()?;
    let window_name = CString::new("Pageant").ok()?;
    // SAFETY: both C strings are valid, nul-terminated, and outlive this call.
    let hwnd = unsafe {
        FindWindowA(
            class_name.as_ptr() as *const u8,
            window_name.as_ptr() as *const u8,
        )
    };
    if hwnd == 0 {
        None
    } else {
        Some(hwnd)
    }
}

/// Reads one length-prefixed SSH-agent-protocol frame at a time from
/// `server_side` (the end `AgentClient` writes requests into and reads
/// responses from), performs one blocking Pageant round trip per frame,
/// and writes the response back. Exits cleanly when the duplex stream
/// closes (i.e. when `AgentClient`/its owner is dropped).
async fn bridge_task(mut server_side: DuplexStream) {
    loop {
        let mut len_buf = [0u8; 4];
        if server_side.read_exact(&mut len_buf).await.is_err() {
            return; // stream closed -- normal shutdown path.
        }
        let payload_len = u32::from_be_bytes(len_buf) as usize;
        let mut payload = vec![0u8; payload_len];
        if server_side.read_exact(&mut payload).await.is_err() {
            return;
        }

        let mut request = Vec::with_capacity(4 + payload_len);
        request.extend_from_slice(&len_buf);
        request.extend_from_slice(&payload);

        let response = match tokio::task::spawn_blocking(move || pageant_round_trip(&request)).await
        {
            Ok(Ok(resp)) => resp,
            Ok(Err(e)) => {
                tracing::error!(target: "hyperterm::ssh_engine::pageant", "Pageant round trip failed: {e}");
                return;
            }
            Err(e) => {
                tracing::error!(target: "hyperterm::ssh_engine::pageant", "Pageant bridge task panicked: {e}");
                return;
            }
        };

        if server_side.write_all(&response).await.is_err() {
            return;
        }
    }
}

/// Performs exactly one Pageant request/response round trip. `request`
/// must already be a complete, length-prefixed SSH-agent-protocol
/// message. Blocking -- must be called from `spawn_blocking`, never
/// directly on an async task (this does a synchronous `SendMessageA` IPC
/// call, which can take an arbitrary amount of time).
fn pageant_round_trip(request: &[u8]) -> io::Result<Vec<u8>> {
    let hwnd = find_pageant_window()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Pageant window disappeared"))?;

    // Response buffer sizing: SSH agent protocol responses (identity
    // lists, signatures) are bounded in practice; 16 KiB comfortably
    // covers a handful of RSA/ED25519 identities and their signatures.
    // Real Pageant itself uses a similar-sized shared buffer.
    const MAPPING_SIZE: usize = 16 * 1024;

    let mapping_name = format!("PageantRequest{:08x}", unsafe { GetCurrentThreadId() });
    let mapping_name_c = CString::new(mapping_name.clone())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    // SAFETY: INVALID_HANDLE_VALUE requests a page-file-backed mapping
    // (not tied to a real file); all other args are valid per the Win32
    // reference for CreateFileMappingA.
    let mapping: HANDLE = unsafe {
        CreateFileMappingA(
            INVALID_HANDLE_VALUE,
            std::ptr::null(),
            PAGE_READWRITE,
            0,
            MAPPING_SIZE as u32,
            mapping_name_c.as_ptr() as *const u8,
        )
    };
    if mapping == 0 {
        return Err(io::Error::last_os_error());
    }
    let _mapping_guard = HandleGuard(mapping);

    // SAFETY: `mapping` was just created successfully above.
    let view = unsafe { MapViewOfFile(mapping, FILE_MAP_WRITE, 0, 0, MAPPING_SIZE) };
    if view.Value.is_null() {
        return Err(io::Error::last_os_error());
    }
    let _view_guard = ViewGuard(view.Value);

    if request.len() > MAPPING_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "agent request ({} bytes) exceeds Pageant mapping size",
                request.len()
            ),
        ));
    }

    // SAFETY: `view.Value` points to a writable region of at least
    // `MAPPING_SIZE` bytes we just mapped, and `request.len() <=
    // MAPPING_SIZE` was just checked above.
    unsafe {
        std::ptr::copy_nonoverlapping(request.as_ptr(), view.Value as *mut u8, request.len());
    }

    let mapping_name_bytes = mapping_name_c.as_bytes_with_nul();
    let cds = COPYDATASTRUCT {
        dwData: AGENT_COPYDATA_ID,
        cbData: mapping_name_bytes.len() as u32,
        lpData: mapping_name_bytes.as_ptr() as *mut _,
    };

    // SAFETY: `hwnd` was just confirmed to exist; `cds` is a valid,
    // stack-local `COPYDATASTRUCT` whose pointed-to data (`mapping_name_bytes`)
    // outlives this call.
    let result =
        unsafe { SendMessageA(hwnd, WM_COPYDATA, 0, &cds as *const COPYDATASTRUCT as isize) };
    if result == 0 {
        return Err(io::Error::other("Pageant returned failure for agent request"));
    }

    // Response is a length-prefixed frame written back into the same
    // shared mapping; read the 4-byte length first, then exactly that
    // many more bytes.
    // SAFETY: `view.Value` is still valid (guarded until this function
    // returns) and points to at least 4 readable bytes.
    let resp_len = unsafe {
        let len_ptr = view.Value as *const u8;
        u32::from_be_bytes([*len_ptr, *len_ptr.add(1), *len_ptr.add(2), *len_ptr.add(3)]) as usize
    };
    if resp_len > MAPPING_SIZE - 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Pageant reported an implausible response length ({resp_len} bytes)"),
        ));
    }

    let mut out = Vec::with_capacity(4 + resp_len);
    // SAFETY: bounds already checked (`4 + resp_len <= MAPPING_SIZE`).
    unsafe {
        let base = view.Value as *const u8;
        out.extend_from_slice(std::slice::from_raw_parts(base, 4 + resp_len));
    }
    Ok(out)
}

struct HandleGuard(HANDLE);
impl Drop for HandleGuard {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct ViewGuard(*mut core::ffi::c_void);
impl Drop for ViewGuard {
    fn drop(&mut self) {
        unsafe {
            UnmapViewOfFile(
                windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS { Value: self.0 },
            );
        }
    }
}
