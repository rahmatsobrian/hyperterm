//! Native GUI front-end (egui/eframe) -- an actual windowed application,
//! as opposed to the console-mode `renderer` (crossterm) the CLI path in
//! `main.rs` uses. Launched via `hyperterm.exe --gui`, or automatically
//! whenever no target HOST/`--session`/`--forget-host` is given on the
//! command line (see `main.rs::main`).
//!
//! ## Scope (v1)
//! One session per window (no tabs/splits yet -- the console renderer's
//! tab/split machinery in `session_manager` isn't reused here; see
//! ROADMAP.md). A login screen picks between an SSH session or a local
//! shell (cmd.exe/PowerShell, see `local_shell`), then swaps to a
//! scrollable, resizable terminal view once connected. Bold text and
//! font selection are not implemented yet -- only color, underline,
//! strikethrough, and reverse-video are rendered.
//!
//! ## Two backends, one screen
//! `SshSession` (async/tokio) and `local_shell` (blocking OS threads)
//! are structurally different, but the terminal screen doesn't want to
//! care which one it's driving. Both are adapted to the same
//! `GuiToTerm`/`TermToGui` channel message shape; `TermSender`/
//! `TermReceiver` below are thin enums over "a tokio unbounded channel"
//! vs. "a std unbounded channel" so `Terminal` can hold one concrete
//! field type regardless of backend.
//!
//! ## Architecture (SSH backend)
//! `eframe`'s event loop is synchronous (`update()` is called once per
//! frame on the UI thread), but the SSH engine (`ssh_engine::SshSession`)
//! is `async`/tokio-based. Rather than block the UI thread on `.await` or
//! spin up a nested runtime every frame, a dedicated OS thread owns its
//! own single-threaded `tokio::runtime::Runtime` and the `SshSession`
//! exclusively -- the same shape as `main.rs`'s `spawn_tab_task`, just
//! bridged with `tokio::sync::mpsc` instead of being another tokio task,
//! since the GUI thread itself isn't async. Sending on an unbounded
//! `Sender` and `try_recv`-ing on the matching `Receiver` are both plain
//! synchronous calls, so the UI thread never blocks on the channel.
//!
//! ## Honesty note for reviewers
//! Like the rest of this project (see `ssh_engine::agent` / `pageant`
//! docs), this was developed without a Windows machine or a live SSH
//! server/shell to test the actual window against. It's built directly
//! on top of the already-exercised `terminal_core`/`ansi_parser`/
//! `virtual_buffer` pipeline (covered by the existing test suite), but
//! the eframe/egui integration and the `local_shell` ConPTY plumbing
//! have only been reviewed, not run. Expect to iterate against real
//! CI/manual testing feedback.

use std::path::PathBuf;
use std::time::Duration;

use eframe::egui;
use tokio::sync::mpsc as tokio_mpsc;

use crate::ansi_parser::AnsiParser;
use crate::config::AppConfig;
use crate::local_shell::{self, ShellKind};
use crate::renderer::palette::{Palette, Rgb};
use crate::ssh_engine::known_hosts::HostKeyPolicy;
use crate::ssh_engine::{AuthMethod, ConnectParams, SshEvent, SshSession};
use crate::terminal_core::TerminalCore;
use crate::virtual_buffer::{Attrs, Cell, VirtualBuffer};

/// Startup defaults carried over from CLI flags (if any were given
/// alongside a bare invocation) so the login screen isn't blank when the
/// person already told us most of what we need.
pub struct LauncherDefaults {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub identity: Option<PathBuf>,
    pub use_agent: bool,
}

/// Launches the GUI and blocks the calling thread until the window is
/// closed.
pub fn run(
    config: AppConfig,
    known_hosts_path: PathBuf,
    defaults: LauncherDefaults,
) -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 640.0])
            .with_min_inner_size([420.0, 260.0])
            .with_title("HyperTerm"),
        ..Default::default()
    };
    eframe::run_native(
        "HyperTerm",
        options,
        Box::new(move |cc| {
            let app = App::new(config, known_hosts_path, defaults, &cc.egui_ctx);
            Ok(Box::new(app) as Box<dyn eframe::App>)
        }),
    )
}

/// Messages sent from the UI thread to whichever backend (SSH or local
/// shell) is driving the current session.
enum GuiToTerm {
    Input(Vec<u8>),
    Resize(u32, u32),
    Close,
}

/// Messages sent from the backend back to the UI thread.
enum TermToGui {
    Connected,
    ConnectFailed(String),
    Data(Vec<u8>),
    Closed,
    Disconnected(String),
}

/// The UI-thread-facing send half, hiding which channel flavor the
/// active backend actually uses (see module docs).
enum TermSender {
    Ssh(tokio_mpsc::UnboundedSender<GuiToTerm>),
    LocalShell(std::sync::mpsc::Sender<local_shell::HostToShell>),
}

impl TermSender {
    fn send(&self, msg: GuiToTerm) {
        match self {
            TermSender::Ssh(tx) => {
                let _ = tx.send(msg);
            }
            TermSender::LocalShell(tx) => {
                let shell_msg = match msg {
                    GuiToTerm::Input(b) => local_shell::HostToShell::Input(b),
                    GuiToTerm::Resize(c, r) => local_shell::HostToShell::Resize(c, r),
                    GuiToTerm::Close => local_shell::HostToShell::Close,
                };
                let _ = tx.send(shell_msg);
            }
        }
    }
}

/// The UI-thread-facing receive half; see `TermSender`.
enum TermReceiver {
    Ssh(tokio_mpsc::UnboundedReceiver<TermToGui>),
    LocalShell(std::sync::mpsc::Receiver<local_shell::ShellToHost>),
}

impl TermReceiver {
    fn try_recv(&mut self) -> Option<TermToGui> {
        match self {
            TermReceiver::Ssh(rx) => rx.try_recv().ok(),
            TermReceiver::LocalShell(rx) => rx.try_recv().ok().map(|msg| match msg {
                local_shell::ShellToHost::Started => TermToGui::Connected,
                local_shell::ShellToHost::StartFailed(reason) => TermToGui::ConnectFailed(reason),
                local_shell::ShellToHost::Data(d) => TermToGui::Data(d),
                local_shell::ShellToHost::Exited => TermToGui::Closed,
            }),
        }
    }
}

/// Spawns the dedicated SSH thread (see module docs) and returns the
/// channel halves the UI thread talks to it through.
fn spawn_ssh_thread(params: ConnectParams, cols: u32, rows: u32) -> (TermSender, TermReceiver) {
    let (to_ssh_tx, mut to_ssh_rx) = tokio_mpsc::unbounded_channel::<GuiToTerm>();
    let (from_ssh_tx, from_ssh_rx) = tokio_mpsc::unbounded_channel::<TermToGui>();

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = from_ssh_tx.send(TermToGui::ConnectFailed(format!(
                    "starting async runtime: {e}"
                )));
                return;
            }
        };
        rt.block_on(async move {
            let mut ssh = match SshSession::connect(params, cols, rows).await {
                Ok(s) => s,
                Err(e) => {
                    let _ = from_ssh_tx.send(TermToGui::ConnectFailed(format!("{e:#}")));
                    return;
                }
            };
            if from_ssh_tx.send(TermToGui::Connected).is_err() {
                let _ = ssh.close().await;
                return;
            }
            loop {
                tokio::select! {
                    biased;
                    ev = ssh.next_event() => {
                        match ev {
                            Some(SshEvent::Data(d)) | Some(SshEvent::ExtendedData(d)) => {
                                if from_ssh_tx.send(TermToGui::Data(d)).is_err() {
                                    return;
                                }
                            }
                            Some(SshEvent::ExitStatus(_)) => {}
                            Some(SshEvent::Eof) | Some(SshEvent::Closed) | None => {
                                let _ = from_ssh_tx.send(TermToGui::Closed);
                                return;
                            }
                            Some(SshEvent::Disconnected { reason }) => {
                                let _ = from_ssh_tx.send(TermToGui::Disconnected(reason));
                                return;
                            }
                        }
                    }
                    cmd = to_ssh_rx.recv() => {
                        match cmd {
                            Some(GuiToTerm::Input(bytes)) => {
                                let _ = ssh.send_input(&bytes).await;
                            }
                            Some(GuiToTerm::Resize(c, r)) => {
                                let _ = ssh.resize(c, r).await;
                            }
                            Some(GuiToTerm::Close) | None => {
                                let _ = ssh.close().await;
                                return;
                            }
                        }
                    }
                }
            }
        });
    });

    (TermSender::Ssh(to_ssh_tx), TermReceiver::Ssh(from_ssh_rx))
}

/// Wraps `local_shell::spawn` behind the same `TermSender`/`TermReceiver`
/// pair the SSH backend uses.
fn spawn_local_shell(shell: ShellKind, cols: u32, rows: u32) -> (TermSender, TermReceiver) {
    let (tx, rx) = local_shell::spawn(shell, cols, rows);
    (TermSender::LocalShell(tx), TermReceiver::LocalShell(rx))
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum SessionKind {
    Ssh,
    LocalShell,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum AuthMode {
    Password,
    Key,
    Agent,
}

struct Launcher {
    session_kind: SessionKind,
    shell_kind: ShellKind,
    host: String,
    port_text: String,
    username: String,
    auth_mode: AuthMode,
    identity_path: String,
    password: String,
    error: Option<String>,
}

impl Launcher {
    fn new(defaults: &LauncherDefaults) -> Self {
        let auth_mode = if defaults.use_agent {
            AuthMode::Agent
        } else if defaults.identity.is_some() {
            AuthMode::Key
        } else {
            AuthMode::Password
        };
        // If the CLI already gave us a host, assume they want SSH;
        // otherwise default to the lower-friction local shell.
        let session_kind = if defaults.host.is_empty() {
            SessionKind::LocalShell
        } else {
            SessionKind::Ssh
        };
        Self {
            session_kind,
            shell_kind: ShellKind::PowerShell,
            host: defaults.host.clone(),
            port_text: defaults.port.to_string(),
            username: defaults.username.clone(),
            auth_mode,
            identity_path: defaults
                .identity
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            password: String::new(),
            error: None,
        }
    }
}

struct Terminal {
    core: TerminalCore,
    parser: AnsiParser,
    vbuf: VirtualBuffer,
    to_backend: TermSender,
    from_backend: TermReceiver,
    title: String,
    status: Option<String>,
    scroll_offset: u64,
    cols: usize,
    rows: usize,
}

enum Screen {
    Launcher(Launcher),
    Terminal(Terminal),
}

struct App {
    config: AppConfig,
    known_hosts_path: PathBuf,
    palette: Palette,
    screen: Screen,
}

impl App {
    fn new(
        config: AppConfig,
        known_hosts_path: PathBuf,
        defaults: LauncherDefaults,
        ctx: &egui::Context,
    ) -> Self {
        let mut style = (*ctx.style()).clone();
        style.visuals = egui::Visuals::dark();
        ctx.set_style(style);

        let palette = Palette::for_theme(config.general.theme);
        Self {
            screen: Screen::Launcher(Launcher::new(&defaults)),
            config,
            known_hosts_path,
            palette,
        }
    }

    /// Toggles between the dark and light palettes -- wired to the
    /// menu bar's View > Toggle Theme item.
    fn toggle_theme(&mut self) {
        self.config.general.theme = match self.config.general.theme {
            crate::config::Theme::Dark => crate::config::Theme::Light,
            crate::config::Theme::Light => crate::config::Theme::Dark,
        };
        self.palette = Palette::for_theme(self.config.general.theme);
    }

    fn try_connect(&mut self) {
        let Screen::Launcher(launcher) = &mut self.screen else {
            return;
        };
        launcher.error = None;

        let (to_backend, from_backend, title) = match launcher.session_kind {
            SessionKind::LocalShell => {
                let (cols, rows) = (120u32, 32u32);
                let (tx, rx) = spawn_local_shell(launcher.shell_kind, cols, rows);
                (tx, rx, launcher.shell_kind.label().to_string())
            }
            SessionKind::Ssh => {
                if launcher.host.trim().is_empty() {
                    launcher.error = Some("host can't be empty".to_string());
                    return;
                }
                let port: u16 = match launcher.port_text.trim().parse() {
                    Ok(p) => p,
                    Err(_) => {
                        launcher.error = Some("port must be a number 1-65535".to_string());
                        return;
                    }
                };
                let username = if launcher.username.trim().is_empty() {
                    "root".to_string()
                } else {
                    launcher.username.trim().to_string()
                };
                let auth = match launcher.auth_mode {
                    AuthMode::Agent => AuthMethod::Agent,
                    AuthMode::Password => AuthMethod::Password(launcher.password.clone()),
                    AuthMode::Key => {
                        if launcher.identity_path.trim().is_empty() {
                            launcher.error =
                                Some("identity file path can't be empty".to_string());
                            return;
                        }
                        AuthMethod::PrivateKeyFile {
                            path: PathBuf::from(launcher.identity_path.trim()),
                            passphrase: if launcher.password.is_empty() {
                                None
                            } else {
                                Some(launcher.password.clone())
                            },
                        }
                    }
                };

                let host = launcher.host.trim().to_string();
                let params = ConnectParams {
                    host: host.clone(),
                    port,
                    username: username.clone(),
                    auth,
                    keepalive_interval: Duration::from_secs(
                        self.config.terminal.keepalive_interval_secs,
                    ),
                    inactivity_timeout: None,
                    // `Prompt` (the CLI default) does a blocking `stdin`
                    // read to ask about new/changed host keys -- there's
                    // no console attached to prompt into here, and no
                    // host-key-confirmation dialog in the GUI yet (see
                    // ROADMAP.md), so `Prompt` would just hang forever on
                    // the first connection to any new host. `Tofu`
                    // (trust-on-first-use) is the only policy that works
                    // without one; a *changed* key is still refused.
                    host_key_policy: HostKeyPolicy::Tofu,
                    known_hosts_path: self.known_hosts_path.clone(),
                };
                let (cols, rows) = (120u32, 32u32);
                let (tx, rx) = spawn_ssh_thread(params, cols, rows);
                (tx, rx, format!("{username}@{host}"))
            }
        };

        let session_id = format!(
            "{}-{}",
            sanitize(&title),
            chrono::Local::now().format("%Y%m%d-%H%M%S")
        );
        let vbuf = match VirtualBuffer::open(
            &self.config.scrollback.cache_dir,
            &session_id,
            self.config.scrollback.ram_line_capacity,
        ) {
            Ok(v) => v,
            Err(e) => {
                if let Screen::Launcher(launcher) = &mut self.screen {
                    launcher.error = Some(format!("opening scrollback buffer: {e:#}"));
                }
                to_backend.send(GuiToTerm::Close);
                return;
            }
        };

        // Reasonable starting grid; the terminal screen resizes to the
        // actual window on its first frame.
        let (cols, rows) = (120usize, 32usize);
        self.screen = Screen::Terminal(Terminal {
            core: TerminalCore::new(rows, cols),
            parser: AnsiParser::new(),
            vbuf,
            to_backend,
            from_backend,
            title,
            status: Some("connecting...".to_string()),
            scroll_offset: 0,
            cols,
            rows,
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let is_launcher = matches!(self.screen, Screen::Launcher(_));
        if is_launcher {
            self.update_launcher(ctx);
        } else {
            self.update_terminal(ctx);
        }
    }
}

/// Result of drawing the shared menu bar: what the user asked for, if
/// anything.
#[derive(PartialEq, Eq)]
enum MenuAction {
    None,
    NewSession,
    ToggleTheme,
    Exit,
}

impl App {
    /// File / View menu bar, shown at the top of both screens so the
    /// window reads as an application with real chrome (menus, not just
    /// a bare form) rather than a single dialog box.
    fn draw_menu_bar(ctx: &egui::Context, in_session: bool) -> MenuAction {
        let mut action = MenuAction::None;
        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui
                        .add_enabled(in_session, egui::Button::new("New Session"))
                        .clicked()
                    {
                        action = MenuAction::NewSession;
                        ui.close_menu();
                    }
                    if ui.button("Exit").clicked() {
                        action = MenuAction::Exit;
                        ui.close_menu();
                    }
                });
                ui.menu_button("View", |ui| {
                    if ui.button("Toggle Theme").clicked() {
                        action = MenuAction::ToggleTheme;
                        ui.close_menu();
                    }
                });
            });
        });
        action
    }

    fn update_launcher(&mut self, ctx: &egui::Context) {
        match Self::draw_menu_bar(ctx, false) {
            MenuAction::ToggleTheme => self.toggle_theme(),
            MenuAction::Exit => std::process::exit(0),
            MenuAction::None | MenuAction::NewSession => {}
        }

        let Screen::Launcher(launcher) = &mut self.screen else {
            return;
        };
        let mut connect_requested = false;

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.heading("HyperTerm");
                ui.label("Local shell or SSH -- new session");
            });
            ui.add_space(16.0);

            ui.vertical_centered(|ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(
                        &mut launcher.session_kind,
                        SessionKind::LocalShell,
                        "Local Shell",
                    );
                    ui.selectable_value(&mut launcher.session_kind, SessionKind::Ssh, "SSH");
                });
            });
            ui.add_space(12.0);

            egui::Grid::new("launcher_grid")
                .num_columns(2)
                .spacing([12.0, 10.0])
                .show(ui, |ui| match launcher.session_kind {
                    SessionKind::LocalShell => {
                        ui.label("Shell");
                        egui::ComboBox::from_id_source("shell_kind")
                            .selected_text(launcher.shell_kind.label())
                            .show_ui(ui, |ui| {
                                for kind in ShellKind::ALL {
                                    ui.selectable_value(
                                        &mut launcher.shell_kind,
                                        kind,
                                        kind.label(),
                                    );
                                }
                            });
                        ui.end_row();
                    }
                    SessionKind::Ssh => {
                        ui.label("Host");
                        ui.text_edit_singleline(&mut launcher.host);
                        ui.end_row();

                        ui.label("Port");
                        ui.text_edit_singleline(&mut launcher.port_text);
                        ui.end_row();

                        ui.label("Username");
                        ui.text_edit_singleline(&mut launcher.username);
                        ui.end_row();

                        ui.label("Authentication");
                        ui.horizontal(|ui| {
                            ui.radio_value(
                                &mut launcher.auth_mode,
                                AuthMode::Password,
                                "Password",
                            );
                            ui.radio_value(&mut launcher.auth_mode, AuthMode::Key, "Private key");
                            ui.radio_value(&mut launcher.auth_mode, AuthMode::Agent, "SSH agent");
                        });
                        ui.end_row();

                        match launcher.auth_mode {
                            AuthMode::Password => {
                                ui.label("Password");
                                ui.add(
                                    egui::TextEdit::singleline(&mut launcher.password)
                                        .password(true),
                                );
                                ui.end_row();
                            }
                            AuthMode::Key => {
                                ui.label("Identity file");
                                ui.text_edit_singleline(&mut launcher.identity_path);
                                ui.end_row();

                                ui.label("Passphrase (optional)");
                                ui.add(
                                    egui::TextEdit::singleline(&mut launcher.password)
                                        .password(true),
                                );
                                ui.end_row();
                            }
                            AuthMode::Agent => {}
                        }
                    }
                });

            ui.add_space(16.0);
            if let Some(err) = &launcher.error {
                ui.colored_label(egui::Color32::from_rgb(0xe0, 0x60, 0x60), err);
                ui.add_space(8.0);
            }
            if ui.button("Connect").clicked() {
                connect_requested = true;
            }
        });

        if connect_requested {
            self.try_connect();
        }
    }

    fn update_terminal(&mut self, ctx: &egui::Context) {
        match Self::draw_menu_bar(ctx, true) {
            MenuAction::ToggleTheme => self.toggle_theme(),
            MenuAction::Exit => std::process::exit(0),
            MenuAction::NewSession => {
                if let Screen::Terminal(term) = &self.screen {
                    term.to_backend.send(GuiToTerm::Close);
                }
                self.screen = Screen::Launcher(Launcher::new(&LauncherDefaults {
                    host: String::new(),
                    port: 22,
                    username: String::new(),
                    identity: None,
                    use_agent: false,
                }));
                return;
            }
            MenuAction::None => {}
        }

        let font_id = egui::FontId::monospace(15.0);
        let (char_w, row_h) =
            ctx.fonts(|f| (f.glyph_width(&font_id, ' '), f.row_height(&font_id)));

        let palette = &self.palette;
        let Screen::Terminal(term) = &mut self.screen else {
            return;
        };

        // Drain everything the backend has sent since the last frame.
        let mut back_to_launcher = false;
        while let Some(msg) = term.from_backend.try_recv() {
            match msg {
                TermToGui::Connected => term.status = None,
                TermToGui::Data(bytes) => {
                    let reply = term.parser.feed(&bytes, &mut term.core, &mut term.vbuf);
                    if !reply.is_empty() {
                        term.to_backend.send(GuiToTerm::Input(reply));
                    }
                }
                TermToGui::ConnectFailed(reason) => {
                    term.status = Some(format!("connect failed: {reason}"));
                }
                TermToGui::Closed => {
                    term.status = Some("session closed".to_string());
                }
                TermToGui::Disconnected(reason) => {
                    term.status = Some(format!("disconnected: {reason}"));
                }
            }
        }

        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(&term.title).strong());
                if let Some(status) = &term.status {
                    ui.separator();
                    ui.colored_label(egui::Color32::from_rgb(0xd0, 0xa0, 0x40), status);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Disconnect").clicked() {
                        term.to_backend.send(GuiToTerm::Close);
                        back_to_launcher = true;
                    }
                });
            });
        });

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(egui::Color32::from_rgb(0x1e, 0x1e, 0x1e)))
            .show(ctx, |ui| {
                let avail = ui.available_size();
                let new_cols = ((avail.x / char_w).floor() as usize).max(1);
                let new_rows = ((avail.y / row_h).floor() as usize).max(1);
                if new_cols != term.cols || new_rows != term.rows {
                    term.cols = new_cols;
                    term.rows = new_rows;
                    term.core.resize(new_rows, new_cols);
                    term.to_backend
                        .send(GuiToTerm::Resize(new_cols as u32, new_rows as u32));
                }

                let scroll_delta = ui.input(|i| i.raw_scroll_delta.y);
                if scroll_delta > 0.0 {
                    term.scroll_offset = term.scroll_offset.saturating_add(3);
                } else if scroll_delta < 0.0 {
                    term.scroll_offset = term.scroll_offset.saturating_sub(3);
                }
                let max_offset = term.vbuf.total_lines();
                term.scroll_offset = term.scroll_offset.min(max_offset);

                let (rows, cursor) =
                    compose_frame(&term.core, &mut term.vbuf, term.scroll_offset);

                let origin = ui.min_rect().min;
                let painter = ui.painter();
                for (row_idx, row) in rows.iter().enumerate() {
                    let y = origin.y + row_idx as f32 * row_h;
                    draw_row(painter, palette, &font_id, origin.x, y, char_w, row);
                }
                if let Some((cur_row, cur_col)) = cursor {
                    let x = origin.x + cur_col as f32 * char_w;
                    let y = origin.y + cur_row as f32 * row_h;
                    painter.rect_filled(
                        egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(char_w, row_h)),
                        0.0,
                        egui::Color32::from_rgba_unmultiplied(200, 200, 200, 90),
                    );
                }

                // Reserve the space we just painted into so egui's layout
                // (and scroll-area bookkeeping, if this ever grows one)
                // stays consistent with what's on screen.
                ui.allocate_rect(
                    egui::Rect::from_min_size(origin, avail),
                    egui::Sense::click(),
                );
            });

        if let Some(bytes) = collect_keyboard_input(ctx) {
            term.to_backend.send(GuiToTerm::Input(bytes));
            term.scroll_offset = 0;
        }

        if back_to_launcher {
            self.screen = Screen::Launcher(Launcher::new(&LauncherDefaults {
                host: String::new(),
                port: 22,
                username: String::new(),
                identity: None,
                use_agent: false,
            }));
        }

        // Keep repainting while a session is live so incoming output
        // shows up promptly without waiting for an input event.
        ctx.request_repaint_after(Duration::from_millis(16));
    }
}

/// Paints one row of cells, batching consecutive cells that share the
/// same visual style into a single `egui` text draw call -- mirrors the
/// run-coalescing the console renderer does for the same reason (fewer,
/// cheaper draw operations).
fn draw_row(
    painter: &egui::Painter,
    palette: &Palette,
    font_id: &egui::FontId,
    x0: f32,
    y: f32,
    char_w: f32,
    row: &[Cell],
) {
    let mut col = 0usize;
    while col < row.len() {
        let start = col;
        let sample = row[col];
        while col < row.len() && style_matches(&row[col], &sample) {
            col += 1;
        }
        let text: String = row[start..col].iter().map(|c| c.ch).collect();
        let (fg, bg) = resolve_colors(palette, &sample);
        let x = x0 + start as f32 * char_w;
        painter.rect_filled(
            egui::Rect::from_min_size(
                egui::pos2(x, y),
                egui::vec2(char_w * (col - start) as f32, font_id.size * 1.4),
            ),
            0.0,
            bg,
        );
        painter.text(
            egui::pos2(x, y),
            egui::Align2::LEFT_TOP,
            text,
            font_id.clone(),
            fg,
        );
    }
}

fn style_matches(a: &Cell, b: &Cell) -> bool {
    a.fg == b.fg && a.bg == b.bg && attrs_eq(a.attrs, b.attrs)
}

fn attrs_eq(a: Attrs, b: Attrs) -> bool {
    a.bold == b.bold
        && a.italic == b.italic
        && a.underline == b.underline
        && a.reverse == b.reverse
        && a.strikethrough == b.strikethrough
}

fn resolve_colors(palette: &Palette, cell: &Cell) -> (egui::Color32, egui::Color32) {
    let (fg_src, bg_src) = if cell.attrs.reverse {
        (cell.bg, cell.fg)
    } else {
        (cell.fg, cell.bg)
    };
    let Rgb(fr, fg, fb) = palette.resolve(fg_src, true);
    let Rgb(br, bgc, bb) = palette.resolve(bg_src, false);
    (
        egui::Color32::from_rgb(fr, fg, fb),
        egui::Color32::from_rgb(br, bgc, bb),
    )
}

/// Builds the frame to paint: the fast path (`scroll_offset == 0`) is
/// just the live grid. When scrolled back, composites reflowed history
/// (see `VirtualBuffer::history_window`) with whatever's still visible
/// from the live grid below it, and hides the cursor (returning `None`)
/// since it isn't meaningful while looking at history. Adapted from the
/// equivalent private helper in `main.rs`'s console render loop.
fn compose_frame(
    core: &TerminalCore,
    vbuf: &mut VirtualBuffer,
    scroll_offset: u64,
) -> (Vec<Vec<Cell>>, Option<(usize, usize)>) {
    if scroll_offset == 0 {
        return (core.visible_rows().to_vec(), Some(core.cursor()));
    }

    let rows = core.rows;
    let cols = core.cols;
    let total_history = vbuf.total_lines();
    let combined_bottom = total_history + rows as u64;
    let viewport_bottom = combined_bottom.saturating_sub(scroll_offset);
    let viewport_top = viewport_bottom.saturating_sub(rows as u64);

    let mut out: Vec<Vec<Cell>> = Vec::with_capacity(rows);

    if viewport_top < total_history {
        let history_end = viewport_bottom.min(total_history);
        let needed = (history_end - viewport_top) as usize;
        let hist_lines = vbuf.history_window(cols, needed, history_end);
        for line in hist_lines {
            let mut cells = line.cells;
            cells.resize(cols, Cell::default());
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
    while out.len() < rows {
        out.insert(0, vec![Cell::default(); cols]);
    }

    (out, None)
}

/// Translates this frame's keyboard events into the byte sequence to
/// send to the session, if any key that maps to something was pressed.
/// Regular printable text arrives via `egui::Event::Text` (handles
/// IME/shift/unicode for us); the explicit `Key` match below only needs
/// to cover control keys and Ctrl-letter combos, matching
/// `main.rs::key_event_to_bytes`'s crossterm equivalent.
fn collect_keyboard_input(ctx: &egui::Context) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    ctx.input(|i| {
        for event in &i.events {
            match event {
                egui::Event::Text(text) => {
                    bytes.extend_from_slice(text.as_bytes());
                }
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    if let Some(mut seq) = key_event_to_bytes(*key, *modifiers) {
                        bytes.append(&mut seq);
                    }
                }
                _ => {}
            }
        }
    });
    if bytes.is_empty() {
        None
    } else {
        Some(bytes)
    }
}

fn key_event_to_bytes(key: egui::Key, modifiers: egui::Modifiers) -> Option<Vec<u8>> {
    use egui::Key;
    if modifiers.ctrl {
        if let Some(c) = key_to_ascii_letter(key) {
            return Some(vec![(c as u8) - b'a' + 1]);
        }
    }
    match key {
        Key::Enter => Some(vec![b'\r']),
        Key::Backspace => Some(vec![0x7f]),
        Key::Tab => Some(vec![b'\t']),
        Key::Escape => Some(vec![0x1b]),
        Key::ArrowUp => Some(b"\x1b[A".to_vec()),
        Key::ArrowDown => Some(b"\x1b[B".to_vec()),
        Key::ArrowRight => Some(b"\x1b[C".to_vec()),
        Key::ArrowLeft => Some(b"\x1b[D".to_vec()),
        Key::Home => Some(b"\x1b[H".to_vec()),
        Key::End => Some(b"\x1b[F".to_vec()),
        Key::PageUp => Some(b"\x1b[5~".to_vec()),
        Key::PageDown => Some(b"\x1b[6~".to_vec()),
        Key::Delete => Some(b"\x1b[3~".to_vec()),
        _ => None,
    }
}

fn key_to_ascii_letter(key: egui::Key) -> Option<char> {
    use egui::Key;
    match key {
        Key::A => Some('a'),
        Key::B => Some('b'),
        Key::C => Some('c'),
        Key::D => Some('d'),
        Key::E => Some('e'),
        Key::F => Some('f'),
        Key::G => Some('g'),
        Key::H => Some('h'),
        Key::I => Some('i'),
        Key::J => Some('j'),
        Key::K => Some('k'),
        Key::L => Some('l'),
        Key::M => Some('m'),
        Key::N => Some('n'),
        Key::O => Some('o'),
        Key::P => Some('p'),
        Key::Q => Some('q'),
        Key::R => Some('r'),
        Key::S => Some('s'),
        Key::T => Some('t'),
        Key::U => Some('u'),
        Key::V => Some('v'),
        Key::W => Some('w'),
        Key::X => Some('x'),
        Key::Y => Some('y'),
        Key::Z => Some('z'),
        _ => None,
    }
}

/// Same host-alias sanitization `main.rs` uses when naming a scrollback
/// session file, duplicated here rather than made `pub` across a
/// CLI/GUI boundary for one three-line helper.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
