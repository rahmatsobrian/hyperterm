//! Reflow
//!
//! Addresses the "resizing doesn't re-flow history" limitation: when the
//! live grid auto-wraps a long line across multiple rows (as opposed to a
//! real `\n`), each stored [`Line`] already carries a `wrapped: bool` flag
//! set by `TerminalCore` marking "this row's content continues onto the
//! next row, it isn't a real line break". That's enough information to
//! reconstruct the original *logical* line (arbitrary length) from its
//! *physical* rows (fixed to whatever `cols` was at write time), and then
//! re-wrap it to a *different* width on demand.
//!
//! ## Design: view-time, not storage-time
//! Reflow happens lazily, only for the lines currently being rendered in
//! the scrollback viewport (see `main.rs`'s scrollback view mode) -- never
//! by rewriting the disk cache. Rewriting potentially tens of millions of
//! already-persisted lines every time the user resizes the window would be
//! slow and would defeat the append-only design that makes the disk cache
//! crash-safe in the first place. The cost of view-time reflow is O(visible
//! lines), paid only while actually looking at history at a width that
//! doesn't match how it was originally written.

use crate::virtual_buffer::{Attrs, Cell, Color, Line};

/// Groups a flat sequence of physical rows back into logical lines: a
/// logical line is a maximal run of consecutive rows where every row
/// except the last has `wrapped == true`.
fn group_logical_lines(lines: &[Line]) -> Vec<Vec<Cell>> {
    let mut logical = Vec::new();
    let mut current: Vec<Cell> = Vec::new();

    for line in lines {
        current.extend_from_slice(&line.cells);
        if !line.wrapped {
            logical.push(std::mem::take(&mut current));
        }
        // If `line.wrapped` is true, keep accumulating into `current` --
        // the next row continues this same logical line.
    }
    if !current.is_empty() {
        // Trailing soft-wrapped run with no closing hard break yet (e.g.
        // history ends mid-line at the point the buffer was opened) --
        // still worth showing as its own logical line.
        logical.push(current);
    }
    logical
}

/// Trims trailing blank cells (space, default style) from a logical line
/// before re-wrapping, so reflowing doesn't introduce a growing tail of
/// empty rows every time you resize -- matches how every other terminal's
/// reflow behaves.
fn trim_trailing_blanks(cells: &[Cell]) -> &[Cell] {
    let mut end = cells.len();
    while end > 0 {
        let c = &cells[end - 1];
        if c.ch == ' ' && c.fg == Color::Default && c.bg == Color::Default {
            end -= 1;
        } else {
            break;
        }
    }
    &cells[..end]
}

/// Splits one logical line's cells into physical rows of exactly
/// `new_width` columns (except possibly the last), marking every row but
/// the last as `wrapped = true`.
fn rewrap_logical_line(cells: &[Cell], new_width: usize) -> Vec<Line> {
    if new_width == 0 {
        return vec![Line {
            cells: cells.to_vec(),
            wrapped: false,
        }];
    }
    let trimmed = trim_trailing_blanks(cells);
    if trimmed.is_empty() {
        return vec![Line {
            cells: Vec::new(),
            wrapped: false,
        }];
    }

    let mut out = Vec::with_capacity(trimmed.len() / new_width + 1);
    let mut chunks = trimmed.chunks(new_width).peekable();
    while let Some(chunk) = chunks.next() {
        let is_last = chunks.peek().is_none();
        out.push(Line {
            cells: chunk.to_vec(),
            wrapped: !is_last,
        });
    }
    out
}

/// Takes a run of physical rows (as stored/retrieved from the
/// `VirtualBuffer`, potentially written at several different widths across
/// multiple resizes during the session) and returns them re-wrapped for
/// `new_width`, preserving logical line boundaries (real `\n`s stay real
/// breaks; only soft-wrap points move).
pub fn reflow(lines: &[Line], new_width: usize) -> Vec<Line> {
    let logical_lines = group_logical_lines(lines);
    let mut out = Vec::with_capacity(lines.len());
    for logical in &logical_lines {
        out.extend(rewrap_logical_line(logical, new_width));
    }
    out
}

fn plain_cell(ch: char) -> Cell {
    Cell {
        ch,
        fg: Color::Default,
        bg: Color::Default,
        attrs: Attrs::default(),
    }
}

/// Convenience for tests / callers building synthetic rows without caring
/// about styling.
pub fn plain_row(text: &str, width: usize, wrapped: bool) -> Line {
    let mut cells: Vec<Cell> = text.chars().map(plain_cell).collect();
    cells.resize(width, plain_cell(' '));
    Line { cells, wrapped }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_text(line: &Line) -> String {
        line.plain_text()
    }

    #[test]
    fn hard_break_lines_stay_separate() {
        let lines = vec![plain_row("hello", 10, false), plain_row("world", 10, false)];
        let reflowed = reflow(&lines, 10);
        assert_eq!(reflowed.len(), 2);
        assert_eq!(row_text(&reflowed[0]), "hello");
        assert_eq!(row_text(&reflowed[1]), "world");
    }

    #[test]
    fn soft_wrapped_line_rejoins_then_rewraps_narrower() {
        // Originally written at width 10 as two wrapped physical rows:
        // "abcdefghij" (wrapped) + "klmno" (hard break) = logical line
        // "abcdefghijklmno" (15 chars).
        let lines = vec![
            plain_row("abcdefghij", 10, true),
            plain_row("klmno", 10, false),
        ];
        let reflowed = reflow(&lines, 5);
        // At width 5: "abcde" "fghij" "klmno" -- three rows, first two wrapped.
        assert_eq!(reflowed.len(), 3);
        assert_eq!(row_text(&reflowed[0]), "abcde");
        assert!(reflowed[0].wrapped);
        assert_eq!(row_text(&reflowed[1]), "fghij");
        assert!(reflowed[1].wrapped);
        assert_eq!(row_text(&reflowed[2]), "klmno");
        assert!(!reflowed[2].wrapped);
    }

    #[test]
    fn soft_wrapped_line_rejoins_then_rewraps_wider() {
        let lines = vec![
            plain_row("abcde", 5, true),
            plain_row("fghij", 5, true),
            plain_row("klmno", 5, false),
        ];
        let reflowed = reflow(&lines, 15);
        assert_eq!(reflowed.len(), 1);
        assert_eq!(row_text(&reflowed[0]), "abcdefghijklmno");
        assert!(!reflowed[0].wrapped);
    }

    #[test]
    fn trailing_blanks_trimmed_not_endlessly_rewrapped() {
        let lines = vec![plain_row("hi", 20, false)];
        let reflowed = reflow(&lines, 5);
        // "hi" padded to 20 cols should NOT become 4 rows of mostly blanks
        // when rewrapped at width 5 -- trailing blank padding is trimmed.
        assert_eq!(reflowed.len(), 1);
        assert_eq!(row_text(&reflowed[0]), "hi");
    }

    #[test]
    fn mixed_widths_across_a_resize_still_reflow_correctly() {
        // Simulates history that was written at width 10, then the user
        // resized to width 6 mid-session for a second logical line.
        let lines = vec![
            plain_row("first line", 10, false),
            plain_row("second", 6, true),
            plain_row(" line ", 6, false), // note: raw cells, trimmed by reflow
        ];
        let reflowed = reflow(&lines, 20);
        assert_eq!(reflowed.len(), 2);
        assert_eq!(row_text(&reflowed[0]), "first line");
        assert_eq!(row_text(&reflowed[1]), "second line");
    }

    #[test]
    fn empty_input_produces_empty_output() {
        assert!(reflow(&[], 10).is_empty());
    }
}
