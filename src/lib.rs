//! HyperTerm library root.
//!
//! Modules mirror the architecture described in the project spec:
//! SSH Engine, Terminal Core, ANSI Parser, Renderer, Virtual Buffer,
//! Search Engine, Disk Cache, Logger, Config Manager. `main.rs` (the UI /
//! orchestration layer) wires them together.

pub mod ansi_parser;
pub mod config;
pub mod disk_cache;
pub mod gui;
#[cfg(windows)]
pub mod local_shell;
pub mod logger;
pub mod renderer;
pub mod search;
pub mod session_manager;
pub mod ssh_engine;
pub mod terminal_core;
pub mod virtual_buffer;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
