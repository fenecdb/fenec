//! What `@ttl` costs: `make ttl-bench`.
//!
//! ```text
//! cargo run --release -p fenec-core --example expiry -- reads [rows]
//! cargo run --release -p fenec-core --example expiry -- sweep [expired] [batch]
//! cargo run --release -p fenec-core --example expiry -- hash [rows] [expired] [batch]
//! cargo run --release -p fenec-core --example expiry -- writes [rows]
//! ```
//!
//! `reads` holds the same documents in three collections: `t` under a plain
//! `@sorted`, `e` under `@ttl(1h)` -- half of each past its time -- and asks
//! each query of `t` as written, of `e` as written, and of `t` with the
//! expiry written out by hand. The median of nine, the median of five
//! such rounds.
//!
//! `sweep` writes twice `expired` rows into a file, half past their time,
//! and sweeps them as a server's sweeper does: a batch found under the
//! read lock (`Database::expired`), deleted under the write lock
//! (`Database::sweep`), the file's durability run after it.
//!
//! `hash` writes `rows` events in the order they came, a `@hash` field of
//! five values beside the `@ttl` one, the oldest `expired` of them past
//! their time, and sweeps those as `sweep` does: each row leaves a bucket
//! of a fifth of the collection. `writes` times a lone `put` and a lone
//! `del` of a row among `rows`, with that field under `@hash` and without.

use fenec_core::prelude::*;
use std::time::Instant;

const NOW: i64 = 1_800_000_000_000;
const HOUR: i64 = 3_600_000;

fn exec(db: &mut Database, sql: &str) {
    for s in fenec_ql::parse(sql).expect("parse") {
        db.execute(&s).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

/// Rows whose times are spread over the two hours before `NOW`.
fn fill(db: &mut Database, collections: &[&str], rows: usize) {
    let mut x: u64 = 7;
    for _ in 0..rows.div_ceil(10_000) {
        let mut docs = Vec::with_capacity(10_000);
        for _ in 0..10_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            docs.push(format!(
                "{{n: {}, s: \"w{}\", at: {}}}",
                x % 1000,
                (x >> 10) % 100,
                NOW - ((x >> 20) % (2 * HOUR as u64)) as i64
            ));
        }
        let docs = docs.join(",");
        for c in collections {
            exec(db, &format!("put {c} [{docs}]"));
        }
    }
}

fn median_ms(db: &Database, sql: &str) -> f64 {
    let stmt = fenec_ql::parse_one(sql).unwrap();
    let mut rounds: Vec<f64> = (0..5)
        .map(|_| {
            let mut t: Vec<f64> = (0..9)
                .map(|_| {
                    let s = Instant::now();
                    std::hint::black_box(db.query(&stmt, &[]).unwrap());
                    s.elapsed().as_secs_f64() * 1e3
                })
                .collect();
            t.sort_by(|a, b| a.total_cmp(b));
            t[4]
        })
        .collect();
    rounds.sort_by(|a, b| a.total_cmp(b));
    rounds[2]
}

fn reads(rows: usize) {
    let mut db = Database::new();
    db.set_clock(Some(NOW));
    exec(
        &mut db,
        "create collection t (n int, s text, at timestamp @sorted);
         create collection e (n int, s text, at timestamp @ttl(1h))",
    );
    fill(&mut db, &["t", "e"], rows);
    println!("{rows} rows each, half past their time; median of 5 rounds of 9");
    println!(
        "  {:<44} {:>10} {:>10} {:>12}",
        "query", "no ttl", "@ttl", "written out"
    );
    let alive = format!("not (at <= {})", NOW - HOUR);
    for (q, hand) in [
        ("get {} count", format!("get t where {alive} count")),
        (
            "get {} where n > 500 count",
            format!("get t where n > 500 and {alive} count"),
        ),
        (
            "get {} where s = \"w7\" count",
            format!("get t where s = \"w7\" and {alive} count"),
        ),
        (
            "get {} where n = 5 limit 20",
            format!("get t where n = 5 and {alive} limit 20"),
        ),
        (
            "get {} order at desc limit 20",
            format!("get t where {alive} order at desc limit 20"),
        ),
        (
            "get {} select count(*), sum(n) where s = \"w3\"",
            format!("get t select count(*), sum(n) where s = \"w3\" and {alive}"),
        ),
    ] {
        println!(
            "  {:<44} {:>7.3} ms {:>7.3} ms {:>9.3} ms",
            q.replace("{}", "c"),
            median_ms(&db, &q.replace("{}", "t")),
            median_ms(&db, &q.replace("{}", "e")),
            median_ms(&db, &hand),
        );
    }
}

fn sweep(expired: usize, batch: usize) {
    let dir = std::env::temp_dir().join(format!("fenec-expiry-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sweep.fenec");
    let mut db = fenec_core::fs::open(path.to_str().unwrap()).expect("open");
    exec(
        &mut db,
        "create collection sessions (user text @hash, n int, s text, at timestamp @ttl(1h))",
    );
    // Half past their time, half within it, interleaved as sessions are.
    let mut x: u64 = 11;
    for _ in 0..(2 * expired).div_ceil(10_000) {
        let mut docs = Vec::with_capacity(10_000);
        for i in 0..10_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let at = match i % 2 {
                0 => NOW - HOUR - 1 - (x % HOUR as u64) as i64,
                _ => NOW - (x % HOUR as u64) as i64,
            };
            docs.push(format!(
                "{{user: \"u{}\", n: {}, s: \"session {x}\", at: {at}}}",
                x % 50_000,
                x % 1000
            ));
        }
        exec(&mut db, &format!("put sessions [{}]", docs.join(",")));
    }
    db.sync().unwrap();
    let lock = std::sync::RwLock::new(db);
    let (mut found, mut held, mut synced) = (Vec::new(), Vec::new(), Vec::new());
    let mut swept = 0;
    let t0 = Instant::now();
    loop {
        let s = Instant::now();
        let ids = lock
            .read()
            .unwrap()
            .expired("sessions", NOW, batch)
            .unwrap();
        found.push(s.elapsed().as_secs_f64() * 1e3);
        if ids.is_empty() {
            break;
        }
        let s = Instant::now();
        let durable = {
            let mut g = lock.write().unwrap();
            swept += g.sweep("sessions", NOW, &ids).unwrap();
            g.flush().unwrap()
        };
        held.push(s.elapsed().as_secs_f64() * 1e3);
        let s = Instant::now();
        if let Some(d) = durable {
            d().unwrap();
        }
        synced.push(s.elapsed().as_secs_f64() * 1e3);
        if ids.len() < batch {
            break;
        }
    }
    let total = t0.elapsed();
    let at = |v: &mut Vec<f64>, q: f64| {
        v.sort_by(|a, b| a.total_cmp(b));
        v[((v.len() - 1) as f64 * q).round() as usize]
    };
    println!(
        "{swept} of {} rows swept in {} batches of {batch}: {:.1} ms",
        2 * expired,
        held.len(),
        total.as_secs_f64() * 1e3
    );
    println!(
        "  found under the read lock: p50 {:.2} ms, max {:.2}",
        at(&mut found, 0.5),
        at(&mut found, 1.0)
    );
    println!(
        "  write lock held a batch:   p50 {:.2} ms, p99 {:.2}, max {:.2}",
        at(&mut held, 0.5),
        at(&mut held, 0.99),
        at(&mut held, 1.0)
    );
    println!(
        "  fsync after it, no lock:   p50 {:.2} ms, max {:.2}",
        at(&mut synced, 0.5),
        at(&mut synced, 1.0)
    );
    let g = lock.read().unwrap();
    assert_eq!(swept, expired);
    let left = g
        .query(&fenec_ql::parse_one("get sessions count").unwrap(), &[])
        .unwrap();
    drop(g);
    println!("  left: {:?}", left.rows().unwrap().rows[0].values[0]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The write lock's hold a batch of the sweep, as `sweep` takes it.
fn sweep_batches(db: Database, collection: &str, batch: usize) -> (usize, Vec<f64>) {
    let lock = std::sync::RwLock::new(db);
    let (mut held, mut swept) = (Vec::new(), 0);
    loop {
        let ids = lock
            .read()
            .unwrap()
            .expired(collection, NOW, batch)
            .unwrap();
        if ids.is_empty() {
            break;
        }
        let s = Instant::now();
        {
            let mut g = lock.write().unwrap();
            swept += g.sweep(collection, NOW, &ids).unwrap();
        }
        held.push(s.elapsed().as_secs_f64() * 1e3);
        if ids.len() < batch {
            break;
        }
    }
    (swept, held)
}

/// `rows` events a page at a time, times ascending with the ids, the
/// first `expired` of them past their time; `kind` of five values.
fn events(db: &mut Database, collection: &str, rows: usize, expired: usize) {
    let base = NOW - HOUR - expired as i64 + 1;
    for page in 0..rows.div_ceil(10_000) {
        let docs: Vec<String> = (page * 10_000..((page + 1) * 10_000).min(rows))
            .map(|i| {
                format!(
                    "{{kind: \"k{}\", n: {}, at: {}}}",
                    i % 5,
                    i % 1000,
                    base + i as i64
                )
            })
            .collect();
        exec(db, &format!("put {collection} [{}]", docs.join(",")));
    }
}

fn hash_sweep(rows: usize, expired: usize, batch: usize) {
    let mut db = Database::new();
    db.set_clock(Some(NOW));
    exec(
        &mut db,
        "create collection events (kind text @hash, n int, at timestamp @ttl(1h))",
    );
    let t = Instant::now();
    events(&mut db, "events", rows, expired);
    // Built, as the first read after an open builds it.
    exec(&mut db, "get events where kind = \"k0\" limit 1");
    println!(
        "{rows} rows, a @hash field of 5 values: written in {:.1} s",
        t.elapsed().as_secs_f64()
    );
    let (swept, mut held) = sweep_batches(db, "events", batch);
    held.sort_by(|a, b| a.total_cmp(b));
    let at = |q: f64| held[((held.len() - 1) as f64 * q).round() as usize];
    println!(
        "  {swept} expired rows swept in {} batches of {batch}: write lock held p50 {:.2} ms, \
         max {:.2}",
        held.len(),
        at(0.5),
        at(1.0)
    );
}

/// A lone `put` and a lone `del` among `rows` rows: the median of 21
/// rounds of 1 000 each, in nanoseconds a write.
fn writes(rows: usize) {
    for (name, decl) in [
        (
            "@hash",
            "create collection events (kind text @hash, n int, at timestamp)",
        ),
        (
            "none ",
            "create collection events (kind text, n int, at timestamp)",
        ),
    ] {
        let mut db = Database::new();
        exec(&mut db, decl);
        events(&mut db, "events", rows, 0);
        exec(&mut db, "get events where kind = \"k0\" limit 1");
        let put = fenec_ql::parse_one("put events {kind: $1, n: 1, at: 5}").unwrap();
        let del = fenec_ql::parse_one("del events where id = $1").unwrap();
        let (mut puts, mut dels) = (Vec::new(), Vec::new());
        let mut x: u64 = 3;
        for _ in 0..21 {
            let kinds: Vec<Value> = (0..1000)
                .map(|i| Value::Text(format!("k{}", i % 5)))
                .collect();
            let s = Instant::now();
            for k in &kinds {
                db.execute_with(&put, std::slice::from_ref(k)).unwrap();
            }
            puts.push(s.elapsed().as_nanos() as f64 / 1000.0);
            // Rows here and there among the first `rows`.
            let ids: Vec<Value> = (0..1000)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    Value::Int((x % rows as u64) as i64 + 1)
                })
                .collect();
            let s = Instant::now();
            for id in &ids {
                db.execute_with(&del, std::slice::from_ref(id)).unwrap();
            }
            dels.push(s.elapsed().as_nanos() as f64 / 1000.0);
        }
        puts.sort_by(|a, b| a.total_cmp(b));
        dels.sort_by(|a, b| a.total_cmp(b));
        println!(
            "{rows} rows, kind {name}: a put {:.0} ns, a del {:.0} ns",
            puts[10], dels[10]
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let num = |i: usize, d: usize| args.get(i).and_then(|a| a.parse().ok()).unwrap_or(d);
    match args.get(1).map(String::as_str) {
        Some("sweep") => sweep(num(2, 100_000), num(3, 1_000)),
        Some("hash") => hash_sweep(num(2, 10_000_000), num(3, 20_000), num(4, 1_000)),
        Some("writes") => writes(num(2, 1_000_000)),
        _ => reads(num(2, 1_000_000)),
    }
}
