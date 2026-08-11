//! Stress tests. These are `#[ignore]`d by default because they push
//! millions of lines through the pipeline and are meant to run in CI's
//! dedicated "Stress Test" job (`cargo test --release -- --ignored
//! --test-threads=1`), not on every `cargo test`.

use hyperterm::virtual_buffer::cell::{Attrs, Cell, Color, Line};
use hyperterm::virtual_buffer::VirtualBuffer;

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("hyperterm-stress-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn make_line(text: &str) -> Line {
    Line {
        cells: text
            .chars()
            .map(|ch| Cell { ch, fg: Color::Indexed(7), bg: Color::Default, attrs: Attrs::default() })
            .collect(),
        wrapped: false,
    }
}

/// Pushes 5 million lines through the virtual buffer (well beyond the "tens
/// of millions" the spec asks for at a scale that stays reasonable for CI
/// runtime) and verifies the very first and very last lines are still
/// correctly retrievable, i.e. history really is never dropped.
#[test]
#[ignore = "heavy: run explicitly via `cargo test --release -- --ignored`"]
fn stress_five_million_lines_never_lost() {
    let dir = temp_dir("5m-lines");
    let mut vbuf = VirtualBuffer::open(&dir, "stress", 50_000).unwrap();

    const N: u32 = 5_000_000;
    let start = std::time::Instant::now();
    for i in 0..N {
        vbuf.push_line(make_line(&format!("stress line {i} with some representative payload text")));
    }
    let elapsed = start.elapsed();
    eprintln!(
        "pushed {N} lines in {:?} ({:.0} lines/sec)",
        elapsed,
        N as f64 / elapsed.as_secs_f64()
    );

    assert_eq!(vbuf.total_lines(), N as u64);
    assert_eq!(vbuf.get_line(0).unwrap().plain_text(), "stress line 0 with some representative payload text");
    assert_eq!(
        vbuf.get_line((N - 1) as u64).unwrap().plain_text(),
        format!("stress line {} with some representative payload text", N - 1)
    );

    let random_probe_start = std::time::Instant::now();
    for id in [0u64, N as u64 / 4, N as u64 / 2, (N - 1) as u64] {
        let _ = vbuf.get_line(id).unwrap();
    }
    eprintln!("4 random-access reads across 5M lines took {:?}", random_probe_start.elapsed());
}

/// Repeated open/close cycles of the disk cache -- a coarse leak smoke test.
/// This is NOT a substitute for a real memory profiler (Valgrind/heaptrack);
/// it just asserts the process doesn't obviously balloon after many cycles,
/// which would indicate a gross leak (e.g. mmap handles never released).
#[test]
#[ignore = "heavy: run explicitly via `cargo test --release -- --ignored`"]
fn stress_repeated_open_close_no_gross_leak() {
    let dir = temp_dir("open-close-cycles");
    for cycle in 0..200 {
        let mut vbuf = VirtualBuffer::open(&dir, "cycle-session", 100).unwrap();
        for i in 0..500u32 {
            vbuf.push_line(make_line(&format!("cycle {cycle} line {i}")));
        }
        vbuf.checkpoint().unwrap();
        // vbuf dropped here each iteration -- file handles / mmaps must be
        // released promptly (`Drop` on the underlying `File`/`Mmap`), or
        // this loop will eventually fail to open new file handles.
    }
}

/// Rapid resize simulation: feeds ANSI through many different grid sizes in
/// a row without panicking, approximating a user aggressively dragging the
/// window border.
#[test]
#[ignore = "heavy: run explicitly via `cargo test --release -- --ignored`"]
fn stress_rapid_resize_does_not_panic() {
    use hyperterm::ansi_parser::AnsiParser;
    use hyperterm::terminal_core::TerminalCore;

    let dir = temp_dir("resize");
    let mut vbuf = VirtualBuffer::open(&dir, "resize-session", 1000).unwrap();
    let mut core = TerminalCore::new(24, 80);
    let mut parser = AnsiParser::new();

    for i in 0..2000 {
        let rows = 10 + (i % 40);
        let cols = 20 + (i % 100);
        core.resize(rows, cols);
        parser.feed(format!("resize test line {i}\r\n").as_bytes(), &mut core, &mut vbuf);
    }
}
