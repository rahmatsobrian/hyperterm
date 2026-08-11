//! Benchmark: Virtual Scrollback Buffer throughput.
//!
//! Run with: `cargo run --release --example bench_scrollback`
//!
//! This is a deliberately dependency-free benchmark (no `criterion`) so the
//! project doesn't take on extra MSRV/build-time risk just for benchmarking;
//! see CONTRIBUTING.md for why that trade-off was made. It reports:
//!   - sustained line-push throughput (RAM-resident phase)
//!   - sustained line-push throughput (once spilling to disk cache)
//!   - random-access read latency (disk-resident lines)
//!   - sequential range-read throughput (disk-resident lines)

use std::time::Instant;

use hyperterm::virtual_buffer::cell::{Attrs, Cell, Color, Line};
use hyperterm::virtual_buffer::VirtualBuffer;

fn make_line(i: u64) -> Line {
    let text = format!("[{i:>10}] the quick brown fox jumps over the lazy dog 0123456789");
    Line {
        cells: text
            .chars()
            .map(|ch| Cell { ch, fg: Color::Indexed((i % 16) as u8), bg: Color::Default, attrs: Attrs::default() })
            .collect(),
        wrapped: false,
    }
}

fn main() {
    let dir = std::env::temp_dir().join(format!("hyperterm-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let ram_capacity = 50_000;
    let total_lines: u64 = 2_000_000;

    println!("== HyperTerm Scrollback Benchmark ==");
    println!("ram_capacity = {ram_capacity}, total_lines = {total_lines}");
    println!();

    let mut vbuf = VirtualBuffer::open(&dir, "bench", ram_capacity).unwrap();

    let start = Instant::now();
    for i in 0..total_lines {
        vbuf.push_line(make_line(i));
        if i == ram_capacity as u64 {
            let elapsed = start.elapsed();
            println!(
                "RAM-resident phase: {ram_capacity} lines in {:?} ({:.0} lines/sec)",
                elapsed,
                ram_capacity as f64 / elapsed.as_secs_f64()
            );
        }
    }
    vbuf.checkpoint().unwrap();
    let total_elapsed = start.elapsed();
    println!(
        "Full push (RAM+disk spill): {total_lines} lines in {:?} ({:.0} lines/sec)",
        total_elapsed,
        total_lines as f64 / total_elapsed.as_secs_f64()
    );

    // Random access across disk-resident history.
    let sample_ids: Vec<u64> = (0..1000).map(|k| (k * total_lines) / 1000).collect();
    let start = Instant::now();
    for id in &sample_ids {
        let _ = vbuf.get_line(*id).unwrap();
    }
    let elapsed = start.elapsed();
    println!(
        "Random access: {} reads in {:?} ({:.2} µs/read avg)",
        sample_ids.len(),
        elapsed,
        elapsed.as_micros() as f64 / sample_ids.len() as f64
    );

    // Sequential range read (simulating a fast scroll through history).
    let start = Instant::now();
    let range = vbuf.get_range(0, 500_000);
    let elapsed = start.elapsed();
    println!(
        "Sequential range read: {} lines in {:?} ({:.0} lines/sec)",
        range.len(),
        elapsed,
        range.len() as f64 / elapsed.as_secs_f64()
    );

    println!();
    println!("cache file: {:?}", vbuf.cache_file_path());
    let _ = std::fs::remove_dir_all(&dir);
}
