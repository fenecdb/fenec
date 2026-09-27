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
//!   * a read by id beside a writer that holds a transaction open 20 ms at a
//!     time, as a client between two statements does: fenecdb's readers
//!     read what has landed, the transaction's block parked while it waits
//!     for its next statement, SQLite's the last commit (WAL). p50, p99 and
//!     the longest.
//!
//! SQLite threads each hold a connection with a 30 s busy timeout, the
//! usual answer to `SQLITE_BUSY`; fenecdb's share one database behind a
//! `RwLock`, taken as `fenec-pg` takes it (`fenec_http::held`).

use fenec_core::prelude::*;
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const ROWS: i64 = 10_000;
const RUN: Duration = Duration::from_secs(2);

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
                        let out = fenec_http::held::read_landed(&db).query(&get, &[Value::Int(id)]);
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

fn main() {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("fenec-concurrency-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
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

    // A transaction held open 20 ms at a time, as a client between its
    // statements holds one: two writes, the pause between them, and the
    // write lock taken for each statement alone, the block left open
    // between them, as `fenec-pg` takes it.
    let hold = Duration::from_millis(20);
    let mut f = fenec_readers(&db, |stop| {
        let put = stmt("put t {k: $1, n: $2}");
        let end = Instant::now() + RUN;
        let mut n = 0i64;
        while Instant::now() < end && !stop.load(Ordering::Relaxed) {
            let mut g = fenec_http::held::write_unheld(&db);
            g.begin().unwrap();
            g.execute_with(&put, &[Value::Text("held".into()), Value::Int(n)])
                .unwrap();
            g.leave_block();
            drop(g);
            std::thread::sleep(hold);
            let mut g = db.write().unwrap();
            g.rejoin_block();
            g.execute_with(&put, &[Value::Text("held".into()), Value::Int(n + 1)])
                .unwrap();
            g.commit().unwrap();
            drop(g);
            n += 2;
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let mut s = sqlite_readers(&spath, |stop| {
        let c = sqlite_conn(&spath, false);
        let end = Instant::now() + RUN;
        let mut n = 0i64;
        while Instant::now() < end && !stop.load(Ordering::Relaxed) {
            c.execute_batch("begin immediate").unwrap();
            c.execute("insert into t (k, n) values ('held', ?1)", [n])
                .unwrap();
            std::thread::sleep(hold);
            c.execute("insert into t (k, n) values ('held', ?1)", [n + 1])
                .unwrap();
            c.execute_batch("commit").unwrap();
            n += 2;
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    row(
        "  beside a transaction held 20 ms",
        reads(&mut f),
        reads(&mut s),
    );
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
