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
    /// The file mapped with room past its end, which a mapped database's
    /// stores read from and take the records appended since in from
    /// ([`Sink::written_through`]). Let go of when a rewrite puts another
    /// file in its place.
    #[cfg(all(unix, target_pointer_width = "64"))]
    mapping: Option<Arc<Mapping>>,
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
    /// What a sync writes into the sync log rather than fsync the file.
    #[cfg(unix)]
    tail: Tail,
}

#[cfg(unix)]
thread_local! {
    static KEEP_LOG: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Has the files this thread opens keep a sync log ([`crate::synclog`])
/// where the platform would not -- macOS, whose `F_FULLFSYNC` costs the
/// same either way -- so that a test there goes through it.
#[doc(hidden)]
#[cfg(unix)]
pub fn keep_sync_log(on: bool) {
    KEEP_LOG.with(|k| k.set(on));
}

/// The bytes written since the last sync, which the next one puts into the
/// sync log ([`crate::synclog`]) and syncs there, rather than fsync the
/// file: on ext4 an fsync of a file that grew commits the journal, 356 us
/// in Docker's VM where the log's takes 65.
#[cfg(unix)]
struct Tail {
    /// Whether this sink keeps a log (`synclog::ENABLED`, or a test's).
    keeps: bool,
    log_path: PathBuf,
    /// Opened at the first sync that begins a generation; `None` where it
    /// could not be made, and every sync then fsyncs the file.
    log: Option<crate::synclog::SyncLog>,
    /// The file's length: where the next byte written goes.
    end: u64,
    /// The bytes written since the last sync and where they start, while
    /// they fit an entry.
    since: Vec<u8>,
    since_at: u64,
    /// The next sync fsyncs the file itself and begins a generation: what
    /// was written since outgrew an entry, or the file is new to the sink
    /// -- whose bytes an earlier process may have left unsynced.
    direct: bool,
}

#[cfg(unix)]
impl Tail {
    fn new(path: &Path, end: u64) -> Tail {
        Tail {
            keeps: crate::synclog::ENABLED || KEEP_LOG.with(|k| k.get()),
            log_path: crate::synclog::path_for(path),
            log: None,
            end,
            since: Vec::new(),
            since_at: end,
            direct: true,
        }
    }

    /// `bytes` reached the file.
    fn wrote(&mut self, bytes: &[u8]) {
        self.end += bytes.len() as u64;
        if !self.keeps || self.direct {
            return;
        }
        if self.since.len() + bytes.len() > crate::synclog::ENTRY_MAX {
            self.direct = true;
            self.since = Vec::new();
        } else {
            self.since.extend_from_slice(bytes);
        }
    }

    /// The file starts again at `end`, synced: a cut, a rewrite.
    fn reset(&mut self, end: u64) {
        self.end = end;
        self.since.clear();
        self.since_at = end;
        self.direct = true;
    }

    /// Makes what was written since the last sync durable: through the log
    /// while it fits there, else by an fsync of the file, after which the
    /// log begins a generation from where the file now stands.
    fn sync(&mut self, file: &File) -> std::io::Result<()> {
        if self.keeps && !self.direct {
            if self.since.is_empty() {
                return Ok(());
            }
            if let Some(log) = self
                .log
                .as_mut()
                .filter(|l| l.takes(self.since_at, self.since.len()))
            {
                log.append(self.since_at, &self.since)?;
                self.since.clear();
                self.since_at = self.end;
                return Ok(());
            }
        }
        file.sync_data()?;
        self.since.clear();
        self.since_at = self.end;
        if self.keeps {
            if self.log.is_none() {
                self.log = crate::synclog::SyncLog::open(&self.log_path).ok();
            }
            if let Some(log) = &mut self.log {
                log.begin(file, self.end)?;
                self.direct = false;
            }
        }
        Ok(())
    }

    /// The file was put in place by a rename: the log, written beside the
    /// one it replaced, is let go of, and the next sync begins one anew.
    fn replaced(&mut self, end: u64) {
        self.log = None;
        let _ = std::fs::remove_file(&self.log_path);
        self.reset(end);
    }
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
        #[cfg(unix)]
        self.tail.wrote(&bytes);
        Ok(())
    }

    /// Writes whatever is pending and then `bytes`, which do not go through
    /// the buffer.
    fn write_through(&mut self, pending: &Mutex<Vec<u8>>, bytes: &[u8]) -> Result<()> {
        self.write_pending(pending)?;
        if let Err(e) = self.file.write_all(bytes) {
            self.failed = Some(e.into());
            return Err(self.failed.clone().unwrap());
        }
        self.written += bytes.len() as u64;
        #[cfg(unix)]
        self.tail.wrote(bytes);
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
        #[cfg(unix)]
        let synced = self.tail.sync(&self.file);
        #[cfg(not(unix))]
        let synced = self.file.sync_data();
        if let Err(e) = synced {
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
    /// Renames `from`, a whole image fsynced, over the file and appends to
    /// it from then on. Whatever was pending is in the image: written after
    /// it, it would be there twice. A failure after the rename leaves the
    /// sink failed, so nothing waiting is told its write is durable.
    fn swap_in(&self, disk: &mut Disk, from: &Path) -> Result<()> {
        let swapped = (|| -> Result<File> {
            std::fs::rename(from, &self.path)?;
            sync_dir(&self.path)?;
            let mut f = OpenOptions::new().read(true).write(true).open(&self.path)?;
            f.seek(SeekFrom::End(0))?;
            Ok(f)
        })();
        lock(&self.pending).clear();
        match swapped {
            Ok(f) => {
                // Everything appended so far is in the image, and the image
                // is on disk.
                #[cfg(unix)]
                {
                    let end = f.metadata().map_or(0, |m| m.len());
                    disk.tail.replaced(end);
                }
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
        // An existing file is the database: never truncated.
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        if file.metadata()?.len() == 0 {
            // A log left beside a file of that name is not this one's.
            #[cfg(unix)]
            let _ = std::fs::remove_file(crate::synclog::path_for(&path));
            file.write_all(&MAGIC[..])?;
            file.flush()?;
            file.seek(SeekFrom::Start(0))?;
        } else {
            // What the last process made durable through the sync log and
            // the machine lost from the file goes back before it is read.
            #[cfg(unix)]
            crate::synclog::recover(&file, &crate::synclog::path_for(&path))?;
        }
        Ok((file, path))
    }

    /// A sink appending to `file`, which is positioned at its end.
    fn over(file: File, path: PathBuf) -> FileSink {
        #[cfg(unix)]
        let end = file.metadata().map_or(0, |m| m.len());
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
                #[cfg(unix)]
                tail: Tail::new(&path, end),
            })),
            path,
            #[cfg(all(unix, target_pointer_width = "64"))]
            mapping: None,
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
        #[cfg(unix)]
        disk.tail.reset(len as u64);
        Ok(())
    }
}

impl Sink for FileSink {
    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        self.appended += bytes.len() as u64;
        // A record the buffer could not hold is written where it lies, as
        // the buffer would have been once it held it: copied in first, a
        // block of 50 000 768-dim rows was 154 MB more of heap for the
        // time of the write, which macOS's allocator then kept.
        if bytes.len() >= WRITE_BUF {
            return lock(&self.disk).write_through(&self.pending, bytes);
        }
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
    /// Pending until the durability runs, however large: written here, a
    /// kept graph's record waited out any fsync under way as well.
    fn append_deferred(&mut self, bytes: &[u8]) -> Result<()> {
        self.appended += bytes.len() as u64;
        lock(&self.pending).extend_from_slice(bytes);
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
        #[cfg(all(unix, target_pointer_width = "64"))]
        {
            self.mapping = None;
        }
        self.swap_in(&mut disk, &tmp)
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn side(&self) -> Option<PathBuf> {
        Some(self.path.with_extension("fenec.beside"))
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn adopt(&mut self, side: &Path) -> Result<()> {
        let mut disk = lock(&self.disk);
        if let Some(e) = &disk.failed {
            return Err(e.clone());
        }
        #[cfg(all(unix, target_pointer_width = "64"))]
        {
            self.mapping = None;
        }
        self.swap_in(&mut disk, side)
    }
    /// The file as it stands, mapped read-only, with room past its end:
    /// what a mapped database points its stores at after a rewrite. The
    /// mapping it had covers the file the rename replaced, which is
    /// unlinked and goes when the last location pointing into it does.
    #[cfg(all(unix, target_pointer_width = "64"))]
    fn remapped(&mut self) -> Option<crate::store::Base> {
        let disk = lock(&self.disk);
        let len = disk.file.metadata().ok()?.len() as usize;
        if len == 0 {
            return None;
        }
        let m = Arc::new(Mapping::with_room(&disk.file, len).ok()?);
        self.mapping = Some(m.clone());
        Some(m)
    }

    /// Writes what is pending -- under the database's lock, where the
    /// appends leave it to the durability, but only once a handover is due
    /// -- and hands back the file mapped as far as it now goes: the mapping
    /// the stores read from, grown over the appends, or where they outgrew
    /// its room a new one.
    #[cfg(all(unix, target_pointer_width = "64"))]
    fn written_through(&mut self) -> Result<Option<crate::store::Base>> {
        let mut disk = lock(&self.disk);
        disk.write_pending(&self.pending)?;
        let len = disk.file.metadata()?.len() as usize;
        if let Some(m) = self.mapping.as_ref().filter(|m| m.cover(len)) {
            return Ok(Some(m.clone()));
        }
        let m = Arc::new(Mapping::with_room(&disk.file, len)?);
        self.mapping = Some(m.clone());
        Ok(Some(m))
    }

    /// Pushes the whole file to disk, the bytes an earlier process wrote
    /// and never synced included. After a crash of the process alone they
    /// are in the file but may be only in the kernel's cache: a primary
    /// calls this before it tells a replica they exist.
    /// The pending appends written, and no fsync: past the process's end,
    /// in the kernel's cache.
    #[cfg(not(target_arch = "wasm32"))]
    fn write_out(&mut self) -> Result<()> {
        lock(&self.disk).write_pending(&self.pending)
    }

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
/// rather than taken from a crate, as `signal` is in fenec-server.
///
/// The file is never truncated or written in place while mapped: writes
/// append past the mapped length, and a rewrite (`compact`, `checkpoint`)
/// renames a new file over it, so the mapping keeps the old one's pages
/// until it is dropped.
///
/// A database's own file is mapped with room past its end (`with_room`),
/// which the appends grow into: the pages a write adds to the file are the
/// mapping's once the write is made, on Linux and on macOS alike, so the
/// records written since the open are taken in where they lie
/// ([`Database::hand_over`]) rather than through a new mapping, whose pages
/// every reader would fault in again -- and the old one's torn down under
/// the write lock.
#[cfg(all(unix, target_pointer_width = "64"))]
pub struct Mapping {
    ptr: *mut u8,
    /// What is mapped: the file's length when it was, and the room.
    reserved: usize,
    /// How much of it the file holds, as far as a reader looks.
    len: std::sync::atomic::AtomicUsize,
}

/// The room a database's file is mapped with past its end: the file's own
/// length, and a gigabyte at the least. It is address space alone, of which
/// a 64-bit process has terabytes, and it is taken again, twice the file's
/// length, only when the file outgrows it.
#[cfg(all(unix, target_pointer_width = "64"))]
const ROOM: usize = 1 << 30;

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
        Mapping::reserving(file, len, len)
    }

    /// [`Self::of`], with room past the file's end for what is appended to
    /// it ([`ROOM`]); the file alone where the system will not map that
    /// much -- an address space limit -- and a handover then maps it anew.
    fn with_room(file: &File, len: usize) -> Result<Mapping> {
        Mapping::reserving(file, len, len + len.max(ROOM)).or_else(|_| Mapping::of(file, len))
    }

    fn reserving(file: &File, len: usize, reserved: usize) -> Result<Mapping> {
        use std::os::fd::AsRawFd;
        const PROT_READ: i32 = 1;
        const MAP_SHARED: i32 = 1;
        let ptr = unsafe {
            mmap(
                std::ptr::null_mut(),
                reserved,
                PROT_READ,
                MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr as isize == -1 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Mapping {
            ptr,
            reserved,
            len: std::sync::atomic::AtomicUsize::new(len),
        })
    }

    /// Takes the file as `len` bytes long, where the mapping reaches that
    /// far. A reader never looks past what the file holds: a page past its
    /// end is the process's end (`SIGBUS`).
    fn cover(&self, len: usize) -> bool {
        if len > self.reserved {
            return false;
        }
        // Grown under the database's write lock, which is what orders it
        // before the reads of what it covers.
        self.len.store(len, std::sync::atomic::Ordering::Relaxed);
        true
    }
}

/// Reads a byte of every page of `m`, so that its pages are in memory
/// before anything waits on one.
#[cfg(all(unix, target_pointer_width = "64"))]
pub fn touch(m: &crate::store::Base) {
    let bytes: &[u8] = (**m).as_ref();
    let mut sum = 0u8;
    for at in (0..bytes.len()).step_by(4096) {
        // Volatile, or the compiler drops reads whose result is unused.
        sum ^= unsafe { std::ptr::read_volatile(bytes.as_ptr().add(at)) };
    }
    std::hint::black_box(sum);
}

/// Reads the byte at each of `at` in `m`: the pages those places are on,
/// brought into memory.
#[cfg(all(unix, target_pointer_width = "64"))]
pub fn touch_at(m: &crate::store::Base, at: &[usize]) {
    let bytes: &[u8] = (**m).as_ref();
    let mut sum = 0u8;
    for &a in at.iter().filter(|&&a| a < bytes.len()) {
        sum ^= unsafe { std::ptr::read_volatile(bytes.as_ptr().add(a)) };
    }
    std::hint::black_box(sum);
}

#[cfg(all(unix, target_pointer_width = "64"))]
impl AsRef<[u8]> for Mapping {
    fn as_ref(&self) -> &[u8] {
        let len = self.len.load(std::sync::atomic::Ordering::Relaxed);
        unsafe { std::slice::from_raw_parts(self.ptr, len) }
    }
}

#[cfg(all(unix, target_pointer_width = "64"))]
impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe { munmap(self.ptr, self.reserved) };
    }
}

/// The file a rewrite beside the database writes (`compact` on a server):
/// the image with no lock held, and then, under the write lock, the writes
/// made meanwhile, before the sink takes it in place of the database's file
/// ([`Sink::adopt`]). Removed when dropped unless it was adopted.
#[cfg(all(unix, target_pointer_width = "64"))]
pub struct SideFile {
    path: PathBuf,
    out: FileImage,
    adopted: bool,
}

#[cfg(all(unix, target_pointer_width = "64"))]
impl SideFile {
    pub fn create(path: &Path) -> Result<SideFile> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        Ok(SideFile {
            path: path.to_path_buf(),
            out: FileImage {
                w: BufWriter::with_capacity(WRITE_BUF, file),
                at: 0,
            },
            adopted: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Pushes what is written to disk: the image, before the lock is taken
    /// for the rest, so that the fsync under it covers only that.
    pub fn sync(&mut self) -> Result<()> {
        self.out.w.flush()?;
        self.out.w.get_ref().sync_data()?;
        Ok(())
    }

    /// The file as written so far, mapped: where the stores of the image
    /// read from once it is the database's file. Appends after it land past
    /// the mapping, as a database's own do.
    pub fn map(&mut self) -> Result<crate::store::Base> {
        self.out.w.flush()?;
        let m = Mapping::of(self.out.w.get_ref(), self.out.at as usize)?;
        Ok(Arc::new(m))
    }

    /// Taken in place of the database's file: not to be removed.
    pub fn adopted(&mut self) {
        self.adopted = true;
    }
}

#[cfg(all(unix, target_pointer_width = "64"))]
impl ImageOut for SideFile {
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.out.write(bytes)
    }
    fn at(&self) -> u64 {
        self.out.at()
    }
    fn patch(&mut self, at: u64, bytes: &[u8]) -> Result<()> {
        self.out.patch(at, bytes)
    }
}

#[cfg(all(unix, target_pointer_width = "64"))]
impl Drop for SideFile {
    fn drop(&mut self) {
        if !self.adopted {
            let _ = std::fs::remove_file(&self.path);
        }
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
/// every server path (`fenec-server --no-mmap`), replicated files and tenant
/// directories included.
pub fn open_with(path: impl AsRef<Path>, mapped: bool, wrap: Box<Wrap<'_>>) -> Result<Database> {
    open_into(Database::new(), path, mapped, wrap)
}

/// [`open_with`] as a server opens its file: the vectors the open would
/// link into a graph -- the writes after the last checkpoint, every vector
/// of a graph it cannot restore -- are left for a thread beside the queries
/// to link ([`Database::defer_linking`]), and the file opens in the time
/// its documents take to read.
pub fn open_serving(path: impl AsRef<Path>, mapped: bool, wrap: Box<Wrap<'_>>) -> Result<Database> {
    let mut db = Database::new();
    db.defer_linking();
    open_into(db, path, mapped, wrap)
}

fn open_into(
    db: Database,
    path: impl AsRef<Path>,
    mapped: bool,
    wrap: Box<Wrap<'_>>,
) -> Result<Database> {
    #[cfg(all(unix, target_pointer_width = "64"))]
    if mapped {
        return open_mapped_into(db, path, wrap);
    }
    let _ = mapped;
    open_in_memory_into(db, path, wrap)
}

/// Opens a fenecdb file (creating it when missing) and reads it into memory,
/// records and all; a last record a crash cut short is cut off the file.
/// What a network file system wants, whose read errors a mapping would turn
/// into the process's death, and what has `--max-memory` count the data.
pub fn open_in_memory(path: impl AsRef<Path>) -> Result<Database> {
    open_in_memory_into(Database::new(), path, Box::new(Ok))
}

fn open_in_memory_into(
    mut db: Database,
    path: impl AsRef<Path>,
    wrap: Box<Wrap<'_>>,
) -> Result<Database> {
    let (mut sink, existing) = FileSink::open(path)?;
    // A new file is loaded too, as a mapped one is: it holds the magic by
    // now, which the bytes the database counts as its file's must include.
    if existing.len() >= MAGIC.len() {
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
/// the hash, ordered and text indexes, the graph -- and the documents
/// written since the open until they are handed over to the file
/// ([`Database::hand_over`]). A rewrite (`checkpoint`, `compact`) writes the
/// new file and the stores are pointed at it, so the old one is let go of.
///
/// This is what [`open`] does where the target maps files, which is every
/// one fenecdb serves from.
#[cfg(all(unix, target_pointer_width = "64"))]
pub fn open_mapped(path: impl AsRef<Path>) -> Result<Database> {
    open_mapped_into(Database::new(), path, Box::new(Ok))
}

#[cfg(all(unix, target_pointer_width = "64"))]
fn open_mapped_into(
    mut db: Database,
    path: impl AsRef<Path>,
    wrap: Box<Wrap<'_>>,
) -> Result<Database> {
    let (mut file, path) = FileSink::create(path)?;
    let len = file.seek(SeekFrom::End(0))? as usize;
    let mapping = Arc::new(Mapping::with_room(&file, len)?);
    let mut sink = FileSink::over(file, path);
    sink.mapping = Some(mapping.clone());
    // A new file is loaded this way too -- it holds the magic by now -- so
    // the database is a mapped one from the start, and its first rewrite
    // points the stores at the file it wrote. Left out, a new file, a new
    // tenant and a replica taking its first image kept everything in
    // memory until the process restarted.
    let whole = db.load_mapped(mapping)?;
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

    /// A record appended for its durability waits for it, however far past
    /// the buffer it goes, and the durability puts it on disk in its place;
    /// a plain append that far past is written there and then.
    #[test]
    fn a_deferred_append_waits_for_its_durability() {
        let dir = std::env::temp_dir().join(format!("fenecdb-fs-defer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("defer.fenec");
        let _ = std::fs::remove_file(&path);
        let (mut sink, _) = FileSink::open(&path).unwrap();
        let len = |p: &Path| std::fs::metadata(p).unwrap().len() as usize;

        sink.append(b"head ").unwrap();
        let before = len(&path);
        let big = vec![7u8; 2 * WRITE_BUF];
        sink.append_deferred(&big).unwrap();
        assert_eq!(len(&path), before);
        let durable = sink.flush().unwrap().unwrap();
        durable().unwrap();
        let on_disk = std::fs::read(&path).unwrap();
        assert!(on_disk.ends_with(&big));
        assert!(on_disk[..on_disk.len() - big.len()].ends_with(b"head "));

        sink.append(&big).unwrap();
        assert_eq!(len(&path), on_disk.len() + big.len());

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
