//! Benchmark: ANSI/VT parser throughput.
//!
//! Run with: `cargo run --release --example bench_ansi_parse`
//!
//! Feeds a synthetic mixed stream (plain text + SGR color changes + cursor
//! movement, similar to what `ls --color`, build logs, or `htop` produce)
//! through `AnsiParser` and reports MB/s.

use std::time::Instant;

use hyperterm::ansi_parser::AnsiParser;
use hyperterm::terminal_core::TerminalCore;
use hyperterm::virtual_buffer::VirtualBuffer;

fn synthetic_chunk(i: usize) -> String {
    format!(
        "\x1b[3{}mline {i:>6}\x1b[0m \x1b[1mbold\x1b[0m \x1b[38;5;{}mcolored-256\x1b[0m plain text padding here\r\n",
        i % 8,
        (i * 7) % 256
    )
}

fn main() {
    let dir = std::env::temp_dir().join(format!("hyperterm-ansi-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut vbuf = VirtualBuffer::open(&dir, "ansi-bench", 10_000).unwrap();
    let mut core = TerminalCore::new(50, 200);
    let mut parser = AnsiParser::new();

    const ITERATIONS: usize = 200_000;
    let chunks: Vec<String> = (0..ITERATIONS).map(synthetic_chunk).collect();
    let total_bytes: usize = chunks.iter().map(|c| c.len()).sum();

    println!("== HyperTerm ANSI Parser Benchmark ==");
    println!(
        "{ITERATIONS} lines, {:.2} MB total",
        total_bytes as f64 / 1_000_000.0
    );

    let start = Instant::now();
    for chunk in &chunks {
        parser.feed(chunk.as_bytes(), &mut core, &mut vbuf);
    }
    let elapsed = start.elapsed();

    println!(
        "Parsed in {:?} ({:.1} MB/s, {:.0} lines/sec)",
        elapsed,
        (total_bytes as f64 / 1_000_000.0) / elapsed.as_secs_f64(),
        ITERATIONS as f64 / elapsed.as_secs_f64()
    );

    let _ = std::fs::remove_dir_all(&dir);
}
