//! Virtual Scrollback Buffer
//!
//! ```text
//! RAM Cache -> Virtual Buffer -> Disk Cache -> Persistent History
//! ```
//!
//! Lines are NEVER deleted just because they scroll out of the viewport.
//! The most recently scrolled-off `ram_capacity` lines stay fully in RAM
//! (hot path, zero I/O for typical scrollback usage). Once that capacity
//! is exceeded, the oldest RAM line is serialized (see `cell::Line::encode`)
//! and appended to the on-disk cache, which is append-only and persists
//! across restarts and crashes -- so "unlimited" history really does mean
//! tens of millions of lines bounded only by disk space, not RAM.
//!
//! Line IDs are a single global, monotonically increasing `u64` counter.
//! Lines `[0, disk.line_count())` live on disk; lines
//! `[disk.line_count(), total_lines)` live in RAM. This invariant holds
//! because eviction always happens oldest-first, in order.

pub mod cell;
pub mod reflow;

use std::collections::VecDeque;
use std::path::Path;

use anyhow::Result;

use crate::disk_cache::DiskCache;
use crate::search::trigram_index::TrigramIndex;
pub use cell::{Attrs, Cell, Color, Line};

pub struct VirtualBuffer {
    ram_capacity: usize,
    ram: VecDeque<Line>,
    total_lines: u64,
    disk: DiskCache,
    /// Lines appended since the last `disk.sync()`. Used to fsync
    /// periodically instead of on every single line (keeps the hot path
    /// non-blocking, per the "Async I/O" performance goal).
    dirty_since_sync: u32,
    trigram_index: TrigramIndex,
}

pub const DEFAULT_SYNC_EVERY_N_LINES: u32 = 500;

/// If a session is reopened with more than this many lines of
/// already-persisted history, we skip the eager index rebuild on open (it
/// would mean synchronously reading and re-indexing potentially tens of
/// millions of lines before the app can even show a prompt) and only index
/// lines pushed from this point forward. Search over the pre-existing tail
/// falls back to the sequential scan in that case -- still correct, just
/// not accelerated. See `search::trigram_index` module docs.
pub const REBUILD_INDEX_LINE_CAP: u64 = 500_000;

impl VirtualBuffer {
    pub fn open(cache_dir: &Path, session_id: &str, ram_capacity: usize) -> Result<Self> {
        let mut disk = DiskCache::open(cache_dir, session_id)?;
        let total_lines = disk.line_count();
        tracing::info!(
            target: "hyperterm::virtual_buffer",
            "virtual buffer for session '{}' opened with {} lines of persisted history (ram_capacity={})",
            session_id, total_lines, ram_capacity
        );

        let mut trigram_index = TrigramIndex::new();
        if total_lines > 0 && total_lines <= REBUILD_INDEX_LINE_CAP {
            let start = std::time::Instant::now();
            if let Ok(raw_lines) = disk.read_range(0, total_lines) {
                for (id, raw) in raw_lines.into_iter().enumerate() {
                    if let Ok(line) = Line::decode(&raw) {
                        trigram_index.index_line(id as u64, &line.plain_text());
                    }
                }
            }
            tracing::info!(
                target: "hyperterm::virtual_buffer",
                "rebuilt search index for {} pre-existing lines in {:?}",
                total_lines, start.elapsed()
            );
        } else if total_lines > REBUILD_INDEX_LINE_CAP {
            tracing::warn!(
                target: "hyperterm::virtual_buffer",
                "{} pre-existing lines exceeds REBUILD_INDEX_LINE_CAP ({}); search index will only \
                 cover lines from this point forward, older history falls back to sequential scan",
                total_lines, REBUILD_INDEX_LINE_CAP
            );
        }

        Ok(Self {
            ram_capacity,
            ram: VecDeque::with_capacity(ram_capacity.min(1 << 20)),
            total_lines,
            disk,
            dirty_since_sync: 0,
            trigram_index,
        })
    }

    pub fn total_lines(&self) -> u64 {
        self.total_lines
    }

    /// Push a newly-scrolled-off line into history. O(1) amortized;
    /// touches disk only every `ram_capacity`-th call in steady state.
    pub fn push_line(&mut self, line: Line) {
        let line_id = self.total_lines;
        self.trigram_index.index_line(line_id, &line.plain_text());

        self.ram.push_back(line);
        self.total_lines += 1;

        if self.ram.len() > self.ram_capacity {
            if let Some(evicted) = self.ram.pop_front() {
                let encoded = evicted.encode();
                if let Err(e) = self.disk.append_line(&encoded) {
                    tracing::error!(
                        target: "hyperterm::virtual_buffer",
                        "failed to spill line to disk cache: {e}"
                    );
                }
                self.dirty_since_sync += 1;
                if self.dirty_since_sync >= DEFAULT_SYNC_EVERY_N_LINES {
                    if let Err(e) = self.disk.flush() {
                        tracing::warn!(target: "hyperterm::virtual_buffer", "flush failed: {e}");
                    }
                    self.dirty_since_sync = 0;
                }
            }
        }
    }

    /// Force all buffered writes to disk (call on graceful shutdown or
    /// idle-detection, never on the input hot path).
    pub fn checkpoint(&mut self) -> Result<()> {
        self.disk.sync()?;
        Ok(())
    }

    fn disk_line_count(&self) -> u64 {
        self.total_lines - self.ram.len() as u64
    }

    /// Random access to a single line, transparently pulling from RAM or
    /// disk depending on where it currently lives.
    pub fn get_line(&mut self, id: u64) -> Option<Line> {
        if id >= self.total_lines {
            return None;
        }
        let disk_count = self.disk_line_count();
        if id < disk_count {
            match self.disk.read_line(id) {
                Ok(bytes) => Line::decode(&bytes).ok(),
                Err(e) => {
                    tracing::error!(target: "hyperterm::virtual_buffer", "disk read failed for line {id}: {e}");
                    None
                }
            }
        } else {
            self.ram.get((id - disk_count) as usize).cloned()
        }
    }

    /// Efficient range fetch for rendering a viewport or feeding the
    /// search engine, minimizing disk round-trips by batching the disk
    /// portion of the range in one call.
    pub fn get_range(&mut self, start: u64, end_exclusive: u64) -> Vec<Line> {
        let start = start.min(self.total_lines);
        let end = end_exclusive.min(self.total_lines);
        if start >= end {
            return Vec::new();
        }
        let disk_count = self.disk_line_count();
        let mut out = Vec::with_capacity((end - start) as usize);

        if start < disk_count {
            let disk_end = end.min(disk_count);
            match self.disk.read_range(start, disk_end) {
                Ok(raw_lines) => {
                    for raw in raw_lines {
                        out.push(Line::decode(&raw).unwrap_or_default());
                    }
                }
                Err(e) => {
                    tracing::error!(target: "hyperterm::virtual_buffer", "disk range read failed: {e}");
                }
            }
        }
        if end > disk_count {
            let ram_start = start.max(disk_count) - disk_count;
            let ram_end = end - disk_count;
            for i in ram_start..ram_end {
                if let Some(l) = self.ram.get(i as usize) {
                    out.push(l.clone());
                }
            }
        }
        out
    }

    pub fn cache_file_path(&self) -> &Path {
        self.disk.cache_path()
    }

    /// Exposes the trigram index for `SearchEngine` to query. Returns
    /// `None`-equivalent behavior (empty candidate narrowing) transparently
    /// via `TrigramIndex::candidates` when a query is too short or when the
    /// pre-existing-history size cap was exceeded on open (see
    /// `REBUILD_INDEX_LINE_CAP`) -- callers don't need special-case logic.
    pub fn trigram_index(&self) -> &TrigramIndex {
        &self.trigram_index
    }

    /// Fetches (and reflow-adjusts, see `reflow` module) a window of
    /// history ending at raw line id `end_id_exclusive`, sized to yield at
    /// least `want_rows` *visual* rows once reflowed to `target_width` --
    /// growing the raw fetch window geometrically if the content reflows
    /// to fewer visual rows than raw rows (i.e. `target_width` is wider
    /// than history was originally written at). Used by the scrollback
    /// view in `main.rs`.
    ///
    /// Returns up to `want_rows` lines (fewer only if history itself is
    /// shorter than that near the very start of the buffer).
    pub fn history_window(&mut self, target_width: usize, want_rows: usize, end_id_exclusive: u64) -> Vec<Line> {
        if want_rows == 0 || end_id_exclusive == 0 {
            return Vec::new();
        }
        let mut window_size: u64 = (want_rows as u64) * 2 + 4;
        loop {
            let start_id = end_id_exclusive.saturating_sub(window_size);
            let raw = self.get_range(start_id, end_id_exclusive);
            let reflowed = reflow::reflow(&raw, target_width);
            if reflowed.len() >= want_rows || start_id == 0 {
                let take_from = reflowed.len().saturating_sub(want_rows);
                return reflowed[take_from..].to_vec();
            }
            window_size = window_size.saturating_mul(2);
        }
    }
}
