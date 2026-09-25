//! What serving a mirror costs, against the Makefile's PostgreSQL
//! (`make mirror-bench`, after `make pgvector-up`): a `fenec-pg --follow`
//! over a table of it with a `vector(384)` under an HNSW index, as `make
//! follow-bench` has, served with `--http`.
//!
//! - how long a committed change takes to reach a subscriber of the
//!   collection (`GET /<collection>/changes`), one row a transaction: an
//!   update of a title, as `make follow-bench` makes, the clock started once
//!   the commit has returned, as it starts its own; and a row inserted with
//!   its vector, which the follower puts into the graph first;
//! - that a server killed while the table is written to, and started again
//!   over its file, holds every row the table does, and how long it takes
//!   to catch up.
//!
//! ```text
//! cargo build --release -p fenec-pg
//! cargo run --release -p fenec-pg --example mirror -- [rows]
//! ```

use fenec_pg::client::{Client, Url};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const URL: &str = "postgres://postgres:fenec@127.0.0.1:55432/fenecbench";
const T: &str = "fenec_mirror_bench";
/// The slot and publication the follower names after the collection.
const SLOT: &str = "fenec_fenec_mirror_bench";
const DIM: usize = 384;

/// A vector for row `g`, as `make follow-bench` makes them.
fn embed(g: u64) -> String {
    let v: Vec<String> = (1..=DIM)
        .map(|i| format!("{:.6}", ((g * 7919 + i as u64) as f64).sin()))
        .collect();
    format!("'[{}]'", v.join(","))
}

fn url() -> String {
    std::env::var("FENEC_TEST_PG_URL").unwrap_or_else(|_| URL.into())
}

/// A `fenec-pg --follow` over `file`, and the HTTP port it listens on.
struct Server {
    child: Child,
    http: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The `fenec-pg` built beside this example: `target/release/fenec-pg`.
fn binary() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().and_then(Path::parent).unwrap();
    let bin = dir.join("fenec-pg");
    assert!(
        bin.exists(),
        "build it first: cargo build --release -p fenec-pg"
    );
    bin
}

/// Starts the server and waits until its follower streams.
fn start(file: &Path) -> Server {
    let mut child = Command::new(binary())
        .args(["--listen", "127.0.0.1:0", "--http", "127.0.0.1:0"])
        .arg("--file")
        .arg(file)
        .args(["--follow", &url(), "--follow-table", T])
        .args(["--follow-index", "embed@hnsw(cosine)"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("could not start fenec-pg");
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let mut http = None;
    let mut seen = String::new();
    loop {
        let mut line = String::new();
        if err.read_line(&mut line).unwrap_or(0) == 0 {
            panic!("fenec-pg ended before it streamed:\n{seen}");
        }
        seen.push_str(&line);
        if let Some(rest) = line.split("listening on: http://127.0.0.1:").nth(1) {
            http = rest
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .ok();
        }
        if line.contains("streaming its changes") && http.is_some() {
            break;
        }
    }
    std::thread::spawn(move || {
        let mut line = String::new();
        while err.read_line(&mut line).unwrap_or(0) > 0 {
            line.clear();
        }
    });
    Server {
        child,
        http: http.unwrap(),
    }
}

/// `GET /<T>/changes`: an open subscription, its lines read as they come.
fn subscribe(port: u16) -> BufReader<TcpStream> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    write!(s, "GET /{T}/changes HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
    let mut r = BufReader::new(s);
    // The head, then the seed.
    until_line(&mut r, "event: seed");
    r
}

/// Reads the stream until a line holds `needle`.
fn until_line(r: &mut BufReader<TcpStream>, needle: &str) {
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line).unwrap() == 0 {
            panic!("the stream ended before `{needle}`");
        }
        if line.contains(needle) {
            return;
        }
    }
}

/// The mirror's row count, over HTTP.
fn count(port: u16) -> u64 {
    let body = format!(r#"{{"query":"get {T} count"}}"#);
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        s,
        "POST /query HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let digits = out
        .split("\"count\":")
        .nth(1)
        .map(|r| {
            r.trim_start()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .unwrap_or_default();
    digits.parse().unwrap_or(0)
}

fn pct(sorted: &[Duration], p: f64) -> f64 {
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i].as_secs_f64() * 1e3
}

fn main() {
    let rows: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(10_000);
    let pg = Url::parse(&url()).unwrap();
    let mut c = Client::connect(&pg).expect("could not connect (make pgvector-up?)");
    for q in [
        format!("select pg_drop_replication_slot('{SLOT}') from pg_replication_slots where slot_name = '{SLOT}'"),
        format!("drop publication if exists {SLOT}"),
        "create extension if not exists vector".to_string(),
        format!("drop table if exists {T}"),
        format!("create table {T} (id bigint primary key, title text, n int, embed vector({DIM}))"),
        format!(
            "insert into {T} select g, 'row ' || g, g, \
             (select array_agg(sin(g * 7919 + i))::vector({DIM}) from generate_series(1, {DIM}) i) \
             from generate_series(1, {rows}) g"
        ),
    ] {
        c.query(&q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
    let dir = std::env::temp_dir().join(format!("fenec-mirror-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("mirror.fenec");

    let t = Instant::now();
    let server = start(&file);
    println!(
        "{rows} rows copied and streaming in {:.0} ms",
        t.elapsed().as_secs_f64() * 1e3
    );
    assert_eq!(count(server.http), rows);

    // One row a transaction: commit, then wait for it on the subscription.
    let mut sse = subscribe(server.http);
    let n = 300;
    for (what, insert) in [
        ("an update of a title", false),
        ("a row with its vector", true),
    ] {
        let mut lat = Vec::with_capacity(n);
        for i in 0..n {
            let q = match insert {
                true => {
                    let id = rows + 1 + i as u64;
                    format!(
                        "insert into {T} values ({id}, 'live {i}', {i}, {})",
                        embed(id)
                    )
                }
                false => {
                    let id = 1 + (i as u64 * 37) % rows;
                    format!("update {T} set title = 'live {i}' where id = {id}")
                }
            };
            c.query(&q).unwrap();
            // The commit has returned: the clock starts here.
            let t0 = Instant::now();
            until_line(&mut sse, &format!("\"title\":\"live {i}\""));
            lat.push(t0.elapsed());
        }
        lat.sort();
        println!(
            "commit -> subscriber, {what}: p50 {:.2} ms, p99 {:.2} ms, max {:.2} ms  ({n} transactions)",
            pct(&lat, 0.5),
            pct(&lat, 0.99),
            pct(&lat, 1.0)
        );
    }

    // Killed while the table is written to, one row a transaction, and
    // started again over its file: every row, none twice.
    let written = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (written, stop) = (Arc::clone(&written), Arc::clone(&stop));
        let pg = pg.clone();
        std::thread::spawn(move || {
            let mut c = Client::connect(&pg).unwrap();
            let base = 1_000_000u64;
            let mut i = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let id = base + i;
                c.query(&format!(
                    "insert into {T} values ({id}, 'burst {i}', 0, {})",
                    embed(id)
                ))
                .unwrap();
                i += 1;
                written.store(i, Ordering::Relaxed);
            }
        })
    };
    std::thread::sleep(Duration::from_millis(1_000));
    let mut server = server;
    let _ = server.child.kill();
    let _ = server.child.wait();
    let killed_at = written.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(1_000));
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    let total = rows + n as u64 + written.load(Ordering::Relaxed);
    drop(server);

    let t = Instant::now();
    let server = start(&file);
    let reopened = t.elapsed();
    while count(server.http) != total {
        assert!(
            t.elapsed() < Duration::from_secs(60),
            "the mirror never caught up"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    println!(
        "killed after {killed_at} of {} rows written meanwhile: started again in {:.0} ms, \
         every one of {total} rows there {:.0} ms after the start",
        written.load(Ordering::Relaxed),
        reopened.as_secs_f64() * 1e3,
        t.elapsed().as_secs_f64() * 1e3
    );
    let mut ids = c.query(&format!("select count(*) from {T}")).unwrap().rows;
    assert_eq!(
        ids.pop().unwrap()[0].as_deref(),
        Some(total.to_string().as_str())
    );
    drop(server);
    let _ = c.query(&format!("select pg_drop_replication_slot('{SLOT}')"));
    let _ = c.query(&format!("drop publication if exists {SLOT}"));
    let _ = std::fs::remove_dir_all(&dir);
}
