//! What a transaction costs over the pg wire, and saves:
//! `cargo run --release -p fenec-pg --example transactions -- [WRITES]`
//!
//! A server in this process over a file, one client, per sync policy: a
//! lone `put` a statement, the same writes a hundred to a transaction, each
//! of those in a savepoint of its own -- `SAVEPOINT`, the put, `RELEASE`, as
//! psycopg's nested blocks and SQLAlchemy's `begin_nested` send them -- a
//! `ROLLBACK TO` over a hundred writes, and a read by id: p50 of each. A
//! transaction's writes land as one record, with one fsync where its
//! statements alone took one each.

use fenec_pg::client::{Client, Url};
use fenec_pg::server::SyncPolicy;
use fenec_pg::{Config, Server};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

fn serve(path: &std::path::Path, sync: SyncPolicy) -> u16 {
    let _ = std::fs::remove_file(path);
    let db = fenec_core::fs::open(path).unwrap();
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        sync,
        ..Config::default()
    };
    let server = Server::new(Arc::new(RwLock::new(db)), cfg);
    let listener = server.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    port
}

fn p50(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

fn main() {
    let writes: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(20_000);
    let dir = std::env::temp_dir().join(format!("fenec-tx-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    println!(
        "                 a lone put  100 to a transaction  each in a savepoint  \
         ROLLBACK TO 100  a read by id"
    );
    // An fsync a write takes milliseconds: a tenth of the writes tells as much.
    for (name, sync, n) in [
        (
            "--sync 250",
            SyncPolicy::Interval(Duration::from_millis(250)),
            writes,
        ),
        ("--sync always", SyncPolicy::Always, writes / 10),
    ] {
        let n = n.max(100) / 100 * 100;
        // A file each: the server before this one still holds its own.
        let port = serve(&dir.join(format!("{}.fenec", &name[7..])), sync);
        let mut c = Client::connect(&Url {
            user: "fenec".into(),
            password: None,
            host: "127.0.0.1".into(),
            port,
            database: "fenec".into(),
        })
        .unwrap();
        c.query("create collection t (k text, n int)").unwrap();
        let us = |t: Instant| t.elapsed().as_secs_f64() * 1e6;

        let mut lone = Vec::with_capacity(n);
        for i in 0..n {
            let t = Instant::now();
            c.query(&format!("put t {{k: \"k{i}\", n: {i}}}")).unwrap();
            lone.push(us(t));
        }
        let mut held = Vec::with_capacity(n / 100);
        for r in 0..n / 100 {
            let t = Instant::now();
            c.query("BEGIN").unwrap();
            for i in 0..100 {
                c.query(&format!("put t {{k: \"x{r}-{i}\", n: {i}}}"))
                    .unwrap();
            }
            c.query("COMMIT").unwrap();
            held.push(us(t) / 100.0);
        }
        let mut nested = Vec::with_capacity(n / 100);
        for r in 0..n / 100 {
            let t = Instant::now();
            c.query("BEGIN").unwrap();
            for i in 0..100 {
                c.query("SAVEPOINT s").unwrap();
                c.query(&format!("put t {{k: \"s{r}-{i}\", n: {i}}}"))
                    .unwrap();
                c.query("RELEASE s").unwrap();
            }
            c.query("COMMIT").unwrap();
            nested.push(us(t) / 100.0);
        }
        let mut back = Vec::with_capacity(n / 100);
        for r in 0..n / 100 {
            c.query("BEGIN").unwrap();
            c.query("SAVEPOINT s").unwrap();
            for i in 0..100 {
                c.query(&format!("put t {{k: \"b{r}-{i}\", n: {i}}}"))
                    .unwrap();
            }
            let t = Instant::now();
            c.query("ROLLBACK TO s").unwrap();
            back.push(us(t));
            c.query("COMMIT").unwrap();
        }
        let mut reads = Vec::with_capacity(n);
        for i in 0..n {
            let t = Instant::now();
            c.query(&format!("get t where id = {}", i + 1)).unwrap();
            reads.push(us(t));
        }
        println!(
            "{name:<16}{:>8.1} us{:>19.1} us{:>18.1} us{:>14.1} us{:>12.1} us",
            p50(lone),
            p50(held),
            p50(nested),
            p50(back),
            p50(reads)
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
