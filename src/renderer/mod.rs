//! Renderer
//!
//! Phase 1 renderer: a `crossterm`-based console renderer. This targets the
//! "responsive like VS Code Terminal, works everywhere from Win7 to Win11"
//! requirement pragmatically -- `crossterm` talks to the legacy Windows
//! Console API on Win7/8 and Windows Terminal/VT on Win10+ transparently,
//! with no GPU dependency, so it Just Works on the whole target matrix.
//!
//! Implements the performance techniques the spec asks for at the level
//! that's meaningful for a console renderer:
//!   - **Dirty Region Rendering**: diffs the new frame against the last
//!     painted frame and only emits escape sequences for cells that
//!     actually changed.
//!   - **Incremental Rendering**: changed cells on a row are coalesced into
//!     contiguous runs sharing the same style, minimizing SGR escape
//!     sequence churn.
//!   - **Lazy Rendering**: `draw()` is only ever called when the terminal
//!     state actually changed (driven by the app's event loop, not a busy
//!     poll).
//!
//! Input handling (keyboard/mouse/resize events) lives directly in
//! `main.rs`'s `tokio::select!` loop via `crossterm::event::EventStream`,
//! not here -- this module is display-only.
//!
//! A GPU-accelerated DirectWrite/Direct2D renderer (with this same
//! dirty-region diffing feeding a glyph atlas instead of a console buffer)
//! is the planned Phase 2/3 replacement -- see ROADMAP.md. It is a
//! substantial, Windows-specific subsystem in its own right and is not
//! implemented here.

pub mod palette;

use std::io::{stdout, Stdout, Write};

use anyhow::Result;
use crossterm::style::{Attribute, Color as CtColor, Colors, Print, ResetColor, SetAttribute, SetColors};
use crossterm::terminal::{Clear, ClearType};
use crossterm::{cursor, execute, queue, terminal};

use crate::virtual_buffer::{Attrs, Cell, Color};
pub use palette::Palette;

pub struct CrosstermRenderer {
    out: Stdout,
    last_frame: Vec<Vec<Cell>>,
    last_cursor: Option<(usize, usize)>,
    initialized: bool,
    palette: Palette,
}

impl CrosstermRenderer {
    pub fn new() -> Self {
        Self {
            out: stdout(),
            last_frame: Vec::new(),
            last_cursor: None,
            initialized: false,
            palette: Palette::dark(),
        }
    }

    /// Switches the active color theme. Forces a full repaint on the next
    /// `draw()` call (by clearing `last_frame`) since every cell's
    /// resolved color may have changed even though the logical `Color`
    /// values in the grid haven't.
    pub fn set_theme(&mut self, theme: crate::config::Theme) {
        self.palette = Palette::for_theme(theme);
        self.last_frame.clear();
    }

    pub fn init(&mut self) -> Result<()> {
        terminal::enable_raw_mode()?;
        execute!(self.out, terminal::EnterAlternateScreen, cursor::Hide, crossterm::event::EnableMouseCapture)?;
        self.initialized = true;
        tracing::info!(target: "hyperterm::renderer", "crossterm renderer initialized");
        Ok(())
    }

    pub fn shutdown(&mut self) -> Result<()> {
        if self.initialized {
            execute!(
                self.out,
                crossterm::event::DisableMouseCapture,
                cursor::Show,
                terminal::LeaveAlternateScreen
            )?;
            terminal::disable_raw_mode()?;
            self.initialized = false;
            tracing::info!(target: "hyperterm::renderer", "crossterm renderer shut down");
        }
        Ok(())
    }

    pub fn size(&self) -> Result<(u16, u16)> {
        Ok(terminal::size()?)
    }

    /// Draws `rows` (either the live grid from `TerminalCore::visible_rows`,
    /// or a composite scrollback view built by `main.rs`), diffing against
    /// the previous frame and only touching changed cells.
    ///
    /// `cursor_pos`: `Some((row, col))` shows the cursor there (the normal
    /// live-session case); `None` hides it, used while the scrollback view
    /// is scrolled away from the live bottom -- showing a blinking cursor
    /// in the middle of historical output would be misleading about where
    /// input actually goes.
    pub fn draw(&mut self, rows: &[Vec<Cell>], cursor_pos: Option<(usize, usize)>) -> Result<()> {
        if self.last_frame.len() != rows.len()
            || self.last_frame.first().map(|r| r.len()) != rows.first().map(|r| r.len())
        {
            // Full repaint on resize / first frame.
            queue!(self.out, Clear(ClearType::All))?;
            self.last_frame = vec![Vec::new(); rows.len()];
        }

        for (y, row) in rows.iter().enumerate() {
            let last_row = self.last_frame.get(y);
            let mut x = 0usize;
            while x < row.len() {
                let changed = last_row
                    .and_then(|r| r.get(x))
                    .map(|c| !cell_eq(c, &row[x]))
                    .unwrap_or(true);
                if !changed {
                    x += 1;
                    continue;
                }
                // Coalesce a run of consecutive changed cells that share the
                // same style, to minimize SGR escape churn per the
                // "Incremental Rendering" goal.
                let run_start = x;
                let style = style_key(&row[x]);
                let mut text = String::new();
                while x < row.len() {
                    let still_changed = last_row
                        .and_then(|r| r.get(x))
                        .map(|c| !cell_eq(c, &row[x]))
                        .unwrap_or(true);
                    if !still_changed || style_key(&row[x]) != style {
                        break;
                    }
                    text.push(row[x].ch);
                    x += 1;
                }
                queue!(self.out, cursor::MoveTo(run_start as u16, y as u16))?;
                write_styled(&mut self.out, &self.palette, &row[run_start], &text)?;
            }
        }

        if self.last_cursor != cursor_pos {
            match cursor_pos {
                Some((row, col)) => {
                    queue!(self.out, cursor::Show, cursor::MoveTo(col as u16, row as u16))?;
                }
                None => {
                    queue!(self.out, cursor::Hide)?;
                }
            }
        }
        self.out.flush()?;

        self.last_frame = rows.to_vec();
        self.last_cursor = cursor_pos;
        Ok(())
    }
}

impl Default for CrosstermRenderer {
    fn default() -> Self {
        Self::new()
    }
}

fn cell_eq(a: &Cell, b: &Cell) -> bool {
    a.ch == b.ch && a.fg == b.fg && a.bg == b.bg && attrs_eq(a.attrs, b.attrs)
}

fn attrs_eq(a: Attrs, b: Attrs) -> bool {
    a.bold == b.bold
        && a.italic == b.italic
        && a.underline == b.underline
        && a.reverse == b.reverse
        && a.strikethrough == b.strikethrough
}

fn style_key(c: &Cell) -> (Color, Color, u8) {
    let bits = (c.attrs.bold as u8)
        | ((c.attrs.italic as u8) << 1)
        | ((c.attrs.underline as u8) << 2)
        | ((c.attrs.reverse as u8) << 3)
        | ((c.attrs.strikethrough as u8) << 4);
    (c.fg, c.bg, bits)
}

fn to_ct_color(palette: &Palette, c: Color, is_fg: bool) -> CtColor {
    let palette::Rgb(r, g, b) = palette.resolve(c, is_fg);
    CtColor::Rgb { r, g, b }
}

fn write_styled(out: &mut Stdout, palette: &Palette, sample: &Cell, text: &str) -> Result<()> {
    queue!(out, ResetColor, SetAttribute(Attribute::Reset))?;
    if sample.attrs.bold { queue!(out, SetAttribute(Attribute::Bold))?; }
    if sample.attrs.italic { queue!(out, SetAttribute(Attribute::Italic))?; }
    if sample.attrs.underline { queue!(out, SetAttribute(Attribute::Underlined))?; }
    if sample.attrs.reverse { queue!(out, SetAttribute(Attribute::Reverse))?; }
    if sample.attrs.strikethrough { queue!(out, SetAttribute(Attribute::CrossedOut))?; }
    queue!(
        out,
        SetColors(Colors::new(to_ct_color(palette, sample.fg, true), to_ct_color(palette, sample.bg, false))),
        Print(text)
    )?;
    Ok(())
}
