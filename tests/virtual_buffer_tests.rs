//! Integration-style unit tests for the scrollback pipeline:
//! `Cell`/`Line` encoding, `DiskCache` persistence + crash recovery, and
//! `VirtualBuffer`'s RAM<->disk transparency.

use hyperterm::disk_cache::DiskCache;
use hyperterm::virtual_buffer::cell::{Attrs, Cell, Color, Line};
use hyperterm::virtual_buffer::VirtualBuffer;

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("hyperterm-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn make_line(text: &str) -> Line {
    Line {
        cells: text
            .chars()
            .map(|ch| Cell {
                ch,
                fg: Color::Indexed(2),
                bg: Color::Default,
                attrs: Attrs::default(),
            })
            .collect(),
        wrapped: false,
    }
}

#[test]
fn line_encode_decode_roundtrip() {
    let line = make_line("hello world");
    let encoded = line.encode();
    let decoded = Line::decode(&encoded).unwrap();
    assert_eq!(decoded.plain_text(), "hello world");
    assert_eq!(decoded.cells[0].fg, Color::Indexed(2));
}

#[test]
fn line_ansi_roundtrip_contains_escape() {
    let line = make_line("colored");
    let ansi = line.to_ansi_string();
    assert!(ansi.contains("\x1b["));
    assert!(ansi.contains("colored"));
}

#[test]
fn disk_cache_append_and_read_back() {
    let dir = temp_dir("disk-basic");
    let mut cache = DiskCache::open(&dir, "session-a").unwrap();
    let id1 = cache.append_line(b"first line").unwrap();
    let id2 = cache.append_line(b"second line").unwrap();
    cache.flush().unwrap();

    assert_eq!(id1, 0);
    assert_eq!(id2, 1);
    assert_eq!(cache.read_line(0).unwrap(), b"first line".to_vec());
    assert_eq!(cache.read_line(1).unwrap(), b"second line".to_vec());
    assert_eq!(cache.line_count(), 2);
}

#[test]
fn disk_cache_survives_reopen() {
    let dir = temp_dir("disk-reopen");
    {
        let mut cache = DiskCache::open(&dir, "session-b").unwrap();
        for i in 0..50 {
            cache.append_line(format!("line-{i}").as_bytes()).unwrap();
        }
        cache.sync().unwrap();
    }
    // Simulate app restart: open again, history must still be there.
    let mut reopened = DiskCache::open(&dir, "session-b").unwrap();
    assert_eq!(reopened.line_count(), 50);
    assert_eq!(reopened.read_line(25).unwrap(), b"line-25".to_vec());
}

#[test]
fn virtual_buffer_never_drops_lines_beyond_ram_capacity() {
    let dir = temp_dir("vbuf-spill");
    let mut vbuf = VirtualBuffer::open(&dir, "session-c", /* ram_capacity */ 10).unwrap();

    for i in 0..1000u32 {
        vbuf.push_line(make_line(&format!("row {i}")));
    }

    assert_eq!(vbuf.total_lines(), 1000);
    // First line ever pushed should still be retrievable even though RAM
    // capacity is only 10 -- this is the "never delete history" guarantee.
    let first = vbuf.get_line(0).unwrap();
    assert_eq!(first.plain_text(), "row 0");
    let last = vbuf.get_line(999).unwrap();
    assert_eq!(last.plain_text(), "row 999");
    let middle = vbuf.get_line(500).unwrap();
    assert_eq!(middle.plain_text(), "row 500");
}

#[test]
fn virtual_buffer_persists_across_restart() {
    let dir = temp_dir("vbuf-restart");
    {
        let mut vbuf = VirtualBuffer::open(&dir, "session-d", 5).unwrap();
        for i in 0..100u32 {
            vbuf.push_line(make_line(&format!("persisted {i}")));
        }
        vbuf.checkpoint().unwrap();
    }
    let mut reopened = VirtualBuffer::open(&dir, "session-d", 5).unwrap();
    // Only the lines that were spilled to disk before restart survive;
    // lines still in RAM at time of "restart" (simulated by dropping the
    // struct) are lost in this Phase 1 model -- see ROADMAP.md for
    // write-ahead-log hardening. What must hold is that we don't lose
    // *already-flushed* history.
    assert!(reopened.total_lines() > 0);
    let first = reopened.get_line(0).unwrap();
    assert_eq!(first.plain_text(), "persisted 0");
}

#[test]
fn virtual_buffer_range_fetch_spans_disk_and_ram() {
    let dir = temp_dir("vbuf-range");
    let mut vbuf = VirtualBuffer::open(&dir, "session-e", 20).unwrap();
    for i in 0..100u32 {
        vbuf.push_line(make_line(&format!("r{i}")));
    }
    // Range that starts on disk (id 10) and ends in RAM (id 95).
    let range = vbuf.get_range(10, 95);
    assert_eq!(range.len(), 85);
    assert_eq!(range.first().unwrap().plain_text(), "r10");
    assert_eq!(range.last().unwrap().plain_text(), "r94");
}

#[test]
fn history_window_returns_requested_row_count_at_matching_width() {
    let dir = temp_dir("history-window-basic");
    let mut vbuf = VirtualBuffer::open(&dir, "session-f", 20).unwrap();
    for i in 0..50u32 {
        vbuf.push_line(make_line(&format!("history line {i}")));
    }
    let window = vbuf.history_window(80, 10, 50);
    assert_eq!(window.len(), 10);
    assert_eq!(window.last().unwrap().plain_text(), "history line 49");
    assert_eq!(window.first().unwrap().plain_text(), "history line 40");
}

#[test]
fn history_window_near_start_of_buffer_returns_fewer_rows_not_panic() {
    let dir = temp_dir("history-window-short");
    let mut vbuf = VirtualBuffer::open(&dir, "session-g", 20).unwrap();
    for i in 0..3u32 {
        vbuf.push_line(make_line(&format!("only {i}")));
    }
    // Ask for more rows than exist -- must not panic, just return what's there.
    let window = vbuf.history_window(80, 10, 3);
    assert_eq!(window.len(), 3);
    assert_eq!(window[0].plain_text(), "only 0");
}

#[test]
fn history_window_grows_fetch_when_reflow_narrows_width() {
    // 20 lines of ~40-char content written "wide"; asking for a much
    // narrower target width means each raw line reflows into multiple
    // visual rows, so the window must grow to still satisfy `want_rows`.
    let dir = temp_dir("history-window-reflow-grow");
    let mut vbuf = VirtualBuffer::open(&dir, "session-h", 20).unwrap();
    for i in 0..20u32 {
        vbuf.push_line(make_line(&format!(
            "this is a moderately long history line number {i:02}"
        )));
    }
    let window = vbuf.history_window(10, 15, 20);
    assert_eq!(window.len(), 15);
    // Every row should fit within the requested width (content, before
    // trailing-space padding a caller might add for display).
    for line in &window {
        assert!(line.plain_text().chars().count() <= 10);
    }
}
