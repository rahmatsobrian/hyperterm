//! HyperTerm -- entry point.
//!
//! Wires together: CLI args -> Config Manager -> SSH Engine -> ANSI Parser
//! -> Terminal Core -> Virtual Buffer -> Renderer, driven by a single
//! `tokio::select!` loop so SSH I/O, keyboard input, and rendering never
//! block one another (the "no input lag" performance goal).
//!
//! Supports multiple concurrent SSH sessions as tabs (see
//! `hyperterm::session_manager` for the pure tab-switching/rendering logic
//! this file wires up). **Honest scope**: tabs are populated from the
//! targets given at startup (positional host + repeatable `--session`);
//! there's no in-app "open a new tab" connection dialog yet (that needs
//! text-input UI widgets the console renderer doesn't have -- see
//! ROADMAP.md). All tabs currently share one set of auth/host-key-policy
//! settings from the CLI, not per-tab credentials.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use futures::StreamExt;

use hyperterm::ansi_parser::AnsiParser;
use hyperterm::config;
use hyperterm::logger::{self, crash};
use hyperterm::renderer::CrosstermRenderer;
use hyperterm::session_manager::{self, SplitFocus, TabAction};
use hyperterm::ssh_engine::{AuthMethod, ConnectParams, HostKeyPolicy, SshEvent, SshSession};
use hyperterm::terminal_core::TerminalCore;
use hyperterm::virtual_buffer::VirtualBuffer;

/// HyperTerm: a high-performance, low-latency SSH terminal with unlimited
/// virtual scrollback.
#[derive(Parser, Debug)]
#[command(name = "hyperterm", version = hyperterm::VERSION, about)]
struct Cli {
    /// Target host, e.g. example.com or 192.168.1.10. Not required when
    /// using `--forget-host`. Opened as the first tab if given.
    host: Option<String>,

    /// Additional session to open as another tab, format
    /// `[user@]host[:port]` (repeatable: `--session a.example.com --session
    /// user@b.example.com:2222`). All tabs share the auth method and
    /// host-key policy given by the other flags below.
    #[arg(long = "session")]
    extra_sessions: Vec<String>,

    /// SSH port (applies to the positional HOST; use `host:port` syntax in
    /// `--session` for other tabs)
    #[arg(short = 'p', long, default_value_t = 22)]
    port: u16,

    /// Username (applies to the positional HOST and any `--session` entry
    /// that doesn't specify its own `user@`)
    #[arg(short = 'l', long)]
    username: Option<String>,

    /// Path to a private key file (ED25519 / RSA / OpenSSH format)
    #[arg(short = 'i', long)]
    identity: Option<PathBuf>,

    /// Prompt for a password instead of using a key
    #[arg(long)]
    password: bool,

    /// Authenticate via a running SSH agent (Unix: $SSH_AUTH_SOCK; Windows:
    /// the OpenSSH Authentication Agent named pipe) instead of a key file
    /// or password.
    #[arg(long)]
    agent: bool,

    /// Lines kept fully in RAM before spilling to the disk cache
    #[arg(long)]
    ram_capacity: Option<usize>,

    /// How to handle first-time / changed SSH host keys.
    /// `prompt` (default) asks interactively, like every other SSH client;
    /// `tofu` auto-trusts new keys without asking; `strict` refuses any
    /// host not already in known_hosts. A *changed* key is always refused
    /// regardless of this setting.
    #[arg(long, value_enum, default_value = "prompt")]
    host_key_policy: HostKeyPolicy,

    /// Override the known_hosts file location (default: HyperTerm's config
    /// directory).
    #[arg(long)]
    known_hosts: Option<PathBuf>,

    /// Remove a host's saved key from known_hosts (format: "host:port",
    /// e.g. "example.com:22") and exit without connecting. Use this after
    /// a legitimate host key change (e.g. the server was reinstalled).
    #[arg(long)]
    forget_host: Option<String>,

    /// Color theme (overrides config.toml). `dark` (default) or `light`.
    #[arg(long, value_enum)]
    theme: Option<CliTheme>,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum CliTheme {
    Dark,
    Light,
}

#[tokio::main]
async fn main() {
    let _guard = logger::init();
    let cli = Cli::parse();

    if let Err(err) = run(cli).await {
        tracing::error!(target: "hyperterm::main", "fatal error: {:#}", err);
        let _ = crash::write_crash_log("main::run", &err);
        eprintln!("HyperTerm exited with an error: {err:#}");
        eprintln!("Details were written to the logs/ directory (crash.log).");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    tracing::info!(target: "hyperterm::main", "HyperTerm v{} starting", hyperterm::VERSION);

    let known_hosts_path = cli
        .known_hosts
        .clone()
        .unwrap_or_else(hyperterm::ssh_engine::known_hosts::KnownHostsStore::default_path);

    if let Some(host_port) = &cli.forget_host {
        let mut store = hyperterm::ssh_engine::known_hosts::KnownHostsStore::load(&known_hosts_path)?;
        if store.forget(host_port)? {
            println!("Removed known_hosts entry for '{host_port}'.");
        } else {
            println!("No known_hosts entry found for '{host_port}' (nothing to remove).");
        }
        return Ok(());
    }

    let mut app_config = config::load_or_default().unwrap_or_else(|e| {
        tracing::warn!(target: "hyperterm::main", "failed to load config, using defaults: {e}");
        config::AppConfig::default()
    });
    if let Some(cap) = cli.ram_capacity {
        app_config.scrollback.ram_line_capacity = cap;
    }

    let default_username = match &cli.username {
        Some(u) => u.clone(),
        None => std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_else(|_| "root".into()),
    };

    let mut targets: Vec<Target> = Vec::new();
    if let Some(host) = &cli.host {
        targets.push(Target { host: host.clone(), port: cli.port, username: default_username.clone() });
    }
    for spec in &cli.extra_sessions {
        targets.push(parse_target(spec, &default_username, cli.port)?);
    }
    if targets.is_empty() {
        anyhow::bail!("a target HOST is required unless --forget-host is used (or pass --session)");
    }

    let auth = resolve_auth(&cli)?;
    let (cols, term_rows) = crossterm::terminal::size().unwrap_or((120, 32));
    // One row is reserved for the tab bar; content gets the rest. With a
    // single tab this still shows a (single-entry) tab bar -- consistent
    // behavior is simpler to reason about than a mode that appears/
    // disappears as tabs are opened and closed.
    let content_rows = term_rows.saturating_sub(1).max(1);

    let mut tabs: Vec<Tab> = Vec::with_capacity(targets.len());
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<(usize, SshEvent)>();
    for (index, target) in targets.iter().enumerate() {
        let connect_params = ConnectParams {
            host: target.host.clone(),
            port: target.port,
            username: target.username.clone(),
            auth: auth.clone(),
            keepalive_interval: Duration::from_secs(app_config.terminal.keepalive_interval_secs),
            inactivity_timeout: None,
            host_key_policy: cli.host_key_policy,
            known_hosts_path: known_hosts_path.clone(),
        };

        tracing::info!(
            target: "hyperterm::main",
            "connecting to {}@{}:{}", target.username, target.host, target.port
        );
        let ssh = SshSession::connect(connect_params, cols as u32, content_rows as u32)
            .await
            .with_context(|| format!("connecting to {}@{}:{}", target.username, target.host, target.port))?;
        tracing::info!(target: "hyperterm::main", "connected and shell requested for {}", target.host);

        let session_id = format!(
            "{}-{}-{}",
            sanitize(&target.host),
            target.username,
            chrono::Local::now().format("%Y%m%d-%H%M%S")
        );
        let vbuf = VirtualBuffer::open(
            &app_config.scrollback.cache_dir,
            &session_id,
            app_config.scrollback.ram_line_capacity,
        )
        .context("opening virtual scrollback buffer")?;

        let cmd_tx = spawn_tab_task(ssh, index, event_tx.clone());

        tabs.push(Tab {
            cmd_tx,
            core: TerminalCore::new(content_rows as usize, cols as usize),
            parser: AnsiParser::new(),
            vbuf,
            title: target.host.clone(),
            scroll_offset: 0,
        });
    }

    let mut renderer = CrosstermRenderer::new();
    let theme = match cli.theme {
        Some(CliTheme::Dark) => config::Theme::Dark,
        Some(CliTheme::Light) => config::Theme::Light,
        None => app_config.general.theme,
    };
    renderer.set_theme(theme);
    renderer.init().context("initializing renderer")?;

    // main loop's `event_rx` end keeps working as long as at least one
    // sender clone survives; new runtime-spawned tabs (Ctrl+Alt+T) get
    // their own clone via `new_tab_ctx.event_tx` below.
    let new_tab_ctx = NewTabContext {
        auth: auth.clone(),
        host_key_policy: cli.host_key_policy,
        known_hosts_path: known_hosts_path.clone(),
        keepalive_interval: Duration::from_secs(app_config.terminal.keepalive_interval_secs),
        cache_dir: app_config.scrollback.cache_dir.clone(),
        ram_capacity: app_config.scrollback.ram_line_capacity,
        default_username: default_username.clone(),
        event_tx: event_tx.clone(),
    };
    drop(event_tx);
    let result = main_loop(&mut tabs, event_rx, &mut renderer, new_tab_ctx).await;

    // Always try to leave the terminal in a sane state and flush every
    // tab's history to disk, even if the main loop returned an error.
    let _ = renderer.shutdown();
    for tab in &mut tabs {
        let _ = tab.cmd_tx.send(TabCommand::Close);
        if let Err(e) = tab.vbuf.checkpoint() {
            tracing::error!(target: "hyperterm::main", "failed to checkpoint scrollback for '{}': {e}", tab.title);
        }
        tracing::info!(
            target: "hyperterm::main",
            "tab '{}' ended; {} total lines of history persisted at {:?}",
            tab.title, tab.vbuf.total_lines(), tab.vbuf.cache_file_path()
        );
    }

    result
}

struct Target {
    host: String,
    port: u16,
    username: String,
}

/// Parses `[user@]host[:port]`.
fn parse_target(spec: &str, default_username: &str, default_port: u16) -> Result<Target> {
    let (user_part, host_part) = match spec.split_once('@') {
        Some((u, h)) => (Some(u.to_string()), h),
        None => (None, spec),
    };
    let (host, port) = match host_part.rsplit_once(':') {
        // Careful not to misparse a bare IPv6 address; this simple scheme
        // only supports `host:port`, not `[::1]:port` -- documented
        // limitation, IPv6 users can still connect via the positional HOST
        // argument with `-p`.
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
            (h.to_string(), p.parse().unwrap_or(default_port))
        }
        _ => (host_part.to_string(), default_port),
    };
    Ok(Target { host, port, username: user_part.unwrap_or_else(|| default_username.to_string()) })
}

/// Commands sent from the main loop to a tab's dedicated background task
/// (see `spawn_tab_task`). Using owned background tasks -- rather than
/// juggling `&mut SshSession` borrows for N sessions inside one
/// `tokio::select!` -- sidesteps a real borrow-checker dead end: futures
/// that capture `&mut tab.ssh` for `next_event()` would need to stay alive
/// for the whole `select!` block, which conflicts with every other arm
/// that also wants `&mut tabs`. Each task owning its `SshSession`
/// exclusively and talking over channels has no such conflict.
enum TabCommand {
    Input(Vec<u8>),
    Resize(u32, u32),
    Close,
}

/// Spawns a task that owns `ssh` exclusively: forwards every `SshEvent` to
/// `event_tx` tagged with `index`, and applies `TabCommand`s received on
/// the returned sender's receiving end. The task exits (dropping `ssh`)
/// after `TabCommand::Close` or once the SSH channel itself closes.
fn spawn_tab_task(
    mut ssh: SshSession,
    index: usize,
    event_tx: tokio::sync::mpsc::UnboundedSender<(usize, SshEvent)>,
) -> tokio::sync::mpsc::UnboundedSender<TabCommand> {
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<TabCommand>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                ev = ssh.next_event() => {
                    let is_terminal = matches!(ev, None | Some(SshEvent::Closed));
                    let event = ev.unwrap_or(SshEvent::Closed);
                    if event_tx.send((index, event)).is_err() {
                        // Main loop is gone (shutting down) -- nothing left to do.
                        return;
                    }
                    if is_terminal {
                        return;
                    }
                }
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(TabCommand::Input(bytes)) => {
                            if let Err(e) = ssh.send_input(&bytes).await {
                                tracing::error!(target: "hyperterm::main", "tab {index}: failed to send input: {e}");
                                let _ = event_tx.send((index, SshEvent::Disconnected { reason: e.to_string() }));
                                return;
                            }
                        }
                        Some(TabCommand::Resize(cols, rows)) => {
                            if let Err(e) = ssh.resize(cols, rows).await {
                                tracing::warn!(target: "hyperterm::main", "tab {index}: failed to resize: {e}");
                            }
                        }
                        Some(TabCommand::Close) | None => {
                            let _ = ssh.close().await;
                            return;
                        }
                    }
                }
            }
        }
    });
    cmd_tx
}

/// One tab's local state: its own live grid, ANSI parser, and scrollback
/// buffer, plus a channel to the background task that owns the actual
/// `SshSession` (see `spawn_tab_task`).
struct Tab {
    cmd_tx: tokio::sync::mpsc::UnboundedSender<TabCommand>,
    core: TerminalCore,
    parser: AnsiParser,
    vbuf: VirtualBuffer,
    title: String,
    scroll_offset: u64,
}

/// Everything needed to open another tab at runtime (Ctrl+Alt+T), reusing
/// the same auth/host-key-policy/scrollback settings resolved once at
/// startup -- see the module doc "Honest scope" note: all tabs share one
/// set of credentials, there's no per-tab auth picker.
#[derive(Clone)]
struct NewTabContext {
    auth: AuthMethod,
    host_key_policy: HostKeyPolicy,
    known_hosts_path: PathBuf,
    keepalive_interval: Duration,
    cache_dir: PathBuf,
    ram_capacity: usize,
    default_username: String,
    event_tx: tokio::sync::mpsc::UnboundedSender<(usize, SshEvent)>,
}

async fn main_loop(
    tabs: &mut Vec<Tab>,
    mut event_rx: tokio::sync::mpsc::UnboundedReceiver<(usize, SshEvent)>,
    renderer: &mut CrosstermRenderer,
    new_tab_ctx: NewTabContext,
) -> Result<()> {
    let mut input_stream = crossterm::event::EventStream::new();
    // ~60 FPS render tick; actual draw only happens if something changed
    // (Lazy Rendering), so this is not a busy-spin.
    let mut render_tick = tokio::time::interval(Duration::from_millis(16));
    let mut dirty = true;
    let mut active: usize = 0;
    let mut closed = vec![false; tabs.len()];
    // `Some((left_index, right_index, focus))` when a 2-pane vertical
    // split is active (Ctrl+Alt+S); `None` for the normal single-tab view.
    // See `session_manager::split_widths` for the pure width math and its
    // "honest scope" note (2-pane only, not full recursive layouts).
    let mut split: Option<(usize, usize, SplitFocus)> = None;

    // New-tab dialog (Ctrl+Alt+T): `Some(buffer)` while the input line is
    // being edited; connecting to the target it produced happens in a
    // background task (via `new_tab_tx`/`new_tab_rx`) so a slow DNS
    // lookup, TCP connect, or host-key prompt never freezes the rest of
    // the UI (other tabs keep receiving/rendering output the whole time).
    let mut dialog: Option<String> = None;
    let mut dialog_error: Option<String> = None;
    let mut connecting = false;
    let (new_tab_tx, mut new_tab_rx) =
        tokio::sync::mpsc::unbounded_channel::<std::result::Result<Tab, (String, String)>>();

    loop {
        if closed.iter().all(|&c| c) {
            tracing::info!(target: "hyperterm::main", "all tabs closed, exiting");
            return Ok(());
        }
        if closed[active] {
            active = next_open_tab(&closed, active, TabAction::Next);
        }

        tokio::select! {
            biased;

            // SSH data has top priority: this is the "responsive like VS
            // Code Terminal" path. Any tab's traffic marks that tab dirty;
            // only the active tab actually gets drawn, but background tabs
            // still need their grid/scrollback updated live (so switching
            // to them shows current output, not a stale snapshot).
            Some((i, ev)) = event_rx.recv() => {
                if i >= tabs.len() || closed[i] {
                    continue;
                }
                match ev {
                    SshEvent::Data(bytes) | SshEvent::ExtendedData(bytes) => {
                        let tab = &mut tabs[i];
                        tab.parser.feed(&bytes, &mut tab.core, &mut tab.vbuf);
                        if i == active {
                            dirty = true;
                        }
                    }
                    SshEvent::ExitStatus(code) => {
                        tracing::info!(target: "hyperterm::main", "tab '{}' remote command exited with status {code}", tabs[i].title);
                    }
                    SshEvent::Eof => {
                        tracing::info!(target: "hyperterm::main", "tab '{}' remote sent EOF", tabs[i].title);
                    }
                    SshEvent::Closed => {
                        tracing::info!(target: "hyperterm::main", "tab '{}' SSH channel closed", tabs[i].title);
                        closed[i] = true;
                        dirty = true;
                    }
                    SshEvent::Disconnected { reason } => {
                        tracing::warn!(target: "hyperterm::main", "tab '{}' disconnected: {reason}", tabs[i].title);
                        closed[i] = true;
                        dirty = true;
                    }
                }
            }

            // Result of a background "new tab" connection attempt
            // (Ctrl+Alt+T) -- see `dialog`/`connecting` above.
            Some(result) = new_tab_rx.recv() => {
                connecting = false;
                match result {
                    Ok(new_tab) => {
                        let idx = tabs.len();
                        tracing::info!(target: "hyperterm::main", "opened new tab '{}' (index {idx})", new_tab.title);
                        tabs.push(new_tab);
                        closed.push(false);
                        active = idx;
                    }
                    Err((title, err)) => {
                        tracing::error!(target: "hyperterm::main", "failed to open new tab '{title}': {err}");
                        dialog_error = Some(format!("connect to '{title}' failed: {err}"));
                    }
                }
                dirty = true;
            }

            // Keyboard / mouse / resize input.
            maybe_ev = input_stream.next() => {
                match maybe_ev {
                    Some(Ok(crossterm::event::Event::Key(key))) => {
                        use crossterm::event::{KeyCode, KeyModifiers};

                        // Dialog mode (Ctrl+Alt+T "new tab") captures every
                        // key until Enter/Esc, taking priority over all
                        // other bindings below.
                        if let Some(buffer) = dialog.as_mut() {
                            match key.code {
                                KeyCode::Enter => {
                                    let spec = buffer.clone();
                                    dialog = None;
                                    dialog_error = None;
                                    if spec.trim().is_empty() {
                                        continue;
                                    }
                                    match parse_target(&spec, &new_tab_ctx.default_username, 22) {
                                        Ok(target) => {
                                            connecting = true;
                                            dirty = true;
                                            let ctx = new_tab_ctx.clone();
                                            let (term_cols, term_rows) = renderer.size().unwrap_or((80, 24));
                                            let content_rows = (term_rows as usize).saturating_sub(1).max(1);
                                            let index = tabs.len();
                                            let tx = new_tab_tx.clone();
                                            tokio::spawn(async move {
                                                let result = open_new_tab(target, index, term_cols as usize, content_rows, ctx).await;
                                                let _ = tx.send(result);
                                            });
                                        }
                                        Err(e) => {
                                            dialog_error = Some(format!("invalid target: {e}"));
                                        }
                                    }
                                    continue;
                                }
                                KeyCode::Esc => {
                                    dialog = None;
                                    dialog_error = None;
                                    dirty = true;
                                    continue;
                                }
                                KeyCode::Backspace => {
                                    buffer.pop();
                                    dirty = true;
                                    continue;
                                }
                                KeyCode::Char(c) => {
                                    buffer.push(c);
                                    dirty = true;
                                    continue;
                                }
                                _ => continue,
                            }
                        }

                        if key.code == KeyCode::Char('t')
                            && key.modifiers.contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        {
                            if !connecting {
                                dialog = Some(String::new());
                                dialog_error = None;
                                dirty = true;
                            }
                            continue;
                        }

                        if key.code == KeyCode::Char('q')
                            && key.modifiers.contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        {
                            tracing::info!(target: "hyperterm::main", "user requested quit (Ctrl+Alt+Q)");
                            return Ok(());
                        }

                        // Tab management: Ctrl+PageUp/PageDown to switch
                        // (Ctrl+Tab is intercepted by too many terminal
                        // emulators/window managers to be reliable), Ctrl+
                        // Alt+<1-9> to jump directly, Ctrl+Alt+W to close
                        // the active tab.
                        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::PageUp {
                            if let Some((l, r, _)) = split.take() {
                                let (term_cols, term_rows) = renderer.size().unwrap_or((80, 24));
                                let content_rows = (term_rows as u32).saturating_sub(1).max(1);
                                resize_pane(tabs, l, term_cols as u32, content_rows);
                                resize_pane(tabs, r, term_cols as u32, content_rows);
                            }
                            active = next_open_tab(&closed, active, TabAction::Previous);
                            dirty = true;
                            continue;
                        }
                        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::PageDown {
                            if let Some((l, r, _)) = split.take() {
                                let (term_cols, term_rows) = renderer.size().unwrap_or((80, 24));
                                let content_rows = (term_rows as u32).saturating_sub(1).max(1);
                                resize_pane(tabs, l, term_cols as u32, content_rows);
                                resize_pane(tabs, r, term_cols as u32, content_rows);
                            }
                            active = next_open_tab(&closed, active, TabAction::Next);
                            dirty = true;
                            continue;
                        }
                        if key.modifiers.contains(KeyModifiers::CONTROL | KeyModifiers::ALT) {
                            // Exiting split before any tab-switch action
                            // keeps behavior predictable: switching tabs
                            // always means "back to a single full-width
                            // view", not "silently desync from the split
                            // pair".
                            let exit_split_then = |split: &mut Option<(usize, usize, SplitFocus)>, tabs: &mut [Tab], term_cols: u16, content_rows: usize| {
                                if let Some((l, r, _)) = split.take() {
                                    resize_pane(tabs, l, term_cols as u32, content_rows as u32);
                                    resize_pane(tabs, r, term_cols as u32, content_rows as u32);
                                }
                            };

                            if let KeyCode::Char(c @ '1'..='9') = key.code {
                                let idx = (c as u8 - b'1') as usize;
                                if idx < tabs.len() && !closed[idx] {
                                    let (term_cols, term_rows) = renderer.size().unwrap_or((80, 24));
                                    exit_split_then(&mut split, tabs, term_cols, (term_rows as usize).saturating_sub(1).max(1));
                                    active = idx;
                                    dirty = true;
                                }
                                continue;
                            }
                            if key.code == KeyCode::Char('w') {
                                tracing::info!(target: "hyperterm::main", "closing tab '{}' (Ctrl+Alt+W)", tabs[active].title);
                                let _ = tabs[active].cmd_tx.send(TabCommand::Close);
                                closed[active] = true;
                                if let Some((l, r, _)) = split {
                                    if l == active || r == active {
                                        let other = if l == active { r } else { l };
                                        let (term_cols, term_rows) = renderer.size().unwrap_or((80, 24));
                                        resize_pane(tabs, other, term_cols as u32, (term_rows as u32).saturating_sub(1).max(1));
                                        split = None;
                                    }
                                }
                                dirty = true;
                                continue;
                            }
                            if key.code == KeyCode::Char('s') {
                                let (term_cols, term_rows) = renderer.size().unwrap_or((80, 24));
                                let content_rows = (term_rows as usize).saturating_sub(1).max(1);
                                if split.is_some() {
                                    exit_split_then(&mut split, tabs, term_cols, content_rows);
                                    dirty = true;
                                    continue;
                                }
                                let partner = next_open_tab(&closed, active, TabAction::Next);
                                if partner == active {
                                    tracing::info!(target: "hyperterm::main", "only one open tab, nothing to split with");
                                    continue;
                                }
                                let (lw, rw) = session_manager::split_widths(term_cols as usize);
                                if rw == 0 {
                                    tracing::info!(target: "hyperterm::main", "terminal too narrow to split");
                                    continue;
                                }
                                resize_pane(tabs, active, lw as u32, content_rows as u32);
                                resize_pane(tabs, partner, rw as u32, content_rows as u32);
                                split = Some((active, partner, SplitFocus::Left));
                                dirty = true;
                                continue;
                            }
                            if key.code == KeyCode::Char('o') {
                                if let Some((l, r, focus)) = split.as_mut() {
                                    *focus = focus.toggled();
                                    active = if *focus == SplitFocus::Left { *l } else { *r };
                                    dirty = true;
                                }
                                continue;
                            }
                        }

                        let tab = &mut tabs[active];

                        // Shift+PageUp/PageDown control the local
                        // scrollback view (xterm convention: plain
                        // PageUp/PageDown are forwarded to the remote
                        // shell/app below, since e.g. `less` or `vim`
                        // want to handle those themselves).
                        if key.modifiers.contains(KeyModifiers::SHIFT) && key.code == KeyCode::PageUp {
                            let page = tab.core.rows.saturating_sub(2).max(1) as u64;
                            tab.scroll_offset = (tab.scroll_offset + page).min(tab.vbuf.total_lines());
                            dirty = true;
                            continue;
                        }
                        if key.modifiers.contains(KeyModifiers::SHIFT) && key.code == KeyCode::PageDown {
                            let page = tab.core.rows.saturating_sub(2).max(1) as u64;
                            tab.scroll_offset = tab.scroll_offset.saturating_sub(page);
                            dirty = true;
                            continue;
                        }

                        // Any other keypress is real input for the remote
                        // session -- snap back to the live view first, like
                        // every other terminal does, so the user doesn't
                        // type "blind" into a scrolled-away viewport.
                        if tab.scroll_offset != 0 {
                            tab.scroll_offset = 0;
                            dirty = true;
                        }

                        if let Some(bytes) = key_event_to_bytes(key) {
                            let _ = tab.cmd_tx.send(TabCommand::Input(bytes));
                        }
                    }
                    Some(Ok(crossterm::event::Event::Mouse(mouse))) => {
                        use crossterm::event::MouseEventKind;
                        // Mouse wheel always drives the local scrollback
                        // view (there's no mouse-reporting passthrough to
                        // the remote app yet -- see ROADMAP.md -- so
                        // alt-screen apps like `less` that want their own
                        // wheel handling won't get these events; a
                        // documented simplification, not an oversight).
                        let tab = &mut tabs[active];
                        match mouse.kind {
                            MouseEventKind::ScrollUp => {
                                tab.scroll_offset = (tab.scroll_offset + 3).min(tab.vbuf.total_lines());
                                dirty = true;
                            }
                            MouseEventKind::ScrollDown => {
                                tab.scroll_offset = tab.scroll_offset.saturating_sub(3);
                                dirty = true;
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(crossterm::event::Event::Resize(new_cols, new_rows))) => {
                        let content_rows = (new_rows as usize).saturating_sub(1).max(1);
                        if let Some((l, r, _)) = split {
                            let (lw, rw) = session_manager::split_widths(new_cols as usize);
                            for (i, tab) in tabs.iter_mut().enumerate() {
                                if closed[i] {
                                    continue;
                                }
                                let w = if i == l { lw } else if i == r { rw } else { new_cols as usize };
                                tab.core.resize(content_rows, w);
                                let _ = tab.cmd_tx.send(TabCommand::Resize(w as u32, content_rows as u32));
                            }
                        } else {
                            for (i, tab) in tabs.iter_mut().enumerate() {
                                if closed[i] {
                                    continue;
                                }
                                tab.core.resize(content_rows, new_cols as usize);
                                let _ = tab.cmd_tx.send(TabCommand::Resize(new_cols as u32, content_rows as u32));
                            }
                        }
                        dirty = true;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        tracing::error!(target: "hyperterm::main", "input stream error: {e}");
                    }
                    None => {
                        tracing::warn!(target: "hyperterm::main", "input stream ended unexpectedly");
                        return Ok(());
                    }
                }
            }

            _ = render_tick.tick() => {
                if dirty {
                    let (term_cols, _) = renderer.size().unwrap_or((80, 24));

                    let (content_rows, cursor) = if let Some((l, r, focus)) = split {
                        let (lw, rw) = session_manager::split_widths(term_cols as usize);
                        let (l_rows, l_cursor) = {
                            let t = &mut tabs[l];
                            render_frame(&t.core, &mut t.vbuf, t.scroll_offset)
                        };
                        let (r_rows, r_cursor) = {
                            let t = &mut tabs[r];
                            render_frame(&t.core, &mut t.vbuf, t.scroll_offset)
                        };
                        let border = hyperterm::virtual_buffer::Cell {
                            ch: '│',
                            fg: hyperterm::virtual_buffer::Color::Indexed(8),
                            bg: hyperterm::virtual_buffer::Color::Default,
                            attrs: hyperterm::virtual_buffer::Attrs::default(),
                        };
                        let composited: Vec<Vec<hyperterm::virtual_buffer::Cell>> = l_rows.iter().zip(r_rows.iter())
                            .map(|(lrow, rrow)| {
                                let mut row = Vec::with_capacity(lw + 1 + rw);
                                row.extend(lrow.iter().cloned());
                                row.push(border);
                                row.extend(rrow.iter().cloned());
                                row
                            })
                            .collect();
                        // Only the focused pane's cursor is shown, shifted
                        // right by `lw + 1` if the focus is the right pane.
                        let cursor = match focus {
                            SplitFocus::Left => l_cursor,
                            SplitFocus::Right => r_cursor.map(|(row, col)| (row, col + lw + 1)),
                        };
                        (composited, cursor)
                    } else {
                        let tab = &mut tabs[active];
                        render_frame(&tab.core, &mut tab.vbuf, tab.scroll_offset)
                    };

                    let titles: Vec<String> = tabs.iter().enumerate()
                        .filter(|(i, _)| !closed[*i])
                        .map(|(_, t)| t.title.clone())
                        .collect();
                    // Map `active` (an index into the full `tabs` slice) to
                    // an index into `titles` (which skips closed tabs).
                    let active_among_open = tabs.iter().enumerate()
                        .filter(|(i, _)| !closed[*i])
                        .position(|(i, _)| i == active)
                        .unwrap_or(0);

                    // Row 0 is normally the tab bar, but while the
                    // Ctrl+Alt+T "new tab" dialog is open (or right after
                    // it failed) it's replaced with the input line / error
                    // message instead -- there's nowhere else to put a
                    // single-line text prompt in this console-mode UI
                    // without stealing a content row.
                    let top_row = if let Some(buffer) = &dialog {
                        render_dialog_line(&format!("New session (user@host:port): {buffer}"), term_cols as usize, false)
                    } else if let Some(err) = &dialog_error {
                        render_dialog_line(err, term_cols as usize, true)
                    } else if connecting {
                        render_dialog_line("Connecting...", term_cols as usize, false)
                    } else {
                        session_manager::render_tab_bar(&titles, active_among_open, term_cols as usize)
                    };

                    let mut frame = Vec::with_capacity(content_rows.len() + 1);
                    frame.push(top_row);
                    frame.extend(content_rows);

                    // Cursor row indices from `render_frame` are relative
                    // to the content area, which starts at row 1 (row 0 is
                    // the tab bar) -- shift down by one to match.
                    let shifted_cursor = cursor.map(|(r, c)| (r + 1, c));

                    if let Err(e) = renderer.draw(&frame, shifted_cursor) {
                        tracing::error!(target: "hyperterm::main", "render error: {e}");
                    }
                    dirty = false;
                }
            }
        }
    }
}

/// Connects a brand-new tab in the background (see the Ctrl+Alt+T dialog
/// handling above) so a slow DNS lookup / TCP connect / host-key prompt
/// never blocks the rest of the UI. Returns `Err((title, message))` on
/// failure so the caller can show a plain-text error without needing
/// `anyhow::Error` to cross the channel.
async fn open_new_tab(
    target: Target,
    index: usize,
    cols: usize,
    content_rows: usize,
    ctx: NewTabContext,
) -> std::result::Result<Tab, (String, String)> {
    let title = target.host.clone();
    let run = async {
        let connect_params = ConnectParams {
            host: target.host.clone(),
            port: target.port,
            username: target.username.clone(),
            auth: ctx.auth,
            keepalive_interval: ctx.keepalive_interval,
            inactivity_timeout: None,
            host_key_policy: ctx.host_key_policy,
            known_hosts_path: ctx.known_hosts_path,
        };
        let ssh = SshSession::connect(connect_params, cols as u32, content_rows as u32)
            .await
            .with_context(|| format!("connecting to {}@{}:{}", target.username, target.host, target.port))?;

        let session_id = format!(
            "{}-{}-{}",
            sanitize(&target.host),
            target.username,
            chrono::Local::now().format("%Y%m%d-%H%M%S")
        );
        let vbuf = VirtualBuffer::open(&ctx.cache_dir, &session_id, ctx.ram_capacity)
            .context("opening virtual scrollback buffer")?;

        let cmd_tx = spawn_tab_task(ssh, index, ctx.event_tx);

        Ok::<Tab, anyhow::Error>(Tab {
            cmd_tx,
            core: TerminalCore::new(content_rows, cols),
            parser: AnsiParser::new(),
            vbuf,
            title: target.host.clone(),
            scroll_offset: 0,
        })
    };
    run.await.map_err(|e: anyhow::Error| (title, format!("{e:#}")))
}

fn resize_pane(tabs: &mut [Tab], index: usize, cols: u32, rows: u32) {
    let tab = &mut tabs[index];
    tab.core.resize(rows as usize, cols as usize);
    let _ = tab.cmd_tx.send(TabCommand::Resize(cols, rows));
}

fn next_open_tab(closed: &[bool], active: usize, action: TabAction) -> usize {
    let n = closed.len();
    if n == 0 || closed.iter().all(|&c| c) {
        return active;
    }
    let mut candidate = active;
    for _ in 0..n {
        candidate = session_manager::apply_action(candidate, n, action);
        if !closed[candidate] {
            return candidate;
        }
    }
    active
}

/// Builds the content-area frame to actually paint (excludes the tab bar
/// row, added by the caller): the fast path (`scroll_offset == 0`) is just
/// the live grid, untouched. When scrolled back, composites reflowed
/// history (see `VirtualBuffer::history_window`) with whatever's still
/// visible from the live grid below it, and hides the cursor (returning
/// `None`) since it isn't meaningful while looking at history.
/// Renders a single full-width status/input line (used for row 0 while the
/// new-tab dialog is open, showing an error, or connecting), styled as
/// plain text on a colored background -- red for errors, the same dark
/// grey as the tab bar otherwise, so it's visually distinct from ordinary
/// terminal content without needing a whole separate UI layer.
fn render_dialog_line(text: &str, width: usize, is_error: bool) -> Vec<hyperterm::virtual_buffer::Cell> {
    use hyperterm::virtual_buffer::{Attrs, Cell, Color};
    let bg = if is_error { Color::Indexed(1) } else { Color::Indexed(8) };
    let fg = Color::Indexed(15);
    let mut cells = vec![Cell { ch: ' ', fg, bg, attrs: Attrs::default() }; width];
    for (i, ch) in text.chars().enumerate() {
        if i >= width {
            break;
        }
        cells[i] = Cell { ch, fg, bg, attrs: Attrs::default() };
    }
    cells
}

fn render_frame(
    core: &TerminalCore,
    vbuf: &mut VirtualBuffer,
    scroll_offset: u64,
) -> (Vec<Vec<hyperterm::virtual_buffer::Cell>>, Option<(usize, usize)>) {
    if scroll_offset == 0 {
        return (core.visible_rows().to_vec(), Some(core.cursor()));
    }

    let rows = core.rows;
    let cols = core.cols;
    let total_history = vbuf.total_lines();
    let combined_bottom = total_history + rows as u64;
    let viewport_bottom = combined_bottom.saturating_sub(scroll_offset);
    let viewport_top = viewport_bottom.saturating_sub(rows as u64);

    let mut out: Vec<Vec<hyperterm::virtual_buffer::Cell>> = Vec::with_capacity(rows);

    if viewport_top < total_history {
        let history_end = viewport_bottom.min(total_history);
        let needed = (history_end - viewport_top) as usize;
        let hist_lines = vbuf.history_window(cols, needed, history_end);
        for line in hist_lines {
            let mut cells = line.cells;
            cells.resize(cols, hyperterm::virtual_buffer::Cell::default());
            out.push(cells);
        }
    }
    if viewport_bottom > total_history {
        let live_start = (total_history.max(viewport_top) - total_history) as usize;
        let live_end = (viewport_bottom - total_history) as usize;
        let live_rows = core.visible_rows();
        for row in &live_rows[live_start.min(live_rows.len())..live_end.min(live_rows.len())] {
            out.push(row.clone());
        }
    }
    // Pad to exactly `rows` if history was shorter than requested (near the
    // very start of the buffer) so the renderer always gets a consistent
    // frame size.
    while out.len() < rows {
        out.insert(0, vec![hyperterm::virtual_buffer::Cell::default(); cols]);
    }

    (out, None)
}

fn key_event_to_bytes(key: crossterm::event::KeyEvent) -> Option<Vec<u8>> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char(c) if ctrl && c.is_ascii_alphabetic() => {
            Some(vec![(c.to_ascii_uppercase() as u8) - b'A' + 1])
        }
        KeyCode::Char(c) => Some(c.to_string().into_bytes()),
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        _ => None,
    }
}

fn resolve_auth(cli: &Cli) -> Result<AuthMethod> {
    if cli.agent {
        return Ok(AuthMethod::Agent);
    }
    if let Some(path) = &cli.identity {
        return Ok(AuthMethod::PrivateKeyFile { path: path.clone(), passphrase: None });
    }
    if cli.password {
        let pw = rpassword_prompt("Password: ")?;
        return Ok(AuthMethod::Password(pw));
    }
    // Default: try the conventional ~/.ssh/id_ed25519, falling back to a
    // password prompt if it doesn't exist.
    if let Some(base) = directories::BaseDirs::new() {
        let default_key = base.home_dir().join(".ssh").join("id_ed25519");
        if default_key.exists() {
            return Ok(AuthMethod::PrivateKeyFile { path: default_key, passphrase: None });
        }
    }
    let pw = rpassword_prompt("Password: ")?;
    Ok(AuthMethod::Password(pw))
}

/// Minimal, dependency-free password prompt with echo disabled via
/// crossterm raw mode (avoids pulling in the separate `rpassword` crate
/// for one call site).
fn rpassword_prompt(prompt: &str) -> Result<String> {
    use crossterm::event::{Event, KeyCode};
    use std::io::Write;

    print!("{prompt}");
    std::io::stdout().flush().ok();
    crossterm::terminal::enable_raw_mode()?;
    let mut input = String::new();
    let result = loop {
        if let Event::Key(key) = crossterm::event::read()? {
            match key.code {
                KeyCode::Enter => break Ok(input.clone()),
                KeyCode::Backspace => { input.pop(); }
                KeyCode::Char(c) => input.push(c),
                KeyCode::Esc => break Err(anyhow::anyhow!("password entry cancelled")),
                _ => {}
            }
        }
    };
    crossterm::terminal::disable_raw_mode()?;
    println!();
    result
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '.' { c } else { '_' }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_target_bare_host() {
        let t = parse_target("example.com", "defaultuser", 22).unwrap();
        assert_eq!(t.host, "example.com");
        assert_eq!(t.port, 22);
        assert_eq!(t.username, "defaultuser");
    }

    #[test]
    fn parse_target_with_user_and_port() {
        let t = parse_target("alice@example.com:2222", "defaultuser", 22).unwrap();
        assert_eq!(t.host, "example.com");
        assert_eq!(t.port, 2222);
        assert_eq!(t.username, "alice");
    }

    #[test]
    fn parse_target_user_only() {
        let t = parse_target("bob@internal.local", "defaultuser", 22).unwrap();
        assert_eq!(t.host, "internal.local");
        assert_eq!(t.port, 22);
        assert_eq!(t.username, "bob");
    }

    #[test]
    fn parse_target_port_only() {
        let t = parse_target("example.com:2200", "defaultuser", 22).unwrap();
        assert_eq!(t.host, "example.com");
        assert_eq!(t.port, 2200);
        assert_eq!(t.username, "defaultuser");
    }

    #[test]
    fn parse_target_non_numeric_after_colon_is_not_treated_as_port() {
        // A colon followed by non-digits shouldn't be misparsed as a port
        // (e.g. this would matter for future IPv6-ish or scoped-name
        // inputs); falls back to treating the whole thing as the host.
        let t = parse_target("weird:host", "defaultuser", 22).unwrap();
        assert_eq!(t.host, "weird:host");
        assert_eq!(t.port, 22);
    }
}
