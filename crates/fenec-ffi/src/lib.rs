//! fenecdb as a native library: a C ABI over the engine, for an app that
//! keeps its database in a file on the device -- what the Swift, Kotlin and
//! Dart bindings (`integrations/`) call, and what any language with a C FFI
//! can. The header is `include/fenec.h`, written by hand: a few functions,
//! and cbindgen would be the crate's one dependency.
//!
//! ## What it shares with the browser module
//!
//! A statement comes in as UTF-8 text, its parameters as a JSON array and
//! the vectors among them apart as `f32`s, and the answer goes out as JSON:
//! `fenec_query` and `fenec_changes` answer through `fenec-abi`, the code the
//! browser module's `fenec_query` and `fenec_changes` answer through, so a
//! page and an app handed the same text get the same bytes. What differs is
//! around it. The module holds a database on its one thread and is handed
//! the time; this holds one behind a lock -- a read takes the shared side
//! and runs beside other reads, a write the exclusive one, as a server does
//! (`fenec-http`) -- opens a file with `fs::open` (mapped, its graphs linked
//! on every core) and has the system's clock.
//!
//! ## Threads, and what blocks
//!
//! A handle is a number, safe to use from any thread at once. Every call
//! that takes one may wait:
//!
//! - `fenec_query` waits for the lock: a read for a write under way, a
//!   write for the reads and the write under way. A lone `create index` or
//!   `compact` is built beside the database (`Database::maintain`), reads
//!   and writes going on, and the caller waits for the build.
//! - A write on a handle opened to sync (the default) waits for its fsync
//!   too -- outside the lock, so others read and write meanwhile, and
//!   writes arriving together share one fsync, as a server's do.
//! - `fenec_sync` waits for an fsync, `fenec_flush` for a `write`, and
//!   `fenec_checkpoint` writes the whole file under the write lock.
//! - `fenec_open` reads the file's index and links the vectors written since
//!   its graphs were last saved; `fenec_close` waits for the calls in flight,
//!   then saves the graphs and syncs.
//!
//! None of them belongs on an app's main thread; each binding calls them
//! off it.
//!
//! ## Errors
//!
//! Every call returns a code, 0 for success; an error's JSON,
//! `{"kind":"error","message":...}` as the browser module writes it, comes
//! back through the out pointer where an answer would have. A panic never
//! crosses the boundary: each call runs under `catch_unwind`, and one comes
//! back as `FENEC_PANIC` with its message. That needs unwinding, which is
//! why the library is built in the `ffi` profile (`panic = "unwind"`) and
//! not the shell's `cli` one: aborting would take the app down with it.
//!
//! ## Strings
//!
//! Text goes in as a pointer and a length, UTF-8, read where it lies.
//! Text comes out NUL-terminated, its length beside it, and is the caller's
//! to free with `fenec_free_string`. JSON writes a NUL as `\u0000`, so an
//! answer never holds one.

use fenec_abi::Refused;
use fenec_core::json;
use fenec_core::prelude::*;
use std::ffi::{c_char, CString};
use std::fs::File;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::result::Result;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

#[cfg(feature = "jni")]
mod jni;

/// The codes a call returns: 0, an `Error`'s kind, or one of the boundary's.
pub const FENEC_OK: i32 = 0;
pub const FENEC_TYPE: i32 = 1;
pub const FENEC_NOT_FOUND: i32 = 2;
pub const FENEC_EXISTS: i32 = 3;
pub const FENEC_DUPLICATE: i32 = 4;
pub const FENEC_CORRUPT: i32 = 5;
pub const FENEC_QUERY: i32 = 6;
pub const FENEC_IO: i32 = 7;
pub const FENEC_PLUGIN: i32 = 8;
pub const FENEC_READ_ONLY: i32 = 9;
pub const FENEC_DENIED: i32 = 10;
/// A panic inside the library, caught at the boundary.
pub const FENEC_PANIC: i32 = 11;
/// A call the library cannot make sense of: a handle that is not open, a
/// null where text goes, text that is not UTF-8.
pub const FENEC_MISUSE: i32 = 12;
/// The file is open already, in this process or another: two databases
/// over one file corrupt it.
pub const FENEC_LOCKED: i32 = 13;

/// `fenec_open`'s flags. Without any, every write is fsynced before the call
/// returns and the file is mapped.
///
/// No fsync a write: the writes wait in a buffer until `fenec_sync`,
/// `fenec_flush`, `fenec_close` or a mebibyte of them -- for a burst of
/// writes, which then costs a fsync rather than one each.
pub const FENEC_OPEN_NO_SYNC: u32 = 1;
/// Read the file into memory rather than map it: for a file whose pages
/// may become unreadable while it is open -- iOS's `complete` protection
/// class, as the device locks, where a mapped page read is the process's
/// end (`SIGBUS`) rather than an error.
pub const FENEC_OPEN_IN_MEMORY: u32 = 2;

/// An open database.
struct Native {
    db: RwLock<Database>,
    /// Whether a write waits for its fsync (no `FENEC_OPEN_NO_SYNC`).
    durable: bool,
    /// Whether it holds a file: a memory database has nothing to sync or
    /// keep a graph in.
    file: bool,
    /// Set under the write lock by `fenec_close`, so a call that found the
    /// handle before the close and takes the lock after it is refused.
    closed: AtomicBool,
    /// The `<file>.lock` held while the file is open (`lock_file`).
    lock: Mutex<Option<File>>,
    /// The sync with a server, once `fenec_sync_start` attached one: then
    /// a write to a synced collection goes through it. Taken before the
    /// database's lock, never after.
    sync: Mutex<Option<fenec_abi::sync::Sync>>,
}

/// The open databases by handle. A number rather than a pointer: a handle
/// used after its close finds nothing and is refused, where a pointer would
/// reach freed memory, and each call holds the database (an `Arc`) for as
/// long as it runs, whatever closes meanwhile. A handle is never given out
/// twice. A `Vec` searched in turn, since an app opens a few.
static HANDLES: Mutex<Vec<(u64, Arc<Native>)>> = Mutex::new(Vec::new());
static NEXT: AtomicU64 = AtomicU64::new(1);

fn handles() -> std::sync::MutexGuard<'static, Vec<(u64, Arc<Native>)>> {
    HANDLES.lock().unwrap_or_else(|e| e.into_inner())
}

/// What a call failed with: its code and its JSON.
type Failed = (i32, String);

fn failed(e: &Error) -> Failed {
    (code(e), json::error_to_string(e))
}

/// The boundary's own refusal: its message alone, with no engine error's
/// kind before it.
fn misuse(why: &str) -> Failed {
    let mut text = String::from("{\"kind\":\"error\",\"message\":");
    json::escape_into(&mut text, why);
    text.push('}');
    (FENEC_MISUSE, text)
}

/// An error's code: its kind, which a binding turns into its own error type
/// without reading the message.
pub fn code(e: &Error) -> i32 {
    match e {
        Error::Type(_) => FENEC_TYPE,
        Error::NotFound(_) => FENEC_NOT_FOUND,
        Error::Exists(_) => FENEC_EXISTS,
        Error::Duplicate(_) => FENEC_DUPLICATE,
        Error::Corrupt(_) => FENEC_CORRUPT,
        Error::Query(_) => FENEC_QUERY,
        Error::Io(_) => FENEC_IO,
        Error::Plugin(_) => FENEC_PLUGIN,
        Error::ReadOnly(_) => FENEC_READ_ONLY,
        Error::Denied(_) => FENEC_DENIED,
    }
}

fn native(handle: u64) -> Result<Arc<Native>, Failed> {
    handles()
        .iter()
        .find(|(h, _)| *h == handle)
        .map(|(_, n)| n.clone())
        .ok_or_else(|| misuse(&format!("no database is open under handle {handle}")))
}

fn closed() -> Failed {
    misuse("the database was closed")
}

impl Native {
    /// The shared side, past a poisoned lock: a panic is caught at the
    /// boundary, and the database it left is the one every later call reads.
    fn read(&self) -> Result<RwLockReadGuard<'_, Database>, Failed> {
        let db = self.db.read().unwrap_or_else(|e| e.into_inner());
        match self.closed.load(Ordering::Acquire) {
            true => Err(closed()),
            false => Ok(db),
        }
    }

    fn sync_lock(&self) -> std::sync::MutexGuard<'_, Option<fenec_abi::sync::Sync>> {
        self.sync.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> Result<RwLockWriteGuard<'_, Database>, Failed> {
        let db = self.db.write().unwrap_or_else(|e| e.into_inner());
        match self.closed.load(Ordering::Acquire) {
            true => Err(closed()),
            false => Ok(db),
        }
    }

    /// Runs what `Database::flush` handed back, with no lock held: readers
    /// and writers go on while the disk works, and a write arriving
    /// meanwhile finds its bytes covered by this fsync or the next. A
    /// failure is reported to the engine, which then refuses every later
    /// write as after one it saw itself.
    fn durable(&self, durability: Option<Durability>) -> Result<(), Error> {
        let Some(durable) = durability else {
            return Ok(());
        };
        durable().inspect_err(|e| {
            self.db.write().unwrap_or_else(|p| p.into_inner()).fail(e);
        })
    }
}

// ----------------------------------------------------------- the boundary

/// Runs a call under `catch_unwind` and writes what it answered, or failed
/// with, through the out pointers.
///
/// # Safety
/// `out` and `out_len` are null or valid to write.
unsafe fn call(
    out: *mut *mut c_char,
    out_len: *mut usize,
    f: impl FnOnce() -> Result<Option<String>, Failed>,
) -> i32 {
    let (code, text) = match catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(text)) => (FENEC_OK, text),
        Ok(Err((code, text))) => (code, Some(text)),
        Err(panic) => {
            let why = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "a panic".into());
            let mut text = String::from("{\"kind\":\"error\",\"message\":");
            json::escape_into(&mut text, &format!("internal error: {why}"));
            text.push('}');
            (FENEC_PANIC, Some(text))
        }
    };
    let (ptr, len) = match text.map(CString::new) {
        Some(Ok(c)) => {
            let len = c.as_bytes().len();
            (c.into_raw(), len)
        }
        _ => (std::ptr::null_mut(), 0),
    };
    if out.is_null() {
        if !ptr.is_null() {
            drop(CString::from_raw(ptr));
        }
    } else {
        *out = ptr;
    }
    if !out_len.is_null() {
        *out_len = len;
    }
    code
}

/// Text the caller handed over, read where it lies.
///
/// # Safety
/// `ptr` is null with `len` 0, or valid for `len` bytes.
unsafe fn text<'a>(ptr: *const u8, len: usize, what: &str) -> Result<&'a str, Failed> {
    if ptr.is_null() || len == 0 {
        return match len {
            0 => Ok(""),
            _ => Err(misuse(&format!("{what} is null"))),
        };
    }
    std::str::from_utf8(std::slice::from_raw_parts(ptr, len))
        .map_err(|_| misuse(&format!("{what} is not UTF-8")))
}

/// Bytes the caller handed over: the vectors.
///
/// # Safety
/// As [`text`].
unsafe fn bytes<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    match ptr.is_null() || len == 0 {
        true => &[],
        false => std::slice::from_raw_parts(ptr, len),
    }
}

// ------------------------------------------------------------- lifecycle

/// The library's version, NUL-terminated and static: not to be freed.
#[no_mangle]
pub extern "C" fn fenec_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

/// Opens the file at `path` (made when missing) and writes its handle.
/// `flags`: `FENEC_OPEN_NO_SYNC`, `FENEC_OPEN_IN_MEMORY`. A file open
/// already -- under another handle, or by another process, an app extension
/// sharing the app's container -- is refused (`FENEC_LOCKED`).
///
/// # Safety
/// `path` is valid for `path_len` bytes; `handle` is valid to write; `out`
/// and `out_len` are null or valid to write.
#[no_mangle]
pub unsafe extern "C" fn fenec_open(
    path: *const u8,
    path_len: usize,
    flags: u32,
    handle: *mut u64,
    out: *mut *mut c_char,
    out_len: *mut usize,
) -> i32 {
    call(out, out_len, || {
        let path = text(path, path_len, "the path")?;
        if path.is_empty() || handle.is_null() {
            return Err(misuse("fenec_open needs a path and a handle to write"));
        }
        let lock = lock_file(path)?;
        let mapped = flags & FENEC_OPEN_IN_MEMORY == 0;
        let db = fenec_core::fs::open_with(path, mapped, Box::new(Ok)).map_err(|e| failed(&e))?;
        *handle = keep(db, flags & FENEC_OPEN_NO_SYNC == 0, Some(lock));
        Ok(None)
    })
}

/// Opens a database held in memory alone, and writes its handle.
///
/// # Safety
/// `handle` is valid to write; `out` and `out_len` are null or valid to write.
#[no_mangle]
pub unsafe extern "C" fn fenec_open_memory(
    handle: *mut u64,
    out: *mut *mut c_char,
    out_len: *mut usize,
) -> i32 {
    call(out, out_len, || {
        if handle.is_null() {
            return Err(misuse("fenec_open_memory needs a handle to write"));
        }
        *handle = keep(Database::new(), false, None);
        Ok(None)
    })
}

fn keep(db: Database, durable: bool, lock: Option<File>) -> u64 {
    let file = lock.is_some();
    let n = Arc::new(Native {
        db: RwLock::new(db),
        durable: durable && file,
        file,
        closed: AtomicBool::new(false),
        lock: Mutex::new(lock),
        sync: Mutex::new(None),
    });
    let h = NEXT.fetch_add(1, Ordering::Relaxed);
    handles().push((h, n));
    h
}

/// Holds `<path>.lock` exclusively for as long as the file is open. The
/// file itself cannot carry the lock: a checkpoint or a compact renames a
/// new file over it, and a lock on the old one would let a second opener
/// in. `flock` is per open file, so it also refuses a second open in this
/// process, and the system lets it go when the process ends, however.
fn lock_file(path: &str) -> Result<File, Failed> {
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(format!("{path}.lock"))
        .map_err(|e| failed(&e.into()))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        extern "C" {
            fn flock(fd: i32, op: i32) -> i32;
        }
        const LOCK_EX: i32 = 2;
        const LOCK_NB: i32 = 4;
        if unsafe { flock(lock.as_raw_fd(), LOCK_EX | LOCK_NB) } != 0 {
            return Err((
                FENEC_LOCKED,
                json::error_to_string(&Error::Io(format!(
                    "{path} is open already, in this process or another: \
                     two databases over one file corrupt it"
                ))),
            ));
        }
    }
    Ok(lock)
}

/// Closes the database: waits for the calls in flight, appends each graph
/// that changed since it was last saved -- so the next open restores it
/// rather than link every vector written since -- syncs, and lets the file
/// and its lock go. The handle is refused from then on.
///
/// A graph is saved once the file has grown three times its record since
/// the last save, as a server keeps its graphs: saved at every close, an
/// app that opens and closes often would grow its file by a graph each
/// time; the open links at most that third's vectors.
///
/// # Safety
/// `out` and `out_len` are null or valid to write.
#[no_mangle]
pub unsafe extern "C" fn fenec_close(
    handle: u64,
    out: *mut *mut c_char,
    out_len: *mut usize,
) -> i32 {
    call(out, out_len, || {
        let n = {
            let mut hs = handles();
            let at = hs
                .iter()
                .position(|(h, _)| *h == handle)
                .ok_or_else(|| misuse(&format!("no database is open under handle {handle}")))?;
            hs.swap_remove(at).1
        };
        drop(n.sync_lock().take());
        let mut db = n.db.write().unwrap_or_else(|e| e.into_inner());
        n.closed.store(true, Ordering::Release);
        let mut result = Ok(());
        if n.file {
            db.set_graph_saves(1, 3);
            result = db
                .save_graphs()
                .and_then(|(_, durable)| durable.map_or(Ok(()), |d| d()))
                .and_then(|_| db.sync());
        }
        // Dropped here, not when the last call holding the `Arc` ends: the
        // file's buffer written, its mapping let go, and the lock with them.
        drop(std::mem::take(&mut *db));
        drop(db);
        drop(n.lock.lock().unwrap_or_else(|e| e.into_inner()).take());
        result.map(|_| None).map_err(|e| failed(&e))
    })
}

// ------------------------------------------------------------------ query

/// Runs FenecQL: `text` one or more statements, `params` a JSON array for
/// `$1`, `$2` ... (empty for none), and `vectors` the parameters that are
/// vectors handed over as `f32`s -- for each, its place among the
/// parameters and its length as little-endian `u32`s, then its values,
/// where the JSON holds `null` -- or null. Writes the answer's JSON,
/// `{"kind":"rows"|"affected"|"ok"|"schemas", ...}`, or the error's. An
/// error answering `"exact":[places]` asks for those parameters again as
/// JSON numbers: a json field keeps a list of numbers as written.
///
/// Several statements are one block: their writes land together or not at
/// all, the answer being the last one's.
///
/// # Safety
/// `text`, `params` and `vectors` are null with length 0 or valid for their
/// length; `out` and `out_len` are null or valid to write.
#[no_mangle]
pub unsafe extern "C" fn fenec_query(
    handle: u64,
    text_ptr: *const u8,
    text_len: usize,
    params_ptr: *const u8,
    params_len: usize,
    vectors_ptr: *const u8,
    vectors_len: usize,
    out: *mut *mut c_char,
    out_len: *mut usize,
) -> i32 {
    call(out, out_len, || {
        let n = native(handle)?;
        let sql = text(text_ptr, text_len, "the text")?;
        let params = text(params_ptr, params_len, "the parameters")?;
        let vectors = bytes(vectors_ptr, vectors_len);
        match query(&n, sql, params, vectors) {
            (FENEC_OK, answer) => Ok(Some(answer)),
            failed => Err(failed),
        }
    })
}

/// [`fenec_query`] as Rust: the code and the answer's JSON.
fn query(n: &Native, sql: &str, params: &str, vectors: &[u8]) -> (i32, String) {
    let r = fenec_abi::prepare(sql, params, vectors)
        .map_err(Stop::from)
        .and_then(|mut p| run(n, &mut p, sql, params, vectors));
    match r {
        Ok(r) => (FENEC_OK, fenec_abi::answer(&Ok(r))),
        Err(Stop::Failed(f)) => f,
        Err(Stop::Refused(r)) => {
            let code = match &r {
                Refused::Error(e, _) => code(e),
                Refused::Exact(_) => FENEC_QUERY,
            };
            (code, fenec_abi::answer(&Err(r)))
        }
    }
}

/// Why [`run`] stopped: the engine refused the statement, as the browser
/// module answers it, or the boundary did -- a closed handle.
enum Stop {
    Refused(Refused),
    Failed(Failed),
}

impl From<Refused> for Stop {
    fn from(r: Refused) -> Stop {
        Stop::Refused(r)
    }
}

impl From<Error> for Stop {
    fn from(e: Error) -> Stop {
        Stop::Refused(Refused::from(e))
    }
}

impl From<Failed> for Stop {
    fn from(f: Failed) -> Stop {
        Stop::Failed(f)
    }
}

fn run(
    n: &Native,
    p: &mut fenec_abi::Prepared,
    sql: &str,
    params: &str,
    vectors: &[u8],
) -> Result<Response, Stop> {
    // A read beside the other reads.
    if fenec_abi::read_only(p) {
        let db = n.read()?;
        fenec_abi::exact(&db, p, sql, params, vectors)?;
        return Ok(fenec_abi::query(&db, p)?);
    }
    // A write to a synced collection is the sync's: applied at once, and
    // queued for the server in the same block.
    {
        let mut sync = n.sync_lock();
        if let Some(sync) = sync.as_mut() {
            if sync.claims(&p.stmts)? {
                let mut db = n.write()?;
                fenec_abi::exact(&db, p, sql, params, vectors)?;
                let r = sync.write(&mut db, p, sql)?;
                let durability = match n.durable {
                    true => db.flush()?,
                    false => None,
                };
                drop(db);
                n.durable(durability)?;
                return Ok(r);
            }
        }
    }
    // A lone `create index` or `compact` is built beside the database, as a
    // server builds it: an HNSW index over 100 000 x 128 held the write lock
    // ~20 s, where reads now wait at most 21 ms (`Database::maintain`).
    if let [stmt] = p.stmts.as_slice() {
        if n.closed.load(Ordering::Acquire) {
            return Err(Stop::Failed(closed()));
        }
        if let Some(built) = Database::maintain(&n.db, stmt) {
            let r = built?;
            let durability = match n.durable {
                true => n.write()?.flush()?,
                false => None,
            };
            n.durable(durability)?;
            return Ok(r);
        }
    }
    let mut db = n.write()?;
    fenec_abi::exact(&db, p, sql, params, vectors)?;
    let before = db.change_seq();
    let r = fenec_abi::execute(&mut db, p);
    // A text holding a compact runs a statement at a time, so one that fails
    // may follow some that wrote: whatever moved the counter is synced.
    let durability = match n.durable && db.change_seq() != before {
        true => db.flush()?,
        false => None,
    };
    drop(db);
    n.durable(durability)?;
    Ok(r?)
}

// ---------------------------------------------------------------- changes

/// What has changed since `since`, as the browser module answers it:
/// `{"seq":N,"horizon":M,"collections":["a","b"]}`, `collections` null when
/// it cannot be told which -- everything is stale then. A live query asks
/// it once after a burst of writes, with the `seq` it last saw.
///
/// # Safety
/// `out` and `out_len` are null or valid to write.
#[no_mangle]
pub unsafe extern "C" fn fenec_changes(
    handle: u64,
    since: u64,
    out: *mut *mut c_char,
    out_len: *mut usize,
) -> i32 {
    call(out, out_len, || {
        let n = native(handle)?;
        let db = n.read()?;
        Ok(Some(fenec_abi::changes(&db, since)))
    })
}

// ------------------------------------------------------------- durability

/// Makes every write so far durable: written and fsynced, the fsync with no
/// lock held. What a handle opened with `FENEC_OPEN_NO_SYNC` calls when
/// its writes must last.
///
/// # Safety
/// `out` and `out_len` are null or valid to write.
#[no_mangle]
pub unsafe extern "C" fn fenec_sync(
    handle: u64,
    out: *mut *mut c_char,
    out_len: *mut usize,
) -> i32 {
    call(out, out_len, || {
        let n = native(handle)?;
        let durability = n.write()?.flush().map_err(|e| failed(&e))?;
        n.durable(durability).map_err(|e| failed(&e))?;
        Ok(None)
    })
}

/// Hands the buffered writes to the system with no fsync: they outlive the
/// app being killed, not the device losing power. Microseconds, where an
/// fsync is milliseconds: what an app does as it goes to the background.
///
/// # Safety
/// `out` and `out_len` are null or valid to write.
#[no_mangle]
pub unsafe extern "C" fn fenec_flush(
    handle: u64,
    out: *mut *mut c_char,
    out_len: *mut usize,
) -> i32 {
    call(out, out_len, || {
        let n = native(handle)?;
        let mut db = n.write()?;
        db.write_out().map_err(|e| failed(&e))?;
        Ok(None)
    })
}

/// Writes the file anew as an image of the database, graphs and all, so the
/// next open links nothing and reads no superseded record. Holds the write
/// lock while it writes.
///
/// # Safety
/// `out` and `out_len` are null or valid to write.
#[no_mangle]
pub unsafe extern "C" fn fenec_checkpoint(
    handle: u64,
    out: *mut *mut c_char,
    out_len: *mut usize,
) -> i32 {
    call(out, out_len, || {
        let n = native(handle)?;
        let mut db = n.write()?;
        db.checkpoint().map_err(|e| failed(&e))?;
        Ok(None)
    })
}

// ------------------------------------------------------------------- sync

/// What `fenec_sync_feed` is told: nothing (it hands back what is due), an
/// answer to a request, a stream's status, a piece of its body or its end, a
/// timer run out, or a signal.
pub const FENEC_SYNC_POLL: u32 = 0;
pub const FENEC_SYNC_RESPONSE: u32 = 1;
pub const FENEC_SYNC_OPENED: u32 = 2;
pub const FENEC_SYNC_BYTES: u32 = 3;
pub const FENEC_SYNC_CLOSED: u32 = 4;
pub const FENEC_SYNC_TIMER: u32 = 5;
pub const FENEC_SYNC_SIGNAL: u32 = 6;

/// Makes the database a replica that syncs with a server: `config` is
/// `{"url":..,"token":..,"seed":"<32 hex digits>","shapes":[{collection,
/// where?, select?, key?}]}`, `seed` from the platform's secure random
/// source (the keys of optimistic rows come from it). The sync's own
/// collections are made, and what they kept read. Writes the first
/// actions to perform, a JSON array (`fenec_abi::sync`). From here on a
/// write to a synced collection through `fenec_query` is applied at once
/// and queued for the server.
///
/// # Safety
/// `config` is valid for `config_len` bytes; `out` and `out_len` are null or
/// valid to write.
#[no_mangle]
pub unsafe extern "C" fn fenec_sync_start(
    handle: u64,
    config: *const u8,
    config_len: usize,
    out: *mut *mut c_char,
    out_len: *mut usize,
) -> i32 {
    call(out, out_len, || {
        let n = native(handle)?;
        let config = text(config, config_len, "the configuration")?;
        let mut slot = n.sync_lock();
        if slot.is_some() {
            return Err(misuse("the database syncs already"));
        }
        let mut db = n.write()?;
        let mut sync = fenec_abi::sync::Sync::start(&mut db, config).map_err(|e| failed(&e))?;
        let actions = sync.actions();
        *slot = Some(sync);
        Ok(Some(actions))
    })
}

/// Tells the sync what happened, and writes the actions now due: `kind` one
/// of the `FENEC_SYNC_*`, `id` the request, stream or timer it is about,
/// `status` a response's (0: no answer, `bytes` saying why), `seq` the
/// response's `Fenec-Seq` (0 for none), `bytes` a body, a piece of a stream,
/// why it ended, or a signal's JSON (`{"online":bool}`, `{"token":..}`,
/// `{"stop":true}`).
///
/// # Safety
/// `bytes` is null with length 0 or valid for `len`; `out` and `out_len`
/// are null or valid to write.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn fenec_sync_feed(
    handle: u64,
    kind: u32,
    id: u64,
    status: i32,
    seq: u64,
    bytes_ptr: *const u8,
    len: usize,
    out: *mut *mut c_char,
    out_len: *mut usize,
) -> i32 {
    call(out, out_len, || {
        let n = native(handle)?;
        let body = bytes(bytes_ptr, len);
        let mut slot = n.sync_lock();
        let Some(sync) = slot.as_mut() else {
            return Err(misuse("the database does not sync: fenec_sync_start first"));
        };
        let utf8 = || String::from_utf8_lossy(body);
        let status = status.clamp(0, u16::MAX as i32) as u16;
        if kind != FENEC_SYNC_POLL {
            let mut db = n.write()?;
            match kind {
                FENEC_SYNC_RESPONSE => sync.response(&mut db, id, status, seq, &utf8()),
                FENEC_SYNC_OPENED => sync.opened(&mut db, id, status, &utf8()),
                FENEC_SYNC_BYTES => sync.bytes(&mut db, id, body),
                FENEC_SYNC_CLOSED => sync.closed(&mut db, id, &utf8()),
                FENEC_SYNC_TIMER => sync.timer(&mut db, id),
                FENEC_SYNC_SIGNAL => sync.signal(&mut db, &utf8()).map_err(|e| failed(&e))?,
                _ => return Err(misuse(&format!("no sync event of kind {kind}"))),
            }
        }
        Ok(Some(sync.actions()))
    })
}

/// The sync's state:
/// `{"state":"online"|"offline"|"catching_up","pending":N,"error":null|
/// {"message":..,"status":N},"shapes":[{"collection","cursor","seeded","connected"}]}`.
///
/// # Safety
/// `out` and `out_len` are null or valid to write.
#[no_mangle]
pub unsafe extern "C" fn fenec_sync_status(
    handle: u64,
    out: *mut *mut c_char,
    out_len: *mut usize,
) -> i32 {
    call(out, out_len, || {
        let n = native(handle)?;
        let slot = n.sync_lock();
        match slot.as_ref() {
            Some(sync) => Ok(Some(sync.status())),
            None => Err(misuse("the database does not sync: fenec_sync_start first")),
        }
    })
}

/// Frees text the library handed out.
///
/// # Safety
/// `s` is null, or text a call wrote through its out pointer, freed once.
#[no_mangle]
pub unsafe extern "C" fn fenec_free_string(s: *mut c_char) {
    if !s.is_null() {
        drop(CString::from_raw(s));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_has_a_code_of_its_own() {
        let all = [
            Error::Type(String::new()),
            Error::NotFound(String::new()),
            Error::Exists(String::new()),
            Error::Duplicate(String::new()),
            Error::Corrupt(String::new()),
            Error::Query(String::new()),
            Error::Io(String::new()),
            Error::Plugin(String::new()),
            Error::ReadOnly(String::new()),
            Error::Denied(String::new()),
        ];
        let codes: Vec<i32> = all.iter().map(code).collect();
        assert_eq!(codes, (1..=10).collect::<Vec<_>>());
    }

    /// A panic comes back as a code and its message, never across the
    /// boundary.
    #[test]
    fn a_panic_stops_at_the_boundary() {
        let (mut out, mut len) = (std::ptr::null_mut(), 0);
        let code = unsafe { call(&mut out, &mut len, || panic!("a bug")) };
        assert_eq!(code, FENEC_PANIC);
        let text = unsafe { std::ffi::CStr::from_ptr(out) }
            .to_str()
            .unwrap()
            .to_string();
        unsafe { fenec_free_string(out) };
        assert_eq!(
            text,
            r#"{"kind":"error","message":"internal error: a bug"}"#
        );
        assert_eq!(len, text.len());
    }
}
