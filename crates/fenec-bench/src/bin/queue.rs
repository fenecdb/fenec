//! A job queue's claim: `make queue-bench`.
//!
//!   * one worker claiming batches of 10 from a million ready jobs, the claim
//!     a statement (`set ... order run_at limit 10 returning *`), through
//!     `@sorted` on the ready time and through a scan of the same rows;
//!   * 16 threads under the lock a server takes, claiming batches of 10
//!     and acking each job until the million are done -- and the same with
//!     the claim a client sends without `returning`: read the ready page,
//!     compare-and-set it, read back which jobs it won, three statements
//!     and a race lost whenever another worker read the same page;
//!   * the same over HTTP against fenec-server, 16 clients: a claim and a
//!     `/batch` of its acks, two round trips a batch, against the three
//!     statements' three.
//!
//! `JOBS=200000` runs it over fewer jobs.

#[path = "../http.rs"]
mod http;

use fenec_core::prelude::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

const THREADS: usize = 16;
const BATCH: usize = 10;

const CLAIM: &str = "set jobs {owner: $1, run_at: now() + 600000, attempts: attempts + 1} \
                     where run_at <= now() order run_at limit 10 returning *";
const ACK: &str = "del jobs where id = $1 and owner = $2 require 1";
/// The client's claim without `returning`: the ready page read ...
const READ: &str = "get jobs select id where run_at <= now() order run_at limit 10";

fn jobs() -> usize {
    std::env::var("JOBS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(1_000_000)
}

fn run(db: &mut Database, sql: &str) {
    db.execute(&fenec_ql::parse_one(sql).unwrap()).unwrap();
}

/// `n` jobs, ready a millisecond apart up to now, the oldest first: the
/// ready time indexed (`@sorted`) or not.
fn queue(db: &mut Database, n: usize, index: &str) {
    run(
        db,
        &format!(
            "create collection jobs (kind text, payload json, run_at timestamp {index}, \
             owner text, attempts int)"
        ),
    );
    let now = fenec_core::time::now_ms().unwrap();
    for b in 0..n.div_ceil(1000) {
        let docs: Vec<String> = (b * 1000..((b + 1) * 1000).min(n))
            .map(|i| {
                format!(
                    "{{kind: \"mail\", payload: {{\"to\": \"user{i}@example.com\", \"n\": {i}}}, \
                     run_at: {}, attempts: 0}}",
                    now - (n - i) as i64
                )
            })
            .collect();
        run(db, &format!("put jobs [{}]", docs.join(", ")));
    }
}

fn percentiles(mut us: Vec<f64>) -> String {
    us.sort_by(|a, b| a.total_cmp(b));
    let at = |p: f64| us[((us.len() as f64 * p) as usize).min(us.len() - 1)];
    format!(
        "p50 {:.1} us, p99 {:.1}, max {:.1}",
        at(0.5),
        at(0.99),
        us[us.len() - 1]
    )
}

fn ids_of(r: &Response) -> Vec<i64> {
    match r {
        Response::Rows(rs) => rs.rows.iter().map(|r| r.id as i64).collect(),
        r => panic!("{r:?}"),
    }
}

/// One worker's claims, each timed: through the index, and a scan.
fn latency(n: usize) {
    for (index, claims) in [("@sorted", 2000), ("", 20)] {
        let mut db = Database::new();
        queue(&mut db, n, index);
        let claim = fenec_ql::parse_one(CLAIM).unwrap();
        let owner = [Value::Text("w".into())];
        let mut us = Vec::with_capacity(claims);
        for _ in 0..claims {
            let at = Instant::now();
            let r = db.execute_with(&claim, &owner).unwrap();
            us.push(at.elapsed().as_secs_f64() * 1e6);
            assert_eq!(ids_of(&r).len(), BATCH);
        }
        let how = if index.is_empty() {
            "a scan, no index"
        } else {
            "@sorted run_at"
        };
        println!(
            "  one worker, a claim of {BATCH} over {n} jobs, {how}: {}",
            percentiles(us)
        );
    }
}

/// `threads` workers under a server's lock, until every job is acked: the
/// claim one statement, or the three a client sends without `returning`.
fn workers(n: usize, three: bool) {
    let mut db = Database::new();
    queue(&mut db, n, "@sorted");
    let db = Arc::new(RwLock::new(db));
    let lost = Arc::new(AtomicUsize::new(0));
    let at = Instant::now();
    let hs: Vec<_> = (0..THREADS)
        .map(|t| {
            let (db, lost) = (db.clone(), lost.clone());
            std::thread::spawn(move || {
                let claim = fenec_ql::parse_one(CLAIM).unwrap();
                let ack = fenec_ql::parse_one(ACK).unwrap();
                let read = fenec_ql::parse_one(READ).unwrap();
                let (mut done, mut us) = (0usize, Vec::new());
                for k in 0.. {
                    let owner = Value::Text(format!("w{t}:{k}"));
                    let began = Instant::now();
                    let ids = if three {
                        let page = ids_of(&db.read().unwrap().query(&read, &[]).unwrap());
                        if page.is_empty() {
                            break;
                        }
                        let list = (0..page.len())
                            .map(|i| format!("${}", i + 2))
                            .collect::<Vec<_>>()
                            .join(", ");
                        let mut params = vec![owner.clone()];
                        params.extend(page.iter().map(|&i| Value::Int(i)));
                        let cas = format!(
                            "set jobs {{owner: $1, run_at: now() + 600000, attempts: attempts + 1}} \
                             where id in [{list}] and run_at <= now()"
                        );
                        db.write()
                            .unwrap()
                            .execute_with(&fenec_ql::parse_one(&cas).unwrap(), &params)
                            .unwrap();
                        let back = format!("get jobs select id where id in [{list}] and owner = $1");
                        let won = ids_of(
                            &db.read()
                                .unwrap()
                                .query(&fenec_ql::parse_one(&back).unwrap(), &params)
                                .unwrap(),
                        );
                        lost.fetch_add(page.len() - won.len(), Ordering::Relaxed);
                        won
                    } else {
                        let r = db
                            .write()
                            .unwrap()
                            .execute_with(&claim, std::slice::from_ref(&owner))
                            .unwrap();
                        let ids = ids_of(&r);
                        if ids.is_empty() {
                            break;
                        }
                        ids
                    };
                    us.push(began.elapsed().as_secs_f64() * 1e6);
                    for id in ids {
                        let r = db
                            .write()
                            .unwrap()
                            .execute_with(&ack, &[Value::Int(id), owner.clone()])
                            .unwrap();
                        assert_eq!(r, Response::Affected(1));
                        done += 1;
                    }
                }
                (done, us)
            })
        })
        .collect();
    let (mut done, mut us) = (0, Vec::new());
    for h in hs {
        let (d, u) = h.join().unwrap();
        done += d;
        us.extend(u);
    }
    let s = at.elapsed().as_secs_f64();
    assert_eq!(done, n, "every job acked once");
    let claims = us.len();
    let how = if three {
        "read, compare-and-set, read back"
    } else {
        "set ... returning, one statement"
    };
    println!(
        "  {THREADS} threads, {how}: {n} jobs claimed and acked in {s:.2} s, {:.0} jobs/s, \
         {:.0} claims/s; a claim {}{}",
        n as f64 / s,
        claims as f64 / s,
        percentiles(us),
        if three {
            format!(
                "; {} jobs read and lost to another worker",
                lost.load(Ordering::Relaxed)
            )
        } else {
            String::new()
        }
    );
}

/// The same over HTTP: 16 clients against fenec-server, a claim and a
/// `/batch` of its acks, or the three statements and the acks.
fn over_http(n: usize, three: bool) {
    let dir = std::env::temp_dir().join(format!("fenecbench-queue-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("queue.fenec");
    let _ = std::fs::remove_file(&file);
    {
        let mut db = fenec_core::fs::open(&file).unwrap();
        queue(&mut db, n, "@sorted");
        db.checkpoint().unwrap();
    }
    let port = http::free_port();
    let server = http::start_fenec(&file, port, "queue-bench", &[]);
    let addr = format!("127.0.0.1:{port}");
    let lost = Arc::new(AtomicUsize::new(0));
    let at = Instant::now();
    let hs: Vec<_> = (0..THREADS)
        .map(|t| {
            let (addr, lost) = (addr.clone(), lost.clone());
            std::thread::spawn(move || {
                let mut c = http::Http::connect(&addr);
                let (mut done, mut us) = (0usize, Vec::new());
                for k in 0.. {
                    let owner = format!("\"w{t}:{k}\"");
                    let began = Instant::now();
                    let ids = if three {
                        let page = http::ids(c.query(READ, ""));
                        if page.is_empty() {
                            break;
                        }
                        let list = (0..page.len())
                            .map(|i| format!("${}", i + 2))
                            .collect::<Vec<_>>()
                            .join(", ");
                        let params = std::iter::once(owner.clone())
                            .chain(page.iter().map(|i| i.to_string()))
                            .collect::<Vec<_>>()
                            .join(",");
                        c.query(
                            &format!(
                                "set jobs {{owner: $1, run_at: now() + 600000, attempts: attempts + 1}} \
                                 where id in [{list}] and run_at <= now()"
                            ),
                            &params,
                        );
                        let won = http::ids(c.query(
                            &format!("get jobs select id where id in [{list}] and owner = $1"),
                            &params,
                        ));
                        lost.fetch_add(page.len() - won.len(), Ordering::Relaxed);
                        won
                    } else {
                        let ids = http::ids(c.query(CLAIM, &owner));
                        if ids.is_empty() {
                            break;
                        }
                        ids
                    };
                    us.push(began.elapsed().as_secs_f64() * 1e6);
                    if ids.is_empty() {
                        continue;
                    }
                    let acks: Vec<String> = ids
                        .iter()
                        .map(|id| {
                            format!(
                                "{{\"query\":{},\"params\":[{id},{owner}]}}",
                                http::json_string(ACK)
                            )
                        })
                        .collect();
                    c.post("/batch", "application/x-ndjson", acks.join("\n").as_bytes());
                    done += ids.len();
                }
                (done, us)
            })
        })
        .collect();
    let (mut done, mut us) = (0, Vec::new());
    for h in hs {
        let (d, u) = h.join().unwrap();
        done += d;
        us.extend(u);
    }
    let s = at.elapsed().as_secs_f64();
    server.terminate();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(done, n, "every job acked once");
    let how = if three {
        "read, compare-and-set, read back: three round trips"
    } else {
        "set ... returning: one round trip"
    };
    println!(
        "  {THREADS} HTTP clients, {how}, acks a /batch: {n} jobs in {s:.2} s, {:.0} jobs/s; \
         a claim {}{}",
        n as f64 / s,
        percentiles(us),
        if three {
            format!(
                "; {} jobs read and lost to another client",
                lost.load(Ordering::Relaxed)
            )
        } else {
            String::new()
        }
    );
}

fn main() {
    let n = jobs();
    let only = std::env::args().nth(1);
    let wants = |part: &str| only.as_deref().is_none_or(|o| o == part);
    println!("a job queue's claim, {n} jobs:");
    if wants("latency") {
        latency(n);
    }
    if wants("threads") {
        workers(n, false);
        workers(n, true);
    }
    if wants("http") {
        over_http(n, false);
        over_http(n, true);
    }
}
