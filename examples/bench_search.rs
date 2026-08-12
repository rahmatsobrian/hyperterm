//! Benchmark: indexed (trigram) vs. sequential search.
//!
//! Run with: `cargo run --release --example bench_search`
//!
//! Pushes a large synthetic scrollback (mostly noise lines, with a rare
//! marker line) and times finding the marker via the trigram-accelerated
//! path (`SearchEngine::search_all` on a plain-text query, which consults
//! the index automatically) vs. a regex query for the same text (which
//! bypasses the index, per `trigram_index` module docs, and always does a
//! full sequential scan) -- giving an apples-to-apples view of what the
//! index actually buys you.

use std::time::Instant;

use hyperterm::search::{SearchEngine, SearchOptions};
use hyperterm::virtual_buffer::cell::{Attrs, Cell, Color, Line};
use hyperterm::virtual_buffer::VirtualBuffer;

fn make_line(text: &str) -> Line {
    Line {
        cells: text
            .chars()
            .map(|ch| Cell {
                ch,
                fg: Color::Default,
                bg: Color::Default,
                attrs: Attrs::default(),
            })
            .collect(),
        wrapped: false,
    }
}

fn main() {
    let dir = std::env::temp_dir().join(format!("hyperterm-search-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut vbuf = VirtualBuffer::open(&dir, "search-bench", 20_000).unwrap();

    const N: u32 = 1_000_000;
    println!("== HyperTerm Search Benchmark ==");
    println!("pushing {N} lines...");
    for i in 0..N {
        vbuf.push_line(make_line(&format!(
            "log entry {i} - routine status ok, nothing unusual here"
        )));
    }
    vbuf.push_line(make_line(
        "CRITICAL-MARKER-4711 something went wrong in the pipeline",
    ));
    for i in 0..N {
        vbuf.push_line(make_line(&format!(
            "log entry {} - more routine noise for padding",
            N + i
        )));
    }
    vbuf.checkpoint().unwrap();
    println!("total lines: {}", vbuf.total_lines());
    println!();

    // Plain-text query -> accelerated by the trigram index.
    let plain_opts = SearchOptions {
        query: "CRITICAL-MARKER-4711".into(),
        regex: false,
        case_sensitive: true,
        whole_word: false,
    };
    let start = Instant::now();
    let plain_matches = SearchEngine::search_all(&mut vbuf, &plain_opts).unwrap();
    let plain_elapsed = start.elapsed();
    println!(
        "Indexed plain-text search: {} match(es) in {:?}",
        plain_matches.len(),
        plain_elapsed
    );

    // Same text, but as a regex -> bypasses the index (sequential scan).
    let regex_opts = SearchOptions {
        query: "CRITICAL-MARKER-4711".into(),
        regex: true,
        case_sensitive: true,
        whole_word: false,
    };
    let start = Instant::now();
    let regex_matches = SearchEngine::search_all(&mut vbuf, &regex_opts).unwrap();
    let regex_elapsed = start.elapsed();
    println!(
        "Sequential-scan (regex) search: {} match(es) in {:?}",
        regex_matches.len(),
        regex_elapsed
    );

    println!();
    println!(
        "Speedup from indexing: {:.1}x",
        regex_elapsed.as_secs_f64() / plain_elapsed.as_secs_f64().max(0.000_001)
    );

    let _ = std::fs::remove_dir_all(&dir);
}
