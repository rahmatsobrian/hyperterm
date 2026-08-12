//! ANSI Parser
//!
//! Thin adapter between the byte stream coming off the SSH channel and
//! [`crate::terminal_core::TerminalCore`], implemented on top of the
//! battle-tested `vte` crate (the same VT parser core used by Alacritty),
//! so we get correct UTF-8, CSI/OSC/DCS handling for free instead of
//! hand-rolling a state machine.
//!
//! Supports (per spec): standard ANSI escapes, 16-color, 256-color,
//! true-color (24-bit) SGR, UTF-8 / Unicode / emoji (vte + Rust `char`
//! handles this natively), and box-drawing characters (they're just
//! ordinary Unicode code points to this layer).

use vte::{Params, Parser as VteParser, Perform};

use crate::terminal_core::TerminalCore;
use crate::virtual_buffer::{Color, VirtualBuffer};

pub struct AnsiParser {
    inner: VteParser,
}

impl Default for AnsiParser {
    fn default() -> Self {
        Self {
            inner: VteParser::new(),
        }
    }
}

impl AnsiParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of raw bytes received from the SSH channel into the
    /// parser, mutating `core`'s live grid and spilling scrolled-off lines
    /// into `vbuf`.
    pub fn feed(&mut self, bytes: &[u8], core: &mut TerminalCore, vbuf: &mut VirtualBuffer) {
        let mut performer = Performer { core, vbuf };
        for byte in bytes {
            self.inner.advance(&mut performer, *byte);
        }
    }
}

struct Performer<'a> {
    core: &'a mut TerminalCore,
    vbuf: &'a mut VirtualBuffer,
}

impl<'a> Perform for Performer<'a> {
    fn print(&mut self, c: char) {
        self.core.print(c, self.vbuf);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => self.core.line_feed(self.vbuf),
            b'\r' => self.core.carriage_return(),
            0x08 => self.core.backspace(), // BS
            b'\t' => self.core.tab(),
            0x07 => self.core.bell(), // BEL
            _ => {
                tracing::trace!(target: "hyperterm::ansi_parser", "unhandled C0 control 0x{byte:02x}");
            }
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {
        // DCS (Device Control String) start -- not needed for standard shell
        // usage (Sixel/DEC private DCS sequences are Phase 2).
    }

    fn put(&mut self, _byte: u8) {
        // DCS payload byte.
    }

    fn unhook(&mut self) {
        // DCS end.
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        // OSC 0/2 = set window/tab title.
        if let Some(&first) = params.first() {
            if first == b"0" || first == b"2" {
                if let Some(&title_bytes) = params.get(1) {
                    let title = String::from_utf8_lossy(title_bytes).to_string();
                    self.core.set_title(title);
                }
            }
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &Params,
        _intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        let p = params_to_vec(params);
        let n = |i: usize, default: usize| -> usize {
            p.get(i)
                .copied()
                .filter(|&v| v != 0)
                .unwrap_or(default as u16) as usize
        };

        match action {
            'A' => self.core.cursor_up(n(0, 1)),
            'B' => self.core.cursor_down(n(0, 1)),
            'C' => self.core.cursor_forward(n(0, 1)),
            'D' => self.core.cursor_back(n(0, 1)),
            'H' | 'f' => {
                let row = p.get(0).copied().unwrap_or(1).max(1) as usize;
                let col = p.get(1).copied().unwrap_or(1).max(1) as usize;
                self.core.cursor_to(row, col);
            }
            'J' => self
                .core
                .erase_in_display(p.get(0).copied().unwrap_or(0), self.vbuf),
            'K' => self.core.erase_in_line(p.get(0).copied().unwrap_or(0)),
            'm' => apply_sgr(self.core, &p),
            'r' => {
                let top = p.get(0).copied().unwrap_or(1).max(1) as usize;
                let bottom = p.get(1).copied().unwrap_or(self.core.rows as u16) as usize;
                self.core.set_scroll_region(top, bottom);
            }
            's' => self.core.save_cursor(),
            'u' => self.core.restore_cursor(),
            _ => {
                tracing::trace!(
                    target: "hyperterm::ansi_parser",
                    "unhandled CSI sequence: params={:?} action={}",
                    p, action
                );
            }
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {
        // Single-char ESC sequences (e.g. ESC 7/8 save/restore, ESC M reverse
        // index). Minimal set for Phase 1; extend as real-world shells need.
    }
}

fn params_to_vec(params: &Params) -> Vec<u16> {
    params.iter().flat_map(|p| p.iter().copied()).collect()
}

fn apply_sgr(core: &mut TerminalCore, p: &[u16]) {
    if p.is_empty() {
        core.reset_sgr();
        return;
    }
    let mut i = 0;
    while i < p.len() {
        match p[i] {
            0 => core.reset_sgr(),
            1 => core.set_bold(true),
            3 => core.set_italic(true),
            4 => core.set_underline(true),
            7 => core.set_reverse(true),
            9 => core.set_strikethrough(true),
            22 => core.set_bold(false),
            23 => core.set_italic(false),
            24 => core.set_underline(false),
            27 => core.set_reverse(false),
            29 => core.set_strikethrough(false),
            30..=37 => core.set_fg(Color::Indexed((p[i] - 30) as u8)),
            39 => core.set_fg(Color::Default),
            40..=47 => core.set_bg(Color::Indexed((p[i] - 40) as u8)),
            49 => core.set_bg(Color::Default),
            90..=97 => core.set_fg(Color::Indexed((p[i] - 90 + 8) as u8)),
            100..=107 => core.set_bg(Color::Indexed((p[i] - 100 + 8) as u8)),
            38 | 48 => {
                // Extended color: 38;5;N (256-color) or 38;2;R;G;B (truecolor)
                let is_fg = p[i] == 38;
                if let Some(&mode) = p.get(i + 1) {
                    match mode {
                        5 => {
                            if let Some(&idx) = p.get(i + 2) {
                                let c = Color::Indexed(idx as u8);
                                if is_fg {
                                    core.set_fg(c);
                                } else {
                                    core.set_bg(c);
                                }
                            }
                            i += 2;
                        }
                        2 => {
                            if let (Some(&r), Some(&g), Some(&b)) =
                                (p.get(i + 2), p.get(i + 3), p.get(i + 4))
                            {
                                let c = Color::Rgb(r as u8, g as u8, b as u8);
                                if is_fg {
                                    core.set_fg(c);
                                } else {
                                    core.set_bg(c);
                                }
                            }
                            i += 4;
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
}
