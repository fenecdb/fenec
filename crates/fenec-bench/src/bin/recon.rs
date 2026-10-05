//! What a reconciliation's snapshot costs the writers beside it, the
//! payments ledger's case (`examples/ledger`): `make recon-bench`.
//!
//! fenec-server (`--sync 250`) over 1 000 accounts and a journal of a
//! million entries, four clients sending transfers -- a debit and a credit
//! each required to write its row, and the two entries, one `/batch` --
//! for 10 s, alone and beside a reconciliation every second: one `/batch`
//! of reads, every balance, the journal summed by account and its count.
//! Two more clients read a balance by id meanwhile, as a console's pages
//! do. Printed: each kind's rate, p50, p99 and longest, and how long each
//! snapshot took. A batch of reads alone runs under the read lock, so the
//! reads by id go on beside it where the lock lets a reader past a waiting
//! writer (macOS); a transfer, a write, waits for the snapshot to end
//! either way -- one writer, and no snapshot but the lock.
//!
//! `FENEC_SERVER` runs another build of the server (a binary from before,
//! to compare in turns).

#[path = "../http.rs"]
mod http;

use fenec_core::prelude::*;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ACCOUNTS: usize = 1_000;
const TRANSFERS: usize = 500_000;
const START: i64 = 1_000_000_000;
const CLIENTS: usize = 4;
const READERS: usize = 2;
const RUN: Duration = Duration::from_secs(10);

fn ledger_file(path: &Path) {
    let _ = std::fs::remove_file(path);
    let mut db = fenec_core::fs::open(path).unwrap();
    let run = |db: &mut Database, sql: &str| {
        db.execute(&fenec_ql::parse_one(sql).unwrap()).unwrap();
    };
    run(
        &mut db,
        "create collection accounts (ext text @unique, balance int)",
    );
    run(
        &mut db,
        "create collection journal (entry text @unique, tx int, account text @hash, amount int)",
    );
    let accounts: Vec<String> = (0..ACCOUNTS)
        .map(|a| format!("{{ext: \"a{a}\", balance: {START}}}"))
        .collect();
    run(&mut db, &format!("put accounts [{}]", accounts.join(", ")));
    // Each transfer moves 1 between two accounts, so the balances stay
    // what the journal sums to with the starting balance beside it.
    for b in 0..TRANSFERS / 1000 {
        let mut docs = Vec::with_capacity(2000);
        for i in 0..1000 {
            let tx = b * 1000 + i;
            let (from, to) = (tx % ACCOUNTS, (tx * 7 + 1) % ACCOUNTS);
            docs.push(format!(
                "{{entry: \"{tx}:dr\", tx: {tx}, account: \"a{from}\", amount: -1}}"
            ));
            docs.push(format!(
                "{{entry: \"{tx}:cr\", tx: {tx}, account: \"a{to}\", amount: 1}}"
            ));
        }
        run(&mut db, &format!("put journal [{}]", docs.join(", ")));
    }
    for a in 0..ACCOUNTS {
        let moved: i64 = (0..TRANSFERS)
            .map(|tx| {
                let (from, to) = (tx % ACCOUNTS, (tx * 7 + 1) % ACCOUNTS);
                (to == a) as i64 - (from == a) as i64
            })
            .sum();
        if moved != 0 {
            run(
                &mut db,
                &format!(
                    "set accounts {{balance: {}}} where ext = \"a{a}\"",
                    START + moved
                ),
            );
        }
    }
    db.checkpoint().unwrap();
}

fn line(q: &str, params: &str) -> String {
    format!(
        "{{\"query\":{},\"params\":[{params}]}}",
        http::json_string(q)
    )
}

fn transfer(from: usize, to: usize, tx: usize) -> String {
    // The entry ids as parameters of their own, so a server from before
    // `+` joined texts runs the same batch.
    let p = format!("\"a{from}\",\"a{to}\",1,\"{tx}:dr\",{tx},\"{tx}:cr\"");
    [
        "set accounts {balance: balance - $3} where ext = $1 and balance >= $3 require 1",
        "set accounts {balance: balance + $3} where ext = $2 require 1",
        "insert journal [{entry: $4, tx: $5, account: $1, amount: 0 - $3}, \
         {entry: $6, tx: $5, account: $2, amount: $3}]",
    ]
    .iter()
    .map(|q| line(q, &p))
    .collect::<Vec<_>>()
    .join("\n")
}

fn snapshot() -> String {
    [
        "get accounts select ext, balance limit 10000",
        "get journal select account, sum(amount) group account limit 10000",
        "get journal count",
    ]
    .iter()
    .map(|q| line(q, ""))
    .collect::<Vec<_>>()
    .join("\n")
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[((v.len() as f64 * p) as usize).min(v.len() - 1)]
}

/// Transfers from [`CLIENTS`] clients and reads of a balance by id from
/// [`READERS`] for [`RUN`], and a snapshot a second beside them when
/// `reconcile`: the transfers' times, the reads' and the snapshots'.
fn round(addr: &str, transfers: bool, reconcile: bool, first_tx: usize) -> [Vec<f64>; 3] {
    let stop = Arc::new(AtomicBool::new(false));
    let first = if transfers { 0 } else { CLIENTS };
    let clients: Vec<_> = (first..CLIENTS + READERS)
        .map(|c| {
            let (addr, stop) = (addr.to_string(), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut h = http::Http::connect(&addr);
                let mut times = Vec::new();
                let mut n = 0usize;
                while !stop.load(Ordering::Relaxed) {
                    let tx = first_tx + c * 10_000_000 + n;
                    let (from, to) = (tx % ACCOUNTS, (tx * 13 + 5) % ACCOUNTS);
                    let to = if to == from { (to + 1) % ACCOUNTS } else { to };
                    let t = Instant::now();
                    match c < CLIENTS {
                        true => {
                            let body = transfer(from, to, tx);
                            h.post("/batch", "application/x-ndjson", body.as_bytes());
                        }
                        false => {
                            h.query(
                                "get accounts select balance where ext = $1",
                                &format!("\"a{from}\""),
                            );
                        }
                    }
                    times.push(t.elapsed().as_secs_f64() * 1e3);
                    n += 1;
                }
                (c < CLIENTS, times)
            })
        })
        .collect();
    let mut snaps = Vec::new();
    let end = Instant::now() + RUN;
    if reconcile {
        let mut h = http::Http::connect(addr);
        let body = snapshot();
        while Instant::now() + Duration::from_secs(1) <= end {
            let next = Instant::now() + Duration::from_secs(1);
            let t = Instant::now();
            h.post("/batch", "application/x-ndjson", body.as_bytes());
            snaps.push(t.elapsed().as_secs_f64() * 1e3);
            std::thread::sleep(next.saturating_duration_since(Instant::now()));
        }
    }
    std::thread::sleep(end.saturating_duration_since(Instant::now()));
    stop.store(true, Ordering::Relaxed);
    let (mut writes, mut reads) = (Vec::new(), Vec::new());
    for c in clients {
        match c.join().unwrap() {
            (true, t) => writes.extend(t),
            (false, t) => reads.extend(t),
        }
    }
    [writes, reads, snaps]
}

fn main() {
    let dir = std::env::temp_dir().join(format!("fenecbench-recon-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("ledger.fenec");
    let t = Instant::now();
    ledger_file(&file);
    println!(
        "{ACCOUNTS} accounts, {} journal entries, made in {:.1} s",
        2 * TRANSFERS,
        t.elapsed().as_secs_f64()
    );
    let port = http::free_port();
    let server = match std::env::var("FENEC_SERVER") {
        Ok(bin) => {
            let child = std::process::Command::new(&bin)
                .args(["--http", &format!("127.0.0.1:{port}")])
                .args(["--file", file.to_str().unwrap(), "--no-checkpoint"])
                .args(["--sync", "250"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap_or_else(|e| panic!("{bin}: {e}"));
            while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
                std::thread::sleep(Duration::from_millis(20));
            }
            http::Server::from_child(child)
        }
        Err(_) => http::start_fenec(&file, port, "recon-bench", &["--sync", "250"]),
    };
    let addr = format!("127.0.0.1:{port}");
    // The indexes a snapshot reads, and the `@unique` one a transfer's
    // entries are asked against, built before anything is timed: the
    // first write over a million entries builds that one under the write
    // lock, 300 ms.
    let mut h = http::Http::connect(&addr);
    h.post("/batch", "application/x-ndjson", snapshot().as_bytes());
    h.post(
        "/batch",
        "application/x-ndjson",
        transfer(0, 1, 1 << 43).as_bytes(),
    );
    println!(
        "{:<52} {:>9} {:>8} {:>8} {:>8} {:>9}",
        "", "a second", "p50 ms", "p99 ms", "max ms", "over 50ms"
    );
    for (name, transfers, reconcile, first) in [
        ("with transfers", true, false, 1 << 40),
        (
            "with transfers, a reconciliation a second",
            true,
            true,
            1 << 41,
        ),
        (
            "no transfers, a reconciliation a second",
            false,
            true,
            1 << 42,
        ),
    ] {
        let [writes, reads, mut snaps] = round(&addr, transfers, reconcile, first);
        for (what, mut times) in [("transfers", writes), ("reads", reads)] {
            if times.is_empty() {
                continue;
            }
            let rate = times.len() as f64 / RUN.as_secs_f64();
            let over = times.iter().filter(|&&t| t > 50.0).count();
            let (p50, p99, max) = (
                pct(&mut times, 0.5),
                pct(&mut times, 0.99),
                pct(&mut times, 1.0),
            );
            let label = format!("{what}, {name}");
            println!("{label:<52} {rate:>9.0} {p50:>8.2} {p99:>8.2} {max:>8.1} {over:>9}");
        }
        if !snaps.is_empty() {
            println!(
                "  the snapshot took {:.0} to {:.0} ms",
                pct(&mut snaps, 0.0),
                pct(&mut snaps, 1.0)
            );
        }
    }
    drop(server);
    let _ = std::fs::remove_dir_all(&dir);
}
