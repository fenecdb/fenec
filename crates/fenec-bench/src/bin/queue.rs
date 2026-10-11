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
//!   * a claim held until a job comes (`Fenec-Wait`), against a worker
//!     polling every 10 and 100 ms: how soon a job enqueued, or a delayed
//!     one come due, reaches a worker, and what the server spends meanwhile;
//!     100 and 1 000 held claims on an empty queue for 10 s -- the server's
//!     CPU, resident memory and threads, beside as many clients polling and
//!     as many subscriptions -- and the million jobs over HTTP again with
//!     the header on every claim, and beside 16 claims held on a queue that
//!     stays empty (`held`).
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

// ------------------------------------------------------------ held claims

/// Microseconds since the bench began, the clock a job's `sent` is on.
fn micros() -> i64 {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as i64
}

/// The server's CPU seconds so far, resident bytes and threads.
fn usage(pid: u32) -> (f64, u64, u64) {
    let ps = |args: &[&str]| {
        let out = std::process::Command::new("ps")
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let pid = pid.to_string();
    let line = ps(&["-o", "time=,rss=", "-p", &pid]);
    let mut parts = line.split_whitespace();
    // `[[dd-]hh:]mm:ss.cc`
    let cpu = parts.next().map_or(0.0, |t| {
        t.split(['-', ':'])
            .fold(0.0, |acc, p| acc * 60.0 + p.parse::<f64>().unwrap_or(0.0))
    });
    let rss = parts
        .next()
        .and_then(|r| r.parse::<u64>().ok())
        .unwrap_or(0)
        * 1024;
    // Linux names the threads' count; macOS lists them a line each.
    let threads = match ps(&["-o", "nlwp=", "-p", &pid]).trim().parse::<u64>() {
        Ok(n) => n,
        Err(_) => ps(&["-M", "-p", &pid]).lines().count().saturating_sub(1) as u64,
    };
    (cpu, rss, threads)
}

/// An empty queue in a fresh server's file, and the server.
fn empty_queue(extra: &[&str]) -> (http::Server, String, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("fenecbench-held-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("held.fenec");
    let _ = std::fs::remove_file(&file);
    let port = http::free_port();
    let server = http::start_fenec(&file, port, "queue-bench", extra);
    let addr = format!("127.0.0.1:{port}");
    http::Http::connect(&addr).query(
        "create collection jobs (kind text, sent int, run_at timestamp @sorted, owner text, \
         attempts int)",
        "",
    );
    (server, addr, dir)
}

/// A claim held beside a busy queue: of jobs a year overdue, which none
/// is, so its look is a step of the index and finds nothing, at every
/// claim and ack the busy workers write.
const ASIDE: &str = "set jobs {owner: $1} where run_at <= now() - 31536000000 and kind = $2 \
                     order run_at limit 1 returning id, sent";

const HELD_CLAIM: &str = "set jobs {owner: $1, run_at: now() + 600000, attempts: attempts + 1} \
                          where run_at <= now() and kind = $2 order run_at limit 1 \
                          returning id, kind, sent, run_at";

/// How a worker waits for a job: held by the server, or polling.
#[derive(Clone, Copy)]
enum Wait {
    Held,
    Poll(u64),
}

impl Wait {
    fn name(self) -> String {
        match self {
            Wait::Held => "held (Fenec-Wait)".into(),
            Wait::Poll(ms) => format!("polling every {ms} ms"),
        }
    }
}

/// A worker `claimers` started: the jobs it held, as (job's `sent`,
/// received) in us, and the claims it sent.
type Claimer = std::thread::JoinHandle<(Vec<(i64, i64)>, usize)>;

/// `workers` claiming jobs of `kind` as they come until each has claimed a
/// stop job (`sent` -1): what each saw, as (job's `sent`, received), in us,
/// and how many claims it sent.
fn claimers(
    addr: &str,
    workers: usize,
    (claim, kind): (&'static str, &'static str),
    wait: Wait,
) -> Vec<Claimer> {
    (0..workers)
        .map(|w| {
            let addr = addr.to_string();
            std::thread::spawn(move || {
                let mut c = http::Http::connect(&addr);
                let (mut got, mut claims) = (Vec::new(), 0);
                for k in 0.. {
                    let owner = format!("\"{kind}{w}:{k}\"");
                    let params = format!("{owner},\"{kind}\"");
                    claims += 1;
                    let body = match wait {
                        Wait::Held => c
                            .query_with(claim, &params, "Fenec-Wait: 30000\r\n")
                            .to_vec(),
                        Wait::Poll(_) => c.query(claim, &params).to_vec(),
                    };
                    let at = micros();
                    let sent = http::ints(&body, "sent");
                    if sent.is_empty() {
                        if let Wait::Poll(ms) = wait {
                            std::thread::sleep(std::time::Duration::from_millis(ms));
                        }
                        continue;
                    }
                    if sent.contains(&-1) {
                        break;
                    }
                    for s in sent {
                        got.push((s, at));
                        RECEIVED.fetch_add(1, Ordering::AcqRel);
                    }
                    for id in http::ids(&body) {
                        c.query(ACK, &format!("{id},{owner}"));
                    }
                }
                (got, claims)
            })
        })
        .collect()
}

/// Jobs the workers `claimers` started have held: a part waits for all
/// of them before it stops the workers, whose stop jobs, the oldest, would
/// otherwise go before the last jobs.
static RECEIVED: AtomicUsize = AtomicUsize::new(0);

fn received(n: usize) {
    let until = Instant::now() + std::time::Duration::from_secs(60);
    while RECEIVED.load(Ordering::Acquire) < n {
        assert!(
            Instant::now() < until,
            "{} of {n} jobs held",
            RECEIVED.load(Ordering::Acquire)
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// Ends the workers `claimers` started on `kind`: a stop job each.
fn stop(addr: &str, workers: usize, kind: &str) {
    let mut c = http::Http::connect(addr);
    for _ in 0..workers {
        c.query(
            "put jobs {kind: $1, sent: -1, run_at: 0, attempts: 0}",
            &format!("\"{kind}\""),
        );
    }
}

fn ms_percentiles(mut us: Vec<f64>) -> String {
    us.sort_by(|a, b| a.total_cmp(b));
    let at = |p: f64| us[((us.len() as f64 * p) as usize).min(us.len() - 1)];
    format!(
        "p50 {:.2} ms, p99 {:.2}, max {:.2}",
        at(0.5) / 1e3,
        at(0.99) / 1e3,
        us[us.len() - 1] / 1e3
    )
}

/// 8 workers waiting for 1 000 jobs enqueued one at a time, 2 to 20 ms
/// apart: from a job's put sent to a worker holding it, and the server's
/// CPU meanwhile.
fn pickup(wait: Wait) {
    const WORKERS: usize = 8;
    const N: usize = 1000;
    let (server, addr, dir) = empty_queue(&["--max-connections", "200"]);
    RECEIVED.store(0, Ordering::Release);
    let hs = claimers(&addr, WORKERS, (HELD_CLAIM, "mail"), wait);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let (cpu0, _, _) = usage(server.pid());
    let began = Instant::now();
    let mut c = http::Http::connect(&addr);
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    for _ in 0..N {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        std::thread::sleep(std::time::Duration::from_micros(2000 + seed % 18_000));
        c.query(
            "put jobs {kind: \"mail\", sent: $1, run_at: now(), attempts: 0}",
            &micros().to_string(),
        );
    }
    received(N);
    let took = began.elapsed().as_secs_f64();
    let (cpu1, _, _) = usage(server.pid());
    stop(&addr, WORKERS, "mail");
    let (mut lat, mut claims) = (Vec::new(), 0);
    for h in hs {
        let (got, k) = h.join().unwrap();
        lat.extend(got.iter().map(|(s, at)| (at - s) as f64));
        claims += k;
    }
    assert_eq!(lat.len(), N, "every job picked up once");
    server.terminate();
    let _ = std::fs::remove_dir_all(&dir);
    println!(
        "  {WORKERS} workers, {}: a job enqueued to a worker holding it {}; \
         {claims} claims sent, the server's CPU {:.1}% of a core",
        wait.name(),
        ms_percentiles(lat),
        (cpu1 - cpu0) / took * 100.0
    );
}

/// 400 jobs delayed 50 to 2 000 ms, all enqueued at once, 8 workers
/// waiting: how long after its time each is held by a worker.
fn delayed(wait: Wait) {
    const WORKERS: usize = 8;
    const N: usize = 400;
    let (server, addr, dir) = empty_queue(&["--max-connections", "200"]);
    RECEIVED.store(0, Ordering::Release);
    let hs = claimers(&addr, WORKERS, (HELD_CLAIM, "mail"), wait);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let mut c = http::Http::connect(&addr);
    let now = fenec_core::time::now_ms().unwrap();
    let docs: Vec<String> = (0..N)
        .map(|i| {
            let due = now + 50 + (i as i64 * 7919) % 1950;
            // `sent` carries the job's time on the bench's clock.
            let sent = micros() + (due - now) * 1000;
            format!("{{kind: \"mail\", sent: {sent}, run_at: {due}, attempts: 0}}")
        })
        .collect();
    c.query(&format!("put jobs [{}]", docs.join(", ")), "");
    let began = Instant::now();
    let (cpu0, _, _) = usage(server.pid());
    while began.elapsed() < std::time::Duration::from_millis(2300) {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    received(N);
    let (cpu1, _, _) = usage(server.pid());
    stop(&addr, WORKERS, "mail");
    let mut lat = Vec::new();
    for h in hs {
        lat.extend(h.join().unwrap().0.iter().map(|(s, at)| (at - s) as f64));
    }
    assert_eq!(lat.len(), N, "every job picked up once");
    assert!(
        lat.iter().all(|l| *l > -2000.0),
        "none taken before its time"
    );
    server.terminate();
    let _ = std::fs::remove_dir_all(&dir);
    println!(
        "  {WORKERS} workers, {}: a delayed job from its time to a worker holding it {}; \
         the server's CPU {:.1}% of a core",
        wait.name(),
        ms_percentiles(lat),
        (cpu1 - cpu0) / 2.3 * 100.0
    );
}

/// `n` clients waiting on an empty queue for 10 s, held or polling every
/// 100 ms, or as many subscriptions to it: the server's CPU, and its
/// resident memory and threads past what it held before they came.
fn idle(n: usize, how: &str) {
    let (server, addr, dir) =
        empty_queue(&["--max-connections", "5000", "--http-max-streams", "5000"]);
    let (_, rss0, threads0) = usage(server.pid());
    let mut subs = Vec::new();
    let hs = match how {
        "held" => claimers(&addr, n, (HELD_CLAIM, "mail"), Wait::Held),
        "poll" => claimers(&addr, n, (HELD_CLAIM, "mail"), Wait::Poll(100)),
        _ => {
            for _ in 0..n {
                use std::io::{Read, Write};
                let mut s = std::net::TcpStream::connect(&addr).unwrap();
                s.write_all(b"GET /jobs/changes HTTP/1.1\r\nHost: b\r\n\r\n")
                    .unwrap();
                let mut seed = [0u8; 256];
                let _ = s.read(&mut seed).unwrap();
                subs.push(s);
            }
            Vec::new()
        }
    };
    // Every client in: held, its claim run once and answered nothing.
    let mut m = http::Http::connect(&addr);
    let until = Instant::now() + std::time::Duration::from_secs(60);
    if how == "held" {
        while !String::from_utf8_lossy(m.get("/_metrics"))
            .contains(&format!("fenec_held_requests {n}\n"))
        {
            assert!(Instant::now() < until, "{n} never held");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    } else {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    let (cpu0, rss1, threads1) = usage(server.pid());
    std::thread::sleep(std::time::Duration::from_secs(10));
    let (cpu1, rss2, _) = usage(server.pid());
    let rss = rss1.max(rss2);
    if how != "subscriptions" {
        stop(&addr, n, "mail");
    }
    for h in hs {
        h.join().unwrap();
    }
    drop(subs);
    server.terminate();
    let _ = std::fs::remove_dir_all(&dir);
    let what = match how {
        "held" => "claims held",
        "poll" => "clients polling every 100 ms",
        _ => "subscriptions",
    };
    println!(
        "  {n} {what}, 10 s on an empty queue: the server's CPU {:.2}% of a core, \
         +{:.1} MB resident ({:.1} KB each), +{} threads",
        (cpu1 - cpu0) / 10.0 * 100.0,
        (rss as f64 - rss0 as f64) / 1e6,
        (rss as f64 - rss0 as f64) / n as f64 / 1e3,
        threads1.saturating_sub(threads0)
    );
}

/// The million jobs over HTTP again, each claim sent with `Fenec-Wait`
/// (`header`), or beside `aside` claims held that no job is for ([`ASIDE`]),
/// whose group's head every write to the collection wakes to look.
fn busy(n: usize, header: bool, aside: usize) {
    let dir = std::env::temp_dir().join(format!("fenecbench-busy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("queue.fenec");
    let _ = std::fs::remove_file(&file);
    {
        let mut db = fenec_core::fs::open(&file).unwrap();
        queue(&mut db, n, "@sorted");
        // What a held claim's stop job is told by.
        run(&mut db, "alter collection jobs add field sent int");
        db.checkpoint().unwrap();
    }
    let port = http::free_port();
    let server = http::start_fenec(&file, port, "queue-bench", &[]);
    let addr = format!("127.0.0.1:{port}");
    let idle = claimers(&addr, aside, (ASIDE, "none"), Wait::Held);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let at = Instant::now();
    let hs: Vec<_> = (0..THREADS)
        .map(|t| {
            let addr = addr.clone();
            std::thread::spawn(move || {
                let mut c = http::Http::connect(&addr);
                let (mut done, mut us) = (0usize, Vec::new());
                for k in 0.. {
                    let owner = format!("\"w{t}:{k}\"");
                    let began = Instant::now();
                    let ids = match header {
                        true => http::ids(c.query_with(CLAIM, &owner, "Fenec-Wait: 1\r\n")),
                        false => http::ids(c.query(CLAIM, &owner)),
                    };
                    us.push(began.elapsed().as_secs_f64() * 1e6);
                    if ids.is_empty() {
                        break;
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
    // What the held claims cost while the busy ones ran: their heads' looks.
    let metrics = String::from_utf8_lossy(http::Http::connect(&addr).get("/_metrics")).into_owned();
    let looks = metrics
        .lines()
        .find_map(|l| l.strip_prefix("fenec_held_looks_total "))
        .and_then(|n| n.trim().parse::<u64>().ok())
        .unwrap_or(0);
    stop(&addr, aside, "none");
    for h in idle {
        h.join().unwrap();
    }
    server.terminate();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(done, n, "every job acked once");
    let how = match (header, aside) {
        (true, _) => "each claim with Fenec-Wait".to_string(),
        (false, 0) => "no claim held".to_string(),
        (false, a) => format!("beside {a} claims held that no job is for"),
    };
    println!(
        "  {THREADS} HTTP clients, {how}: {n} jobs in {s:.2} s, {:.0} jobs/s; a claim {}{}",
        n as f64 / s,
        percentiles(us),
        match aside {
            0 => String::new(),
            _ => format!("; {:.0} looks/s by the held claims", looks as f64 / s),
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
    if wants("held") {
        // `held busy`, `held idle`: one of its parts.
        let sub = std::env::args().nth(2);
        let part = |p: &str| sub.as_deref().is_none_or(|s| s == p);
        println!("a claim held until a job comes (Fenec-Wait), against polling:");
        if part("pickup") {
            for w in [Wait::Held, Wait::Poll(10), Wait::Poll(100)] {
                pickup(w);
            }
        }
        if part("delayed") {
            for w in [Wait::Held, Wait::Poll(10), Wait::Poll(100)] {
                delayed(w);
            }
        }
        if part("idle") {
            for clients in [100, 1000] {
                for how in ["held", "poll", "subscriptions"] {
                    idle(clients, how);
                }
            }
        }
        if part("busy") {
            busy(n, false, 0);
            busy(n, true, 0);
            busy(n, false, 16);
        }
    }
}
