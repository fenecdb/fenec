//! A handle opened with `FENEC_OPEN_NO_SYNC` writes its buffer into the
//! file once the call has let go of the database's lock
//! (`Database::write_behind`), as a server's buffered writes are: the bytes
//! reach the file as each call returns once they amount to 64 KB, the file
//! the same byte for byte as one every write of which was fsynced; a write
//! the disk refuses there stops the writes as a failed fsync does; a reader
//! -- and another writer -- go on while the buffer is being written; and
//! what a sync applies from its server, which no fsync follows on any
//! handle, is written behind the lock as well.
//!
//! The disk is held or made to fail at the file's sink, wrapped as the
//! library opens it (`open_wrapped`), so each test waits for an event, not
//! for a time.

use fenec_core::engine::{Durability, ImageOut, Sink};
use fenec_core::error::{Error, Result};
use fenec_ffi::*;
use std::ffi::{c_char, CStr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What a test waits for at most for something that must happen: a bound
/// on a hang, never on how soon a thread runs.
const MUST: Duration = Duration::from_secs(60);

const DIM: usize = 128;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fenec-ffi-behind-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn taken(code: i32, out: *mut c_char) -> (i32, String) {
    let text = match out.is_null() {
        true => String::new(),
        false => unsafe {
            let s = CStr::from_ptr(out).to_str().unwrap().to_string();
            fenec_free_string(out);
            s
        },
    };
    (code, text)
}

fn open(path: &Path, flags: u32) -> u64 {
    let p = path.to_str().unwrap();
    let (mut h, mut out) = (0, std::ptr::null_mut());
    let code = unsafe {
        fenec_open(
            p.as_ptr(),
            p.len(),
            flags,
            &mut h,
            &mut out,
            std::ptr::null_mut(),
        )
    };
    assert_eq!(taken(code, out), (FENEC_OK, String::new()));
    h
}

/// The file's sink handed to `wrap` as the library opens it.
fn open_with(path: &Path, hook: Hook) -> u64 {
    let flags = FENEC_OPEN_NO_SYNC | FENEC_OPEN_NO_AUTO_COMPACT;
    open_wrapped(
        path.to_str().unwrap(),
        flags,
        Box::new(move |inner| Ok(Box::new(Wrapped { inner, hook }) as Box<dyn Sink>)),
    )
    .unwrap()
}

fn query_with(h: u64, sql: &str, params: &str, vectors: &[u8]) -> (i32, String) {
    let mut out = std::ptr::null_mut();
    let code = unsafe {
        fenec_query(
            h,
            sql.as_ptr(),
            sql.len(),
            params.as_ptr(),
            params.len(),
            vectors.as_ptr(),
            vectors.len(),
            &mut out,
            std::ptr::null_mut(),
        )
    };
    taken(code, out)
}

fn ok(h: u64, sql: &str, params: &str, vectors: &[u8]) -> String {
    let (code, text) = query_with(h, sql, params, vectors);
    assert_eq!(code, FENEC_OK, "{sql}: {text}");
    text
}

fn by_handle(
    f: unsafe extern "C" fn(u64, *mut *mut c_char, *mut usize) -> i32,
    h: u64,
) -> (i32, String) {
    let mut out = std::ptr::null_mut();
    let code = unsafe { f(h, &mut out, std::ptr::null_mut()) };
    taken(code, out)
}

fn count(h: u64) -> u64 {
    let text = ok(h, "get r count", "", &[]);
    let at = text.find("\"count\":").expect(&text) + 8;
    let digits: String = text[at..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().unwrap()
}

/// A text of `rows` rows with a 128-dim vector each, the vectors handed
/// over as `f32`s as the bindings hand them: 200 rows are 106 KB of record,
/// past the 64 KB a write behind waits for.
fn text(from: usize, rows: usize) -> (String, String, Vec<u8>) {
    let (mut sql, mut bytes) = (String::from("put r ["), Vec::new());
    for i in 0..rows {
        if i > 0 {
            sql.push_str(", ");
        }
        sql.push_str(&format!(
            "{{k: \"row {}\", n: {}, e: ${}}}",
            from + i,
            from + i,
            i + 1
        ));
        bytes.extend((i as u32).to_le_bytes());
        bytes.extend((DIM as u32).to_le_bytes());
        for d in 0..DIM {
            bytes.extend((((from + i) * 7 + d) as f32 / 1000.0).to_le_bytes());
        }
    }
    sql.push(']');
    (sql, format!("[{}]", vec!["null"; rows].join(",")), bytes)
}

fn write(h: u64, from: usize, rows: usize) -> String {
    let (sql, params, bytes) = text(from, rows);
    ok(h, &sql, &params, &bytes)
}

const SCHEMA: &str = "create collection r (k text, n int, e vector<128>)";

fn len(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().len()
}

/// What a test does to the buffer's write a call hands back, before it
/// runs it.
type Hook = Arc<dyn Fn(Durability) -> Durability + Send + Sync>;

/// The file's sink with every call passed on, the buffer's write through
/// `hook`.
struct Wrapped {
    inner: Box<dyn Sink>,
    hook: Hook,
}

impl Sink for Wrapped {
    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        self.inner.append(bytes)
    }
    fn record(&mut self, seq: u64, bytes: &[u8]) -> Result<()> {
        self.inner.record(seq, bytes)
    }
    fn append_deferred(&mut self, bytes: &[u8]) -> Result<()> {
        self.inner.append_deferred(bytes)
    }
    fn rewrite(&mut self, bytes: &[u8]) -> Result<()> {
        self.inner.rewrite(bytes)
    }
    fn rewrite_with(
        &mut self,
        image: &mut dyn FnMut(&mut dyn ImageOut) -> Result<()>,
    ) -> Result<()> {
        self.inner.rewrite_with(image)
    }
    fn remapped(&mut self) -> Option<fenec_core::store::Base> {
        self.inner.remapped()
    }
    fn written_through(&mut self) -> Result<Option<fenec_core::store::Base>> {
        self.inner.written_through()
    }
    fn syncing(&self) -> bool {
        self.inner.syncing()
    }
    fn land(&mut self, seq: u64, spilled: &[&[u8]], record: &[u8]) -> Result<()> {
        self.inner.land(seq, spilled, record)
    }
    fn side(&self) -> Option<PathBuf> {
        self.inner.side()
    }
    fn adopt(&mut self, side: &Path) -> Result<()> {
        self.inner.adopt(side)
    }
    fn sync_existing(&mut self) -> Result<()> {
        self.inner.sync_existing()
    }
    fn write_out(&mut self) -> Result<()> {
        self.inner.write_out()
    }
    fn write_behind(&mut self, all: bool) -> Option<Durability> {
        self.inner.write_behind(all).map(|w| (self.hook)(w))
    }
    fn written_all(&self) -> bool {
        self.inner.written_all()
    }
    fn sync(&mut self) -> Result<()> {
        self.inner.sync()
    }
    fn flush(&mut self) -> Result<Option<Durability>> {
        self.inner.flush()
    }
}

/// Every text past 64 KB is in the file as its call returns, with no flush
/// or sync -- where the appends left up to a mebibyte to the next -- and a
/// copy taken then opens to every row; a few small writes wait in the
/// buffer until `fenec_flush`, which writes them all; and the file is, byte
/// for byte, the one a handle fsyncing every write leaves.
#[test]
fn the_buffer_written_behind_the_lock_lands_in_the_file() {
    let dir = scratch("lands");
    let (a_path, b_path) = (dir.join("behind.fenec"), dir.join("durable.fenec"));
    let a = open(&a_path, FENEC_OPEN_NO_SYNC | FENEC_OPEN_NO_AUTO_COMPACT);
    let b = open(&b_path, FENEC_OPEN_NO_AUTO_COMPACT);
    ok(a, SCHEMA, "", &[]);
    ok(b, SCHEMA, "", &[]);
    let mut rows = 0;
    for _ in 0..30 {
        assert_eq!(write(a, rows, 200), write(b, rows, 200));
        rows += 200;
        assert_eq!(len(&a_path), len(&b_path), "after {rows} rows");
    }
    // Taken now, with the file open and nothing flushed, a copy holds every
    // row written so far.
    let copy = dir.join("copy.fenec");
    std::fs::copy(&a_path, &copy).unwrap();
    let c = open(&copy, FENEC_OPEN_NO_AUTO_COMPACT);
    assert_eq!(count(c), count(b));
    assert_eq!(
        ok(c, "get r select k, n, e where n = 4321", "", &[]),
        ok(b, "get r select k, n, e where n = 4321", "", &[])
    );
    by_handle(fenec_close, c);

    // Under 64 KB the bytes wait for more, or for a flush.
    for i in 0..3 {
        assert_eq!(write(a, rows + i, 1), write(b, rows + i, 1));
    }
    rows += 3;
    assert!(len(&a_path) < len(&b_path));
    assert_eq!(by_handle(fenec_flush, a), (FENEC_OK, String::new()));
    assert_eq!(len(&a_path), len(&b_path));

    assert_eq!(by_handle(fenec_close, a), (FENEC_OK, String::new()));
    assert_eq!(by_handle(fenec_close, b), (FENEC_OK, String::new()));
    let (x, y) = (
        std::fs::read(&a_path).unwrap(),
        std::fs::read(&b_path).unwrap(),
    );
    assert!(x == y, "{} and {} bytes differ", x.len(), y.len());
    let a = open(&a_path, FENEC_OPEN_NO_SYNC | FENEC_OPEN_NO_AUTO_COMPACT);
    assert_eq!(count(a), rows as u64);
    by_handle(fenec_close, a);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A write behind the lock that the disk refuses is the call's error, as a
/// failed fsync is a durable write's, and the engine's from then on: every
/// later write, flush and sync is refused with `FENEC_IO`, and reads go on.
#[test]
fn a_write_behind_the_lock_that_fails_stops_the_writes() {
    let dir = scratch("fails");
    let path = dir.join("app.fenec");
    let failing = Arc::new(AtomicBool::new(false));
    let fails = Arc::clone(&failing);
    let h = open_with(
        &path,
        Arc::new(
            move |write: Durability| match fails.load(Ordering::Acquire) {
                false => write,
                true => Box::new(move || {
                    drop(write);
                    Err(Error::Io("No space left on device (os error 28)".into()))
                }),
            },
        ),
    );
    ok(h, SCHEMA, "", &[]);
    write(h, 0, 200);
    failing.store(true, Ordering::Release);

    let (sql, params, bytes) = text(200, 200);
    let (code, why) = query_with(h, &sql, &params, &bytes);
    assert_eq!(code, FENEC_IO, "{why}");
    assert!(why.contains("No space left on device"), "{why}");
    failing.store(false, Ordering::Release);

    let (sql, params, bytes) = text(400, 1);
    let (code, why) = query_with(h, &sql, &params, &bytes);
    assert_eq!(code, FENEC_IO, "a write after the failure: {why}");
    assert_eq!(by_handle(fenec_flush, h).0, FENEC_IO);
    assert_eq!(by_handle(fenec_sync, h).0, FENEC_IO);
    // Reads go on, over what the database holds.
    assert_eq!(count(h), 400);
    assert_eq!(by_handle(fenec_close, h).0, FENEC_IO);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Where a held write behind says it runs, and what lets it go on.
type Gate = (Sender<()>, Receiver<()>);

/// Held at the disk -- as a `write` on macOS is once the file's dirty pages
/// pass 8 MB -- the buffer's write keeps neither a reader nor another
/// writer waiting: both answer while it is held, and it lands once let go.
#[test]
fn a_reader_goes_on_while_the_buffer_is_written() {
    let dir = scratch("reader");
    let path = dir.join("app.fenec");
    // Armed once: the first write behind tells `entered` it runs, and waits
    // for `release`.
    let (entered_tx, entered) = channel::<()>();
    let (release, release_rx) = channel::<()>();
    let gate: Arc<Mutex<Option<Gate>>> = Arc::new(Mutex::new(None));
    let armed = Arc::clone(&gate);
    let h = open_with(
        &path,
        Arc::new(
            move |write: Durability| match armed.lock().unwrap().take() {
                None => write,
                Some((entered, release)) => Box::new(move || {
                    entered.send(()).unwrap();
                    release.recv().unwrap();
                    write()
                }),
            },
        ),
    );
    ok(h, SCHEMA, "", &[]);
    write(h, 0, 10);
    *gate.lock().unwrap() = Some((entered_tx, release_rx));

    let writer = std::thread::spawn(move || write(h, 10, 200));
    entered
        .recv_timeout(MUST)
        .expect("the buffer was never written behind the lock");

    // The write behind is held: a read and a write each answer meanwhile.
    let (read_tx, read) = channel();
    std::thread::spawn(move || {
        read_tx
            .send(ok(h, "get r select k where id = 3", "", &[]))
            .unwrap()
    });
    let answer = read
        .recv_timeout(MUST)
        .expect("a read waited for the buffer's write");
    assert!(answer.contains("row 2"), "{answer}");
    let (wrote_tx, wrote) = channel();
    std::thread::spawn(move || wrote_tx.send(write(h, 210, 1)).unwrap());
    wrote
        .recv_timeout(MUST)
        .expect("a write waited for the buffer's write");

    release.send(()).unwrap();
    writer.join().unwrap();
    assert_eq!(count(h), 211);
    assert_eq!(by_handle(fenec_close, h), (FENEC_OK, String::new()));
    let h = open(&path, FENEC_OPEN_NO_SYNC | FENEC_OPEN_NO_AUTO_COMPACT);
    assert_eq!(count(h), 211);
    by_handle(fenec_close, h);
    let _ = std::fs::remove_dir_all(&dir);
}

/// What a sync applies from its server is fsynced by nothing, on a handle
/// that fsyncs its own writes too: a seed of 90 KB is in the file as the
/// call that fed it returns, a copy taken then holding every row of it.
#[test]
fn what_a_sync_applies_is_written_behind_the_lock_too() {
    let dir = scratch("sync");
    let path = dir.join("app.fenec");
    let h = open(&path, FENEC_OPEN_NO_AUTO_COMPACT);
    ok(
        h,
        "create collection tasks (key text @hash, title text)",
        "",
        &[],
    );
    let config = r#"{"url":"https://example.test","token":"tk","seed":"ab","shapes":[{"collection":"tasks","key":"key"}]}"#;
    let mut out = std::ptr::null_mut();
    let code = unsafe {
        fenec_sync_start(
            h,
            config.as_ptr(),
            config.len(),
            &mut out,
            std::ptr::null_mut(),
        )
    };
    assert_eq!(taken(code, out).0, FENEC_OK);
    let feed = |kind: u32, status: i32, bytes: &[u8]| {
        let mut out = std::ptr::null_mut();
        let code = unsafe {
            fenec_sync_feed(
                h,
                kind,
                1,
                status,
                0,
                bytes.as_ptr(),
                bytes.len(),
                &mut out,
                std::ptr::null_mut(),
            )
        };
        let (code, actions) = taken(code, out);
        assert_eq!(code, FENEC_OK, "{actions}");
        actions
    };
    feed(FENEC_SYNC_OPENED, 200, b"");
    let rows: Vec<String> = (1..=400)
        .map(|i| format!(r#"{{"id":{i},"key":"k{i}","title":"{}"}}"#, "t".repeat(200)))
        .collect();
    let seed = format!(
        "event: seed\ndata: {{\"seq\":3,\"rows\":[{}]}}\n\n",
        rows.join(",")
    );
    let actions = feed(FENEC_SYNC_BYTES, 0, seed.as_bytes());
    assert!(actions.contains("changed"), "{actions}");

    let copy = dir.join("copy.fenec");
    std::fs::copy(&path, &copy).unwrap();
    let c = open(&copy, FENEC_OPEN_NO_AUTO_COMPACT);
    let text = ok(c, "get tasks count", "", &[]);
    assert!(text.contains(r#"{"count":400}"#), "{text}");
    by_handle(fenec_close, c);
    assert_eq!(by_handle(fenec_close, h), (FENEC_OK, String::new()));
    let _ = std::fs::remove_dir_all(&dir);
}
