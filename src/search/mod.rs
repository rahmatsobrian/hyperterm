//! Search Engine
//!
//! Implements Ctrl+F search over the full virtual scrollback (RAM + disk),
//! with regex, case-sensitivity, and whole-word options.
//!
//! Plain-text queries are accelerated by an in-memory trigram inverted
//! index (see the `trigram_index` submodule) that narrows the candidate
//! line set before doing a real substring check, so most searches don't
//! need to touch every line. Regex queries still do a sequential scan
//! (see `trigram_index` module docs for why extracting literal trigrams
//! from an arbitrary regex is out of scope here) -- correct, just not
//! accelerated.

pub mod trigram_index;

use anyhow::Result;
use regex::{Regex, RegexBuilder};

use crate::virtual_buffer::VirtualBuffer;

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub query: String,
    pub regex: bool,
    pub case_sensitive: bool,
    pub whole_word: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct SearchMatch {
    pub line_id: u64,
    pub start_col: usize,
    pub end_col: usize,
}

pub struct SearchEngine;

const SCAN_CHUNK: u64 = 4096;

impl SearchEngine {
    /// Sequential scan across the whole buffer, returning every match.
    /// For very large histories, prefer `search_stream` so callers (e.g. the
    /// renderer's "jump to next match") can react to the first hits without
    /// waiting for the entire scan.
    pub fn search_all(vbuf: &mut VirtualBuffer, opts: &SearchOptions) -> Result<Vec<SearchMatch>> {
        let mut out = Vec::new();
        Self::search_stream(vbuf, opts, |m| {
            out.push(m);
            true
        })?;
        Ok(out)
    }

    /// Streaming search: calls `on_match` for each hit in ascending line
    /// order; return `false` from the callback to stop early (e.g. once the
    /// UI has enough results for "jump to next").
    ///
    /// For plain-text (non-regex) queries, this consults
    /// `VirtualBuffer::trigram_index()` first and only visits lines the
    /// index says can possibly match, instead of scanning the whole
    /// buffer -- see `trigram_index` module docs for exactly which
    /// queries get accelerated vs. fall back to a full scan.
    pub fn search_stream(
        vbuf: &mut VirtualBuffer,
        opts: &SearchOptions,
        mut on_match: impl FnMut(SearchMatch) -> bool,
    ) -> Result<()> {
        let matcher = CompiledMatcher::compile(opts)?;

        if !opts.regex {
            if let Some(mut candidates) = vbuf.trigram_index().candidates(&opts.query) {
                candidates.sort_unstable();
                for line_id in candidates {
                    if let Some(line) = vbuf.get_line(line_id) {
                        let text = line.plain_text();
                        for (start, end_col) in matcher.find_all(&text) {
                            let keep_going = on_match(SearchMatch {
                                line_id,
                                start_col: start,
                                end_col,
                            });
                            if !keep_going {
                                return Ok(());
                            }
                        }
                    }
                }
                return Ok(());
            }
            // `candidates()` returned `None`: query too short to have any
            // trigrams (e.g. single character) -- fall through to the
            // full scan below, which is the only correct option there.
        }

        let total = vbuf.total_lines();
        let mut cursor = 0u64;
        while cursor < total {
            let end = (cursor + SCAN_CHUNK).min(total);
            let lines = vbuf.get_range(cursor, end);
            for (offset, line) in lines.iter().enumerate() {
                let line_id = cursor + offset as u64;
                let text = line.plain_text();
                for (start, end_col) in matcher.find_all(&text) {
                    let keep_going = on_match(SearchMatch {
                        line_id,
                        start_col: start,
                        end_col,
                    });
                    if !keep_going {
                        return Ok(());
                    }
                }
            }
            cursor = end;
        }
        Ok(())
    }
}

enum CompiledMatcher {
    Plain {
        needle: String,
        case_sensitive: bool,
        whole_word: bool,
    },
    Regex(Regex),
}

impl CompiledMatcher {
    fn compile(opts: &SearchOptions) -> Result<Self> {
        if opts.regex {
            let mut pattern = opts.query.clone();
            if opts.whole_word {
                pattern = format!(r"\b(?:{pattern})\b");
            }
            let re = RegexBuilder::new(&pattern)
                .case_insensitive(!opts.case_sensitive)
                .build()?;
            Ok(CompiledMatcher::Regex(re))
        } else {
            Ok(CompiledMatcher::Plain {
                needle: opts.query.clone(),
                case_sensitive: opts.case_sensitive,
                whole_word: opts.whole_word,
            })
        }
    }

    fn find_all(&self, text: &str) -> Vec<(usize, usize)> {
        match self {
            CompiledMatcher::Regex(re) => {
                re.find_iter(text).map(|m| (m.start(), m.end())).collect()
            }
            CompiledMatcher::Plain {
                needle,
                case_sensitive,
                whole_word,
            } => {
                if needle.is_empty() {
                    return Vec::new();
                }
                let (hay, pat): (String, String) = if *case_sensitive {
                    (text.to_string(), needle.clone())
                } else {
                    (text.to_lowercase(), needle.to_lowercase())
                };
                let mut out = Vec::new();
                let mut start = 0usize;
                while let Some(pos) = hay[start..].find(&pat) {
                    let abs_start = start + pos;
                    let abs_end = abs_start + pat.len();
                    if !*whole_word || is_whole_word(&hay, abs_start, abs_end) {
                        out.push((abs_start, abs_end));
                    }
                    start = abs_start + pat.len().max(1);
                }
                out
            }
        }
    }
}

fn is_whole_word(hay: &str, start: usize, end: usize) -> bool {
    let before_ok = hay[..start]
        .chars()
        .next_back()
        .map(|c| !c.is_alphanumeric() && c != '_')
        .unwrap_or(true);
    let after_ok = hay[end..]
        .chars()
        .next()
        .map(|c| !c.is_alphanumeric() && c != '_')
        .unwrap_or(true);
    before_ok && after_ok
}
