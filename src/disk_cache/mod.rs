//! Disk Cache
//!
//! Persistent, append-only backing store for terminal scrollback history.
//! This is the bottom layer of the architecture described in the spec:
//!
//! ```text
//! RAM Cache -> Virtual Buffer -> Disk Cache -> Persistent History
//! ```
//!
//! Layout on disk, per session (default: `logs/<session-id>.cache` +
//! `logs/<session-id>.idx`):
//!
//! * `*.cache` -- append-only log of length-prefixed UTF-8 line records:
//!   `[u32 LE length][bytes...]`, repeated. Never rewritten in place, only
//!   appended to, so a crash mid-write can at worst leave one dangling
//!   partial record at EOF (detected & truncated on next open).
//!
//! * `*.idx`   -- fixed-size index records, one per line, `[u64 LE offset][u32 LE length]`
//!   (12 bytes each). Lets us binary-index into `*.cache` in O(1) without
//!   scanning, and lets us recover the line count instantly on restart.
//!
//! Reads use `memmap2` for zero-copy random access once a file is >0 bytes;
//! writes go through buffered, `fsync`-on-flush appends.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use memmap2::Mmap;

const INDEX_RECORD_SIZE: u64 = 12; // u64 offset + u32 length

pub struct DiskCache {
    cache_path: PathBuf,
    index_path: PathBuf,
    cache_writer: BufWriter<File>,
    index_writer: BufWriter<File>,
    /// Current write offset into `*.cache` (i.e. its length).
    write_offset: u64,
    /// Number of lines committed so far.
    line_count: u64,
    /// Read-only mmap of the cache file, refreshed after writes are flushed
    /// and a reader is requested. `None` until first read or if the file is empty.
    mmap: Option<Mmap>,
}

impl DiskCache {
    /// Open (creating if needed) the cache+index pair for a session, recovering
    /// from any previous run so history survives restarts and crashes.
    pub fn open(session_dir: &Path, session_id: &str) -> Result<Self> {
        std::fs::create_dir_all(session_dir)
            .with_context(|| format!("creating cache dir {session_dir:?}"))?;
        let cache_path = session_dir.join(format!("{session_id}.cache"));
        let index_path = session_dir.join(format!("{session_id}.idx"));

        let (write_offset, line_count) = recover(&cache_path, &index_path)?;

        let cache_file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&cache_path)
            .with_context(|| format!("opening {cache_path:?}"))?;
        let index_file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&index_path)
            .with_context(|| format!("opening {index_path:?}"))?;

        tracing::info!(
            target: "hyperterm::disk_cache",
            "opened session cache {:?} ({} lines, {} bytes recovered)",
            cache_path, line_count, write_offset
        );

        Ok(Self {
            cache_path,
            index_path,
            cache_writer: BufWriter::new(cache_file),
            index_writer: BufWriter::new(index_file),
            write_offset,
            line_count,
            mmap: None,
        })
    }

    pub fn line_count(&self) -> u64 {
        self.line_count
    }

    /// Append one line (already-rendered text, e.g. plain UTF-8 or
    /// UTF-8 + embedded SGR-preserving representation) to the cache.
    /// Returns the new line's id (0-indexed).
    pub fn append_line(&mut self, content: &[u8]) -> io::Result<u64> {
        let len = content.len() as u32;
        self.cache_writer.write_u32::<LittleEndian>(len)?;
        self.cache_writer.write_all(content)?;

        self.index_writer
            .write_u64::<LittleEndian>(self.write_offset)?;
        self.index_writer.write_u32::<LittleEndian>(len)?;

        let id = self.line_count;
        self.write_offset += 4 + len as u64;
        self.line_count += 1;
        // Invalidate mmap; will be rebuilt lazily on next read.
        self.mmap = None;
        Ok(id)
    }

    /// Flush buffered writers to the OS (does not force fsync every call --
    /// call `sync()` at safe points, e.g. every N lines or on idle, per the
    /// "Async I/O" performance goal so the render/input path never blocks
    /// on disk).
    pub fn flush(&mut self) -> io::Result<()> {
        self.cache_writer.flush()?;
        self.index_writer.flush()?;
        Ok(())
    }

    /// Force fsync of both files. Use sparingly (e.g. on clean shutdown or
    /// every few seconds from a background task), never on the hot input path.
    pub fn sync(&mut self) -> io::Result<()> {
        self.flush()?;
        self.cache_writer.get_ref().sync_data()?;
        self.index_writer.get_ref().sync_data()?;
        Ok(())
    }

    /// Random-access read of a single historical line by id.
    pub fn read_line(&mut self, line_id: u64) -> io::Result<Vec<u8>> {
        if line_id >= self.line_count {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "line_id out of range",
            ));
        }
        self.flush()?;
        let (offset, len) = self.read_index_entry(line_id)?;
        self.ensure_mmap()?;
        let mmap = self.mmap.as_ref().expect("mmap ensured above");
        let start = (offset + 4) as usize; // skip the length prefix
        let end = start + len as usize;
        if end > mmap.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "cache file truncated",
            ));
        }
        Ok(mmap[start..end].to_vec())
    }

    /// Reads a contiguous range of lines efficiently (used by the virtual
    /// buffer when the viewport scrolls into disk-resident history and by
    /// the search engine for sequential scanning).
    pub fn read_range(&mut self, start_id: u64, end_id_exclusive: u64) -> io::Result<Vec<Vec<u8>>> {
        let mut out = Vec::with_capacity((end_id_exclusive.saturating_sub(start_id)) as usize);
        for id in start_id..end_id_exclusive.min(self.line_count) {
            out.push(self.read_line(id)?);
        }
        Ok(out)
    }

    fn read_index_entry(&mut self, line_id: u64) -> io::Result<(u64, u32)> {
        let mut f = File::open(&self.index_path)?;
        f.seek(SeekFrom::Start(line_id * INDEX_RECORD_SIZE))?;
        let offset = f.read_u64::<LittleEndian>()?;
        let len = f.read_u32::<LittleEndian>()?;
        Ok((offset, len))
    }

    fn ensure_mmap(&mut self) -> io::Result<()> {
        if self.mmap.is_some() {
            return Ok(());
        }
        let file = File::open(&self.cache_path)?;
        // SAFETY: the cache file is append-only and owned by this process;
        // we never truncate/shrink it, only grow, and we re-open a fresh
        // mmap (`self.mmap = None`) after every write.
        let mmap = unsafe { Mmap::map(&file)? };
        self.mmap = Some(mmap);
        Ok(())
    }

    pub fn cache_path(&self) -> &Path {
        &self.cache_path
    }
}

/// Recovers `(write_offset, line_count)` from an existing cache/index pair,
/// truncating a dangling partial record left by a crash mid-write so the
/// cache file and index file agree with each other again.
fn recover(cache_path: &Path, index_path: &Path) -> Result<(u64, u64)> {
    if !cache_path.exists() || !index_path.exists() {
        return Ok((0, 0));
    }

    let index_len = std::fs::metadata(index_path)?.len();
    let mut line_count = index_len / INDEX_RECORD_SIZE;
    // Index file itself could have a dangling partial record.
    let index_remainder = index_len % INDEX_RECORD_SIZE;

    let mut idx_file = File::open(index_path)?;
    if line_count == 0 {
        return Ok((0, 0));
    }

    // Read the last complete index entry to know where the cache file
    // should end.
    idx_file.seek(SeekFrom::Start((line_count - 1) * INDEX_RECORD_SIZE))?;
    let last_offset = idx_file.read_u64::<LittleEndian>()?;
    let last_len = idx_file.read_u32::<LittleEndian>()?;
    let expected_cache_len = last_offset + 4 + last_len as u64;

    let actual_cache_len = std::fs::metadata(cache_path)?.len();

    if actual_cache_len < expected_cache_len {
        // The last record's payload never fully hit disk. Drop it from the
        // index (both in-file and in our returned count) and re-derive the
        // write offset from the second-to-last, fully-committed record.
        tracing::warn!(
            target: "hyperterm::disk_cache",
            "detected truncated write (cache {} bytes < expected {} bytes); dropping last line",
            actual_cache_len, expected_cache_len
        );
        line_count -= 1;
        if line_count == 0 {
            return Ok((0, 0));
        }
        idx_file.seek(SeekFrom::Start((line_count - 1) * INDEX_RECORD_SIZE))?;
        let prev_offset = idx_file.read_u64::<LittleEndian>()?;
        let prev_len = idx_file.read_u32::<LittleEndian>()?;
        let recovered_offset = prev_offset + 4 + prev_len as u64;
        truncate_index_file(index_path, line_count)?;
        truncate_cache_file(cache_path, recovered_offset)?;
        return Ok((recovered_offset, line_count));
    }

    if index_remainder != 0 {
        tracing::warn!(
            target: "hyperterm::disk_cache",
            "index file had {} dangling bytes, truncating to {} whole records",
            index_remainder, line_count
        );
        truncate_index_file(index_path, line_count)?;
    }

    Ok((expected_cache_len, line_count))
}

fn truncate_index_file(path: &Path, keep_records: u64) -> Result<()> {
    let f = OpenOptions::new().write(true).open(path)?;
    f.set_len(keep_records * INDEX_RECORD_SIZE)?;
    Ok(())
}

fn truncate_cache_file(path: &Path, keep_bytes: u64) -> Result<()> {
    let f = OpenOptions::new().write(true).open(path)?;
    f.set_len(keep_bytes)?;
    Ok(())
}

/// Reads the whole index file into `(offset, len)` pairs. Used by
/// `search::indexed` to build an in-RAM search index without re-scanning
/// the (potentially huge) cache file.
pub fn read_all_index_entries(index_path: &Path) -> io::Result<Vec<(u64, u32)>> {
    let mut f = File::open(index_path)?;
    let len = f.metadata()?.len();
    let count = (len / INDEX_RECORD_SIZE) as usize;
    let mut out = Vec::with_capacity(count);
    let mut buf = vec![0u8; len as usize];
    f.read_exact(&mut buf)?;
    let mut cursor = io::Cursor::new(buf);
    for _ in 0..count {
        let offset = cursor.read_u64::<LittleEndian>()?;
        let l = cursor.read_u32::<LittleEndian>()?;
        out.push((offset, l));
    }
    Ok(out)
}
