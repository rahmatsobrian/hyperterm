//! Terminal Core
//!
//! Owns the "live" screen grid (the visible `rows x cols` viewport that's
//! currently being written to) plus cursor and SGR (color/attribute) state.
//! When a line scrolls off the top of the live grid it is handed to the
//! [`crate::virtual_buffer::VirtualBuffer`] as permanent, immutable history
//! -- this is the boundary between "what the shell is actively drawing"
//! and "what the user can scroll back through forever".

use crate::virtual_buffer::{Attrs, Cell, Color, Line, VirtualBuffer};

#[derive(Clone, Copy, Debug, Default)]
struct SgrState {
    fg: Color,
    bg: Color,
    attrs: Attrs,
}

pub struct TerminalCore {
    pub rows: usize,
    pub cols: usize,
    grid: Vec<Vec<Cell>>,
    cursor_row: usize,
    cursor_col: usize,
    saved_cursor: Option<(usize, usize)>,
    sgr: SgrState,
    /// Scroll region (top, bottom), 0-indexed, inclusive. Defaults to full screen.
    scroll_top: usize,
    scroll_bottom: usize,
    pub title: String,
    pub bell_count: u64,
}

impl TerminalCore {
    pub fn new(rows: usize, cols: usize) -> Self {
        Self {
            rows,
            cols,
            grid: vec![vec![Cell::default(); cols]; rows],
            cursor_row: 0,
            cursor_col: 0,
            saved_cursor: None,
            sgr: SgrState::default(),
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            title: String::new(),
            bell_count: 0,
        }
    }

    pub fn resize(&mut self, rows: usize, cols: usize) {
        self.grid.resize(rows, vec![Cell::default(); cols]);
        for row in &mut self.grid {
            row.resize(cols, Cell::default());
        }
        self.rows = rows;
        self.cols = cols;
        self.scroll_bottom = rows.saturating_sub(1);
        self.cursor_row = self.cursor_row.min(rows.saturating_sub(1));
        self.cursor_col = self.cursor_col.min(cols.saturating_sub(1));
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.cursor_row, self.cursor_col)
    }

    /// Snapshot of the currently visible rows, for the renderer.
    pub fn visible_rows(&self) -> &[Vec<Cell>] {
        &self.grid
    }

    // ---- Printable characters ----

    pub fn print(&mut self, ch: char, vbuf: &mut VirtualBuffer) {
        if self.cursor_col >= self.cols {
            // Auto-wrap: the row we're about to scroll off (if scrolling
            // happens) is a *soft* break -- its content continues onto the
            // next row, so mark it `wrapped = true` for reflow purposes
            // (see `virtual_buffer::reflow`), unlike a real `\n`.
            self.newline(vbuf, true);
        }
        let cell = Cell {
            ch,
            fg: self.sgr.fg,
            bg: self.sgr.bg,
            attrs: self.sgr.attrs,
        };
        self.grid[self.cursor_row][self.cursor_col] = cell;
        self.cursor_col += 1;
    }

    // ---- Control characters ----

    pub fn carriage_return(&mut self) {
        self.cursor_col = 0;
    }

    pub fn line_feed(&mut self, vbuf: &mut VirtualBuffer) {
        // A real `\n`: the row being scrolled off (if any) is a genuine
        // line break, not a continuation.
        self.newline(vbuf, false);
    }

    pub fn backspace(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        }
    }

    pub fn tab(&mut self) {
        let next_stop = ((self.cursor_col / 8) + 1) * 8;
        self.cursor_col = next_stop.min(self.cols.saturating_sub(1));
    }

    fn newline(&mut self, vbuf: &mut VirtualBuffer, is_soft_wrap: bool) {
        if self.cursor_row >= self.scroll_bottom {
            // Scroll the region up by one, pushing the top line into history.
            let evicted = self.grid.remove(self.scroll_top);
            vbuf.push_line(Line {
                cells: evicted,
                wrapped: is_soft_wrap,
            });
            self.grid
                .insert(self.scroll_bottom, vec![Cell::default(); self.cols]);
        } else {
            self.cursor_row += 1;
        }
        self.cursor_col = 0;
    }

    // ---- Cursor movement (CSI) ----

    pub fn cursor_up(&mut self, n: usize) {
        self.cursor_row = self.cursor_row.saturating_sub(n.max(1));
    }
    pub fn cursor_down(&mut self, n: usize) {
        self.cursor_row = (self.cursor_row + n.max(1)).min(self.rows.saturating_sub(1));
    }
    pub fn cursor_forward(&mut self, n: usize) {
        self.cursor_col = (self.cursor_col + n.max(1)).min(self.cols.saturating_sub(1));
    }
    pub fn cursor_back(&mut self, n: usize) {
        self.cursor_col = self.cursor_col.saturating_sub(n.max(1));
    }
    pub fn cursor_to(&mut self, row1: usize, col1: usize) {
        self.cursor_row = row1.saturating_sub(1).min(self.rows.saturating_sub(1));
        self.cursor_col = col1.saturating_sub(1).min(self.cols.saturating_sub(1));
    }
    pub fn save_cursor(&mut self) {
        self.saved_cursor = Some((self.cursor_row, self.cursor_col));
    }
    pub fn restore_cursor(&mut self) {
        if let Some((r, c)) = self.saved_cursor {
            self.cursor_row = r;
            self.cursor_col = c;
        }
    }
    pub fn set_scroll_region(&mut self, top1: usize, bottom1: usize) {
        let top = top1.saturating_sub(1).min(self.rows.saturating_sub(1));
        let bottom = bottom1.saturating_sub(1).min(self.rows.saturating_sub(1));
        if top < bottom {
            self.scroll_top = top;
            self.scroll_bottom = bottom;
        } else {
            self.scroll_top = 0;
            self.scroll_bottom = self.rows.saturating_sub(1);
        }
    }

    // ---- Erasing ----

    pub fn erase_in_line(&mut self, mode: u16) {
        let end = self
            .cursor_col
            .min(self.grid[self.cursor_row].len().saturating_sub(1));
        let row = &mut self.grid[self.cursor_row];
        match mode {
            0 => row[self.cursor_col..].fill(Cell::default()),
            1 => row[..=end].fill(Cell::default()),
            2 => row.fill(Cell::default()),
            _ => {}
        }
    }

    pub fn erase_in_display(&mut self, mode: u16, vbuf: &mut VirtualBuffer) {
        match mode {
            0 => {
                self.erase_in_line(0);
                for r in (self.cursor_row + 1)..self.rows {
                    self.grid[r].fill(Cell::default());
                }
            }
            1 => {
                self.erase_in_line(1);
                for r in 0..self.cursor_row {
                    self.grid[r].fill(Cell::default());
                }
            }
            2 | 3 => {
                // Push everything currently on screen into history before
                // clearing, matching how real terminals let you scroll back
                // through a `clear`.
                for r in 0..self.rows {
                    let line =
                        std::mem::replace(&mut self.grid[r], vec![Cell::default(); self.cols]);
                    if line.iter().any(|c| c.ch != ' ') {
                        vbuf.push_line(Line {
                            cells: line,
                            wrapped: false,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    // ---- SGR / colors ----

    pub fn reset_sgr(&mut self) {
        self.sgr = SgrState::default();
    }
    pub fn set_bold(&mut self, v: bool) {
        self.sgr.attrs.bold = v;
    }
    pub fn set_italic(&mut self, v: bool) {
        self.sgr.attrs.italic = v;
    }
    pub fn set_underline(&mut self, v: bool) {
        self.sgr.attrs.underline = v;
    }
    pub fn set_reverse(&mut self, v: bool) {
        self.sgr.attrs.reverse = v;
    }
    pub fn set_strikethrough(&mut self, v: bool) {
        self.sgr.attrs.strikethrough = v;
    }
    pub fn set_fg(&mut self, c: Color) {
        self.sgr.fg = c;
    }
    pub fn set_bg(&mut self, c: Color) {
        self.sgr.bg = c;
    }

    pub fn bell(&mut self) {
        self.bell_count += 1;
    }

    pub fn set_title(&mut self, title: String) {
        self.title = title;
    }
}
