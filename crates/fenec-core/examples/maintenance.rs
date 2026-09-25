//! What a `create index` and a `compact` cost the readers and writers of a
//! running database: `cargo run --release -p fenec-core --example maintenance -- [N] [DIM] [compact]`
//!
//! N documents with a DIM vector, in a file -- a compact rewrites it. While
//! each statement runs -- under the write lock as `execute` does, then beside
//! the database as `Database::maintain` does -- four threads read one
//! document by id in a loop and one writes a document every 5 ms; each read
//! and write is timed from the moment it asked for the lock. With `compact`
//! the index is left out and the compact alone is run, over a file of any
//! size: 1 800 000 x 128 is a gigabyte. The heap the statement took at its
//! peak is counted by the allocator.

use fenec_core::prelude::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

struct Counting;

static HEAP: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let now = HEAP.fetch_add(l.size(), Ordering::Relaxed) + l.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
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
            let now = HEAP.fetch_add(new, Ordering::Relaxed) + new;
            PEAK.fetch_max(now, Ordering::Relaxed);
            HEAP.fetch_sub(l.size(), Ordering::Relaxed);
        }
        q
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % 20_000) as f32 / 10_000.0 - 1.0
    }
}

fn exec(db: &mut Database, sql: &str) {
    db.execute(&fenec_ql::parse_one(sql).unwrap())
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn database(n: usize, dim: usize, path: &std::path::Path) -> Database {
    let _ = std::fs::remove_file(path);
    let mut db = fenec_core::fs::open(path).unwrap();
    exec(
        &mut db,
        &format!("create collection c (n int, v vector<{dim}>)"),
    );
    let mut rng = Rng(3);
    let mut batch = Vec::with_capacity(1_000);
    for i in 0..n {
        let v: Vec<String> = (0..dim).map(|_| format!("{:.4}", rng.next())).collect();
        batch.push(format!("{{n: {i}, v: [{}]}}", v.join(",")));
        if batch.len() == 1_000 || i == n - 1 {
            exec(&mut db, &format!("put c [{}]", batch.join(",")));
            batch.clear();
        }
    }
    db
}

fn percentiles(label: &str, mut ms: Vec<f64>) -> String {
    if ms.is_empty() {
        return format!("{label}: none");
    }
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |p: f64| ms[((ms.len() - 1) as f64 * p).round() as usize];
    format!(
        "{label} ({}): p50 {:.3} p99 {:.3} max {:.1} ms",
        ms.len(),
        at(0.5),
        at(0.99),
        ms[ms.len() - 1]
    )
}

/// Runs `stmt` one way while the readers and the writer run, and reports
/// what each of them waited.
fn under_load(db: &Arc<RwLock<Database>>, sql: &str, beside: bool, n: usize) {
    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(Mutex::new(Vec::new()));
    let writes = Arc::new(Mutex::new(Vec::new()));
    let mut threads = Vec::new();
    for r in 0..4u64 {
        let (db, stop, reads) = (Arc::clone(db), Arc::clone(&stop), Arc::clone(&reads));
        threads.push(std::thread::spawn(move || {
            let mut mine = Vec::new();
            let mut id = 1 + r * 7919;
            while !stop.load(Ordering::Relaxed) {
                id = 1 + (id * 48271) % n as u64;
                let stmt = fenec_ql::parse_one(&format!("get c select n where id = {id}")).unwrap();
                let t = Instant::now();
                db.read().unwrap().query(&stmt, &[]).unwrap();
                mine.push(t.elapsed().as_secs_f64() * 1e3);
                std::thread::sleep(Duration::from_micros(200));
            }
            reads.lock().unwrap().extend(mine);
        }));
    }
    {
        let (db, stop, writes) = (Arc::clone(db), Arc::clone(&stop), Arc::clone(&writes));
        threads.push(std::thread::spawn(move || {
            let mut mine = Vec::new();
            let stmt = fenec_ql::parse_one("put c {n: -1}").unwrap();
            while !stop.load(Ordering::Relaxed) {
                let t = Instant::now();
                db.write().unwrap().execute(&stmt).unwrap();
                mine.push(t.elapsed().as_secs_f64() * 1e3);
                std::thread::sleep(Duration::from_millis(5));
            }
            writes.lock().unwrap().extend(mine);
        }));
    }
    std::thread::sleep(Duration::from_millis(300));
    let stmt = fenec_ql::parse_one(sql).unwrap();
    let base = HEAP.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let t = Instant::now();
    if beside {
        Database::maintain(db, &stmt).unwrap().unwrap();
    } else {
        db.write().unwrap().execute(&stmt).unwrap();
    }
    let took = t.elapsed();
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base);
    std::thread::sleep(Duration::from_millis(300));
    stop.store(true, Ordering::Relaxed);
    for t in threads {
        t.join().unwrap();
    }
    println!(
        "{sql}, {}: {:.2} s, heap +{:.1} MB at its peak\n  {}\n  {}",
        if beside {
            "beside the database"
        } else {
            "under the write lock"
        },
        took.as_secs_f64(),
        peak as f64 / 1e6,
        percentiles("reads", std::mem::take(&mut reads.lock().unwrap())),
        percentiles("writes", std::mem::take(&mut writes.lock().unwrap())),
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: usize = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(100_000);
    let dim: usize = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(128);
    let compact_only = args.iter().any(|a| a == "compact");

    let path = std::env::temp_dir().join(format!("fenec-maintenance-{}.fenec", std::process::id()));
    for beside in [false, true] {
        let db = Arc::new(RwLock::new(database(n, dim, &path)));
        db.write().unwrap().sync().unwrap();
        let len = std::fs::metadata(&path).map_or(0, |m| m.len());
        println!("{n} x {dim} in a file of {:.1} MB", len as f64 / 1e6);
        if !compact_only {
            under_load(&db, "create index on c (v) @hnsw(cosine)", beside, n);
        }
        // A fifth of the documents rewritten, so compact has dead bytes to
        // drop and the graph tombstones to leave behind.
        {
            let mut g = db.write().unwrap();
            exec(&mut g, &format!("set c {{n: 0}} where n < {}", n / 5));
        }
        under_load(&db, "compact", beside, n);
    }
    let _ = std::fs::remove_file(&path);
}
