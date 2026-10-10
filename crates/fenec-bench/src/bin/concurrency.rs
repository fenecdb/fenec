//! Writers and readers at once, fenecdb against SQLite, in one process:
//! `make concurrency-bench`.
//!
//! Both take one writer at a time. What this measures is what that costs
//! when many want to write, and what readers wait meanwhile:
//!
//!   * writes a second from 1, 4 and 16 threads, a row a write, each write
//!     made durable before it returns -- fenecdb's fsync runs outside its
//!     lock, and a durability an earlier fsync covered runs none, so the
//!     writers share them; SQLite's WAL under `synchronous=FULL` syncs a
//!     commit while it holds the write lock. Both with `F_FULLFSYNC` on
//!     macOS: Rust's `sync_data` uses it, and `PRAGMA fullfsync` gives
//!     SQLite the same;
//!   * the same without an fsync -- fenecdb's writes stay in its buffer,
//!     SQLite's reach the operating system (`synchronous=NORMAL`);
//!   * a read by id, alone and beside four writers, p50 and p99;
//!   * a read by id beside a writer landing blocks of 1 000 writes one after
//!     another, as `POST /batch` of 1 000 lines does: fenecdb's block holds
//!     the write lock until it lands, SQLite's readers read the last commit
//!     (WAL) beside its transaction. p50, p99 and the longest.
//!
//! SQLite threads each hold a connection with a 30 s busy timeout, the
//! usual answer to `SQLITE_BUSY`; fenecdb's share one database behind a
//! `RwLock`, taken as `fenec-server` takes it.
//!
//! Then fenecdb alone, writers beside long reads: four threads putting a
//! row at a time into a million events, two reading one by id, each at
//! most every half a millisecond, and a long read every second -- a
//! read-only batch of three aggregates (a ledger's reconciliation's shape,
//! about 150 ms), an aggregate of 50 000 groups (about 0.7 s), a full scan
//! (about 60 ms) -- under the read lock throughout, and pinned
//! (`Database::pin`), in turns. Each write's and each read's time, the
//! lock's wait in it: p50, p99, the longest, and how many waited past
//! 50 ms. `long` runs that part alone, `long lock` or `long pin` one way.

use fenec_core::prelude::*;
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const ROWS: i64 = 10_000;
const RUN: Duration = Duration::from_secs(2);
/// The writes a block holds beside the readers, a `/batch` of them.
const BLOCK: i64 = 1_000;

fn stmt(src: &str) -> Statement {
    fenec_ql::parse_one(src).unwrap()
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    v[((v.len() - 1) as f64 * p) as usize]
}

/// A splitmix step: the ids readers ask for.
fn next(x: &mut u64) -> u64 {
    *x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

// ------------------------------------------------------------ fenecdb

fn fenec_open(path: &Path) -> Arc<RwLock<Database>> {
    let _ = std::fs::remove_file(path);
    let mut db = fenec_core::fs::open(path).unwrap();
    db.execute(&stmt("create collection t (k text, n int)"))
        .unwrap();
    let put = stmt("put t {k: $1, n: $2}");
    db.begin().unwrap();
    for i in 0..ROWS {
        db.execute_with(&put, &[Value::Text(format!("k{i}")), Value::Int(i)])
            .unwrap();
    }
    db.commit().unwrap();
    db.sync().unwrap();
    Arc::new(RwLock::new(db))
}

/// Writes a row at a time from `threads` threads for [`RUN`]; returns the
/// writes a second.
fn fenec_writers(db: &Arc<RwLock<Database>>, threads: usize, durable: bool) -> f64 {
    let done = AtomicU64::new(0);
    let end = Instant::now() + RUN;
    std::thread::scope(|s| {
        for t in 0..threads {
            let (db, done) = (Arc::clone(db), &done);
            s.spawn(move || {
                let put = stmt("put t {k: $1, n: $2}");
                let mut n = 0u64;
                while Instant::now() < end {
                    let args = [Value::Text(format!("w{t}-{n}")), Value::Int(n as i64)];
                    let durability = {
                        let mut g = db.write().unwrap();
                        g.execute_with(&put, &args).unwrap();
                        match durable {
                            true => g.flush().unwrap(),
                            false => None,
                        }
                    };
                    if let Some(d) = durability {
                        d().unwrap();
                    }
                    n += 1;
                }
                done.fetch_add(n, Ordering::Relaxed);
            });
        }
    });
    done.load(Ordering::Relaxed) as f64 / RUN.as_secs_f64()
}

/// Reads a row by id from four threads while `beside` runs; returns every
/// read's time in milliseconds.
fn fenec_readers(db: &Arc<RwLock<Database>>, beside: impl FnOnce(&AtomicBool) + Send) -> Vec<f64> {
    let stop = AtomicBool::new(false);
    let mut all = Vec::new();
    std::thread::scope(|s| {
        let readers: Vec<_> = (0..4u64)
            .map(|r| {
                let (db, stop) = (Arc::clone(db), &stop);
                s.spawn(move || {
                    let get = stmt("get t where id = $1");
                    let (mut seed, mut times) = (r + 1, Vec::new());
                    while !stop.load(Ordering::Relaxed) {
                        let id = (next(&mut seed) % ROWS as u64) as i64 + 1;
                        let t = Instant::now();
                        let out = db.read().unwrap().query(&get, &[Value::Int(id)]);
                        times.push(t.elapsed().as_secs_f64() * 1e3);
                        out.unwrap();
                    }
                    times
                })
            })
            .collect();
        beside(&stop);
        stop.store(true, Ordering::Relaxed);
        for r in readers {
            all.extend(r.join().unwrap());
        }
    });
    all
}

// ------------------------------------------------------------- SQLite

fn sqlite_conn(path: &Path, durable: bool) -> Connection {
    let c = Connection::open(path).unwrap();
    c.pragma_update(None, "journal_mode", "WAL").unwrap();
    c.pragma_update(None, "synchronous", if durable { "FULL" } else { "NORMAL" })
        .unwrap();
    c.pragma_update(None, "fullfsync", durable).unwrap();
    c.busy_timeout(Duration::from_secs(30)).unwrap();
    c
}

fn sqlite_open(path: &Path) {
    for ext in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", path.display()));
    }
    let mut c = sqlite_conn(path, false);
    c.execute("create table t (id integer primary key, k text, n int)", [])
        .unwrap();
    let tx = c.transaction().unwrap();
    for i in 0..ROWS {
        tx.execute(
            "insert into t (k, n) values (?1, ?2)",
            params![format!("k{i}"), i],
        )
        .unwrap();
    }
    tx.commit().unwrap();
}

fn sqlite_writers(path: &Path, threads: usize, durable: bool) -> f64 {
    let done = AtomicU64::new(0);
    let end = Instant::now() + RUN;
    std::thread::scope(|s| {
        for t in 0..threads {
            let done = &done;
            s.spawn(move || {
                let c = sqlite_conn(path, durable);
                let mut put = c.prepare("insert into t (k, n) values (?1, ?2)").unwrap();
                let mut n = 0u64;
                while Instant::now() < end {
                    put.execute(params![format!("w{t}-{n}"), n as i64]).unwrap();
                    n += 1;
                }
                done.fetch_add(n, Ordering::Relaxed);
            });
        }
    });
    done.load(Ordering::Relaxed) as f64 / RUN.as_secs_f64()
}

fn sqlite_readers(path: &Path, beside: impl FnOnce(&AtomicBool) + Send) -> Vec<f64> {
    let stop = AtomicBool::new(false);
    let mut all = Vec::new();
    std::thread::scope(|s| {
        let readers: Vec<_> = (0..4u64)
            .map(|r| {
                let stop = &stop;
                s.spawn(move || {
                    let c = sqlite_conn(path, false);
                    let mut get = c.prepare("select k, n from t where id = ?1").unwrap();
                    let (mut seed, mut times) = (r + 1, Vec::new());
                    while !stop.load(Ordering::Relaxed) {
                        let id = (next(&mut seed) % ROWS as u64) as i64 + 1;
                        let t = Instant::now();
                        let row: (String, i64) =
                            get.query_row([id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
                        times.push(t.elapsed().as_secs_f64() * 1e3);
                        std::hint::black_box(row);
                    }
                    times
                })
            })
            .collect();
        beside(&stop);
        stop.store(true, Ordering::Relaxed);
        for r in readers {
            all.extend(r.join().unwrap());
        }
    });
    all
}

// --------------------------------------------------------------- main

fn row(name: &str, a: String, b: String) {
    println!("{name:<40}{a:>28}{b:>28}");
}

/// Reads a second over the run, then p50 and p99 in microseconds and the
/// longest in milliseconds.
fn reads(v: &mut [f64]) -> String {
    let per_s = v.len() as f64 / RUN.as_secs_f64();
    let (p50, p99, max) = (pct(v, 0.5), pct(v, 0.99), pct(v, 1.0));
    format!(
        "{:.0}k/s {:.1}/{:.1}us {:.1}ms",
        per_s / 1e3,
        p50 * 1e3,
        p99 * 1e3,
        max
    )
}

// ------------------------------------------------- beside long reads

/// The events the long reads read.
const EVENTS: i64 = 1_000_000;
/// How long each long read's turn lasts, a long read a second.
const LONG_RUN: Duration = Duration::from_secs(10);
/// Each writer's and reader's pace beside them: one at most every half a
/// millisecond, so the events grow by about as much in every turn and the
/// times are of the waits rather than of a queue of writers flat out.
const PACE: Duration = Duration::from_micros(500);
const NAMES: [&str; 8] = [
    "view", "click", "signup", "cart", "buy", "share", "search", "logout",
];
const COUNTRIES: [&str; 5] = ["TR", "US", "DE", "FR", "JP"];

fn event(seed: &mut u64) -> [Value; 5] {
    let r = next(seed);
    [
        Value::Int((r % 50_000) as i64),
        Value::Text(NAMES[(r >> 16) as usize % 8].into()),
        Value::Timestamp(1_700_000_000_000 + (r >> 20) as i64 % (7 * 86_400_000)),
        Value::Text(COUNTRIES[(r >> 40) as usize % 5].into()),
        Value::Int((r >> 48) as i64 % 1000),
    ]
}

const PUT_EVENT: &str = "put events {user: $1, name: $2, at: $3, country: $4, n: $5}";

fn events_open(path: &Path) -> Arc<RwLock<Database>> {
    let _ = std::fs::remove_file(path);
    let mut db = fenec_core::fs::open(path).unwrap();
    db.execute(&stmt(
        "create collection events (user int @hash, name text @hash, at timestamp, \
         country text, n int)",
    ))
    .unwrap();
    let put = stmt(PUT_EVENT);
    let mut seed = 7;
    for _ in 0..EVENTS / 10_000 {
        let args: Vec<[Value; 5]> = (0..10_000).map(|_| event(&mut seed)).collect();
        let stmts: Vec<(&Statement, &[Value])> = args.iter().map(|a| (&put, &a[..])).collect();
        db.execute_block(&stmts).unwrap();
    }
    db.checkpoint().unwrap();
    Arc::new(RwLock::new(db))
}

/// The long reads, each a list of statements read as one.
fn long_reads() -> Vec<(&'static str, Vec<Statement>)> {
    vec![
        (
            "a read-only batch of three",
            vec![
                stmt("get events select name, sum(n) group name"),
                stmt("get events select country, count(*), min(at), max(at) group country"),
                stmt("get events count"),
            ],
        ),
        (
            "an aggregate of 50 000 groups",
            vec![stmt(
                "get events select user, min(case when name = 'signup' then at end) as a, \
                 min(case when name = 'buy' then at end) as b, count(distinct country) as c, \
                 count(distinct name) as d, max(n) as m, min(at) as f, sum(n) as s group user having b >= a count",
            )],
        ),
        (
            "a full scan",
            vec![stmt("get events where country ~ \"R\" and n > 500 count")],
        ),
    ]
}

/// One long read, every statement at the one change: pinned under the
/// read lock and read with none held when `pin` -- what a server's
/// read-only batch does -- or under the read lock throughout. How long the
/// lock was held, in milliseconds.
fn long_read(db: &RwLock<Database>, stmts: &[Statement], pin: bool) -> f64 {
    let t = Instant::now();
    let g = db.read().unwrap();
    if pin {
        let pairs: Vec<(&Statement, &[Value])> = stmts.iter().map(|s| (s, &[][..])).collect();
        if let Some(p) = g.pin(&pairs) {
            drop(g);
            let held = t.elapsed().as_secs_f64() * 1e3;
            for s in stmts {
                let r = p.query(s, &[]).expect("a read a pin answers");
                std::hint::black_box(r.unwrap());
            }
            return held;
        }
    }
    for s in stmts {
        std::hint::black_box(g.query(s, &[]).unwrap());
    }
    drop(g);
    t.elapsed().as_secs_f64() * 1e3
}

/// Four writers putting events and two readers reading one by id for
/// [`LONG_RUN`], a long read of `stmts` every second beside them when
/// there are any: the writes' times, the reads', the long reads' and how
/// long each held the lock.
fn beside_long(db: &Arc<RwLock<Database>>, stmts: &[Statement], pin: bool) -> [Vec<f64>; 4] {
    let stop = AtomicBool::new(false);
    let (mut writes, mut reads) = (Vec::new(), Vec::new());
    let (mut longs, mut held) = (Vec::new(), Vec::new());
    std::thread::scope(|s| {
        let clients: Vec<_> = (0..6u64)
            .map(|c| {
                let (db, stop) = (Arc::clone(db), &stop);
                s.spawn(move || {
                    let (put, get) = (stmt(PUT_EVENT), stmt("get events where id = $1"));
                    let (mut seed, mut times) = (c + 100, Vec::new());
                    while !stop.load(Ordering::Relaxed) {
                        let t = Instant::now();
                        let due = t + PACE;
                        match c < 4 {
                            true => {
                                let args = event(&mut seed);
                                db.write().unwrap().execute_with(&put, &args).unwrap();
                            }
                            false => {
                                let id = (next(&mut seed) % EVENTS as u64) as i64 + 1;
                                let r = db.read().unwrap().query(&get, &[Value::Int(id)]);
                                std::hint::black_box(r.unwrap());
                            }
                        }
                        times.push(t.elapsed().as_secs_f64() * 1e3);
                        std::thread::sleep(due.saturating_duration_since(Instant::now()));
                    }
                    (c < 4, times)
                })
            })
            .collect();
        let end = Instant::now() + LONG_RUN;
        while !stmts.is_empty() && Instant::now() + Duration::from_secs(1) <= end {
            let next = Instant::now() + Duration::from_secs(1);
            let t = Instant::now();
            held.push(long_read(db, stmts, pin));
            longs.push(t.elapsed().as_secs_f64() * 1e3);
            std::thread::sleep(next.saturating_duration_since(Instant::now()));
        }
        std::thread::sleep(end.saturating_duration_since(Instant::now()));
        stop.store(true, Ordering::Relaxed);
        for c in clients {
            match c.join().unwrap() {
                (true, t) => writes.extend(t),
                (false, t) => reads.extend(t),
            }
        }
    });
    [writes, reads, longs, held]
}

/// The rate, p50, p99 and the longest of `v`, in milliseconds, and how
/// many waited past 50 ms.
fn waits(v: &mut [f64]) -> String {
    let over = v.iter().filter(|&&t| t > 50.0).count();
    format!(
        "{:>9.0} {:>8.3} {:>8.2} {:>8.1} {:>9}",
        v.len() as f64 / LONG_RUN.as_secs_f64(),
        pct(v, 0.5),
        pct(v, 0.99),
        pct(v, 1.0),
        over
    )
}

fn long(dir: &Path) {
    let path = dir.join("events.fenec");
    let t = Instant::now();
    let db = events_open(&path);
    println!(
        "\nfenecdb: writers beside long reads, {EVENTS} events ({:.1} s to make), \
         four writers and two readers by id",
        t.elapsed().as_secs_f64()
    );
    // The indexes built and the pages in, before anything is timed; then
    // each read alone, under the lock and pinned, the best of five.
    for (name, stmts) in long_reads() {
        long_read(&db, &stmts, false);
        for pin in [false, true] {
            let (mut best, mut held) = (f64::MAX, f64::MAX);
            for _ in 0..5 {
                let t = Instant::now();
                held = held.min(long_read(&db, &stmts, pin));
                best = best.min(t.elapsed().as_secs_f64() * 1e3);
            }
            match pin {
                false => println!("  {name}, alone, under the lock: {best:.1} ms"),
                true => println!("  {name}, alone, pinned: {best:.1} ms, the pin {held:.2} ms"),
            }
        }
    }
    println!(
        "{:<56} {:>9} {:>8} {:>8} {:>8} {:>9}",
        "", "a second", "p50 ms", "p99 ms", "max ms", "over 50ms"
    );
    let mut cases = vec![("no long read", Vec::new())];
    cases.extend(long_reads());
    // `lock` or `pin` runs one way alone, a binary from before the pins
    // the first.
    let only = std::env::args().nth(2);
    let ways: Vec<bool> = match only.as_deref() {
        Some("lock") => vec![false],
        Some("pin") => vec![true],
        _ => vec![false, true],
    };
    for (name, stmts) in cases {
        for &pin in &ways {
            if stmts.is_empty() && pin {
                continue;
            }
            let how = match (stmts.is_empty(), pin) {
                (true, _) => String::new(),
                (false, false) => ", under the lock".to_string(),
                (false, true) => ", pinned".to_string(),
            };
            let [mut w, mut r, mut l, mut h] = beside_long(&db, &stmts, pin);
            println!("{:<56} {}", format!("writes, {name}{how}"), waits(&mut w));
            println!(
                "{:<56} {}",
                format!("reads by id, {name}{how}"),
                waits(&mut r)
            );
            if !l.is_empty() {
                println!(
                    "  the long read took {:.0} to {:.0} ms, the lock held {:.2} to {:.2} ms",
                    pct(&mut l, 0.0),
                    pct(&mut l, 1.0),
                    pct(&mut h, 0.0),
                    pct(&mut h, 1.0)
                );
            }
        }
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}

fn main() {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("fenec-concurrency-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    if std::env::args().nth(1).as_deref() == Some("long") {
        long(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let (fpath, spath) = (dir.join("bench.fenec"), dir.join("bench.sqlite"));
    println!(
        "fenecdb {} against SQLite {}, in one process, {} threads of this machine\n",
        fenec_core::VERSION,
        rusqlite::version(),
        std::thread::available_parallelism().map_or(1, |n| n.get())
    );
    row("", "fenecdb".into(), "SQLite".into());
    println!("{}", "-".repeat(96));
    for durable in [true, false] {
        for threads in [1, 4, 16] {
            let db = fenec_open(&fpath);
            let f = fenec_writers(&db, threads, durable);
            drop(db);
            sqlite_open(&spath);
            let s = sqlite_writers(&spath, threads, durable);
            let how = if durable { "fsync each" } else { "no fsync" };
            row(
                &format!("writes/s, {threads} writer(s), {how}"),
                format!("{f:.0}"),
                format!("{s:.0}"),
            );
        }
    }

    let db = fenec_open(&fpath);
    sqlite_open(&spath);
    let mut f = fenec_readers(&db, |_| std::thread::sleep(RUN));
    let mut s = sqlite_readers(&spath, |_| std::thread::sleep(RUN));
    println!("\nreads by id from 4 threads: reads/s, p50/p99, the longest");
    row("  alone", reads(&mut f), reads(&mut s));

    let mut f = fenec_readers(&db, |_| {
        fenec_writers(&db, 4, false);
    });
    let mut s = sqlite_readers(&spath, |_| {
        sqlite_writers(&spath, 4, false);
    });
    row("  beside 4 writers, no fsync", reads(&mut f), reads(&mut s));

    // Blocks of 1 000 writes, one after another, as `POST /batch` of 1 000
    // lines runs them: the block holds the write lock from its first write
    // to its record, and a reader waits for it -- SQLite's WAL reads the
    // last commit meanwhile.
    let mut f = fenec_readers(&db, |stop| {
        let put = stmt("put t {k: $1, n: $2}");
        let end = Instant::now() + RUN;
        let mut n = 0i64;
        while Instant::now() < end && !stop.load(Ordering::Relaxed) {
            let args: Vec<[Value; 2]> = (n..n + BLOCK)
                .map(|i| [Value::Text("block".into()), Value::Int(i)])
                .collect();
            let stmts: Vec<(&Statement, &[Value])> = args.iter().map(|a| (&put, &a[..])).collect();
            db.write().unwrap().execute_block(&stmts).unwrap();
            n += BLOCK;
        }
    });
    let mut s = sqlite_readers(&spath, |stop| {
        let mut c = sqlite_conn(&spath, false);
        let end = Instant::now() + RUN;
        let mut n = 0i64;
        while Instant::now() < end && !stop.load(Ordering::Relaxed) {
            let tx = c.transaction().unwrap();
            for i in n..n + BLOCK {
                tx.execute("insert into t (k, n) values ('block', ?1)", [i])
                    .unwrap();
            }
            tx.commit().unwrap();
            n += BLOCK;
        }
    });
    row(
        "  beside blocks of 1 000 writes",
        reads(&mut f),
        reads(&mut s),
    );
    drop(db);
    long(&dir);
    let _ = std::fs::remove_dir_all(&dir);
}
