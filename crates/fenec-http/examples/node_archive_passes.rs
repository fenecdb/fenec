//! What an archiver of a node's tenants costs the node's own work under
//! `--sync <ms>`: the syncer's passes and a close.
//! `cargo run --release -p fenec-http --example node_archive_passes -- [WAY] [SECONDS] [BIG_MB]`
//! (`make node-archive-bench`; the archiver is `target/release/fenec`, or
//! the one `FENEC_BIN` names).
//!
//! A node of tenants in this process, `--sync 250`'s way: no write synced
//! as it is answered, a syncer every 250 ms. Eight tenants written every
//! 5 ms, eight more each opened, written once and closed in turn every
//! 100 ms -- every close timed -- and one of BIG_MB (32) closed, whose image
//! the archiver takes as it connects, for SECONDS (20). WAY is the archiver
//! beside it, a process of its own: `none`, `streams` (a `fenec archive` of
//! each tenant's own stream) or `node` (one `fenec archive --tenants`).
//! Every syncer pass is timed too. It reads nothing a build from before the
//! node's stream lacks, so it measures one against the other.

use fenec_http::tenants::{Replicated, Tenant, Tenants};
use fenec_http::{Config, Server};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REPL: &str = "r";

fn fenec() -> PathBuf {
    if let Ok(path) = std::env::var("FENEC_BIN") {
        return PathBuf::from(path);
    }
    let exe = std::env::current_exe().unwrap();
    let path = exe.parent().unwrap().parent().unwrap().join("fenec");
    assert!(
        path.exists(),
        "{} is missing: cargo build --release -p fenec-cli",
        path.display()
    );
    path
}

fn put(t: &Tenant, sql: &str) {
    t.db.write()
        .unwrap()
        .execute(&fenec_ql::parse_one(sql).unwrap())
        .unwrap();
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(f64::total_cmp);
    if v.is_empty() {
        return 0.0;
    }
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn report(what: &str, v: &mut [f64]) {
    let n = v.len();
    let (p50, p99, max) = (pct(v, 0.5), pct(v, 0.99), pct(v, 1.0));
    println!("  {what}: {n}, p50 {p50:.2} ms, p99 {p99:.2} ms, max {max:.2} ms");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let way = args.first().cloned().unwrap_or_else(|| "node".into());
    let seconds: u64 = args.get(1).map_or(20, |v| v.parse().unwrap());
    let big_mb: usize = args.get(2).map_or(32, |v| v.parse().unwrap());
    let d = std::env::temp_dir().join(format!("fenec-passes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    fenec_http::audit::set_delay(0);
    let tenants = Arc::new(
        Tenants::new(d.join("node"))
            .unwrap()
            .with_replication(Replicated {
                token: REPL.into(),
                buffer: 8 << 20,
                upstream: None,
                sync_on_write: false,
            }),
    );
    let server = Server::with_tenants(
        Arc::clone(&tenants),
        Config {
            addr: "127.0.0.1:0".into(),
            max_connections: 0,
            ..Config::default()
        },
    );
    let listener = server.bind().unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    let made = |name: &str| {
        let t = tenants.create(name).unwrap();
        put(&t, "create collection notes (title text, n int @hash)");
        t
    };
    let writers: Vec<String> = (0..8).map(|i| format!("w{i}")).collect();
    let churn: Vec<String> = (0..8).map(|i| format!("c{i}")).collect();
    let held: Vec<Arc<Tenant>> = writers.iter().map(|n| made(n)).collect();
    for n in &churn {
        drop(made(n));
    }
    let big = made("big");
    let text = "x".repeat(1000);
    for k in 0..big_mb {
        let docs: Vec<String> = (0..1000)
            .map(|i| format!("{{title: \"{text}\", n: {}}}", k * 1000 + i))
            .collect();
        put(&big, &format!("put notes [{}]", docs.join(",")));
    }
    drop(big);
    tenants.sync_dirty();
    while tenants.close_idle(Duration::ZERO) > 0 {}

    let stop = Arc::new(AtomicBool::new(false));
    let passes = Arc::new(Mutex::new(Vec::new()));
    let syncer = {
        let (tenants, stop, passes) =
            (Arc::clone(&tenants), Arc::clone(&stop), Arc::clone(&passes));
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(250));
                let t0 = Instant::now();
                tenants.sync_dirty();
                passes
                    .lock()
                    .unwrap()
                    .push(t0.elapsed().as_secs_f64() * 1e3);
            }
        })
    };
    let writing: Vec<_> = held
        .iter()
        .cloned()
        .map(|t| {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut i = 0;
                while !stop.load(Ordering::SeqCst) {
                    put(&t, &format!("put notes {{title: \"w\", n: {i}}}"));
                    i += 1;
                    std::thread::sleep(Duration::from_millis(5));
                }
            })
        })
        .collect();
    let closes = Arc::new(Mutex::new(Vec::new()));
    let closer = {
        let (tenants, stop, closes, churn) = (
            Arc::clone(&tenants),
            Arc::clone(&stop),
            Arc::clone(&closes),
            churn.clone(),
        );
        std::thread::spawn(move || {
            let mut k = 0;
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(100));
                let t = tenants.get(&churn[k % churn.len()]).unwrap();
                put(&t, &format!("put notes {{title: \"c\", n: {k}}}"));
                drop(t);
                let t0 = Instant::now();
                tenants.close_idle(Duration::ZERO);
                closes
                    .lock()
                    .unwrap()
                    .push(t0.elapsed().as_secs_f64() * 1e3);
                k += 1;
            }
        })
    };

    // The archiver, beside it from here.
    let root = d.join("root");
    let mut archivers: Vec<Child> = Vec::new();
    let mut spawn = |args: &[&str]| {
        archivers.push(
            Command::new(fenec())
                .args(args)
                .args(["--token", REPL])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    };
    match way.as_str() {
        "none" => {}
        "streams" => {
            for name in writers
                .iter()
                .chain(&churn)
                .chain(["big".to_string()].iter())
            {
                let dir = root.join(name);
                std::fs::create_dir_all(&dir).unwrap();
                spawn(&["archive", &format!("{url}/t/{name}"), dir.to_str().unwrap()]);
            }
        }
        "node" => spawn(&["archive", "--tenants", &url, root.to_str().unwrap()]),
        other => panic!("no way `{other}`: none, streams or node"),
    }
    std::thread::sleep(Duration::from_secs(seconds));
    stop.store(true, Ordering::SeqCst);
    syncer.join().unwrap();
    closer.join().unwrap();
    for w in writing {
        w.join().unwrap();
    }
    for a in &mut archivers {
        let _ = Command::new("kill")
            .args(["-TERM", &a.id().to_string()])
            .status();
        let _ = a.wait();
    }
    println!("== {way}, {seconds} s, a tenant of {big_mb} MB closed");
    report("syncer passes", &mut passes.lock().unwrap());
    report("close_idle", &mut closes.lock().unwrap());
    drop(held);
    let _ = std::fs::remove_dir_all(&d);
}
