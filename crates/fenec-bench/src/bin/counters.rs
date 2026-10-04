//! What an expression in a `set` costs, and whether increments from many
//! writers all land: `make counters-bench`.
//!
//!   * a `set` of a constant against a `set` of `n + 1`, over 100 000 rows
//!     at once and over one row by id, in process;
//!   * 16 threads each incrementing one key 10 000 times in process, under
//!     the lock a server takes for a write, and 16 HTTP clients doing the
//!     same against fenec-server: the count must end at 160 000 exactly;
//!   * each recipe of the docs' "Instead of Redis" page, its statements
//!     timed in process, and a write's way to a subscriber over HTTP.
//!
//! `--constant` runs the first part's constant `set`s alone, in text any
//! build parses, so the same file measures a build from before expressions.

#[path = "../http.rs"]
mod http;

use fenec_core::prelude::*;
use std::sync::{Arc, RwLock};
use std::time::Instant;

const ROWS: usize = 100_000;
const THREADS: usize = 16;
const EACH: usize = 10_000;

fn run(db: &mut Database, sql: &str, params: &[Value]) -> Response {
    db.execute_with(&fenec_ql::parse_one(sql).unwrap(), params)
        .unwrap()
}

fn loaded() -> Database {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection hits (key text @hash, n int, note text)",
        &[],
    );
    for b in 0..ROWS / 1000 {
        let docs: Vec<String> = (0..1000)
            .map(|i| {
                let n = b * 1000 + i;
                format!("{{key: \"k{n}\", n: {n}, note: \"a note of some length, {n}\"}}")
            })
            .collect();
        run(&mut db, &format!("put hits [{}]", docs.join(", ")), &[]);
    }
    db
}

/// The median of `runs` timings of `f`, in ms.
fn median(runs: usize, mut f: impl FnMut()) -> f64 {
    let mut t: Vec<f64> = (0..runs)
        .map(|_| {
            let at = Instant::now();
            f();
            at.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    t.sort_by(|a, b| a.total_cmp(b));
    t[runs / 2]
}

fn sets(constant_only: bool) {
    let mut db = loaded();
    let mut cases = vec![("set hits {n: 7}", "a constant")];
    if !constant_only {
        cases.push(("set hits {n: n + 1}", "n + 1"));
    }
    println!("set over {ROWS} rows, median of 7, in process:");
    for (sql, what) in &cases {
        let st = fenec_ql::parse_one(sql).unwrap();
        let ms = median(7, || {
            db.execute_with(&st, &[]).unwrap();
        });
        println!(
            "  {what:<12} {ms:>8.1} ms  ({:.0} ns a row)",
            ms * 1e6 / ROWS as f64
        );
    }
    let mut one = vec![("set hits {n: 7} where id = $1", "a constant")];
    if !constant_only {
        one.push(("set hits {n: n + 1} where id = $1", "n + 1"));
    }
    println!("set of one row by id, 100 000 times, median of 5 runs:");
    for (sql, what) in &one {
        let st = fenec_ql::parse_one(sql).unwrap();
        let ms = median(5, || {
            for i in 0..100_000u64 {
                let id = Value::Int((i % ROWS as u64 + 1) as i64);
                db.execute_with(&st, &[id]).unwrap();
            }
        });
        println!("  {what:<12} {:>8.3} us a statement", ms * 1e3 / 100_000.0);
    }
}

fn counter(db: &RwLock<Database>) -> i64 {
    let r = db
        .read()
        .unwrap()
        .query(
            &fenec_ql::parse_one("get hits select n where key = \"k0\"").unwrap(),
            &[],
        )
        .unwrap();
    match r.rows().unwrap().rows[0].values[0] {
        Value::Int(n) => n,
        ref v => panic!("{v:?}"),
    }
}

fn in_process() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection hits (key text @hash, n int)",
        &[],
    );
    run(&mut db, "put hits {key: \"k0\", n: 0}", &[]);
    let db = Arc::new(RwLock::new(db));
    let st = Arc::new(fenec_ql::parse_one("set hits {n: n + 1} where key = $1").unwrap());
    let at = Instant::now();
    let hs: Vec<_> = (0..THREADS)
        .map(|_| {
            let (db, st) = (db.clone(), st.clone());
            std::thread::spawn(move || {
                let key = [Value::Text("k0".into())];
                for _ in 0..EACH {
                    db.write().unwrap().execute_with(&st, &key).unwrap();
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    let s = at.elapsed().as_secs_f64();
    let n = counter(&db);
    println!(
        "{THREADS} threads x {EACH} increments of one key, in process: {n} ({}), {:.0} a second",
        if n == (THREADS * EACH) as i64 {
            "every one"
        } else {
            "LOST SOME"
        },
        (THREADS * EACH) as f64 / s
    );
    assert_eq!(n, (THREADS * EACH) as i64);
}

fn over_http() {
    let dir = std::env::temp_dir().join(format!("fenecbench-counters-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("counters.fenec");
    let _ = std::fs::remove_file(&file);
    {
        let mut db = fenec_core::fs::open(&file).unwrap();
        run(
            &mut db,
            "create collection hits (key text @hash, n int)",
            &[],
        );
        run(&mut db, "put hits {key: \"k0\", n: 0}", &[]);
        db.checkpoint().unwrap();
    }
    let port = http::free_port();
    let server = http::start_fenec(&file, port, "counters-bench", &[]);
    let addr = format!("127.0.0.1:{port}");
    let at = Instant::now();
    let hs: Vec<_> = (0..THREADS)
        .map(|_| {
            let addr = addr.clone();
            std::thread::spawn(move || {
                let mut c = http::Http::connect(&addr);
                for _ in 0..EACH {
                    c.query("set hits {n: n + 1} where key = $1", "\"k0\"");
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    let s = at.elapsed().as_secs_f64();
    let mut c = http::Http::connect(&addr);
    let body =
        String::from_utf8_lossy(c.query("get hits select n where key = $1", "\"k0\"")).to_string();
    server.terminate();
    let n: i64 = body
        .split("\"n\":")
        .nth(1)
        .and_then(|t| t.trim_start().split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|t| t.parse().ok())
        .unwrap_or_else(|| panic!("{body}"));
    println!(
        "{THREADS} HTTP clients x {EACH} increments of one key: {n} ({}), {:.0} a second",
        if n == (THREADS * EACH) as i64 {
            "every one"
        } else {
            "LOST SOME"
        },
        (THREADS * EACH) as f64 / s
    );
    assert_eq!(n, (THREADS * EACH) as i64);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The mean of `n` runs of `f`, in us.
fn each(n: usize, mut f: impl FnMut(usize)) -> f64 {
    let at = Instant::now();
    for i in 0..n {
        f(i);
    }
    at.elapsed().as_secs_f64() * 1e6 / n as f64
}

/// The statements of each recipe on site/content/docs/redis.html, timed
/// one after another in process.
fn recipes() {
    const N: usize = 100_000;
    let mut db = Database::new();
    db.set_clock(Some(1_800_000_000_000));
    for ddl in [
        "create collection counters (name text @unique, n int)",
        "create collection hits (key text @unique, n int, at timestamp @ttl(2m))",
        "create collection locks (name text @unique, owner text, at timestamp @ttl(30s))",
        "create collection docs (body text, version int)",
        "create collection sessions (token text @unique, user text, data json, seen timestamp @ttl(30m))",
        "create collection scores (player text @unique, score int @sorted)",
    ] {
        run(&mut db, ddl, &[]);
    }
    let st = |sql: &str| fenec_ql::parse_one(sql).unwrap();
    let text = |s: String| Value::Text(s);
    println!("the recipes, in process, a statement's mean:");

    run(&mut db, "put counters {name: \"visits\", n: 0}", &[]);
    let incr = st("set counters {n: n + 1} where name = $1");
    let us = each(N, |_| {
        db.execute_with(&incr, &[text("visits".into())]).unwrap();
    });
    println!("  counter: an increment {us:.2} us");

    // A key a window and client: 1 000 clients, 100 requests each a window.
    let open = st("put hits {key: $1, n: 0, at: now()} if absent");
    let count = st("set hits {n: n + 1} where key = $1 and n < 100");
    let mut over = 0;
    let us = each(N, |i| {
        let key = [text(format!("ip{}:29630000", i % 1000))];
        db.execute_with(&open, &key).unwrap();
        if let Response::Affected(0) = db.execute_with(&count, &key).unwrap() {
            over += 1;
        }
    });
    println!("  rate limit: a request's two statements {us:.2} us ({over} over the limit)");

    let take = st("put locks {name: $1, owner: $2, at: now()} if absent");
    let renew = st("set locks {at: now()} where name = $1 and owner = $2");
    let release = st("del locks where name = $1 and owner = $2");
    let held = [text("nightly".into()), text("w1".into())];
    let other = [text("nightly".into()), text("w2".into())];
    db.execute_with(&take, &held).unwrap();
    let refused = each(N, |_| {
        db.execute_with(&take, &other).unwrap();
    });
    let r = each(N, |_| {
        db.execute_with(&renew, &held).unwrap();
    });
    let cycle = each(N, |_| {
        db.execute_with(&release, &held).unwrap();
        db.execute_with(&take, &held).unwrap();
    });
    println!(
        "  lock: refused while held {refused:.2} us, renewed {r:.2}, released and taken {cycle:.2}"
    );

    run(
        &mut db,
        "put docs {id: 7, body: \"draft\", version: 1}",
        &[],
    );
    let cas = st("set docs {body: $1, version: version + 1} where id = $2 and version = $3");
    let won = each(N, |i| {
        db.execute_with(
            &cas,
            &[text("x".into()), Value::Int(7), Value::Int(i as i64 + 1)],
        )
        .unwrap();
    });
    let lost = each(N, |_| {
        db.execute_with(&cas, &[text("x".into()), Value::Int(7), Value::Int(0)])
            .unwrap();
    });
    println!("  optimistic concurrency: a write that wins {won:.2} us, one that loses {lost:.2}");

    for b in 0..10 {
        let docs: Vec<String> = (0..1000)
            .map(|i| {
                let n = b * 1000 + i;
                format!(
                    "{{token: \"s-{n}\", user: \"u{n}\", data: {{\"cart\": {n}}}, seen: now()}}"
                )
            })
            .collect();
        run(&mut db, &format!("put sessions [{}]", docs.join(", ")), &[]);
    }
    let read = st("get sessions select user, data where token = $1");
    let touch = st("set sessions {seen: now()} where token = $1");
    let g = each(N, |i| {
        db.query(&read, &[text(format!("s-{}", i % 10_000))])
            .unwrap();
    });
    let s = each(N, |i| {
        db.execute_with(&touch, &[text(format!("s-{}", i % 10_000))])
            .unwrap();
    });
    println!("  sessions (10 000): read {g:.2} us, seen again {s:.2}");

    for b in 0..ROWS / 1000 {
        let docs: Vec<String> = (0..1000)
            .map(|i| {
                let n = b * 1000 + i;
                format!("{{player: \"p{n}\", score: {}}}", (n * 7919) % 1_000_000)
            })
            .collect();
        run(&mut db, &format!("put scores [{}]", docs.join(", ")), &[]);
    }
    let add = st("set scores {score: score + $1} where player = $2");
    let top = st("get scores select player, score order score desc limit 10");
    let rank = st("get scores where score > $1 count");
    let a = each(N, |i| {
        db.execute_with(&add, &[Value::Int(10), text(format!("p{}", i % ROWS))])
            .unwrap();
    });
    let t = each(10_000, |_| {
        db.query(&top, &[]).unwrap();
    });
    let r = each(10_000, |i| {
        db.query(&rank, &[Value::Int((i * 97) as i64 % 1_000_000)])
            .unwrap();
    });
    println!(
        "  leaderboard ({ROWS} players): a score added {a:.2} us, the top ten {t:.2}, a player's rank {r:.2}"
    );
}

/// A write's way to a subscriber: `GET /<name>/changes` held open while
/// another connection writes, the time from the write sent to its event
/// read.
fn notification() {
    use std::io::{BufRead, BufReader, Write};
    let dir = std::env::temp_dir().join(format!("fenecbench-notify-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("notify.fenec");
    let _ = std::fs::remove_file(&file);
    {
        let mut db = fenec_core::fs::open(&file).unwrap();
        run(
            &mut db,
            "create collection jobs (key text @hash, state text)",
            &[],
        );
        run(&mut db, "put jobs {key: \"k0\", state: \"new\"}", &[]);
        db.checkpoint().unwrap();
    }
    let port = http::free_port();
    let server = http::start_fenec(&file, port, "counters-bench", &[]);
    let addr = format!("127.0.0.1:{port}");
    let mut stream = std::net::TcpStream::connect(&addr).unwrap();
    write!(
        stream,
        "GET /jobs/changes?key=eq.k0 HTTP/1.1\r\nHost: bench\r\nAccept: text/event-stream\r\n\r\n"
    )
    .unwrap();
    let mut events = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    // Up to the seed's data.
    loop {
        line.clear();
        events.read_line(&mut line).unwrap();
        if line.starts_with("data:") {
            break;
        }
    }
    let mut writer = http::Http::connect(&addr);
    let mut lat = Vec::with_capacity(2000);
    for i in 0..2000 {
        let at = Instant::now();
        writer.query(
            "set jobs {state: $1} where key = \"k0\"",
            &format!("\"s{i}\""),
        );
        loop {
            line.clear();
            events.read_line(&mut line).unwrap();
            if line.starts_with("data:") {
                break;
            }
        }
        lat.push(at.elapsed().as_secs_f64() * 1e3);
    }
    drop(stream);
    server.terminate();
    lat.sort_by(|a, b| a.total_cmp(b));
    println!(
        "  change notification: a write's round trip and its event at a subscriber, p50 {:.3} ms, p99 {:.3}",
        lat[lat.len() / 2],
        lat[lat.len() * 99 / 100]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn main() {
    let constant_only = std::env::args().any(|a| a == "--constant");
    sets(constant_only);
    if !constant_only {
        in_process();
        over_http();
        recipes();
        notification();
    }
}
