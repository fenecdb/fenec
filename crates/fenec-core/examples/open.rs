//! What opening a file costs, read into memory or mapped: `make open-bench`.
//!
//! ```text
//! cargo run --release -p fenec-core --example open -- write <path> <docs> <body bytes> [h|s|hs|none]
//! cargo run --release -p fenec-core --example open -- open <path> read|mapped [quick]
//! cargo run --release -p fenec-core --example open -- compact <path> read|mapped
//! ```
//!
//! `write` streams a file the way a server that never checkpointed leaves
//! one -- a record per write -- without holding the database, so a file
//! larger than the machine's memory can be made on it. `open` opens it one
//! way or the other, in a process of its own so that the peak RSS is its
//! own, and then times what a server does next: reads that touch a page
//! each, a hash lookup, an ordered page, and a scan of every document.

use fenec_core::codec::put_uvarint;
use fenec_core::engine::MAGIC;
use fenec_core::prelude::*;
use fenec_core::store::{Store, OP_PUT};
use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Counts the heap: what the process allocated, not what the kernel keeps
/// resident. On a machine short of memory the resident set is whatever the
/// pager left, and a mapped file's pages count in it too though the kernel
/// can drop them at will; the heap is what a memory limit has to hold.
struct Counting;

static HEAP: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(by: usize) {
    let now = HEAP.fetch_add(by, Ordering::Relaxed) + by;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            grew(l.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        HEAP.fetch_sub(l.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            if new > l.size() {
                grew(new - l.size());
            } else {
                HEAP.fetch_sub(l.size() - new, Ordering::Relaxed);
            }
        }
        q
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn heap_mb() -> f64 {
    HEAP.load(Ordering::Relaxed) as f64 / 1e6
}

fn peak_heap_mb() -> f64 {
    PEAK.load(Ordering::Relaxed) as f64 / 1e6
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const WORDS: [&str; 16] = [
    "vector", "search", "index", "graph", "segment", "record", "replica", "tenant", "offset",
    "memory", "mapped", "page", "cursor", "shard", "commit", "query",
];

/// `indexes` holds `h` for `@hash` on `kind`, `s` for `@sorted` on `n`.
fn schema(indexes: &str) -> Schema {
    let kind = Field::new("kind", DataType::Text);
    let n = Field::new("n", DataType::Int);
    Schema::new(
        "docs",
        vec![
            if indexes.contains('h') {
                kind.indexed(IndexKind::Hash)
            } else {
                kind
            },
            if indexes.contains('s') {
                n.indexed(IndexKind::Sorted)
            } else {
                n
            },
            Field::new("body", DataType::Text),
        ],
    )
    .unwrap()
}

/// A file-level record: `[kind][collection][length][body]`.
fn record(out: &mut Vec<u8>, kind: u8, cid: u64, body: &[u8]) {
    out.clear();
    out.push(kind);
    put_uvarint(out, cid);
    put_uvarint(out, body.len() as u64);
    out.extend_from_slice(body);
}

fn write(path: &str, docs: u64, body: usize, indexes: &str) {
    let sc = schema(indexes);
    let mut f = std::io::BufWriter::with_capacity(1 << 22, std::fs::File::create(path).unwrap());
    f.write_all(MAGIC).unwrap();
    let mut rec = Vec::new();
    // REC_CREATE and REC_DATA, as the write path writes them.
    record(&mut rec, 1, 1, &sc.encode());
    f.write_all(&rec).unwrap();
    let mut rng = Rng(0x5eed);
    let mut text = String::with_capacity(body + 16);
    let t = Instant::now();
    for id in 1..=docs {
        text.clear();
        while text.len() < body {
            text.push_str(WORDS[rng.below(16) as usize]);
            text.push(' ');
        }
        let doc = Document {
            id,
            fields: vec![
                ("kind".into(), Value::Text(format!("k{}", rng.below(16)))),
                ("n".into(), Value::Int(rng.below(1 << 40) as i64)),
                ("body".into(), Value::Text(text.clone())),
            ],
        };
        let frame = Store::frame(OP_PUT, id, &Store::encode_doc(&sc, &doc));
        record(&mut rec, 3, 1, &frame);
        f.write_all(&rec).unwrap();
    }
    f.flush().unwrap();
    let size = std::fs::metadata(path).unwrap().len();
    println!(
        "{path}: {docs} documents, {:.2} GB, written in {:.1} s",
        size as f64 / 1e9,
        t.elapsed().as_secs_f64()
    );
}

extern "C" {
    fn getrusage(who: i32, usage: *mut Rusage) -> i32;
}

/// `struct rusage` on 64-bit targets: two `timeval`s, then fourteen longs.
#[repr(C)]
struct Rusage {
    utime: [i64; 2],
    stime: [i64; 2],
    maxrss: i64,
    rest: [i64; 13],
}

/// The process's peak resident set, in megabytes.
fn peak_mb() -> f64 {
    let mut u = Rusage {
        utime: [0; 2],
        stime: [0; 2],
        maxrss: 0,
        rest: [0; 13],
    };
    unsafe { getrusage(0, &mut u) };
    // Bytes on macOS, kilobytes on Linux.
    if cfg!(target_os = "macos") {
        u.maxrss as f64 / 1e6
    } else {
        u.maxrss as f64 / 1e3
    }
}

/// The resident set now, as `ps` sees it: mapped pages the process has
/// touched count, until the kernel drops them.
fn now_mb() -> f64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .unwrap_or(0.0)
        / 1e3
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn percentiles(mut v: Vec<f64>) -> String {
    v.sort_by(f64::total_cmp);
    let at = |p: f64| v[((v.len() as f64 * p) as usize).min(v.len() - 1)];
    format!("p50 {:.3} ms, p99 {:.3} ms", at(0.5), at(0.99))
}

fn times(db: &Database, sql: &str, params: impl Fn(u64) -> Vec<Value>, runs: u64) -> String {
    let stmt = fenec_ql::parse_one(sql).unwrap();
    let mut rng = Rng(0xabcdef);
    let v: Vec<f64> = (0..runs)
        .map(|_| {
            let p = params(rng.next());
            let t = Instant::now();
            let r = db.query(&stmt, &p).unwrap();
            std::hint::black_box(r);
            ms(t.elapsed())
        })
        .collect();
    percentiles(v)
}

fn open(path: &str, how: &str, queries: bool) {
    let size = std::fs::metadata(path).unwrap().len() as f64 / 1e9;
    PEAK.store(HEAP.load(Ordering::Relaxed), Ordering::Relaxed);
    let t = Instant::now();
    let db = match how {
        "read" => fenec_core::fs::open_in_memory(path),
        "mapped" => fenec_core::fs::open_mapped(path),
        other => panic!("read or mapped, not {other}"),
    }
    .unwrap();
    let opened = t.elapsed();
    // Room to look at the process from outside (`vmmap --summary <pid>`).
    if std::env::var_os("FENEC_OPEN_PAUSE").is_some() {
        println!("paused after open: pid {}", std::process::id());
        std::thread::sleep(Duration::from_secs(20));
    }
    let docs = match db
        .query(&fenec_ql::parse_one("get docs count").unwrap(), &[])
        .unwrap()
    {
        Response::Rows(r) => format!("{:?}", r.rows[0].values[0]),
        _ => String::new(),
    };
    println!("{how}, {size:.2} GB file, {docs} documents");
    println!(
        "  open                {:9.1} ms   heap peak {:7.0} MB, after {:7.0} MB (the engine counts {:.0}); RSS peak {:.0} MB, now {:.0} MB",
        ms(opened),
        peak_heap_mb(),
        heap_mb(),
        db.memory_bytes() as f64 / 1e6,
        peak_mb(),
        now_mb(),
    );
    if !queries {
        return;
    }
    let n = db
        .query(&fenec_ql::parse_one("get docs count").unwrap(), &[])
        .unwrap();
    let total = match n {
        Response::Rows(r) => match r.rows[0].values[0] {
            Value::Int(n) => n as u64,
            _ => 1,
        },
        _ => 1,
    };
    println!(
        "  by id, 10 000       {}",
        times(
            &db,
            "get docs where id = $1",
            |r| vec![Value::Int((r % total + 1) as i64)],
            10_000
        )
    );
    println!(
        "  hash, 1 000         {}",
        times(
            &db,
            "get docs where kind = $1 limit 20",
            |r| vec![Value::Text(format!("k{}", r % 16))],
            1_000
        )
    );
    println!(
        "  ordered page, 1 000 {}",
        times(
            &db,
            "get docs where n >= $1 order n limit 20",
            |r| vec![Value::Int((r % (1 << 40)) as i64)],
            1_000
        )
    );
    for pass in ["cold", "warm"] {
        let t = Instant::now();
        db.query(
            &fenec_ql::parse_one("get docs where body ~ \"zebra\" count").unwrap(),
            &[],
        )
        .unwrap();
        println!(
            "  scan, {pass}          {:9.1} ms   heap after {:.0} MB; RSS peak {:.0} MB, now {:.0} MB",
            ms(t.elapsed()),
            heap_mb(),
            peak_mb(),
            now_mb()
        );
    }
}

/// What a `compact` costs: the file is opened, a tenth of the rows deleted
/// so there is something to reclaim, and the compaction timed.
fn compact(path: &str, how: &str) {
    let before = std::fs::metadata(path).unwrap().len() as f64 / 1e9;
    PEAK.store(HEAP.load(Ordering::Relaxed), Ordering::Relaxed);
    let t = Instant::now();
    let mut db = match how {
        "read" => fenec_core::fs::open_in_memory(path),
        "mapped" => fenec_core::fs::open_mapped(path),
        other => panic!("read or mapped, not {other}"),
    }
    .unwrap();
    println!(
        "{how}, {before:.2} GB file: open {:9.1} ms   heap {:.0} MB",
        ms(t.elapsed()),
        heap_mb()
    );
    let opened = heap_mb();
    let del = fenec_ql::parse_one("del docs where n < $1").unwrap();
    let t = Instant::now();
    db.execute_with(&del, &[Value::Int(1 << 37)]).unwrap();
    println!(
        "  del a tenth       {:9.1} ms   heap {:.0} MB",
        ms(t.elapsed()),
        heap_mb()
    );
    // The compact's own peak: counted from the open, it was the open's.
    PEAK.store(HEAP.load(Ordering::Relaxed), Ordering::Relaxed);
    let t = Instant::now();
    db.execute(&fenec_ql::parse_one("compact").unwrap())
        .unwrap();
    let took = t.elapsed();
    let after = std::fs::metadata(path).unwrap().len() as f64 / 1e9;
    println!(
        "  compact           {:9.1} ms   heap peak {:7.0} MB, after {:7.0} MB (open left {opened:.0}); \
         RSS peak {:.0} MB; file {before:.2} -> {after:.2} GB",
        ms(took),
        peak_heap_mb(),
        heap_mb(),
        peak_mb(),
    );
    let r = db
        .query(&fenec_ql::parse_one("get docs count").unwrap(), &[])
        .unwrap();
    println!("  rows left: {:?}", r.rows().unwrap().rows[0].values[0]);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("write") => write(
            &args[1],
            args[2].parse().unwrap(),
            args.get(3).map_or(400, |s| s.parse().unwrap()),
            args.get(4).map_or("hs", String::as_str),
        ),
        Some("open") => open(
            &args[1],
            args.get(2).map_or("read", String::as_str),
            args.get(3).map(String::as_str) != Some("quick"),
        ),
        Some("compact") => compact(&args[1], args.get(2).map_or("read", String::as_str)),
        _ => eprintln!(
            "open write <path> <docs> <body bytes> | open open <path> read|mapped | \
             open compact <path> read|mapped"
        ),
    }
}
