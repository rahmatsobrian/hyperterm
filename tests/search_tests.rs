//! Tests for `SearchEngine` (plain text, regex, case-sensitivity, whole-word).

use hyperterm::search::{SearchEngine, SearchOptions};
use hyperterm::virtual_buffer::cell::{Attrs, Cell, Color, Line};
use hyperterm::virtual_buffer::VirtualBuffer;

fn vbuf_with_lines(name: &str, lines: &[&str]) -> VirtualBuffer {
    let dir = std::env::temp_dir().join(format!("hyperterm-search-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut vbuf = VirtualBuffer::open(&dir, "search-test", 1000).unwrap();
    for text in lines {
        let cells = text
            .chars()
            .map(|ch| Cell { ch, fg: Color::Default, bg: Color::Default, attrs: Attrs::default() })
            .collect();
        vbuf.push_line(Line { cells, wrapped: false });
    }
    vbuf
}

#[test]
fn plain_text_search_finds_all_occurrences() {
    let mut vbuf = vbuf_with_lines(
        "plain",
        &["error: connection refused", "info: all good", "error: timeout"],
    );
    let opts = SearchOptions { query: "error".into(), regex: false, case_sensitive: true, whole_word: false };
    let matches = SearchEngine::search_all(&mut vbuf, &opts).unwrap();
    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0].line_id, 0);
    assert_eq!(matches[1].line_id, 2);
}

#[test]
fn case_insensitive_search() {
    let mut vbuf = vbuf_with_lines("case", &["ERROR here", "error there"]);
    let opts = SearchOptions { query: "error".into(), regex: false, case_sensitive: false, whole_word: false };
    let matches = SearchEngine::search_all(&mut vbuf, &opts).unwrap();
    assert_eq!(matches.len(), 2);
}

#[test]
fn case_sensitive_search_excludes_mismatches() {
    let mut vbuf = vbuf_with_lines("case-sens", &["ERROR here", "error there"]);
    let opts = SearchOptions { query: "error".into(), regex: false, case_sensitive: true, whole_word: false };
    let matches = SearchEngine::search_all(&mut vbuf, &opts).unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].line_id, 1);
}

#[test]
fn whole_word_search() {
    let mut vbuf = vbuf_with_lines("whole-word", &["cat category catalog", "a cat sat"]);
    let opts = SearchOptions { query: "cat".into(), regex: false, case_sensitive: true, whole_word: true };
    let matches = SearchEngine::search_all(&mut vbuf, &opts).unwrap();
    // Only the standalone "cat" tokens should match, not "category"/"catalog".
    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0].line_id, 0);
    assert_eq!(matches[1].line_id, 1);
}

#[test]
fn regex_search() {
    let mut vbuf = vbuf_with_lines("regex", &["port 8080 open", "port 22 open", "no numbers here"]);
    let opts = SearchOptions { query: r"port \d+".into(), regex: true, case_sensitive: true, whole_word: false };
    let matches = SearchEngine::search_all(&mut vbuf, &opts).unwrap();
    assert_eq!(matches.len(), 2);
}

#[test]
fn search_across_thousands_of_lines_including_disk_spilled() {
    let mut lines_owned: Vec<String> = (0..5000).map(|i| format!("line number {i}")).collect();
    lines_owned.push("needle-in-haystack marker".to_string());
    let lines_ref: Vec<&str> = lines_owned.iter().map(|s| s.as_str()).collect();
    let mut vbuf = vbuf_with_lines("large", &lines_ref);

    let opts = SearchOptions { query: "needle-in-haystack".into(), regex: false, case_sensitive: true, whole_word: false };
    let matches = SearchEngine::search_all(&mut vbuf, &opts).unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].line_id, 5000);
}
