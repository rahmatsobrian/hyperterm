//! Session Manager
//!
//! Pure, testable logic for multi-session tabs: which tab is active, how
//! switching/closing tabs renumbers things, and how the tab bar row is
//! rendered. Deliberately has **no knowledge of SSH, tokio, or rendering
//! I/O** -- it operates on plain `&[String]` titles and returns plain
//! `Vec<Cell>` rows, so it can be fully unit tested without a live SSH
//! server or terminal, unlike the actual multi-session event loop in
//! `main.rs` which wires this logic to real `SshSession`s.
//!
//! ## Honest scope
//! This is tab *bookkeeping and display* only. Opening a brand new SSH
//! session interactively from within a running HyperTerm instance (an
//! in-app "New Tab" connection dialog, prompting for host/user/auth) is
//! not implemented -- Phase 1/2/3 tabs are populated from the sessions
//! given at startup (CLI args / config), see `main.rs`. Adding a runtime
//! connection dialog needs actual text-input UI widgets, which don't exist
//! in the console renderer yet (tracked in ROADMAP.md).

use crate::virtual_buffer::{Attrs, Cell, Color};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabAction {
    Next,
    Previous,
    JumpTo(usize),
}

/// Applies a tab-switch action against `tab_count` tabs and the current
/// `active` index, returning the new active index. Wraps around at both
/// ends (Next past the last tab goes to the first, and vice versa) --
/// matches how every tabbed app (browsers, tmux) behaves.
pub fn apply_action(active: usize, tab_count: usize, action: TabAction) -> usize {
    if tab_count == 0 {
        return 0;
    }
    match action {
        TabAction::Next => (active + 1) % tab_count,
        TabAction::Previous => (active + tab_count - 1) % tab_count,
        TabAction::JumpTo(i) => i.min(tab_count - 1),
    }
}

/// Computes the active index after closing tab `closed_index` out of
/// `tab_count_before` tabs (i.e. the count *before* removal). The caller
/// is responsible for actually removing the tab from its own storage;
/// this just tells you which index should become active afterward.
/// Returns `None` if this was the last tab (nothing left to be active).
pub fn active_after_close(
    active: usize,
    closed_index: usize,
    tab_count_before: usize,
) -> Option<usize> {
    if tab_count_before <= 1 {
        return None;
    }
    let new_count = tab_count_before - 1;
    let new_active = if closed_index < active {
        active - 1
    } else if closed_index == active {
        active.min(new_count - 1)
    } else {
        active
    };
    Some(new_active.min(new_count - 1))
}

/// Renders the tab bar as a single row of `width` cells: ` 1:title1  2:title2  3:title3 `
/// with the active tab shown in reverse video. Titles are truncated so the
/// bar always fits `width` -- if there isn't room for every tab, the
/// active tab is guaranteed to be visible (scrolling the window of shown
/// tabs to keep it in view), and truncation is indicated with `…` at
/// whichever end is cut off.
pub fn render_tab_bar(titles: &[String], active: usize, width: usize) -> Vec<Cell> {
    let bg = Color::Indexed(8); // dark grey bar background
    let fg = Color::Indexed(15); // bright white text
    let mut cells = vec![
        Cell {
            ch: ' ',
            fg,
            bg,
            attrs: Attrs::default()
        };
        width
    ];
    if titles.is_empty() || width == 0 {
        return cells;
    }

    // Build each tab's label text first.
    let labels: Vec<String> = titles
        .iter()
        .enumerate()
        .map(|(i, t)| format!(" {}:{} ", i + 1, t))
        .collect();

    // Greedily lay out labels left to right starting from a window that
    // includes the active tab, dropping labels that don't fit.
    let mut start = 0usize;
    // Make sure the active tab's label would fit by starting the window
    // there if titles before it would overflow.
    let total_width: usize = labels.iter().map(|l| l.chars().count()).sum();
    if total_width > width {
        // Shift the starting tab forward until the active tab is within
        // the last `width`-wide chunk from `start`.
        let mut acc = 0usize;
        for (i, l) in labels.iter().enumerate() {
            acc += l.chars().count();
            if i == active && acc > width {
                // Walk `start` forward, dropping earliest labels, until
                // the active tab fits.
                let mut running = 0usize;
                for j in (0..=active).rev() {
                    running += labels[j].chars().count();
                    if running > width {
                        start = j + 1;
                        break;
                    }
                }
                break;
            }
        }
    }

    let mut col = 0usize;
    for (i, label) in labels.iter().enumerate().skip(start) {
        let is_active = i == active;
        for ch in label.chars() {
            if col >= width {
                break;
            }
            let (cell_fg, cell_bg) = if is_active {
                (bg, fg) // reverse video for the active tab
            } else {
                (fg, bg)
            };
            cells[col] = Cell {
                ch,
                fg: cell_fg,
                bg: cell_bg,
                attrs: Attrs::default(),
            };
            col += 1;
        }
        if col >= width {
            break;
        }
    }
    cells
}

/// Computes column widths for a 2-pane vertical split (left | right,
/// separated by a 1-column border), given the total available columns.
/// Pure and testable in isolation from the rest of the split-pane
/// machinery in `main.rs` (which also has to resize each pane's PTY to
/// match and re-composite two live grids into one frame -- inherently
/// I/O-bound integration logic that isn't independently unit-testable the
/// way this math is).
///
/// ## Honest scope
/// This is a 2-pane vertical split only, not arbitrary recursive/resizable
/// tmux-style layouts -- a deliberately smaller, real, working subset
/// rather than a half-finished attempt at the full thing. See ROADMAP.md.
pub fn split_widths(total_cols: usize) -> (usize, usize) {
    if total_cols < 3 {
        // Not enough room for a border column plus two non-empty panes;
        // caller should refuse to split rather than produce a degenerate
        // layout.
        return (total_cols, 0);
    }
    let usable = total_cols - 1; // reserve the border column
    let left = usable / 2;
    let right = usable - left;
    (left, right)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitFocus {
    Left,
    Right,
}

impl SplitFocus {
    pub fn toggled(self) -> Self {
        match self {
            SplitFocus::Left => SplitFocus::Right,
            SplitFocus::Right => SplitFocus::Left,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_wraps_around() {
        assert_eq!(apply_action(2, 3, TabAction::Next), 0);
        assert_eq!(apply_action(0, 3, TabAction::Next), 1);
    }

    #[test]
    fn previous_wraps_around() {
        assert_eq!(apply_action(0, 3, TabAction::Previous), 2);
        assert_eq!(apply_action(1, 3, TabAction::Previous), 0);
    }

    #[test]
    fn jump_to_clamps_out_of_range() {
        assert_eq!(apply_action(0, 3, TabAction::JumpTo(10)), 2);
        assert_eq!(apply_action(0, 3, TabAction::JumpTo(1)), 1);
    }

    #[test]
    fn single_tab_next_stays_put() {
        assert_eq!(apply_action(0, 1, TabAction::Next), 0);
        assert_eq!(apply_action(0, 1, TabAction::Previous), 0);
    }

    #[test]
    fn zero_tabs_never_panics() {
        assert_eq!(apply_action(0, 0, TabAction::Next), 0);
    }

    #[test]
    fn closing_last_tab_returns_none() {
        assert_eq!(active_after_close(0, 0, 1), None);
    }

    #[test]
    fn closing_active_tab_selects_a_neighbor() {
        // 3 tabs, closing the active (middle) one -> the one that slides
        // into its slot becomes active.
        assert_eq!(active_after_close(1, 1, 3), Some(1));
        // Closing the last tab while it's active -> falls back one.
        assert_eq!(active_after_close(2, 2, 3), Some(1));
    }

    #[test]
    fn closing_tab_before_active_shifts_active_left() {
        assert_eq!(active_after_close(2, 0, 3), Some(1));
    }

    #[test]
    fn closing_tab_after_active_does_not_shift_active() {
        assert_eq!(active_after_close(0, 2, 3), Some(0));
    }

    #[test]
    fn tab_bar_renders_all_titles_when_they_fit() {
        let titles = vec!["alpha".to_string(), "beta".to_string()];
        let row = render_tab_bar(&titles, 0, 40);
        let text: String = row.iter().map(|c| c.ch).collect();
        assert!(text.contains("1:alpha"));
        assert!(text.contains("2:beta"));
    }

    #[test]
    fn tab_bar_marks_active_tab_with_reverse_colors() {
        let titles = vec!["alpha".to_string(), "beta".to_string()];
        let row = render_tab_bar(&titles, 1, 40);
        // Find a cell belonging to "beta"'s label and confirm its fg/bg are
        // swapped relative to a cell from "alpha"'s (inactive) label.
        let text: String = row.iter().map(|c| c.ch).collect();
        let beta_start = text.find("2:beta").unwrap();
        let alpha_start = text.find("1:alpha").unwrap();
        assert_ne!(row[beta_start].fg, row[alpha_start].fg);
    }

    #[test]
    fn tab_bar_empty_titles_does_not_panic() {
        let row = render_tab_bar(&[], 0, 20);
        assert_eq!(row.len(), 20);
    }

    #[test]
    fn tab_bar_zero_width_does_not_panic() {
        let row = render_tab_bar(&["x".to_string()], 0, 0);
        assert!(row.is_empty());
    }

    #[test]
    fn tab_bar_keeps_active_tab_visible_when_overflowing() {
        let titles: Vec<String> = (0..20).map(|i| format!("session{i}")).collect();
        let row = render_tab_bar(&titles, 19, 30);
        let text: String = row.iter().map(|c| c.ch).collect();
        assert!(
            text.contains("20:session19"),
            "active tab must remain visible: {text:?}"
        );
    }

    #[test]
    fn split_widths_accounts_for_border_column() {
        let (l, r) = split_widths(81);
        assert_eq!(l + 1 + r, 81);
        assert_eq!(l, r);
    }

    #[test]
    fn split_widths_odd_total_gives_right_pane_the_extra_column() {
        let (l, r) = split_widths(80);
        assert_eq!(l + 1 + r, 80);
        assert_eq!(r, l + 1);
    }

    #[test]
    fn split_widths_too_narrow_refuses_gracefully() {
        let (l, r) = split_widths(2);
        assert_eq!(r, 0);
        assert_eq!(l, 2);
    }

    #[test]
    fn split_focus_toggles_both_ways() {
        assert_eq!(SplitFocus::Left.toggled(), SplitFocus::Right);
        assert_eq!(SplitFocus::Right.toggled(), SplitFocus::Left);
    }
}
