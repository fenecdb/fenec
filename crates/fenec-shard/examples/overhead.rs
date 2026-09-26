//! What the router costs: `cargo run --release -p fenec-shard --example overhead -- [N] [DIM]`
//!
//! What is measured, all on loopback with node and router in one process
//!   * one keep-alive client, the same request straight to the node and
//!     through the router: p50 / p95 / p99 of each and the difference, and
//!     what counting a request for `/_metrics` costs
//!   * a tenant move: N documents with a DIM vector and an HNSW index,
//!     image size and wall time, and the first query on the target (the
//!     graph travels in the image, so it is not rebuilt)
//!   * a node's standby: how long a write takes to be visible on the
//!     replica, and what a failover of every tenant costs
//!   * three nodes whose tenants follow on each other (`--replicas`): the
//!     same lag, a node's failover across the other two, and the repair
//!     that gives every tenant a replica again
//!   * the same three leased (`--auto-failover`, a lease of a second): one
//!     cut off, when it stops taking writes, when the router fails it over
//!     on its own and when its tenants take writes again

use fenec_http::tenants::Tenants;
use fenec_shard::directory::Directory;
use fenec_shard::metrics::{self, Route};
use fenec_shard::{Config, Router};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ROUNDS: usize = 5_000;

fn node(tag: &str) -> (String, std::path::PathBuf) {
    started(tag, None, false)
}

/// A node taking the router's lease, syncing as it answers.
fn leased(tag: &str) -> (String, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("fenec-overhead-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let tenants = Tenants::new(&dir)
        .unwrap()
        .with_replication(fenec_http::tenants::Replicated {
            token: "repl".into(),
            buffer: 32 << 20,
            upstream: None,
            sync_on_write: true,
        })
        .with_lease();
    let cfg = fenec_http::Config {
        sync_on_write: true,
        addr: "127.0.0.1:0".into(),
        admin_token: Some("adm".into()),
        ..fenec_http::Config::default()
    };
    let server = fenec_http::Server::with_tenants(Arc::new(tenants), cfg);
    let listener = server.bind().unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    (addr, dir)
}

/// A TCP proxy in front of a node: cut, it drops what it carries and every
/// connection after -- the node gone, as the router and its replicas see
/// it, while its own clients still reach it.
struct Proxy {
    addr: String,
    cut: Arc<AtomicBool>,
    open: Arc<Mutex<Vec<TcpStream>>>,
}

impl Proxy {
    fn to(target: &str) -> Proxy {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let cut = Arc::new(AtomicBool::new(false));
        let open = Arc::new(Mutex::new(Vec::new()));
        let (flag, streams, target) = (Arc::clone(&cut), Arc::clone(&open), target.to_string());
        std::thread::spawn(move || {
            for client in listener.incoming() {
                let Ok(client) = client else { continue };
                if flag.load(Ordering::SeqCst) {
                    let _ = client.shutdown(Shutdown::Both);
                    continue;
                }
                let Ok(server) = TcpStream::connect(&target) else {
                    continue;
                };
                let mut held = streams.lock().unwrap();
                held.push(client.try_clone().unwrap());
                held.push(server.try_clone().unwrap());
                drop(held);
                let (a, b) = (client.try_clone().unwrap(), server.try_clone().unwrap());
                std::thread::spawn(move || copy(a, b));
                std::thread::spawn(move || copy(server, client));
            }
        });
        Proxy { addr, cut, open }
    }

    fn cut(&self) {
        self.cut.store(true, Ordering::SeqCst);
        for s in self.open.lock().unwrap().drain(..) {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
}

fn copy(mut from: TcpStream, mut to: TcpStream) {
    let _ = std::io::copy(&mut from, &mut to);
    let _ = to.shutdown(Shutdown::Write);
}

/// A node; `follows` makes it the standby of the node at that address, and
/// `sync` fsyncs each write before answering, which is what a feed ships.
fn started(tag: &str, follows: Option<&str>, sync: bool) -> (String, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("fenec-overhead-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut tenants = Tenants::new(&dir).unwrap();
    if sync || follows.is_some() {
        tenants = tenants.with_replication(fenec_http::tenants::Replicated {
            token: "repl".into(),
            buffer: 32 << 20,
            upstream: follows.map(|a| format!("http://{a}")),
            // The standby applies and answers; its own fsync follows the
            // node's policy, as a replica of a single file does. Syncing
            // every applied write there costs an fsync a write -- 8 ms on
            // this machine -- for writes the primary can send again.
            sync_on_write: sync,
        });
    }
    let tenants = Arc::new(tenants);
    let cfg = fenec_http::Config {
        sync_on_write: sync,
        addr: "127.0.0.1:0".into(),
        admin_token: Some("adm".into()),
        max_body: 1 << 30,
        ..fenec_http::Config::default()
    };
    let server = fenec_http::Server::with_tenants(tenants, cfg);
    let listener = server.bind().unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    (addr, dir)
}

/// A keep-alive client: one connection, requests one after another.
struct Client {
    reader: BufReader<TcpStream>,
    out: TcpStream,
}

impl Client {
    fn new(addr: &str) -> Client {
        let s = TcpStream::connect(addr).unwrap();
        s.set_nodelay(true).unwrap();
        Client {
            out: s.try_clone().unwrap(),
            reader: BufReader::new(s),
        }
    }

    fn send(
        &mut self,
        method: &str,
        target: &str,
        auth: Option<&str>,
        body: &[u8],
    ) -> (u16, Vec<u8>) {
        let mut head = format!(
            "{method} {target} HTTP/1.1\r\nHost: b\r\nContent-Length: {}\r\n",
            body.len()
        );
        if let Some(t) = auth {
            head.push_str(&format!("Authorization: Bearer {t}\r\n"));
        }
        head.push_str("\r\n");
        self.out.write_all(head.as_bytes()).unwrap();
        self.out.write_all(body).unwrap();
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        let status = line.split_whitespace().nth(1).unwrap().parse().unwrap();
        let mut len = 0;
        loop {
            line.clear();
            self.reader.read_line(&mut line).unwrap();
            let l = line.trim_end();
            if l.is_empty() {
                break;
            }
            if let Some(v) = l
                .split_once(':')
                .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            {
                len = v.1.trim().parse().unwrap();
            }
        }
        let mut body = vec![0u8; len];
        self.reader.read_exact(&mut body).unwrap();
        (status, body)
    }
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() as f64 - 1.0) * p) as usize]
}

fn latencies(c: &mut Client, target: &str) -> Vec<f64> {
    for _ in 0..500 {
        c.send("GET", target, None, b"");
    }
    let mut out: Vec<f64> = (0..ROUNDS)
        .map(|_| {
            let t = Instant::now();
            let (status, _) = c.send("GET", target, None, b"");
            assert_eq!(status, 200);
            t.elapsed().as_secs_f64() * 1e6
        })
        .collect();
    out.sort_by(|a, b| a.partial_cmp(b).unwrap());
    out
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let n: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let dim: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(128);

    let (a1, d1) = node("n1");
    let (a2, d2) = node("n2");
    let router = Router::new(
        Directory::in_memory(),
        Config {
            addr: "127.0.0.1:0".into(),
            max_body: 1 << 30,
            upstream_timeout: Duration::from_secs(600),
            ..Config::default()
        },
    );
    let listener = router.bind().unwrap();
    let raddr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = router.serve_on(listener);
    });

    let mut r = Client::new(&raddr);
    for (name, addr) in [("n1", &a1), ("n2", &a2)] {
        let body = format!(r#"{{"addr":"{addr}","token":"adm"}}"#);
        assert_eq!(
            r.send(
                "PUT",
                &format!("/_shard/nodes/{name}"),
                None,
                body.as_bytes()
            )
            .0,
            201
        );
    }
    assert_eq!(
        r.send("PUT", "/_shard/tenants/acme", None, br#"{"node":"n1"}"#)
            .0,
        201
    );

    let q = |c: &mut Client, sql: &str| {
        let mut body = String::from("{\"query\":");
        fenec_core::json::escape_into(&mut body, sql);
        body.push('}');
        let (status, b) = c.send("POST", "/t/acme/query", None, body.as_bytes());
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&b));
    };
    q(
        &mut r,
        &format!("create collection docs (title text, v vector<{dim}> @hnsw(cosine))"),
    );

    // ------------------------------------------------------------ latency
    q(
        &mut r,
        r#"put docs {title: "probe", v: $1}"#.replace("$1", &vector(dim, 0)).as_str(),
    );
    let target = "/t/acme/docs?limit=1&select=title";
    let direct = latencies(&mut Client::new(&a1), target);
    let routed = latencies(&mut r, target);
    println!("request latency, {ROUNDS} x GET {target} over one keep-alive connection");
    println!("              p50       p95       p99");
    for (label, v) in [("direct", &direct), ("router", &routed)] {
        println!(
            "  {label:<8} {:>6.1} us {:>6.1} us {:>6.1} us",
            pct(v, 0.5),
            pct(v, 0.95),
            pct(v, 0.99)
        );
    }
    println!(
        "  added    {:>6.1} us {:>6.1} us {:>6.1} us",
        pct(&routed, 0.5) - pct(&direct, 0.5),
        pct(&routed, 0.95) - pct(&direct, 0.95),
        pct(&routed, 0.99) - pct(&direct, 0.99)
    );

    // ----------------------------------------------------------- counting
    // What `/_metrics` adds to a request: a count and two histograms in the
    // thread's own shard. One thread, then eight at once, as a busy
    // router's connections count.
    let per = |threads: usize| -> f64 {
        let rounds = 2_000_000u64;
        let t = Instant::now();
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(move || {
                    for i in 0..rounds {
                        let took = Duration::from_micros(20 + i % 50);
                        metrics::request(Route::Tenant, 200, took);
                        metrics::upstream(took);
                    }
                });
            }
        });
        t.elapsed().as_secs_f64() * 1e9 / rounds as f64
    };
    println!(
        "  counting  {:>6.1} ns a request, {:.1} ns with eight threads counting",
        per(1),
        per(8)
    );

    // --------------------------------------------------------------- move
    let t = Instant::now();
    let batch = 500;
    let mut i = 0;
    while i < n {
        let docs: Vec<String> = (i..(i + batch).min(n))
            .map(|k| format!("{{title: \"d{k}\", v: {}}}", vector(dim, k as u64 + 1)))
            .collect();
        q(&mut r, &format!("put docs [{}]", docs.join(",")));
        i += batch;
    }
    println!(
        "\nloaded {n} x {dim} through the router in {:.2} s",
        t.elapsed().as_secs_f64()
    );

    let t = Instant::now();
    let (status, body) = r.send("POST", "/_shard/tenants/acme/move", None, br#"{"to":"n2"}"#);
    let took = t.elapsed();
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let report = String::from_utf8_lossy(&body);
    println!("{report}");
    let bytes: f64 = report
        .split("\"bytes\":")
        .nth(1)
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);
    println!(
        "move n1 -> n2: {:.1} MB in {:.0} ms ({:.0} MB/s)",
        bytes / 1e6,
        took.as_secs_f64() * 1e3,
        bytes / 1e6 / took.as_secs_f64()
    );

    let t = Instant::now();
    let body = format!(
        r#"{{"vector":{},"limit":10,"select":["title"]}}"#,
        vector(dim, 7)
    );
    let (status, _) = r.send("POST", "/t/acme/docs/near", None, body.as_bytes());
    assert_eq!(status, 200);
    println!(
        "first near on the target (opens the tenant): {:.1} ms",
        t.elapsed().as_secs_f64() * 1e3
    );
    let t = Instant::now();
    r.send("POST", "/t/acme/docs/near", None, body.as_bytes());
    println!("second near: {:.2} ms", t.elapsed().as_secs_f64() * 1e3);

    let _ = std::fs::remove_dir_all(d1);
    let _ = std::fs::remove_dir_all(d2);

    replicas();
    spread();
    automatic();
}

/// A node and its standby: what a write costs to reach the replica, and
/// what promoting every tenant of a node costs.
fn replicas() {
    const TENANTS: usize = 20;
    const WRITES: usize = 200;
    let (a1, d1) = started("r1", None, true);
    let (a2, d2) = started("r2", Some(&a1), false);
    let router = Router::new(
        Directory::in_memory(),
        Config {
            addr: "127.0.0.1:0".into(),
            upstream_timeout: Duration::from_secs(600),
            ..Config::default()
        },
    );
    let listener = router.bind().unwrap();
    let raddr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = router.serve_on(listener);
    });
    let mut r = Client::new(&raddr);
    for (name, addr) in [("r1", &a1), ("r2", &a2)] {
        let body = format!(r#"{{"addr":"{addr}","token":"adm"}}"#);
        assert_eq!(
            r.send(
                "PUT",
                &format!("/_shard/nodes/{name}"),
                None,
                body.as_bytes()
            )
            .0,
            201
        );
    }
    let body = format!(r#"{{"addr":"{a1}","token":"adm","standby":"r2"}}"#);
    assert_eq!(
        r.send("PUT", "/_shard/nodes/r1", None, body.as_bytes()).0,
        201
    );
    for i in 0..TENANTS {
        assert_eq!(
            r.send(
                "PUT",
                &format!("/_shard/tenants/t{i}"),
                None,
                br#"{"node":"r1"}"#
            )
            .0,
            201
        );
    }
    let query = |c: &mut Client, tenant: &str, sql: &str| -> (u16, String) {
        let mut body = String::from("{\"query\":");
        fenec_core::json::escape_into(&mut body, sql);
        body.push('}');
        let (status, b) = c.send("POST", &format!("/t/{tenant}/query"), None, body.as_bytes());
        (status, String::from_utf8_lossy(&b).into_owned())
    };
    for i in 0..TENANTS {
        assert_eq!(
            query(&mut r, &format!("t{i}"), "create collection notes (n int)").0,
            200
        );
    }

    // ---- commit to visible on the replica
    let mut replica = Client::new(&a2);
    let mut acked = Vec::with_capacity(WRITES);
    let mut lag = Vec::with_capacity(WRITES);
    for k in 0..WRITES {
        let t = Instant::now();
        assert_eq!(query(&mut r, "t0", &format!("put notes {{n: {k}}}")).0, 200);
        let ack = t.elapsed().as_secs_f64() * 1e3;
        loop {
            let (status, body) = query(&mut replica, "t0", "get notes count");
            assert_eq!(status, 200, "{body}");
            if body.contains(&format!(":{}}}", k + 1)) || body.contains(&format!("[{}]", k + 1)) {
                break;
            }
        }
        acked.push(ack);
        lag.push(t.elapsed().as_secs_f64() * 1e3 - ack);
    }
    acked.sort_by(|a, b| a.partial_cmp(b).unwrap());
    lag.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "\nreplica: a write through the router is answered in {:.3} ms p50 \
         (--sync always: an fsync a write) and is on the standby {:.3} ms later, \
         {:.3} ms at p99 -- a read of the standby's own is what times it",
        pct(&acked, 0.5),
        pct(&lag, 0.5),
        pct(&lag, 0.99)
    );

    // ---- failover of every tenant
    for i in 1..TENANTS {
        assert_eq!(query(&mut r, &format!("t{i}"), "put notes {n: 1}").0, 200);
    }
    let t = Instant::now();
    let (status, body) = r.send("POST", "/_shard/nodes/r1/failover", None, b"");
    let took = t.elapsed().as_secs_f64() * 1e3;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    println!(
        "failover: {TENANTS} tenants promoted on the standby in {took:.0} ms ({:.1} ms each)",
        took / TENANTS as f64
    );
    let t = Instant::now();
    let (status, body) = query(&mut r, "t0", "put notes {n: 999}");
    assert_eq!(status, 200, "{body}");
    println!(
        "first write after it: {:.2} ms",
        t.elapsed().as_secs_f64() * 1e3
    );

    let _ = std::fs::remove_dir_all(d1);
    let _ = std::fs::remove_dir_all(d2);
}

/// Three nodes in no pair, each tenant followed on another (`--replicas`):
/// a write's way to its replica, a node's failover across the two others,
/// and the repair after it.
fn spread() {
    const TENANTS: usize = 30;
    const WRITES: usize = 200;
    let nodes: Vec<(String, std::path::PathBuf)> = (1..=3)
        .map(|i| started(&format!("s{i}"), None, true))
        .collect();
    let router = Router::new(
        Directory::in_memory(),
        Config {
            addr: "127.0.0.1:0".into(),
            upstream_timeout: Duration::from_secs(600),
            replicas: true,
            ..Config::default()
        },
    );
    let listener = router.bind().unwrap();
    let raddr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = router.serve_on(listener);
    });
    let mut r = Client::new(&raddr);
    for (i, (addr, _)) in nodes.iter().enumerate() {
        let body = format!(r#"{{"addr":"{addr}","token":"adm"}}"#);
        let target = format!("/_shard/nodes/n{}", i + 1);
        assert_eq!(r.send("PUT", &target, None, body.as_bytes()).0, 201);
    }
    let query = |c: &mut Client, tenant: &str, sql: &str| -> (u16, String) {
        let mut body = String::from("{\"query\":");
        fenec_core::json::escape_into(&mut body, sql);
        body.push('}');
        let (status, b) = c.send("POST", &format!("/t/{tenant}/query"), None, body.as_bytes());
        (status, String::from_utf8_lossy(&b).into_owned())
    };
    let t = Instant::now();
    let mut replica_of = Vec::with_capacity(TENANTS);
    for i in 0..TENANTS {
        let body = format!(r#"{{"node":"n{}"}}"#, i % 3 + 1);
        let (status, answer) = r.send(
            "PUT",
            &format!("/_shard/tenants/t{i}"),
            None,
            body.as_bytes(),
        );
        let answer = String::from_utf8_lossy(&answer).into_owned();
        assert_eq!(status, 201, "{answer}");
        let on = answer
            .split("\"replica_on\":\"")
            .nth(1)
            .expect("a replica")
            .split('"')
            .next()
            .unwrap();
        replica_of.push(on.to_string());
    }
    let created = t.elapsed().as_secs_f64() * 1e3 / TENANTS as f64;
    for i in 0..TENANTS {
        assert_eq!(
            query(&mut r, &format!("t{i}"), "create collection notes (n int)").0,
            200
        );
    }

    // ---- commit to visible on t0's replica
    let at = |name: &str| &nodes[name[1..].parse::<usize>().unwrap() - 1].0;
    let mut replica = Client::new(at(&replica_of[0]));
    let mut lag = Vec::with_capacity(WRITES);
    for k in 0..WRITES {
        let t = Instant::now();
        assert_eq!(query(&mut r, "t0", &format!("put notes {{n: {k}}}")).0, 200);
        let ack = t.elapsed().as_secs_f64() * 1e3;
        loop {
            let (status, body) = query(&mut replica, "t0", "get notes count");
            assert_eq!(status, 200, "{body}");
            if body.contains(&format!(":{}}}", k + 1)) || body.contains(&format!("[{}]", k + 1)) {
                break;
            }
        }
        lag.push(t.elapsed().as_secs_f64() * 1e3 - ack);
    }
    lag.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let on = |n: &str| replica_of.iter().filter(|r| r.as_str() == n).count();
    println!(
        "\nreplicas of their own: {TENANTS} tenants over 3 nodes, each created with its replica \
         in {created:.1} ms, the replicas {} / {} / {} a node; a write is on t0's {:.3} ms \
         after it was answered, {:.3} ms at p99",
        on("n1"),
        on("n2"),
        on("n3"),
        pct(&lag, 0.5),
        pct(&lag, 0.99)
    );

    // ---- failover of n1, across n2 and n3
    for i in 1..TENANTS {
        assert_eq!(query(&mut r, &format!("t{i}"), "put notes {n: 1}").0, 200);
    }
    let t = Instant::now();
    let (status, body) = r.send("POST", "/_shard/nodes/n1/failover", None, b"");
    let took = t.elapsed().as_secs_f64() * 1e3;
    let body = String::from_utf8_lossy(&body).into_owned();
    assert_eq!(status, 200, "{body}");
    let moved = TENANTS / 3;
    println!(
        "failover: n1's {moved} tenants promoted on their replicas in {took:.0} ms \
         ({:.1} ms each), {} onto n2 and {} onto n3",
        took / moved as f64,
        body.matches("\"to\":\"n2\"").count(),
        body.matches("\"to\":\"n3\"").count()
    );
    let t = Instant::now();
    let (status, body) = query(&mut r, "t0", "put notes {n: 999}");
    assert_eq!(status, 200, "{body}");
    println!(
        "first write after it: {:.2} ms (an fsync: every node here syncs as it answers)",
        t.elapsed().as_secs_f64() * 1e3
    );

    // ---- the repair: every tenant a replica again, n1's copies first
    let t = Instant::now();
    let (status, body) = r.send("POST", "/_shard/replicas", None, b"");
    let took = t.elapsed().as_secs_f64() * 1e3;
    let body = String::from_utf8_lossy(&body).into_owned();
    assert_eq!(status, 200, "{body}");
    println!(
        "repair: {} tenants given a replica in {took:.0} ms, {} of them on n1's copies",
        body.matches("\"replica\":").count(),
        body.matches("\"replica\":\"n1\"").count()
    );

    for (_, dir) in &nodes {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// A deterministic pseudo-random vector as FenecQL list text.
fn vector(dim: usize, seed: u64) -> String {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let v: Vec<String> = (0..dim)
        .map(|_| {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            let f = ((x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32) / ((1u32 << 24) as f32);
            format!("{:.4}", f - 0.5)
        })
        .collect();
    format!("[{}]", v.join(","))
}

/// Three leased nodes and one cut off: when it stops taking writes, when
/// the router fails it over on its own, and when the tenants it held take
/// writes again, on their replicas.
fn automatic() {
    const TENANTS: usize = 30;
    const TERM: Duration = Duration::from_secs(1);
    let nodes: Vec<(String, std::path::PathBuf)> =
        (1..=3).map(|i| leased(&format!("a{i}"))).collect();
    let proxies: Vec<Proxy> = nodes.iter().map(|(addr, _)| Proxy::to(addr)).collect();
    let router = Router::new(
        Directory::in_memory(),
        Config {
            addr: "127.0.0.1:0".into(),
            upstream_timeout: Duration::from_secs(600),
            replicas: true,
            auto_failover: Some(TERM),
            ..Config::default()
        },
    );
    let listener = router.bind().unwrap();
    let raddr = listener.local_addr().unwrap().to_string();
    router.start_leasing().unwrap();
    std::thread::spawn(move || {
        let _ = router.serve_on(listener);
    });
    let mut r = Client::new(&raddr);
    for (i, p) in proxies.iter().enumerate() {
        let body = format!(r#"{{"addr":"{}","token":"adm"}}"#, p.addr);
        let target = format!("/_shard/nodes/n{}", i + 1);
        assert_eq!(r.send("PUT", &target, None, body.as_bytes()).0, 201);
    }
    let query = |c: &mut Client, tenant: &str, sql: &str| -> u16 {
        let mut body = String::from("{\"query\":");
        fenec_core::json::escape_into(&mut body, sql);
        body.push('}');
        c.send("POST", &format!("/t/{tenant}/query"), None, body.as_bytes())
            .0
    };
    for i in 0..TENANTS {
        let body = format!(r#"{{"node":"n{}"}}"#, i % 3 + 1);
        let (status, answer) = r.send(
            "PUT",
            &format!("/_shard/tenants/t{i}"),
            None,
            body.as_bytes(),
        );
        assert_eq!(status, 201, "{}", String::from_utf8_lossy(&answer));
        assert_eq!(
            query(&mut r, &format!("t{i}"), "create collection notes (n int)"),
            200
        );
    }
    // Every tenant's replica caught up before n1 goes.
    std::thread::sleep(Duration::from_millis(500));

    let held: Vec<String> = (0..TENANTS)
        .filter(|i| i % 3 == 0)
        .map(|i| format!("t{i}"))
        .collect();
    let mut direct = Client::new(&nodes[0].0);
    proxies[0].cut();
    let cut = Instant::now();
    let fenced = loop {
        if query(&mut direct, &held[0], "put notes {n: 1}") == 503 {
            break cut.elapsed();
        }
        std::thread::sleep(Duration::from_millis(1));
    };
    let mut back: Vec<Duration> = Vec::with_capacity(held.len());
    for t in &held {
        // A connection of its own each time: the router's to n1 was cut.
        loop {
            let mut c = Client::new(&raddr);
            if query(&mut c, t, "put notes {n: 2}") == 200 {
                back.push(cut.elapsed());
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    println!(
        "\nautomatic failover, a lease of {:.0} ms renewed every {:.0}: n1 cut off, it stopped taking \
         writes {:.0} ms after, and its {} tenants took them again on their replicas {:.0} to {:.0} ms \
         after -- the router waits the lease and a tenth ({:.0} ms), then promotes",
        ms(TERM),
        ms(TERM / 3),
        ms(fenced),
        held.len(),
        ms(*back.iter().min().unwrap()),
        ms(*back.iter().max().unwrap()),
        ms(TERM + TERM / 10)
    );
    for (_, dir) in &nodes {
        let _ = std::fs::remove_dir_all(dir);
    }
}
