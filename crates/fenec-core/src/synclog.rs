//! The sync log: where a durable write is made durable without the file
//! growing under its fsync.
//!
//! The database's file only ever grows, and on Linux's ext4 an fsync of a
//! file whose length changed commits the file system's journal as well --
//! the length is what reads the new bytes back. In Docker's VM a 300-byte
//! append and its `fdatasync` took 356 us at the median, the same bytes
//! written into a file already that long and synced 65 (`ycsb/fsync.txt`),
//! which is why PostgreSQL writes its WAL into segments it filled with
//! zeros first. So a sync writes what the file took since the last one into
//! this log too -- a file of fixed size beside the database
//! (`<file>.fenec.sync`), written in place and never grown -- and syncs the
//! log alone. The file itself is synced only when the log is full, or a
//! sync holds more than an entry takes: once every 256 KB of writes.
//!
//! An open applies the log to the file before it reads it ([`recover`]):
//! each entry is the bytes the file holds at a place, written there again
//! if the file lost them. An entry carries a hash, so one cut short by a
//! crash ends the log; a generation, so the entries of an earlier pass of
//! the log are not taken for this one's; and the log's header names where
//! the file stood synced when the generation began and a hash of the bytes
//! before that, so a log is applied only to the file it was written beside
//! -- a file put in its place by a `compact` is not.
//!
//! On macOS `F_FULLFSYNC` flushes the drive whatever was written, 3.9 ms
//! either way, so the log is used only where it pays (`ENABLED`).

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

/// Where the log pays: an fsync of a grown file commits the journal.
pub(crate) const ENABLED: bool = cfg!(any(target_os = "linux", target_os = "android"));

/// The header's room: its own block, apart from the entries'.
const HEAD: u64 = 4096;
/// The entries' room. The file is synced once a pass: with YCSB's 1 KB
/// records, one update in about 230 waits for it.
const ROOM: u64 = 256 << 10;
/// The largest entry: a sync holding more fsyncs the file, whose journal
/// commit is then a small part of the bytes' own time.
pub(crate) const ENTRY_MAX: usize = 64 << 10;
/// What the fingerprint of the file's synced bytes reads.
const PRINT: u64 = 4096;

const LOG_MAGIC: &[u8; 8] = b"FENECSYN";
const ENTRY_MAGIC: u32 = 0x4E59_5346;
const ENTRY_HEAD: usize = 32;

/// The log beside `main`.
pub(crate) fn path_for(main: &Path) -> PathBuf {
    main.with_extension("fenec.sync")
}

/// A 64-bit hash, eight bytes a step: what tells an entry written whole
/// from one a crash cut short, where the rest is zeros or an older pass's
/// bytes.
fn hash(seed: u64, bytes: &[u8]) -> u64 {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut h = seed ^ (bytes.len() as u64).wrapping_mul(0xA076_1D64_78BD_642F);
    let (words, rest) = bytes.as_chunks::<8>();
    for w in words {
        h = (h ^ u64::from_le_bytes(*w)).wrapping_mul(K).rotate_left(29);
    }
    let mut last = [0u8; 8];
    last[..rest.len()].copy_from_slice(rest);
    h = (h ^ u64::from_le_bytes(last)).wrapping_mul(K);
    // murmur3's finaliser: every bit of the state reaches every bit out.
    h ^= h >> 33;
    h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    h ^= h >> 33;
    h = h.wrapping_mul(0xC4CE_B9FE_1A85_EC53);
    h ^ (h >> 33)
}

/// The hash of the file's bytes just before `base`: what names the file a
/// generation was written beside.
fn fingerprint(main: &File, base: u64) -> io::Result<u64> {
    let from = base.saturating_sub(PRINT);
    let mut buf = vec![0u8; (base - from) as usize];
    main.read_exact_at(&mut buf, from)?;
    Ok(hash(base, &buf))
}

struct Header {
    generation: u64,
    base: u64,
    print: u64,
}

impl Header {
    fn encode(&self) -> [u8; 48] {
        let mut b = [0u8; 48];
        b[..8].copy_from_slice(LOG_MAGIC);
        b[8..12].copy_from_slice(&1u32.to_le_bytes());
        b[16..24].copy_from_slice(&self.generation.to_le_bytes());
        b[24..32].copy_from_slice(&self.base.to_le_bytes());
        b[32..40].copy_from_slice(&self.print.to_le_bytes());
        let h = hash(0, &b[..40]);
        b[40..48].copy_from_slice(&h.to_le_bytes());
        b
    }

    fn decode(b: &[u8]) -> Option<Header> {
        let b = b.get(..48)?;
        let word = |at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
        if &b[..8] != LOG_MAGIC || b[8..12] != 1u32.to_le_bytes() || word(40) != hash(0, &b[..40])
        {
            return None;
        }
        Some(Header {
            generation: word(16),
            base: word(24),
            print: word(32),
        })
    }
}

fn entry_hash(generation: u64, at: u64, bytes: &[u8]) -> u64 {
    hash(generation ^ at.rotate_left(17), bytes)
}

/// An entry at the front of `log`, if one of `generation` starting at `at`
/// is there whole: its bytes.
fn entry(log: &[u8], generation: u64, at: u64) -> Option<&[u8]> {
    let head = log.get(..ENTRY_HEAD)?;
    let u32_at = |i: usize| u32::from_le_bytes(head[i..i + 4].try_into().unwrap());
    let u64_at = |i: usize| u64::from_le_bytes(head[i..i + 8].try_into().unwrap());
    if u32_at(0) != ENTRY_MAGIC || u64_at(8) != generation || u64_at(16) != at {
        return None;
    }
    let len = u32_at(4) as usize;
    let bytes = log.get(ENTRY_HEAD..ENTRY_HEAD + len)?;
    (u64_at(24) == entry_hash(generation, at, bytes)).then_some(bytes)
}

/// Puts back into `main` what the log beside it holds and the file lost:
/// run before the file is read. A log that does not name this file, or
/// none, changes nothing. The file is synced when anything was written.
pub(crate) fn recover(main: &File, log_path: &Path) -> io::Result<()> {
    let log = match std::fs::read(log_path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let Some(h) = Header::decode(&log) else {
        return Ok(());
    };
    let len = main.metadata()?.len();
    if len < h.base || fingerprint(main, h.base)? != h.print {
        return Ok(());
    }
    let (mut at, mut pos, mut wrote) = (h.base, HEAD as usize, false);
    let mut held = Vec::new();
    while let Some(bytes) = log.get(pos..).and_then(|l| entry(l, h.generation, at)) {
        let end = at + bytes.len() as u64;
        let same = end <= len && {
            held.resize(bytes.len(), 0);
            main.read_exact_at(&mut held, at)?;
            held == bytes
        };
        if !same {
            main.write_all_at(bytes, at)?;
            wrote = true;
        }
        at = end;
        pos += ENTRY_HEAD + bytes.len();
    }
    if wrote {
        main.sync_data()?;
    }
    Ok(())
}

/// The log as a sink writes it.
pub(crate) struct SyncLog {
    file: File,
    generation: u64,
    /// Where the next entry goes, and the place in the database's file its
    /// bytes start at.
    pos: u64,
    at: u64,
    /// A generation begun and not yet written: it goes out with the first
    /// entry, under the same fsync.
    begun: Option<[u8; 48]>,
}

impl SyncLog {
    /// The log at `path`, made its full size -- written, not left sparse,
    /// so that writing it never changes what the file system holds of it
    /// -- with no generation begun.
    pub(crate) fn open(path: &Path) -> io::Result<SyncLog> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let size = HEAD + ROOM;
        let mut head = [0u8; 48];
        let had = file.metadata()?.len();
        if had < size {
            file.write_all_at(&vec![0u8; (size - had) as usize], had)?;
            file.sync_all()?;
            if let Some(dir) = path.parent() {
                let dir = if dir.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    dir
                };
                File::open(dir)?.sync_all()?;
            }
        }
        let _ = file.read_exact_at(&mut head, 0);
        file.flush()?;
        // Another generation than any entry the file holds.
        let generation = match Header::decode(&head) {
            Some(h) => h.generation.wrapping_add(1),
            None => std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(1, |d| d.as_nanos() as u64),
        };
        Ok(SyncLog {
            file,
            generation,
            pos: HEAD,
            at: 0,
            begun: None,
        })
    }

    /// Begins a generation over `main` synced up to `base`: the entries
    /// after go on from there.
    pub(crate) fn begin(&mut self, main: &File, base: u64) -> io::Result<()> {
        self.generation = self.generation.wrapping_add(1);
        let print = fingerprint(main, base)?;
        self.begun = Some(
            Header {
                generation: self.generation,
                base,
                print,
            }
            .encode(),
        );
        self.pos = HEAD;
        self.at = base;
        Ok(())
    }

    /// Whether an entry of `n` bytes starting at `at` goes into this
    /// generation: right after the last, and with room for it.
    pub(crate) fn takes(&self, at: u64, n: usize) -> bool {
        n <= ENTRY_MAX
            && at == self.at
            && self.pos + (ENTRY_HEAD + n) as u64 <= HEAD + ROOM
            && (self.begun.is_some() || self.pos > HEAD)
    }

    /// Writes `bytes`, the database file's from `at` on, and syncs the log:
    /// once this returns they survive a crash of the machine.
    pub(crate) fn append(&mut self, at: u64, bytes: &[u8]) -> io::Result<()> {
        if let Some(head) = self.begun.take() {
            self.file.write_all_at(&head, 0)?;
        }
        let mut e = Vec::with_capacity(ENTRY_HEAD + bytes.len());
        e.extend_from_slice(&ENTRY_MAGIC.to_le_bytes());
        e.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        e.extend_from_slice(&self.generation.to_le_bytes());
        e.extend_from_slice(&at.to_le_bytes());
        e.extend_from_slice(&entry_hash(self.generation, at, bytes).to_le_bytes());
        e.extend_from_slice(bytes);
        self.file.write_all_at(&e, self.pos)?;
        self.file.sync_data()?;
        self.pos += e.len() as u64;
        self.at = at + bytes.len() as u64;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fenecdb-synclog-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn rw(path: &Path) -> File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .unwrap()
    }

    /// The entries a log holds are put back into a file that lost them,
    /// in order, and an entry cut short ends the log there.
    #[test]
    fn recover_puts_back_what_the_file_lost() {
        let d = dir("lost");
        let main_path = d.join("db.fenec");
        let main = rw(&main_path);
        main.write_all_at(b"0123456789", 0).unwrap();
        let mut log = SyncLog::open(&path_for(&main_path)).unwrap();
        log.begin(&main, 10).unwrap();
        assert!(log.takes(10, 3));
        log.append(10, b"abc").unwrap();
        assert!(!log.takes(12, 3), "an entry must follow the last");
        log.append(13, b"defg").unwrap();
        log.append(17, b"hij").unwrap();
        // The machine went down before the file's bytes reached the disk,
        // and the last entry is cut short.
        main.set_len(10).unwrap();
        let mut raw = std::fs::read(path_for(&main_path)).unwrap();
        let last = HEAD as usize + 2 * ENTRY_HEAD + 7;
        raw[last + ENTRY_HEAD + 1] ^= 0xFF;
        std::fs::write(path_for(&main_path), &raw).unwrap();
        recover(&main, &path_for(&main_path)).unwrap();
        assert_eq!(std::fs::read(&main_path).unwrap(), b"0123456789abcdefg");
        // Run again over a file that holds them, it changes nothing.
        recover(&main, &path_for(&main_path)).unwrap();
        assert_eq!(std::fs::read(&main_path).unwrap(), b"0123456789abcdefg");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A log is applied only to the file it was written beside: one whose
    /// bytes before the generation's start are others is left alone, and
    /// so is one shorter than that start.
    #[test]
    fn a_log_names_its_file() {
        let d = dir("names");
        let main_path = d.join("db.fenec");
        let main = rw(&main_path);
        main.write_all_at(b"the file as synced", 0).unwrap();
        let mut log = SyncLog::open(&path_for(&main_path)).unwrap();
        log.begin(&main, 18).unwrap();
        log.append(18, b" and more").unwrap();
        drop(main);
        std::fs::write(&main_path, b"another file, longer than that").unwrap();
        let other = rw(&main_path);
        recover(&other, &path_for(&main_path)).unwrap();
        assert_eq!(
            std::fs::read(&main_path).unwrap(),
            b"another file, longer than that"
        );
        std::fs::write(&main_path, b"short").unwrap();
        recover(&rw(&main_path), &path_for(&main_path)).unwrap();
        assert_eq!(std::fs::read(&main_path).unwrap(), b"short");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A new generation leaves the entries of the one before out, though
    /// their bytes are still in the log past the new ones.
    #[test]
    fn a_generation_ends_the_one_before() {
        let d = dir("gen");
        let main_path = d.join("db.fenec");
        let main = rw(&main_path);
        main.write_all_at(b"base", 0).unwrap();
        let mut log = SyncLog::open(&path_for(&main_path)).unwrap();
        log.begin(&main, 4).unwrap();
        log.append(4, b"-one-long-entry").unwrap();
        log.append(19, b"-two").unwrap();
        main.write_all_at(b"-one-long-entry-two", 4).unwrap();
        main.sync_data().unwrap();
        log.begin(&main, 23).unwrap();
        log.append(23, b"!").unwrap();
        drop(log);
        // Reopened, the log starts a generation of its own.
        let log = SyncLog::open(&path_for(&main_path)).unwrap();
        assert!(!log.takes(24, 1), "no generation is begun");
        main.set_len(23).unwrap();
        recover(&main, &path_for(&main_path)).unwrap();
        assert_eq!(std::fs::read(&main_path).unwrap(), b"base-one-long-entry-two!");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The log's room ends a generation: an entry past it is not taken,
    /// and neither is one over the largest.
    #[test]
    fn room_and_size_bound_an_entry() {
        let d = dir("room");
        let main_path = d.join("db.fenec");
        let main = rw(&main_path);
        main.write_all_at(b"x", 0).unwrap();
        let mut log = SyncLog::open(&path_for(&main_path)).unwrap();
        log.begin(&main, 1).unwrap();
        assert!(!log.takes(1, ENTRY_MAX + 1));
        let chunk = vec![7u8; ENTRY_MAX];
        let mut at = 1;
        while log.takes(at, chunk.len()) {
            log.append(at, &chunk).unwrap();
            at += chunk.len() as u64;
        }
        assert!(at > 1 && log.pos + (ENTRY_HEAD + ENTRY_MAX) as u64 > HEAD + ROOM);
        assert_eq!(
            std::fs::metadata(path_for(&main_path)).unwrap().len(),
            HEAD + ROOM
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
