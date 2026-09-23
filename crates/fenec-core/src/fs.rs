//! Native file persistence layer.
//!
//! The file's contents are in *the same format* as the engine's in-memory
//! byte image. A write = append to the file. Open = a single replay pass.
//! No separate WAL + data file pair, no checkpoint, no page cache.

use crate::engine::{Database, Durability, ImageOut, Sink, MAGIC};
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
        let (mut file, path) = FileSink::create(path)?;
        let mut existing = Vec::new();
        file.read_to_end(&mut existing)?;
        file.seek(SeekFrom::End(0))?;
        Ok((FileSink::over(file, path), existing))
    }

    /// Opens the file for reading and appending, creating it with the magic
    /// alone when it is missing or empty.
    fn create(path: impl AsRef<Path>) -> Result<(File, PathBuf)> {
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)?;
        if file.metadata()?.len() == 0 {
            file.write_all(&MAGIC[..])?;
            file.flush()?;
            file.seek(SeekFrom::Start(0))?;
        }
        Ok((file, path))
    }

    /// A sink appending to `file`, which is positioned at its end.
    fn over(file: File, path: PathBuf) -> FileSink {
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
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Cuts the file back to its first `len` bytes, before anything is
    /// appended: past them is a record a crash left cut short (see
    /// [`Database::load`]). Left there, it swallowed the first write
    /// appended after it -- acknowledged, and gone on the next open.
    pub fn cut(&mut self, len: usize) -> Result<()> {
        let mut disk = lock(&self.disk);
        disk.file.set_len(len as u64)?;
        disk.file.sync_data()?;
        disk.file.seek(SeekFrom::End(0))?;
        Ok(())
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
        self.rewrite_with(&mut |out| out.write(bytes))
    }

    /// The image goes to a side file that is renamed over the database, so
    /// until the rename the old file stands whole: an image that cannot be
    /// written -- a full disk during `compact` -- leaves the appends still
    /// pending there to reach the old file, and a durability waiting on them
    /// finds them on disk. They were cleared before the image once, and a
    /// failed one then let `--sync always` answer "durable" for a write in
    /// neither file. From the rename on, the new file holds them; a failure
    /// after it leaves the sink failed, so nothing waiting is told otherwise.
    fn rewrite_with(
        &mut self,
        image: &mut dyn FnMut(&mut dyn ImageOut) -> Result<()>,
    ) -> Result<()> {
        let mut disk = lock(&self.disk);
        if let Some(e) = &disk.failed {
            return Err(e.clone());
        }
        let tmp = self.path.with_extension("fenec.compacting");
        let written = (|| -> Result<()> {
            let mut out = FileImage {
                w: BufWriter::with_capacity(WRITE_BUF, File::create(&tmp)?),
                at: 0,
            };
            image(&mut out)?;
            out.w
                .into_inner()
                .map_err(|e| crate::error::Error::Io(e.to_string()))?
                .sync_all()?;
            Ok(())
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        let swapped = (|| -> Result<File> {
            std::fs::rename(&tmp, &self.path)?;
            sync_dir(&self.path)?;
            let mut f = OpenOptions::new().read(true).write(true).open(&self.path)?;
            f.seek(SeekFrom::End(0))?;
            Ok(f)
        })();
        // Whatever was pending is in the image: written after it, it would
        // be there twice.
        lock(&self.pending).clear();
        match swapped {
            Ok(f) => {
                // Everything appended so far is in the image, and the image
                // is on disk.
                disk.file = f;
                disk.written = self.appended;
                disk.synced = self.appended;
                Ok(())
            }
            Err(e) => {
                disk.failed = Some(e.clone());
                Err(e)
            }
        }
    }
    /// The file as it stands, mapped read-only: what a mapped database
    /// points its stores at after a rewrite. The mapping it had covers the
    /// file the rename replaced, which is unlinked and goes when the last
    /// location pointing into it does.
    #[cfg(all(unix, target_pointer_width = "64"))]
    fn remapped(&self) -> Option<crate::store::Base> {
        let disk = lock(&self.disk);
        let len = disk.file.metadata().ok()?.len() as usize;
        if len == 0 {
            return None;
        }
        Mapping::of(&disk.file, len)
            .ok()
            .map(|m| Arc::new(m) as crate::store::Base)
    }

    /// Pushes the whole file to disk, the bytes an earlier process wrote
    /// and never synced included. After a crash of the process alone they
    /// are in the file but may be only in the kernel's cache: a primary
    /// calls this before it tells a replica they exist.
    fn sync_existing(&mut self) -> Result<()> {
        let disk = lock(&self.disk);
        disk.file.sync_data()?;
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

/// The image written straight into the side file a rewrite renames over the
/// database: `at` counts what went in, and a patch seeks back to it.
struct FileImage {
    w: BufWriter<File>,
    at: u64,
}

impl ImageOut for FileImage {
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.w.write_all(bytes)?;
        self.at += bytes.len() as u64;
        Ok(())
    }

    fn at(&self) -> u64 {
        self.at
    }

    fn patch(&mut self, at: u64, bytes: &[u8]) -> Result<()> {
        // What is buffered has to be in the file before the seek moves off
        // its end.
        self.w.flush()?;
        let f = self.w.get_mut();
        f.seek(SeekFrom::Start(at))?;
        f.write_all(bytes)?;
        f.seek(SeekFrom::End(0))?;
        Ok(())
    }
}

impl Drop for FileSink {
    fn drop(&mut self) {
        // Do not let buffered leftovers vanish silently.
        let _ = lock(&self.disk).write_pending(&self.pending);
    }
}

/// A file mapped read-only into the address space: its pages come in as
/// they are read and can be dropped again by the operating system, which
/// writes nothing back -- they are the file's. `mmap` is declared here
/// rather than taken from a crate, as `signal` is in fenec-pg.
///
/// The file is never truncated or written in place while mapped: writes
/// append past the mapped length, and a rewrite (`compact`, `checkpoint`)
/// renames a new file over it, so the mapping keeps the old one's pages
/// until it is dropped.
#[cfg(all(unix, target_pointer_width = "64"))]
pub struct Mapping {
    ptr: *mut u8,
    len: usize,
}

// The pages are read-only and shared by nothing but readers.
#[cfg(all(unix, target_pointer_width = "64"))]
unsafe impl Send for Mapping {}
#[cfg(all(unix, target_pointer_width = "64"))]
unsafe impl Sync for Mapping {}

#[cfg(all(unix, target_pointer_width = "64"))]
extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

#[cfg(all(unix, target_pointer_width = "64"))]
impl Mapping {
    /// The first `len` bytes of `file`. A mapping cannot be empty; a file
    /// always holds at least the magic by then.
    fn of(file: &File, len: usize) -> Result<Mapping> {
        use std::os::fd::AsRawFd;
        const PROT_READ: i32 = 1;
        const MAP_SHARED: i32 = 1;
        let ptr = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                PROT_READ,
                MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr as isize == -1 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Mapping { ptr, len })
    }
}

#[cfg(all(unix, target_pointer_width = "64"))]
impl AsRef<[u8]> for Mapping {
    fn as_ref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

#[cfg(all(unix, target_pointer_width = "64"))]
impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe { munmap(self.ptr, self.len) };
    }
}

/// Makes a rename in `path`'s directory durable. The new name is an entry
/// in the directory, and until the directory is synced a power loss can
/// bring back the old one: the file from before a `compact`, without the
/// writes the image had taken in.
#[cfg(unix)]
fn sync_dir(path: &Path) -> Result<()> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> Result<()> {
    Ok(())
}

/// What wraps a file's sink before the database takes it: a primary's
/// `Tee` (fenec-http) goes around it here, fsyncing what the file already
/// holds before a replica can be sent any of it.
pub type Wrap<'a> = dyn FnOnce(Box<dyn Sink>) -> Result<Box<dyn Sink>> + 'a;

/// Opens a fenecdb file (creating it when missing): mapped where the target
/// maps files, read into memory where it does not. A 1 GB file of 2.3
/// million rows with a hash and an ordered index opened this way holds 188
/// MB rather than 1 095, and its `compact` peaks at 236 MB rather than
/// 2 012.
pub fn open(path: impl AsRef<Path>) -> Result<Database> {
    open_with(path, true, Box::new(Ok))
}

/// [`open`] -- or [`open_in_memory`] when `mapped` is false -- with the
/// file's sink wrapped before the database takes it. The flag reaches
/// every server path (`fenec-pg --no-mmap`), replicated files and tenant
/// directories included.
pub fn open_with(path: impl AsRef<Path>, mapped: bool, wrap: Box<Wrap<'_>>) -> Result<Database> {
    #[cfg(all(unix, target_pointer_width = "64"))]
    if mapped {
        return open_mapped_with(path, wrap);
    }
    let _ = mapped;
    open_in_memory_with(path, wrap)
}

/// Opens a fenecdb file (creating it when missing) and reads it into memory,
/// records and all; a last record a crash cut short is cut off the file.
/// What a network file system wants, whose read errors a mapping would turn
/// into the process's death, and what has `--max-memory` count the data.
pub fn open_in_memory(path: impl AsRef<Path>) -> Result<Database> {
    open_in_memory_with(path, Box::new(Ok))
}

fn open_in_memory_with(path: impl AsRef<Path>, wrap: Box<Wrap<'_>>) -> Result<Database> {
    let (mut sink, existing) = FileSink::open(path)?;
    let mut db = Database::new();
    if existing.len() > MAGIC.len() {
        let whole = db.load(&existing)?;
        if whole < existing.len() {
            sink.cut(whole)?;
        }
    }
    db.set_sink(wrap(Box::new(sink))?);
    Ok(db)
}

/// [`open_in_memory`], with the documents left in the file: it is mapped
/// rather than read, and a document is decoded from its pages. What the
/// process holds is what is derived from the documents -- the offset index,
/// the hash, ordered and text indexes, the graph -- and the writes made
/// since the open. A rewrite (`checkpoint`, `compact`) writes the new file
/// and the stores are pointed at it, so the old one is let go of.
///
/// This is what [`open`] does where the target maps files, which is every
/// one fenecdb serves from.
#[cfg(all(unix, target_pointer_width = "64"))]
pub fn open_mapped(path: impl AsRef<Path>) -> Result<Database> {
    open_mapped_with(path, Box::new(Ok))
}

#[cfg(all(unix, target_pointer_width = "64"))]
fn open_mapped_with(path: impl AsRef<Path>, wrap: Box<Wrap<'_>>) -> Result<Database> {
    let (mut file, path) = FileSink::create(path)?;
    let len = file.seek(SeekFrom::End(0))? as usize;
    let mapping = Mapping::of(&file, len)?;
    let mut sink = FileSink::over(file, path);
    let mut db = Database::new();
    // A new file is loaded this way too -- it holds the magic by now -- so
    // the database is a mapped one from the start, and its first rewrite
    // points the stores at the file it wrote. Left out, a new file, a new
    // tenant and a replica taking its first image kept everything in
    // memory until the process restarted.
    let whole = db.load_mapped(Arc::new(mapping))?;
    if whole < len {
        // The pages past the cut stay mapped and are never read: no record
        // points there.
        sink.cut(whole)?;
    }
    db.set_sink(wrap(Box::new(sink))?);
    Ok(db)
}

/// Opens a file to read it, and leaves it as it is: nothing is created,
/// cut, synced or written, and every write is refused. What a tool that
/// only looks -- `fenec types` -- opens a file a server may be writing
/// with: from outside, a record the server is in the middle of appending
/// looks torn, and cutting it there destroyed that write, and the file with
/// it once the server's next append landed past the cut.
pub fn open_read_only(path: impl AsRef<Path>) -> Result<Database> {
    let mut db = Database::new();
    #[cfg(all(unix, target_pointer_width = "64"))]
    {
        let file = File::open(path)?;
        let len = file.metadata()?.len() as usize;
        if len < MAGIC.len() {
            return Err(crate::error::Error::Corrupt(
                "invalid fenecdb signature".into(),
            ));
        }
        db.load_mapped(Arc::new(Mapping::of(&file, len)?))?;
    }
    #[cfg(not(all(unix, target_pointer_width = "64")))]
    db.load(&std::fs::read(path)?)?;
    db.set_sink(Box::new(ReadOnly));
    Ok(db)
}

/// The sink of a database [`open_read_only`] opened.
struct ReadOnly;

impl Sink for ReadOnly {
    fn append(&mut self, _bytes: &[u8]) -> Result<()> {
        Err(crate::error::Error::ReadOnly(
            "the file was opened to be read".into(),
        ))
    }
    fn rewrite(&mut self, _bytes: &[u8]) -> Result<()> {
        self.append(&[])
    }
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

    /// An image that cannot be written leaves the old file standing, and the
    /// appends pending when it began still go to it: a durability handed out
    /// before the rewrite finds its bytes on disk rather than being told
    /// they are there when they are in neither file.
    #[test]
    fn a_failed_rewrite_leaves_the_pending_appends_to_the_old_file() {
        let dir = std::env::temp_dir().join(format!("fenecdb-fs-fail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fail.fenec");
        let _ = std::fs::remove_file(&path);
        let (mut sink, _) = FileSink::open(&path).unwrap();

        sink.append(b"kept").unwrap();
        let durable = sink.flush().unwrap().unwrap();
        let err = sink.rewrite_with(&mut |out| {
            out.write(b"half an ima")?;
            Err(crate::error::Error::Io("no space left on device".into()))
        });
        assert!(err.is_err());
        durable().unwrap();
        assert!(std::fs::read(&path).unwrap().ends_with(b"kept"));
        assert!(!path.with_extension("fenec.compacting").exists());
        // The sink is not failed: the next append and sync go on.
        sink.append(b" more").unwrap();
        sink.sync().unwrap();
        assert!(std::fs::read(&path).unwrap().ends_with(b"kept more"));

        drop(sink);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}
