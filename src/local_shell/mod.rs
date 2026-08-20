//! Local shell sessions (cmd.exe / PowerShell) via a pseudo-console, for
//! the GUI's "Local Shell" session type (see `gui` module docs) -- this
//! is what makes HyperTerm usable as an everyday terminal and not just
//! an SSH client.
//!
//! Uses the `portable-pty` crate (the same pty library wezterm itself is
//! built on) rather than hand-rolling the Win32 ConPTY / STARTUPINFOEX /
//! `PROC_THREAD_ATTRIBUTE_LIST` plumbing directly -- that FFI is
//! notoriously easy to get subtly wrong, and `portable-pty` already has
//! it exercised across a large user base.
//!
//! ## Shape
//! Deliberately mirrors `ssh_engine`'s event shape (`Data`/exit/error)
//! so `gui::spawn_local_shell_thread` can map it onto the same
//! `TermToGui`/`GuiToTerm` channel types the SSH backend uses, and the
//! terminal screen never needs to know which backend it's driving.
//! Unlike the SSH engine this has no `async`/tokio dependency at all --
//! ConPTY I/O is blocking, so this just uses two plain OS threads (one
//! blocking-reads output, the other owns the write/resize/kill side and
//! drains a command channel) instead of a tokio task.

use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// Which shell to launch. More could be added later (e.g. a detected
/// `wsl.exe`, `git-bash.exe`, `nu.exe`) -- kept to the two built into
/// every Windows install for now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    Cmd,
    PowerShell,
}

impl ShellKind {
    pub const ALL: [ShellKind; 2] = [ShellKind::Cmd, ShellKind::PowerShell];

    pub fn label(self) -> &'static str {
        match self {
            ShellKind::Cmd => "Command Prompt (cmd.exe)",
            ShellKind::PowerShell => "PowerShell",
        }
    }

    fn program(self) -> &'static str {
        match self {
            ShellKind::Cmd => "cmd.exe",
            ShellKind::PowerShell => "powershell.exe",
        }
    }
}

/// Events from the shell thread to whatever's driving it.
pub enum ShellToHost {
    Started,
    StartFailed(String),
    Data(Vec<u8>),
    Exited,
}

/// Commands the driver can send to a running shell session.
pub enum HostToShell {
    Input(Vec<u8>),
    Resize(u32, u32),
    Close,
}

/// Spawns `shell` inside a pseudo-console sized `cols` x `rows` and
/// returns the two channel halves to drive it. All the actual work
/// happens on background threads; this returns immediately (connection
/// failure is reported as a `ShellToHost::StartFailed` event, not an
/// `Err` here, so the caller doesn't need to special-case "failed to
/// start" vs. "started, then exited").
pub fn spawn(
    shell: ShellKind,
    cols: u32,
    rows: u32,
) -> (Sender<HostToShell>, Receiver<ShellToHost>) {
    let (cmd_tx, cmd_rx) = mpsc::channel::<HostToShell>();
    let (evt_tx, evt_rx) = mpsc::channel::<ShellToHost>();

    std::thread::spawn(move || {
        tracing::info!(
            target: "hyperterm::local_shell",
            ?shell, cols, rows, "spawning local shell"
        );

        let pty_system = native_pty_system();
        let pair = match pty_system.openpty(PtySize {
            rows: rows.min(u16::MAX as u32) as u16,
            cols: cols.min(u16::MAX as u32) as u16,
            pixel_width: 0,
            pixel_height: 0,
        }) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(target: "hyperterm::local_shell", "opening pty failed: {e:#}");
                let _ = evt_tx.send(ShellToHost::StartFailed(format!("opening pty: {e:#}")));
                return;
            }
        };

        // Explicit cwd + inherited env: when the exe is launched from a
        // volatile working directory (e.g. run straight out of an archive
        // viewer's temp-extraction folder instead of a properly extracted
        // copy), leaving `cwd` unset can make CommandBuilder inherit a
        // directory that's about to be cleaned up by the archiver. Pin it
        // to the exe's own directory (falling back to the OS default) so
        // the child shell always starts somewhere stable.
        let mut cmd = CommandBuilder::new(shell.program());
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                cmd.cwd(dir);
            }
        }

        let mut child = match pair.slave.spawn_command(cmd) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(
                    target: "hyperterm::local_shell",
                    "starting {} failed: {e:#}", shell.program()
                );
                let _ = evt_tx.send(ShellToHost::StartFailed(format!(
                    "starting {}: {e:#}",
                    shell.program()
                )));
                return;
            }
        };
        tracing::info!(target: "hyperterm::local_shell", "child process spawned");
        // Important for ConPTY specifically: drop our handle to the
        // slave side once the child owns it. Holding it open keeps an
        // extra reference to the console alive and the master-side
        // reader below never sees EOF when the child exits.
        drop(pair.slave);

        let mut reader = match pair.master.try_clone_reader() {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(target: "hyperterm::local_shell", "opening pty reader failed: {e:#}");
                let _ = evt_tx.send(ShellToHost::StartFailed(format!(
                    "opening pty reader: {e:#}"
                )));
                let _ = child.kill();
                return;
            }
        };
        let mut writer = match pair.master.take_writer() {
            Ok(w) => w,
            Err(e) => {
                tracing::error!(target: "hyperterm::local_shell", "opening pty writer failed: {e:#}");
                let _ = evt_tx.send(ShellToHost::StartFailed(format!(
                    "opening pty writer: {e:#}"
                )));
                let _ = child.kill();
                return;
            }
        };

        if evt_tx.send(ShellToHost::Started).is_err() {
            tracing::warn!(target: "hyperterm::local_shell", "GUI side gone before Started could be sent");
            let _ = child.kill();
            return;
        }

        let reader_evt_tx = evt_tx.clone();
        let reader_thread = std::thread::spawn(move || {
            tracing::debug!(target: "hyperterm::local_shell", "reader thread started");
            let mut buf = [0u8; 8192];
            let mut total: u64 = 0;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        tracing::info!(
                            target: "hyperterm::local_shell",
                            total_bytes = total,
                            "reader got EOF, exiting reader thread"
                        );
                        break;
                    }
                    Ok(n) => {
                        total += n as u64;
                        tracing::trace!(target: "hyperterm::local_shell", n, total, "read chunk from pty");
                        if reader_evt_tx.send(ShellToHost::Data(buf[..n].to_vec())).is_err() {
                            tracing::warn!(target: "hyperterm::local_shell", "GUI side gone, stopping reader thread");
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::error!(target: "hyperterm::local_shell", "pty read error: {e:#}, total_bytes={total}");
                        break;
                    }
                }
            }
        });

        // Poll with a timeout rather than a plain blocking `recv()` so
        // we notice the child exiting on its own (the user typing
        // `exit`) even if no command ever arrives from the GUI side.
        loop {
            match cmd_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(HostToShell::Input(bytes)) => {
                    if writer.write_all(&bytes).is_err() {
                        break;
                    }
                }
                Ok(HostToShell::Resize(c, r)) => {
                    let _ = pair.master.resize(PtySize {
                        rows: r.min(u16::MAX as u32) as u16,
                        cols: c.min(u16::MAX as u32) as u16,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
                Ok(HostToShell::Close) => {
                    let _ = child.kill();
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = child.kill();
                    break;
                }
            }
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
        }

        drop(writer);
        tracing::info!(target: "hyperterm::local_shell", "shell session ending, sending Exited");
        let _ = evt_tx.send(ShellToHost::Exited);
        let _ = reader_thread.join();
    });

    (cmd_tx, evt_rx)
}
