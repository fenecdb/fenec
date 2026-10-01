//! The browser binding -- **no wasm-bindgen**.
//!
//! Why the raw ABI: wasm-bindgen needs `wasm-bindgen-cli`/`wasm-pack` and an
//! npm chain, and it adds a fair amount of glue to the output. Everything
//! fenecdb needs in the browser is "take text, return text". The exported
//! surface is therefore plain C ABI, and the output of `cargo build --target
//! wasm32-unknown-unknown` runs directly through `WebAssembly.instantiate`.
//! The JS glue totals ~100 lines (`web/fenec.js`).
//!
//! ## Memory contract
//! Every returned buffer is `[u32 length (LE)][contents]`. Once the caller
//! has read the contents it must call `fenec_free(ptr, 4 + length)`.

use fenec_core::collate;
use fenec_core::engine::Kept;
use fenec_core::json;
use fenec_core::prelude::*;
use fenec_ql::parse;
use std::alloc::{alloc, dealloc, Layout};
use std::cell::RefCell;
use std::sync::{Arc, Mutex};

thread_local! {
    static HANDLES: RefCell<Vec<Option<Slot>>> = const { RefCell::new(Vec::new()) };
}

/// A handle's database, and the journal `fenec_journal` started for it. In
/// one slot rather than a second table: that table's code was 1.5 KB of the
/// module.
struct Slot {
    db: Database,
    journal: Option<Arc<Mutex<Journal>>>,
    /// The bytes the last `fenec_load` took (`fenec_loaded`).
    loaded: usize,
    /// An image `fenec_snapshot_chunks` began, its chunks yet to be taken.
    chunks: Chunks,
}

/// A snapshot's chunk: a mebibyte, written into and never grown.
const CHUNK: usize = 1 << 20;

/// An image taken [`CHUNK`] bytes at a time. Into one `Vec` it grew by
/// doubling, old and new side by side: a 12 MB image took the module's
/// memory, which WebAssembly never gives back, up by 37 MB. Made whole in
/// chunks, it stood beside the rows until the first was taken: 50 000
/// 128-dim rows at 73 MB went to 109 checkpointed. So the stores' own
/// bytes are held rather than copied ([`Kept`]) -- a segment written to
/// meanwhile is copied then, and the image is the database as it stood --
/// and each chunk is made as it is taken.
#[derive(Default)]
struct Chunks {
    parts: Vec<Part>,
    len: u64,
    /// The first part not taken whole, and how much of it has been.
    next: usize,
    head: usize,
}

enum Part {
    /// Written by the image itself: its records' heads, schemas, graphs.
    Own(Vec<u8>),
    Kept(Kept),
}

impl Part {
    fn bytes(&self) -> &[u8] {
        match self {
            Part::Own(v) => v,
            Part::Kept(k) => k.bytes(),
        }
    }
}

impl Chunks {
    /// The next chunk: [`CHUNK`] bytes, or what is left.
    fn take(&mut self) -> Vec<u8> {
        let mut out = Vec::with_capacity(CHUNK);
        while out.len() < CHUNK {
            let Some(part) = self.parts.get_mut(self.next) else {
                break;
            };
            let rest = &part.bytes()[self.head..];
            let n = rest.len().min(CHUNK - out.len());
            out.extend_from_slice(&rest[..n]);
            self.head += n;
            if self.head == part.bytes().len() {
                // Let go of as soon as it is taken.
                *part = Part::Own(Vec::new());
                self.next += 1;
                self.head = 0;
            }
        }
        out
    }
}

impl fenec_core::engine::ImageOut for Chunks {
    fn write(&mut self, mut bytes: &[u8]) -> fenec_core::error::Result<()> {
        self.len += bytes.len() as u64;
        while !bytes.is_empty() {
            if !matches!(self.parts.last(), Some(Part::Own(v)) if v.len() < CHUNK) {
                self.parts.push(Part::Own(Vec::with_capacity(CHUNK)));
            }
            let Some(Part::Own(part)) = self.parts.last_mut() else {
                break;
            };
            let n = (CHUNK - part.len()).min(bytes.len());
            part.extend_from_slice(&bytes[..n]);
            bytes = &bytes[n..];
        }
        Ok(())
    }

    fn write_kept(&mut self, kept: Kept) -> fenec_core::error::Result<()> {
        self.len += kept.len as u64;
        if kept.len > 0 {
            self.parts.push(Part::Kept(kept));
        }
        Ok(())
    }

    fn at(&self) -> u64 {
        self.len
    }

    fn patch(&mut self, at: u64, bytes: &[u8]) -> fenec_core::error::Result<()> {
        // The counter's header, once the image's length is known: in the
        // image's own bytes, the first it wrote.
        let mut start = 0usize;
        for part in self.parts.iter_mut() {
            let len = part.bytes().len();
            let at = at as usize;
            if at < start + len {
                if let Part::Own(v) = part {
                    if let Some(dst) = v.get_mut(at - start..at - start + bytes.len()) {
                        dst.copy_from_slice(bytes);
                        return Ok(());
                    }
                }
                break;
            }
            start += len;
        }
        Err(Error::Corrupt(
            "an image patched outside its own bytes".into(),
        ))
    }
}

/// What a page's database wrote since the page last took it: the frames
/// appended, or a whole image when a `compact` rewrote it, followed by the
/// frames after that. Either is what a file holds, so the page appends the
/// first to what it stored and replaces what it stored with the second.
#[derive(Default)]
struct Journal {
    image: Option<Vec<u8>>,
    tail: Vec<u8>,
}

/// The sink a journaling database writes through. `Arc<Mutex>` rather than
/// `Rc<RefCell>` only because a sink has to be `Send`; there is one thread.
struct JournalSink(Arc<Mutex<Journal>>);

impl Sink for JournalSink {
    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        let mut j = self.0.lock().unwrap_or_else(|e| e.into_inner());
        j.tail.extend_from_slice(bytes);
        Ok(())
    }
    fn rewrite(&mut self, bytes: &[u8]) -> Result<()> {
        let mut j = self.0.lock().unwrap_or_else(|e| e.into_inner());
        j.image = Some(bytes.to_vec());
        j.tail.clear();
        Ok(())
    }
}

// ------------------------------------------------------------- memory

/// Allocates a buffer of `len` bytes. The JS side uses it to write input.
#[no_mangle]
pub extern "C" fn fenec_alloc(len: usize) -> *mut u8 {
    if len == 0 {
        return std::ptr::NonNull::dangling().as_ptr();
    }
    unsafe { alloc(Layout::from_size_align_unchecked(len, 1)) }
}

/// Frees a `fenec_alloc` buffer or a returned one.
///
/// # Safety
/// `ptr` must come from `fenec_alloc(len)`, or be a returned buffer with
/// `len` its whole length (`4 +` the one written at its front), and not be
/// freed twice.
#[no_mangle]
pub unsafe extern "C" fn fenec_free(ptr: *mut u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    unsafe { dealloc(ptr, Layout::from_size_align_unchecked(len, 1)) }
}

/// Produces a `[u32 len][payload]` buffer and gives up ownership.
fn boxed(payload: &[u8]) -> *mut u8 {
    let total = 4 + payload.len();
    let ptr = fenec_alloc(total);
    unsafe {
        let len_bytes = (payload.len() as u32).to_le_bytes();
        std::ptr::copy_nonoverlapping(len_bytes.as_ptr(), ptr, 4);
        std::ptr::copy_nonoverlapping(payload.as_ptr(), ptr.add(4), payload.len());
    }
    ptr
}

/// The text JavaScript handed over, read where it lies: the page's buffer
/// outlives the call. Copied out, as it was, a page of 200 768-dim vectors'
/// parameters were 3 MB more to write before the first was read, and took
/// 23.7 ms to read against 21.9.
unsafe fn str_from<'a>(ptr: *const u8, len: usize) -> std::borrow::Cow<'a, str> {
    if ptr.is_null() || len == 0 {
        return "".into();
    }
    let bytes = std::slice::from_raw_parts(ptr, len);
    match std::str::from_utf8(bytes) {
        Ok(s) => s.into(),
        Err(_) => String::from_utf8_lossy(bytes),
    }
}

// --------------------------------------------------------- lifecycle

/// Opens a new in-memory database and returns a handle.
#[no_mangle]
pub extern "C" fn fenec_open() -> u32 {
    HANDLES.with(|h| {
        let mut h = h.borrow_mut();
        h.push(Some(Slot {
            db: Database::new(),
            journal: None,
            loaded: 0,
            chunks: Chunks::default(),
        }));
        (h.len() - 1) as u32
    })
}

/// Closes the database and releases its memory.
#[no_mangle]
pub extern "C" fn fenec_close(handle: u32) {
    HANDLES.with(|h| {
        if let Some(slot) = h.borrow_mut().get_mut(handle as usize) {
            *slot = None;
        }
    });
}

#[no_mangle]
pub extern "C" fn fenec_version() -> *mut u8 {
    boxed(fenec_core::VERSION.as_bytes())
}

/// Generic, so every caller compiles its own `LocalKey::with`, 17 of them.
/// Handed its closure as `dyn` through one function that reached `HANDLES`,
/// they went down to 9 and the module grew by 319 bytes: the work moved into
/// each caller's closure, with the glue around it.
fn with_slot<T>(handle: u32, f: impl FnOnce(&mut Slot) -> T) -> Option<T> {
    HANDLES.with(|h| {
        let mut h = h.borrow_mut();
        h.get_mut(handle as usize).and_then(|s| s.as_mut()).map(f)
    })
}

fn with_db<T>(handle: u32, f: impl FnOnce(&mut Database) -> T) -> Option<T> {
    with_slot(handle, |s| f(&mut s.db))
}

// ------------------------------------------------------------- query

/// Runs FenecQL. `params` is a JSON array (it may be empty), and `vectors`
/// the parameters that are vectors, handed over as `f32`s rather than as
/// text (`with_vectors`); a client with none passes none.
/// Returns: JSON (`{"kind":"rows"|"affected"|"ok"|"schemas"|"error", ...}`).
///
/// # Safety
/// `sql_ptr`/`params_ptr`/`vectors_ptr` must be valid and of the given length.
#[no_mangle]
pub unsafe extern "C" fn fenec_query(
    handle: u32,
    sql_ptr: *const u8,
    sql_len: usize,
    params_ptr: *const u8,
    params_len: usize,
    vectors_ptr: *const u8,
    vectors_len: usize,
) -> *mut u8 {
    let sql = str_from(sql_ptr, sql_len);
    let params_src = str_from(params_ptr, params_len);
    let vectors = match vectors_ptr.is_null() || vectors_len == 0 {
        true => &[][..],
        false => std::slice::from_raw_parts(vectors_ptr, vectors_len),
    };

    let out = run(handle, &sql, &params_src, vectors);
    boxed(out.as_bytes())
}

/// The parameters with each vector handed over as `f32`s put in its place:
/// for each, its place among the parameters and its length as two
/// little-endian `u32`s, then its values, where the JSON holds `null`.
/// Written out as text and read back, a page of 200 768-dim vectors spent
/// most of its time on the numbers' digits, both sides of the call.
fn with_vectors(mut params: Vec<Value>, mut bytes: &[u8]) -> Result<Vec<Value>> {
    while let Some((&[a0, a1, a2, a3, n0, n1, n2, n3], rest)) = bytes.split_first_chunk() {
        let at = u32::from_le_bytes([a0, a1, a2, a3]) as usize;
        let n = u32::from_le_bytes([n0, n1, n2, n3]) as usize;
        let body = n.checked_mul(4).and_then(|len| rest.split_at_checked(len));
        let slot = params.get_mut(at).filter(|p| p.is_null());
        let (Some((body, rest)), Some(slot)) = (body, slot) else {
            break;
        };
        *slot = Value::Vector(fenec_core::codec::f32s(body));
        bytes = rest;
    }
    match bytes.is_empty() {
        true => Ok(params),
        false => Err(Error::Query("malformed vector parameters".into())),
    }
}

/// Whether the parameter at `at` was handed over as `f32`s ([`with_vectors`]).
fn apart(mut bytes: &[u8], at: usize) -> bool {
    while let Some((&[a0, a1, a2, a3, n0, n1, n2, n3], rest)) = bytes.split_first_chunk() {
        if u32::from_le_bytes([a0, a1, a2, a3]) as usize == at {
            return true;
        }
        let n = u32::from_le_bytes([n0, n1, n2, n3]) as usize;
        bytes = rest.get(n.saturating_mul(4)..).unwrap_or_default();
    }
    false
}

fn run(handle: u32, sql: &str, params_src: &str, vectors: &[u8]) -> String {
    let mut params = match json::parse_params(params_src).and_then(|p| with_vectors(p, vectors)) {
        Ok(p) => p,
        Err(e) => return json::error_to_string(&e),
    };
    let mut stmts = match parse(sql) {
        Ok(s) => s,
        Err(e) => return json::error_to_string(&e),
    };
    // A note left by anything before is not these statements'.
    collate::take_missing();
    // The places of those to send as JSON, written as the answer lists them.
    let mut as_json = String::new();
    let res = with_db(handle, |db| {
        // A list of numbers a json field is handed, or a path compared
        // with, read again as written: from the text, from the parameters'
        // JSON, and from neither where it came over as `f32`s -- the page
        // is told which to send as JSON (`"exact"`), before anything runs.
        let vectored = stmts.iter().any(|s| s.reads_vectors())
            || params.iter().any(fenec_core::query::holds_vector);
        if vectored {
            let (mut text, mut exact) = (false, false);
            for s in &stmts {
                let need = db.exactly(s);
                text |= need.text;
                for i in need.params {
                    exact = true;
                    if apart(vectors, i) {
                        if !as_json.is_empty() {
                            as_json.push(',');
                        }
                        as_json.push_str(&i.to_string());
                    }
                }
            }
            if !as_json.is_empty() {
                return Err((Error::Query(String::new()), 0));
            }
            if text {
                stmts = fenec_ql::parse_exact(sql).map_err(|e| (e, 0))?;
            }
            if exact {
                params = json::parse_params_exact(params_src)
                    .and_then(|p| with_vectors(p, vectors))
                    .map_err(|e| (e, 0))?;
            }
        }
        // The statements are one block: their writes -- a create, a drop or
        // a create index among them, as a page setting itself up sends --
        // land together or not at all, and one refused for collation data
        // puts back the ones before it, so the page runs the whole text
        // again. A text with a compact runs a statement at a time, each
        // write on its own a block.
        let block = stmts.len() > 1 && stmts.iter().all(|s| s.fits_block());
        if block {
            db.begin().map_err(|e| (e, 0))?;
        }
        let mut last = Response::Ok("empty".into());
        for (ran, s) in stmts.iter().enumerate() {
            match db.execute_with(s, &params) {
                Ok(r) => last = r,
                Err(e) if block => {
                    db.rollback();
                    return Err((e, 0));
                }
                Err(e) => return Err((e, ran)),
            }
        }
        if block {
            db.commit().map_err(|e| (e, 0))?;
        }
        Ok(last)
    });
    match res {
        None => json::error_to_string(&Error::NotFound(format!("handle {handle}"))),
        // The places to send as JSON, for the page to send them so.
        Some(Err(_)) if !as_json.is_empty() => format!(
            "{{\"kind\":\"error\",\"message\":\"a json field is handed a list of numbers \
             sent over as f32s\",\"exact\":[{as_json}]}}"
        ),
        Some(Err((e, ran))) => refused(&e, ran),
        Some(Ok(r)) => json::response_to_string(&r),
    }
}

/// An error as JSON, and when what refused the statement was collation data
/// the module has not been handed, which (`"chunks"`) and how many of the
/// statements before it ran (`"ran"`, left out when none did): the client
/// runs one again by itself only when nothing before it had.
fn refused(e: &Error, ran: usize) -> String {
    let mut out = json::error_to_string(e);
    let missing = collate::take_missing();
    if missing != 0 {
        out.pop();
        out.push_str(",\"chunks\":[");
        for (i, name) in collate::chunk_names(missing).enumerate() {
            if i > 0 {
                out.push(',');
            }
            json::escape_into(&mut out, name);
        }
        out.push(']');
        if ran > 0 {
            out.push_str(&format!(",\"ran\":{ran}"));
        }
        out.push('}');
    }
    out
}

// ----------------------------------------------------------- collation

/// The collation data: `{"chunks":[names],"loaded":mask}`, a chunk's bit its
/// place in the list. The module carries `latin`; the others are handed to
/// it (`fenec_add_chunk`) from `collate/<name>.bin` beside it.
#[no_mangle]
pub extern "C" fn fenec_collation() -> *mut u8 {
    let mut s = String::from("{\"chunks\":[");
    for (i, name) in collate::CHUNKS.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        json::escape_into(&mut s, name);
    }
    s.push_str(&format!("],\"loaded\":{}}}", collate::loaded()));
    boxed(s.as_bytes())
}

/// Hands the module a chunk of collation data, the bytes of its `.bin`.
/// Returns its place in `fenec_collation`'s list, or -1 for bytes that are
/// not one of this module's chunks -- another version's among them.
///
/// # Safety
/// `ptr` must be valid and `len` bytes long.
#[no_mangle]
pub unsafe extern "C" fn fenec_add_chunk(ptr: *const u8, len: usize) -> i32 {
    if ptr.is_null() {
        return -1;
    }
    let bytes = std::slice::from_raw_parts(ptr, len);
    match collate::add_chunk(bytes) {
        Some(name) => collate::CHUNKS
            .iter()
            .position(|n| *n == name)
            .map_or(-1, |i| i as i32),
        None => -1,
    }
}

// ------------------------------------------------------------- changes

/// What has changed since `since`:
/// `{"seq":N,"horizon":M,"collections":["a","b"]}`.
///
/// When `collections` is **null**, the cursor fell behind the ring and it
/// cannot be known which collection changed: the caller must treat
/// everything as stale. Collection granularity is enough for live queries --
/// re-running a local query is already sub-millisecond, and incremental
/// bookkeeping does not pay for itself on that budget.
///
/// Why `since` is an `f64`: a JS number is already an `f64` and the change
/// counter stays exact up to 2^53. As an `i64` it would need a `BigInt`
/// conversion at the ABI boundary -- buying only a range never reached.
#[no_mangle]
pub extern "C" fn fenec_changes(handle: u32, since: f64) -> *mut u8 {
    let since = if since.is_finite() && since >= 0.0 {
        since as u64
    } else {
        0
    };
    let out = with_db(handle, |db| {
        let mut s = format!(
            "{{\"seq\":{},\"horizon\":{},\"collections\":",
            db.change_seq(),
            db.change_horizon()
        );
        match db.changed_collections_since(since) {
            None => s.push_str("null"),
            Some(names) => {
                s.push('[');
                for (i, n) in names.iter().enumerate() {
                    if i > 0 {
                        s.push(',');
                    }
                    json::escape_into(&mut s, n);
                }
                s.push(']');
            }
        }
        s.push('}');
        s
    })
    .unwrap_or_else(|| "{\"seq\":0,\"horizon\":0,\"collections\":null}".to_string());
    boxed(out.as_bytes())
}

/// Sets the entry count of the change ring.
#[no_mangle]
pub extern "C" fn fenec_set_change_capacity(handle: u32, n: u32) {
    with_db(handle, |db| db.set_change_capacity(n.max(1) as usize));
}

// --------------------------------------------------------- persistence

/// Begins the database's image, to be taken [`CHUNK`] bytes at a time by
/// `fenec_snapshot_chunk`, and returns how many: the page stores each and
/// lets it go before the next is made, so the image is never whole in the
/// module, nor in the page -- a Durable Object's storage takes it a piece
/// at a time anyway.
#[no_mangle]
pub extern "C" fn fenec_snapshot_chunks(handle: u32) -> u32 {
    with_slot(handle, |s| {
        let mut out = Chunks::default();
        let _ = s.db.snapshot_into(&mut out);
        let n = (out.len as usize).div_ceil(CHUNK);
        s.chunks = out;
        n as u32
    })
    .unwrap_or(0)
}

/// The next chunk `fenec_snapshot_chunks` made, let go of here; empty when
/// none is left.
#[no_mangle]
pub extern "C" fn fenec_snapshot_chunk(handle: u32) -> *mut u8 {
    let part = with_slot(handle, |s| s.chunks.take()).unwrap_or_default();
    boxed(&part)
}

/// Starts keeping the database's writes for `fenec_drain`, or with `off`
/// stops. Off until asked for: a page that never drains would hold every
/// write it made -- which is why a page that lets its file go stops it. What
/// was written before is not kept; the page stores a snapshot first.
#[no_mangle]
pub extern "C" fn fenec_journal(handle: u32, off: u32) {
    with_slot(handle, |s| {
        if off != 0 {
            s.db.set_sink(Box::new(fenec_core::engine::NullSink));
            s.journal = None;
            return;
        }
        let journal = Arc::new(Mutex::new(Journal::default()));
        s.db.set_sink(Box::new(JournalSink(Arc::clone(&journal))));
        s.journal = Some(journal);
    });
}

/// What the database wrote since the last drain, and forgets it: `[0]` and
/// the frames to append to what the page stored, or `[1]` and an image to
/// replace it with. Empty frames when nothing was written or no journal
/// was started.
#[no_mangle]
pub extern "C" fn fenec_drain(handle: u32) -> *mut u8 {
    let taken = with_slot(handle, |s| {
        let mut j = s
            .journal
            .as_ref()?
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        Some((j.image.take(), std::mem::take(&mut j.tail)))
    })
    .flatten();
    let mut out = Vec::new();
    match taken {
        Some((Some(image), tail)) => {
            out.push(1);
            out.extend_from_slice(&image);
            out.extend_from_slice(&tail);
        }
        Some((None, tail)) => {
            out.push(0);
            out.extend_from_slice(&tail);
        }
        None => out.push(0),
    }
    boxed(&out)
}

/// Loads from a byte image: 0 when it did, 1 when the bytes are not one,
/// and bit 1 when its collated text needs collation data the module has not
/// been handed -- a bit a chunk from bit 2, a chunk's bit its place in
/// `fenec_collation`'s list. Its `@sorted` indexes were then built comparing
/// without it, so the database is emptied, to be loaded again once the
/// module has it.
///
/// # Safety
/// `ptr` must be valid and `len` bytes long.
#[no_mangle]
pub unsafe extern "C" fn fenec_load(handle: u32, ptr: *const u8, len: usize) -> i32 {
    if ptr.is_null() || len == 0 {
        return 1;
    }
    let bytes = std::slice::from_raw_parts(ptr, len);
    collate::take_missing();
    let out = with_slot(handle, |s| {
        s.loaded = s.db.load(bytes).ok()?;
        let missing = s.db.collation_missing() | collate::take_missing();
        if missing != 0 {
            let mut empty = Database::new();
            empty.set_change_capacity(s.db.change_capacity());
            if let Some(j) = &s.journal {
                empty.set_sink(Box::new(JournalSink(Arc::clone(j))));
            }
            s.db = empty;
        }
        Some(missing)
    });
    match out.flatten() {
        Some(0) => 0,
        Some(missing) => (2 | missing << 2) as i32,
        None => 1,
    }
}

/// [`fenec_load`] over bytes it takes: an allocation `fenec_alloc` made
/// for `len`, which the database keeps and reads its documents from rather
/// than copy them out of it. The caller does not free it.
///
/// # Safety
/// `ptr` must come from `fenec_alloc(len)` and not be used after.
#[no_mangle]
pub unsafe extern "C" fn fenec_load_owned(handle: u32, ptr: *mut u8, len: usize) -> i32 {
    if ptr.is_null() || len == 0 {
        return 1;
    }
    let bytes: Vec<u8> = unsafe { Vec::from_raw_parts(ptr, len, len) };
    let base: fenec_core::store::Base = Arc::new(bytes);
    collate::take_missing();
    let out = with_slot(handle, |s| {
        s.loaded = s.db.load_mapped(base).ok()?;
        let missing = s.db.collation_missing() | collate::take_missing();
        if missing != 0 {
            let mut empty = Database::new();
            empty.set_change_capacity(s.db.change_capacity());
            if let Some(j) = &s.journal {
                empty.set_sink(Box::new(JournalSink(Arc::clone(j))));
            }
            s.db = empty;
        }
        Some(missing)
    });
    match out.flatten() {
        Some(0) => 0,
        Some(missing) => (2 | missing << 2) as i32,
        None => 1,
    }
}

/// The bytes the last `fenec_load` took: all of them, or those before a
/// last record a crash cut short. A page's file is cut back there before
/// anything is appended to it: a write appended after the torn bytes is
/// read back as the rest of them, and the next load loses it.
#[no_mangle]
pub extern "C" fn fenec_loaded(handle: u32) -> usize {
    with_slot(handle, |s| s.loaded).unwrap_or(0)
}

/// Returns the collection statistics as JSON.
#[no_mangle]
pub extern "C" fn fenec_stats(handle: u32) -> *mut u8 {
    let out = with_db(handle, |db| {
        let mut s = String::from("[");
        for (i, st) in db.stats().iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str("{\"name\":");
            json::escape_into(&mut s, &st.name);
            s.push_str(&format!(
                ",\"documents\":{},\"bytes\":{},\"dead_bytes\":{},\"segments\":{},\"vector_indexes\":[",
                st.documents, st.bytes, st.dead_bytes, st.segments
            ));
            for (j, v) in st.vector_indexes.iter().enumerate() {
                if j > 0 {
                    s.push(',');
                }
                s.push_str("{\"field\":");
                json::escape_into(&mut s, &v.field);
                s.push_str(&format!(
                    ",\"count\":{},\"dim\":{},\"arena_bytes\":{},\"precision\":\"{}\"}}",
                    v.count,
                    v.dim,
                    v.arena_bytes,
                    if v.precision == fenec_core::value::VecPrec::F16 { "f16" } else { "f32" }
                ));
            }
            s.push_str("]}");
        }
        s.push(']');
        s
    })
    .unwrap_or_else(|| "[]".to_string());
    boxed(out.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_through_abi() {
        let h = fenec_open();
        let r = run(
            h,
            "create collection t (a int, e vector<2> @hnsw(l2))",
            "",
            &[],
        );
        assert!(r.contains("\"ok\""), "{r}");
        let r = run(h, r#"put t {a: 1, e: [1.0, 0.0]}"#, "", &[]);
        assert!(r.contains("\"affected\""), "{r}");
        let r = run(h, "get t near e $1 limit 1", "[[1.0, 0.0]]", &[]);
        assert!(r.contains("_score"), "{r}");
        let r = run(h, "broken query", "", &[]);
        assert!(r.contains("\"error\""), "{r}");
        fenec_close(h);
    }

    /// A vector handed over as `f32`s lands where the JSON holds its `null`,
    /// and a list that does not fit the parameters is refused whole.
    #[test]
    fn vectors_beside_the_json() {
        let h = fenec_open();
        run(h, "create collection t (a int, e vector<2>)", "", &[]);
        let vector = |at: u32, n: u32, xs: &[f32]| {
            let mut b = [at.to_le_bytes(), n.to_le_bytes()].concat();
            xs.iter().for_each(|x| b.extend(x.to_le_bytes()));
            b
        };
        let good = vector(1, 2, &[0.5, 0.25]);
        let r = run(h, "put t {a: $1, e: $2}", "[7, null]", &good);
        assert!(r.contains("\"affected\""), "{r}");
        let r = run(h, "get t select a, e", "", &[]);
        assert!(r.contains(r#"{"a":7,"e":[0.5,0.25]}"#), "{r}");
        for bad in [
            vector(0, 2, &[0.5, 0.25]),
            vector(2, 2, &[0.5, 0.25]),
            vector(1, 3, &[0.5, 0.25]),
            vector(u32::MAX, u32::MAX, &[]),
            [good.clone(), vec![1]].concat(),
            good[..6].to_vec(),
        ] {
            let r = run(h, "put t {a: $1, e: $2}", "[7, null]", &bad);
            assert!(r.contains("malformed vector parameters"), "{r}");
        }
        fenec_close(h);
    }

    #[test]
    fn changes_through_abi() {
        let h = fenec_open();
        run(h, "create collection t (a int)", "", &[]);
        let at = fenec_changes(h, 0.0);
        let seq_only = read(at);
        assert!(seq_only.contains("\"collections\":[\"t\"]"), "{seq_only}");

        // Shrink the ring and overflow it to check that the horizon rises.
        fenec_set_change_capacity(h, 1);
        run(h, "put t [{a: 1}, {a: 2}]", "", &[]);
        let out = read(fenec_changes(h, 0.0));
        assert!(out.contains("\"collections\":null"), "{out}");
        fenec_close(h);
    }

    /// Turns a `boxed` buffer into a string (`[u32 len][contents]`).
    fn read(ptr: *mut u8) -> String {
        unsafe {
            let mut len = [0u8; 4];
            std::ptr::copy_nonoverlapping(ptr, len.as_mut_ptr(), 4);
            let len = u32::from_le_bytes(len) as usize;
            let s =
                String::from_utf8_lossy(std::slice::from_raw_parts(ptr.add(4), len)).into_owned();
            fenec_free(ptr, 4 + len);
            s
        }
    }
}
