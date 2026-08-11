//! Trigram inverted index.
//!
//! Implements the "Indexed Search" requirement from the spec: instead of
//! sequentially scanning every line (`search::search_all`), a plain-text
//! query is broken into overlapping 3-character shingles ("trigrams"), and
//! we look up which lines contain *all* of the query's trigrams before
//! doing a real substring check on just that candidate set. For a query of
//! any reasonable length this prunes the vast majority of lines without
//! reading them, turning "Ctrl+F" on tens of millions of lines from an
//! O(n) scan into roughly O(matches).
//!
//! ## Scope / honest limitations
//! - **Plain-text queries only.** Extracting the "this substring must
//!   appear literally" trigrams out of an arbitrary regex is a real
//!   problem in its own right (see Russ Cox's classic writeup on
//!   regex+trigram indexing); doing it properly is out of scope for this
//!   pass. Regex search still falls back to the sequential scan in
//!   `search::search_all` -- correct, just not accelerated.
//! - **In-memory only, not persisted to disk.** A durable on-disk trigram
//!   index (so a restart doesn't need to rebuild it) is real additional
//!   work -- tracked in ROADMAP.md. What this module gives you today: the
//!   index is built once when a session's `VirtualBuffer` opens (scanning
//!   existing persisted history up to a size cap, see
//!   `VirtualBuffer::REBUILD_INDEX_LINE_CAP`) and then kept up to date
//!   incrementally as new lines are pushed during the session.
//! - Trigram matching is inherently case-insensitive (everything is
//!   lowercased before indexing); `SearchOptions::case_sensitive` and
//!   `whole_word` are still enforced exactly by re-checking the actual
//!   line text for every candidate the index returns, so results remain
//!   correct -- the index only narrows *which* lines get that final check.

use std::collections::HashMap;

/// A trigram is 3 Unicode scalar values, stored as `char` triples rather
/// than raw bytes so multi-byte UTF-8 sequences (accents, box-drawing,
/// emoji) shingle correctly instead of splitting mid-codepoint.
type Trigram = [char; 3];

#[derive(Default)]
pub struct TrigramIndex {
    postings: HashMap<Trigram, Vec<u64>>,
    indexed_line_count: u64,
}

impl TrigramIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn indexed_line_count(&self) -> u64 {
        self.indexed_line_count
    }

    /// Adds one line's trigrams to the index. Must be called with
    /// monotonically increasing `line_id` (matches how `VirtualBuffer`
    /// assigns ids) so postings lists stay sorted without an extra sort
    /// pass.
    pub fn index_line(&mut self, line_id: u64, text: &str) {
        for trigram in trigrams_of(text) {
            let postings = self.postings.entry(trigram).or_default();
            if postings.last() != Some(&line_id) {
                postings.push(line_id);
            }
        }
        self.indexed_line_count = self.indexed_line_count.max(line_id + 1);
    }

    /// Returns the set of line ids that contain every trigram in `query`,
    /// i.e. the *candidate* lines a real substring check should be run
    /// against -- or `None` if the query is too short to have any
    /// trigrams (fewer than 3 characters), meaning the caller should fall
    /// back to a full scan since the index can't help.
    pub fn candidates(&self, query: &str) -> Option<Vec<u64>> {
        let query_trigrams: Vec<Trigram> = trigrams_of(query).collect();
        if query_trigrams.is_empty() {
            return None;
        }

        let mut postings_lists: Vec<&Vec<u64>> = Vec::with_capacity(query_trigrams.len());
        for t in &query_trigrams {
            match self.postings.get(t) {
                Some(list) => postings_lists.push(list),
                // A required trigram doesn't exist anywhere in the index
                // at all -> the query cannot match any indexed line.
                None => return Some(Vec::new()),
            }
        }
        // Intersect starting from the shortest list -- classic
        // smallest-first merge to minimize comparisons.
        postings_lists.sort_by_key(|l| l.len());
        let mut result = postings_lists[0].clone();
        for list in &postings_lists[1..] {
            result = intersect_sorted(&result, list);
            if result.is_empty() {
                break;
            }
        }
        Some(result)
    }
}

fn trigrams_of(text: &str) -> impl Iterator<Item = Trigram> + '_ {
    let chars: Vec<char> = text.to_lowercase().chars().collect();
    (0..chars.len().saturating_sub(2)).map(move |i| [chars[i], chars[i + 1], chars[i + 2]])
}

fn intersect_sorted(a: &[u64], b: &[u64]) -> Vec<u64> {
    let mut out = Vec::with_capacity(a.len().min(b.len()));
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_line_containing_query() {
        let mut idx = TrigramIndex::new();
        idx.index_line(0, "the quick brown fox");
        idx.index_line(1, "lazy dog sleeps");
        idx.index_line(2, "another brown animal");

        let candidates = idx.candidates("brown").unwrap();
        assert_eq!(candidates, vec![0, 2]);
    }

    #[test]
    fn short_query_returns_none_signal_full_scan() {
        let mut idx = TrigramIndex::new();
        idx.index_line(0, "hi");
        assert!(idx.candidates("a").is_none());
        assert!(idx.candidates("ab").is_none());
    }

    #[test]
    fn nonexistent_trigram_yields_empty_not_panic() {
        let mut idx = TrigramIndex::new();
        idx.index_line(0, "hello world");
        assert_eq!(idx.candidates("zzz").unwrap(), Vec::<u64>::new());
    }

    #[test]
    fn is_case_insensitive_at_index_level() {
        let mut idx = TrigramIndex::new();
        idx.index_line(0, "HELLO World");
        assert_eq!(idx.candidates("hello").unwrap(), vec![0]);
        assert_eq!(idx.candidates("WORLD").unwrap(), vec![0]);
    }
}
