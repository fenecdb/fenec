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

use std::alloc::{alloc, dealloc, Layout};
use std::cell::RefCell;
use fenec_core::json;
use fenec_core::prelude::*;
use fenec_ql::parse;

thread_local! {
    static HANDLES: RefCell<Vec<Option<Database>>> = const { RefCell::new(Vec::new()) };
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
#[no_mangle]
pub extern "C" fn fenec_free(ptr: *mut u8, len: usize) {
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

unsafe fn str_from(ptr: *const u8, len: usize) -> String {
    if ptr.is_null() || len == 0 {
        return String::new();
    }
    String::from_utf8_lossy(std::slice::from_raw_parts(ptr, len)).into_owned()
}

// --------------------------------------------------------- lifecycle

/// Opens a new in-memory database and returns a handle.
#[no_mangle]
pub extern "C" fn fenec_open() -> u32 {
    HANDLES.with(|h| {
        let mut h = h.borrow_mut();
        h.push(Some(Database::new()));
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
    })
}

#[no_mangle]
pub extern "C" fn fenec_version() -> *mut u8 {
    boxed(fenec_core::VERSION.as_bytes())
}

fn with_db<T>(handle: u32, f: impl FnOnce(&mut Database) -> T) -> Option<T> {
    HANDLES.with(|h| {
        let mut h = h.borrow_mut();
        match h.get_mut(handle as usize).and_then(|s| s.as_mut()) {
            Some(db) => Some(f(db)),
            None => None,
        }
    })
}

// ------------------------------------------------------------- query

/// Runs FenecQL. `params` is a JSON array (it may be empty).
/// Returns: JSON (`{"kind":"rows"|"affected"|"ok"|"schemas"|"error", ...}`).
///
/// # Safety
/// `sql_ptr`/`params_ptr` must be valid and of the given length.
#[no_mangle]
pub unsafe extern "C" fn fenec_query(
    handle: u32,
    sql_ptr: *const u8,
    sql_len: usize,
    params_ptr: *const u8,
    params_len: usize,
) -> *mut u8 {
    let sql = str_from(sql_ptr, sql_len);
    let params_src = str_from(params_ptr, params_len);

    let out = run(handle, &sql, &params_src);
    boxed(out.as_bytes())
}

fn run(handle: u32, sql: &str, params_src: &str) -> String {
    let params = match json::parse_params(params_src) {
        Ok(p) => p,
        Err(e) => return json::error_to_string(&e),
    };
    let stmts = match parse(sql) {
        Ok(s) => s,
        Err(e) => return json::error_to_string(&e),
    };
    let res = with_db(handle, |db| {
        let mut last = Response::Ok("empty".into());
        for s in &stmts {
            match db.execute_with(s, &params) {
                Ok(r) => last = r,
                Err(e) => return Err(e),
            }
        }
        Ok(last)
    });
    match res {
        None => json::error_to_string(&Error::NotFound(format!("handle {handle}"))),
        Some(Err(e)) => json::error_to_string(&e),
        Some(Ok(r)) => json::response_to_string(&r),
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

/// Returns the full byte image of the database. The JS side writes it to
/// IndexedDB or OPFS for persistence across sessions.
#[no_mangle]
pub extern "C" fn fenec_snapshot(handle: u32) -> *mut u8 {
    match with_db(handle, |db| db.snapshot()) {
        Some(bytes) => boxed(&bytes),
        None => boxed(&[]),
    }
}

/// Loads from a byte image. 0 = success, 1 = error.
///
/// # Safety
/// `ptr` must be valid and `len` bytes long.
#[no_mangle]
pub unsafe extern "C" fn fenec_load(handle: u32, ptr: *const u8, len: usize) -> i32 {
    if ptr.is_null() || len == 0 {
        return 1;
    }
    let bytes = std::slice::from_raw_parts(ptr, len);
    match with_db(handle, |db| db.load(bytes)) {
        Some(Ok(())) => 0,
        _ => 1,
    }
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
        let r = run(h, "create collection t (a int, e vector<2> @hnsw(l2))", "");
        assert!(r.contains("\"ok\""), "{r}");
        let r = run(h, r#"put t {a: 1, e: [1.0, 0.0]}"#, "");
        assert!(r.contains("\"affected\""), "{r}");
        let r = run(h, "get t near e $1 limit 1", "[[1.0, 0.0]]");
        assert!(r.contains("_score"), "{r}");
        let r = run(h, "broken query", "");
        assert!(r.contains("\"error\""), "{r}");
        fenec_close(h);
    }

    #[test]
    fn changes_through_abi() {
        let h = fenec_open();
        run(h, "create collection t (a int)", "");
        let at = fenec_changes(h, 0.0);
        let seq_only = read(at);
        assert!(seq_only.contains("\"collections\":[\"t\"]"), "{seq_only}");

        // Shrink the ring and overflow it to check that the horizon rises.
        fenec_set_change_capacity(h, 1);
        run(h, "put t [{a: 1}, {a: 2}]", "");
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
            let s = String::from_utf8_lossy(std::slice::from_raw_parts(ptr.add(4), len)).into_owned();
            fenec_free(ptr, 4 + len);
            s
        }
    }
}
