//! Tests for the ANSI escape sequence parser feeding into `TerminalCore`.

use hyperterm::ansi_parser::AnsiParser;
use hyperterm::terminal_core::TerminalCore;
use hyperterm::virtual_buffer::{Color, VirtualBuffer};

fn fresh_vbuf(name: &str) -> VirtualBuffer {
    let dir = std::env::temp_dir().join(format!("hyperterm-ansi-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    VirtualBuffer::open(&dir, "ansi-test", 1000).unwrap()
}

#[test]
fn plain_text_is_printed_at_cursor() {
    let mut core = TerminalCore::new(5, 20);
    let mut parser = AnsiParser::new();
    let mut vbuf = fresh_vbuf("plain");

    parser.feed(b"hello", &mut core, &mut vbuf);

    let row0: String = core.visible_rows()[0].iter().map(|c| c.ch).collect();
    assert!(row0.starts_with("hello"));
}

#[test]
fn carriage_return_and_linefeed_move_cursor() {
    let mut core = TerminalCore::new(5, 20);
    let mut parser = AnsiParser::new();
    let mut vbuf = fresh_vbuf("crlf");

    parser.feed(b"line1\r\nline2", &mut core, &mut vbuf);

    let row0: String = core.visible_rows()[0].iter().map(|c| c.ch).collect();
    let row1: String = core.visible_rows()[1].iter().map(|c| c.ch).collect();
    assert!(row0.starts_with("line1"));
    assert!(row1.starts_with("line2"));
}

#[test]
fn sgr_basic_color_is_applied() {
    let mut core = TerminalCore::new(5, 20);
    let mut parser = AnsiParser::new();
    let mut vbuf = fresh_vbuf("sgr-basic");

    // ESC[31m = red foreground, then "X", then reset.
    parser.feed(b"\x1b[31mX\x1b[0m", &mut core, &mut vbuf);

    let cell = &core.visible_rows()[0][0];
    assert_eq!(cell.ch, 'X');
    assert_eq!(cell.fg, Color::Indexed(1)); // 31 -> index 1 (red)
}

#[test]
fn sgr_truecolor_is_applied() {
    let mut core = TerminalCore::new(5, 20);
    let mut parser = AnsiParser::new();
    let mut vbuf = fresh_vbuf("sgr-truecolor");

    // ESC[38;2;10;20;30m = truecolor foreground.
    parser.feed(b"\x1b[38;2;10;20;30mY", &mut core, &mut vbuf);

    let cell = &core.visible_rows()[0][0];
    assert_eq!(cell.fg, Color::Rgb(10, 20, 30));
}

#[test]
fn sgr_256color_is_applied() {
    let mut core = TerminalCore::new(5, 20);
    let mut parser = AnsiParser::new();
    let mut vbuf = fresh_vbuf("sgr-256");

    parser.feed(b"\x1b[38;5;200mZ", &mut core, &mut vbuf);

    let cell = &core.visible_rows()[0][0];
    assert_eq!(cell.fg, Color::Indexed(200));
}

#[test]
fn scrolling_off_screen_pushes_into_virtual_buffer() {
    let mut core = TerminalCore::new(3, 10); // only 3 rows tall
    let mut parser = AnsiParser::new();
    let mut vbuf = fresh_vbuf("scroll");

    for i in 0..10 {
        parser.feed(format!("row{i}\r\n").as_bytes(), &mut core, &mut vbuf);
    }

    // More lines were printed than fit on screen, so history should have
    // accumulated in the virtual buffer.
    assert!(vbuf.total_lines() > 0);
}

#[test]
fn cursor_movement_csi_sequences() {
    let mut core = TerminalCore::new(10, 20);
    let mut parser = AnsiParser::new();
    let mut vbuf = fresh_vbuf("cursor-move");

    // Move to row 3, col 5 (1-indexed).
    parser.feed(b"\x1b[3;5H", &mut core, &mut vbuf);
    assert_eq!(core.cursor(), (2, 4));
}

#[test]
fn unicode_and_emoji_survive_the_pipeline() {
    let mut core = TerminalCore::new(3, 20);
    let mut parser = AnsiParser::new();
    let mut vbuf = fresh_vbuf("unicode");

    parser.feed("héllo 🚀".as_bytes(), &mut core, &mut vbuf);
    let row0: String = core.visible_rows()[0].iter().map(|c| c.ch).collect();
    assert!(row0.starts_with("héllo 🚀"));
}
