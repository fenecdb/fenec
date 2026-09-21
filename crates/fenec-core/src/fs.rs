//! Native file persistence layer.
//!
//! The file's contents are in *the same format* as the engine's in-memory
//! byte image. A write = append to the file. Open = a single replay pass.
//! No separate WAL + data file pair, no checkpoint, no page cache.

use crate::engine::{Database, Durability, Sink, MAGIC};
use crate::error::Result;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

/// Write buffer. One `write` call per document meant one syscall per
/// document; that was the dominant cost during bulk loading.
const WRITE_BUF: usize = 1 << 20;

/// Appends collect in memory and reach the file only in `sync`, in the
/// durability `flush` hands out, or when they outgrow `WRITE_BUF`.
///
/// Not a `BufWriter` over the file. `flush` runs under the database's
/// exclusive lock, and a `write` there waits for the disk whenever an fsync
/// of the same file is under way on another thread -- on macOS's APFS, 27%
/// of them took 0.5-3.3 ms with eight writers under `--sync always`, the
/// lock held all the while. The bytes are therefore written by whoever
/// runs the fsync, outside the lock.
///
/// That is also where writes arriving together are made durable together.
/// Each durability knows how many bytes had been appended when it was
/// handed out; the first to take the disk writes and fsyncs everything
/// pending, and the ones queued behind it find their bytes already durable
/// and return without an fsync of their own.
pub struct FileSink {
    /// The appends not yet written.
    pending: Arc<Mutex<Vec<u8>>>,
    /// Bytes appended since the file was opened or rewritten.
    appended: u64,
    /// Held for every write and fsync, and taken before `pending` whenever
    /// bytes leave it, so they reach the file in the order they came.
    disk: Arc<Mutex<Disk>>,
    path: PathBuf,
}

struct Disk {
    file: File,
    /// Bytes appended that have been written, and that have been fsynced.
    written: u64,
    synced: u64,
    /// fsyncs run, for the test that they are shared.
    #[cfg(test)]
    fsyncs: usize,
    /// A failed write or fsync. The kernel may have dropped the pages it
    /// could not write, so nothing is reported durable after one.
    failed: Option<crate::error::Error>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Disk {
    /// Writes whatever is pending.
    fn write_pending(&mut self, pending: &Mutex<Vec<u8>>) -> Result<()> {
        if let Some(e) = &self.failed {
            return Err(e.clone());
        }
        let bytes = std::mem::take(&mut *lock(pending));
        if let Err(e) = self.file.write_all(&bytes) {
            self.failed = Some(e.into());
            return Err(self.failed.clone().unwrap());
        }
        self.written += bytes.len() as u64;
        Ok(())
    }

    /// Makes the first `upto` appended bytes durable, unless they are.
    fn sync(&mut self, pending: &Mutex<Vec<u8>>, upto: u64) -> Result<()> {
        if let Some(e) = &self.failed {
            return Err(e.clone());
        }
        if self.synced >= upto {
            return Ok(());
        }
        self.write_pending(pending)?;
        if let Err(e) = self.file.sync_data() {
            self.failed = Some(e.into());
            return Err(self.failed.clone().unwrap());
        }
        #[cfg(test)]
        {
            self.fsyncs += 1;
        }
        self.synced = self.written;
        Ok(())
    }
}

impl FileSink {
    pub fn open(path: impl AsRef<Path>) -> Result<(FileSink, Vec<u8>)> {
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)?;
        let mut existing = Vec::new();
        file.read_to_end(&mut existing)?;
        if existing.is_empty() {
            file.write_all(&MAGIC[..])?;
            file.flush()?;
            existing = Vec::from(&MAGIC[..]);
        }
        file.seek(SeekFrom::End(0))?;
        Ok((
            FileSink {
                pending: Arc::new(Mutex::new(Vec::new())),
                appended: 0,
                disk: Arc::new(Mutex::new(Disk {
                    file,
                    written: 0,
                    synced: 0,
                    #[cfg(test)]
                    fsyncs: 0,
                    failed: None,
                })),
                path,
            },
            existing,
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Sink for FileSink {
    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        self.appended += bytes.len() as u64;
        let full = {
            let mut pending = lock(&self.pending);
            pending.extend_from_slice(bytes);
            pending.len() >= WRITE_BUF
        };
        if full {
            lock(&self.disk).write_pending(&self.pending)?;
        }
        Ok(())
    }
    fn rewrite(&mut self, bytes: &[u8]) -> Result<()> {
        let mut disk = lock(&self.disk);
        if let Some(e) = &disk.failed {
            return Err(e.clone());
        }
        // Whatever is pending is in the image already: it would be written
        // twice.
        lock(&self.pending).clear();
        // Atomic replace: write to a side file first, then rename.
        let tmp = self.path.with_extension("fenec.compacting");
        {
            let mut f = BufWriter::with_capacity(WRITE_BUF, File::create(&tmp)?);
            f.write_all(bytes)?;
            f.into_inner()
                .map_err(|e| crate::error::Error::Io(e.to_string()))?
                .sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        let mut f = OpenOptions::new().read(true).write(true).open(&self.path)?;
        f.seek(SeekFrom::End(0))?;
        // Everything appended so far is in the image, and the image is on
        // disk.
        disk.file = f;
        disk.written = self.appended;
        disk.synced = self.appended;
        Ok(())
    }
    /// Writes the pending appends and pushes them to disk. Because of the
    /// buffer, the last writes can be lost if the process dies before `sync`
    /// is called; fenecdb never fsyncs every write anyway -- this buffer
    /// extends that model.
    fn sync(&mut self) -> Result<()> {
        lock(&self.disk).sync(&self.pending, self.appended)
    }
    /// Touches no file: the write and the fsync are both the durability's,
    /// which runs without the database's lock.
    fn flush(&mut self) -> Result<Option<Durability>> {
        let (disk, pending, upto) = (
            Arc::clone(&self.disk),
            Arc::clone(&self.pending),
            self.appended,
        );
        Ok(Some(Box::new(move || lock(&disk).sync(&pending, upto))))
    }
}

impl Drop for FileSink {
    fn drop(&mut self) {
        // Do not let buffered leftovers vanish silently.
        let _ = lock(&self.disk).write_pending(&self.pending);
    }
}

/// Opens a fenecdb file (creating it when missing) and loads its contents.
pub fn open(path: impl AsRef<Path>) -> Result<Database> {
    let (sink, existing) = FileSink::open(path)?;
    let mut db = Database::with_sink(Box::new(sink));
    if existing.len() > MAGIC.len() {
        db.load(&existing)?;
    }
    Ok(db)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A durability finds its bytes on disk when a later one got there
    /// first, and runs no fsync of its own; what it wrote is the file's
    /// tail, in the order it was appended, rewrite or not.
    #[test]
    fn durabilities_share_an_fsync_and_keep_the_order() {
        let dir = std::env::temp_dir().join(format!("fenecdb-fs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("share.fenec");
        let _ = std::fs::remove_file(&path);
        let (mut sink, _) = FileSink::open(&path).unwrap();

        sink.append(b"one ").unwrap();
        let first = sink.flush().unwrap().unwrap();
        sink.append(b"two ").unwrap();
        let second = sink.flush().unwrap().unwrap();
        // The later one writes both and fsyncs once; the earlier one then
        // has nothing left to do.
        second().unwrap();
        first().unwrap();
        assert_eq!(lock(&sink.disk).fsyncs, 1);

        sink.append(b"three").unwrap();
        sink.sync().unwrap();
        assert_eq!(lock(&sink.disk).fsyncs, 2);
        let on_disk = std::fs::read(&path).unwrap();
        assert!(on_disk.ends_with(b"one two three"), "{on_disk:?}");

        // A rewrite replaces the file with an image that already holds
        // everything appended: what was pending is not written after it.
        sink.append(b" lost").unwrap();
        let before = sink.flush().unwrap().unwrap();
        sink.rewrite(b"IMAGE").unwrap();
        before().unwrap();
        sink.append(b" after").unwrap();
        sink.sync().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"IMAGE after");

        drop(sink);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}
