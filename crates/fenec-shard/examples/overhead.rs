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
//!   * the same in a region keeping its own directory (`--region`), cut off
//!     from the global router, at a lease of 1 s and of 5 s: the failover is
//!     its own router's (`regions` alone)
//!   * a read through the router of a region holding a tenant's read
//!     replica: sent home, marked for the replica, and marked after the
//!     client's write, the replica waiting for it (`reads` alone)

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
    /// The last answer's `Fenec-History`, where it had one.
    history: Option<String>,
}

impl Client {
    fn new(addr: &str) -> Client {
        let s = TcpStream::connect(addr).unwrap();
        s.set_nodelay(true).unwrap();
        Client {
            out: s.try_clone().unwrap(),
            reader: BufReader::new(s),
            history: None,
        }
    }

    /// A request and its status and body, read as before read replicas:
    /// the latency's own client, which [`Client::send_with`]'s header
    /// vector and its two more comparisons a line of the answer made
    /// slower for the router's answers, which carry more lines.
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

    /// A request with headers of its own: its status, body and `Fenec-Seq`.
    fn send_with(
        &mut self,
        method: &str,
        target: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> (u16, Vec<u8>, Option<u64>) {
        let mut head = format!(
            "{method} {target} HTTP/1.1\r\nHost: b\r\nContent-Length: {}\r\n",
            body.len()
        );
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        self.out.write_all(head.as_bytes()).unwrap();
        self.out.write_all(body).unwrap();
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        let status = line.split_whitespace().nth(1).unwrap().parse().unwrap();
        let (mut len, mut seq) = (0, None);
        self.history = None;
        loop {
            line.clear();
            self.reader.read_line(&mut line).unwrap();
            let l = line.trim_end();
            if l.is_empty() {
                break;
            }
            match l.split_once(':') {
                Some((k, v)) if k.eq_ignore_ascii_case("content-length") => {
                    len = v.trim().parse().unwrap()
                }
                Some((k, v)) if k.eq_ignore_ascii_case("fenec-seq") => seq = v.trim().parse().ok(),
                Some((k, v)) if k.eq_ignore_ascii_case("fenec-history") => {
                    self.history = Some(v.trim().to_string())
                }
                _ => {}
            }
        }
        let mut body = vec![0u8; len];
        self.reader.read_exact(&mut body).unwrap();
        (status, body, seq)
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
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // A word first runs one part: `latency` the forwarded request alone,
    // `fanout` the query across tenants alone, `standby` a request through
    // a standby router against the primary, `reads` a read answered by a
    // read replica against one sent home.
    let only = match args.first().map(String::as_str) {
        Some(w @ ("latency" | "fanout" | "standby" | "regions" | "reads")) => Some(w.to_string()),
        _ => None,
    };
    if only.is_some() {
        args.remove(0);
    }
    if only.as_deref() == Some("fanout") {
        fanout();
        return;
    }
    if only.as_deref() == Some("reads") {
        reads();
        return;
    }
    if only.as_deref() == Some("regions") {
        regions();
        return;
    }
    if only.as_deref() == Some("standby") {
        standby();
        return;
    }
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
    if only.is_some() {
        let _ = std::fs::remove_dir_all(d1);
        let _ = std::fs::remove_dir_all(d2);
        return;
    }

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
    regions();
    fanout();
}

/// A query across tenants (`POST /_shard/query`) over 100 tenants on three
/// nodes, against a client asking each tenant itself through the router:
/// one after another on a kept-alive connection, and sixteen at once on
/// sixteen, as many as the router has out. A count added up, the twenty
/// newest rows of all of them, each tenant's twenty merged, and the twenty
/// first names in Turkish's order over their folds, merged in it.
fn fanout() {
    const TENANTS: usize = 100;
    const ROWS: usize = 200;
    const ROUNDS: usize = 300;
    const AT_ONCE: usize = 16;
    let nodes: Vec<(String, std::path::PathBuf)> =
        (1..=3).map(|i| node(&format!("f{i}"))).collect();
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
    for (i, (addr, _)) in nodes.iter().enumerate() {
        let body = format!(r#"{{"addr":"{addr}","token":"adm"}}"#);
        let target = format!("/_shard/nodes/n{}", i + 1);
        assert_eq!(r.send("PUT", &target, None, body.as_bytes()).0, 201);
    }
    let schema = "create collection users (email text, created timestamp @sorted)";
    for t in 0..TENANTS {
        let mut body = format!(r#"{{"node":"n{}","schema":"#, t % 3 + 1);
        fenec_core::json::escape_into(&mut body, schema);
        body.push('}');
        let target = format!("/_shard/tenants/t{t:03}");
        let (status, b) = r.send("PUT", &target, None, body.as_bytes());
        assert_eq!(status, 201, "{}", String::from_utf8_lossy(&b));
        let docs: Vec<String> = (0..ROWS)
            .map(|k| {
                let ms = 1_790_000_000_000u64 + ((k * TENANTS + t) as u64) * 1_000;
                format!("{{email: \"u{k}@t{t}\", created: {ms}}}")
            })
            .collect();
        let mut q = String::from("{\"query\":");
        fenec_core::json::escape_into(&mut q, &format!("put users [{}]", docs.join(",")));
        q.push('}');
        let (status, b) = r.send("POST", &format!("/t/t{t:03}/query"), None, q.as_bytes());
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&b));
    }
    // Names in Turkish's order over their folds, a collection of their own,
    // made once the cases before it are timed: those read the tenants they
    // read before it, and measure against a build without it.
    let people = |r: &mut Client| {
        const FIRST: [&str; 22] = [
            "Ali", "Ayşe", "Çağla", "Deniz", "Emre", "Ezgi", "Gökhan", "Hülya", "Irmak", "İpek",
            "Kemal", "Leyla", "Mert", "Nazlı", "Ömer", "Özge", "Selin", "Şule", "Tolga", "Ümit",
            "Yağız", "Zeynep",
        ];
        const LAST: [&str; 12] = [
            "YILMAZ", "Kaya", "Demir", "Çelik", "ŞAHİN", "Yıldız", "Öztürk", "aydın", "Özdemir",
            "Arslan", "Doğan", "Kılıç",
        ];
        for t in 0..TENANTS {
            let people: Vec<String> = (0..ROWS)
                .map(|k| {
                    let (f, l) = (FIRST[(k * 7 + t) % 22], LAST[(k * 5 + t * 3) % 12]);
                    format!("{{name: \"{f} {l}\"}}")
                })
                .collect();
            let make = "create collection people (name text collate tr nocase @sorted)";
            for q in [
                make.to_string(),
                format!("put people [{}]", people.join(",")),
            ] {
                let mut body = String::from("{\"query\":");
                fenec_core::json::escape_into(&mut body, &q);
                body.push('}');
                let target = format!("/t/t{t:03}/query");
                let (status, b) = r.send("POST", &target, None, body.as_bytes());
                assert_eq!(status, 200, "{}", String::from_utf8_lossy(&b));
            }
        }
    };
    let count = "get users select count() as n";
    let top = "get users select email, created order created desc limit 20";
    let names = "get people select name order name limit 20";
    let body = |q: &str, merge: &str| {
        let mut b = String::from("{\"query\":");
        fenec_core::json::escape_into(&mut b, q);
        b.push_str(&format!(",\"merge\":{merge}}}"));
        b
    };
    let cases = [
        ("a count, summed", count, r#"{"sum":["n"]}"#),
        (
            "the 20 newest, merged",
            top,
            r#"{"order":["created desc"],"limit":20}"#,
        ),
        (
            "the 20 first names, collate tr nocase, merged",
            names,
            r#"{"order":[{"column":"name","collate":"tr","nocase":true}],"limit":20}"#,
        ),
    ];
    let tenants: Vec<String> = (0..TENANTS).map(|t| format!("t{t:03}")).collect();
    println!(
        "\nquery across tenants: {TENANTS} tenants of {ROWS} rows on 3 nodes, {ROUNDS} rounds"
    );
    println!("                                    p50        p95        p99");
    for (label, q, merge) in cases {
        if q == names {
            people(&mut r);
        }
        let fan = body(q, merge);
        // The answer is what a client asking each tenant would merge.
        let (status, b) = r.send("POST", "/_shard/query", None, fan.as_bytes());
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&b));
        assert!(String::from_utf8_lossy(&b).contains(&format!("\"answered\":{TENANTS}")));
        let one = |c: &mut Client| {
            let (status, _) = c.send("POST", "/_shard/query", None, fan.as_bytes());
            assert_eq!(status, 200);
        };
        let mut each = String::from("{\"query\":");
        fenec_core::json::escape_into(&mut each, q);
        each.push('}');
        let in_turn = |c: &mut Client| {
            for t in &tenants {
                let (status, _) = c.send("POST", &format!("/t/{t}/query"), None, each.as_bytes());
                assert_eq!(status, 200);
            }
        };
        let mut pool: Vec<Client> = (0..AT_ONCE).map(|_| Client::new(&raddr)).collect();
        let mut at_once = || {
            std::thread::scope(|s| {
                for (k, c) in pool.iter_mut().enumerate() {
                    let (tenants, each) = (&tenants, &each);
                    s.spawn(move || {
                        for t in tenants.iter().skip(k).step_by(AT_ONCE) {
                            let target = format!("/t/{t}/query");
                            let (status, _) = c.send("POST", &target, None, each.as_bytes());
                            assert_eq!(status, 200);
                        }
                    });
                }
            });
        };
        let timed = |f: &mut dyn FnMut()| -> Vec<f64> {
            for _ in 0..20 {
                f();
            }
            let mut out: Vec<f64> = (0..ROUNDS)
                .map(|_| {
                    let t = Instant::now();
                    f();
                    t.elapsed().as_secs_f64() * 1e3
                })
                .collect();
            out.sort_by(|a, b| a.partial_cmp(b).unwrap());
            out
        };
        let fanned = timed(&mut || one(&mut r));
        let turned = timed(&mut || in_turn(&mut r));
        let parallel = timed(&mut at_once);
        println!("  {label}");
        for (how, v) in [
            ("POST /_shard/query", &fanned),
            ("each tenant in turn", &turned),
            ("each tenant, 16 at once", &parallel),
        ] {
            println!(
                "    {how:<28} {:>7.2} ms {:>7.2} ms {:>7.2} ms",
                pct(v, 0.5),
                pct(v, 0.95),
                pct(v, 0.99)
            );
        }
    }
    for (_, dir) in &nodes {
        let _ = std::fs::remove_dir_all(dir);
    }
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

/// A router whose directory is replicated, at `path`: the primary, or with
/// `follows` a standby following the primary at that address.
fn replicated_router(path: &std::path::Path, follows: Option<&str>) -> String {
    use fenec_http::replication::{fresh_id, Follower, Replication};
    const TOKEN: &str = "directory";
    let (mut db, feed) = fenec_http::replication::open(path.to_str().unwrap(), 8 << 20).unwrap();
    match follows {
        Some(_) => {
            let lineage = db.history().lineage.clone();
            db.follow(lineage).unwrap();
        }
        None => {
            db.fork(fresh_id()).unwrap();
            db.sync().unwrap();
        }
    }
    let db = Arc::new(std::sync::RwLock::new(db));
    let dir = Directory::load(Arc::clone(&db)).unwrap();
    let follower = follows.map(|addr| {
        let f = Follower::new(
            &format!("http://{addr}"),
            TOKEN.into(),
            Arc::clone(&db),
            Some(Arc::clone(&feed)),
            true,
        )
        .unwrap();
        f.start("fenec-standby".into()).unwrap();
        f
    });
    let router = Router::replicated(
        dir,
        Config {
            addr: "127.0.0.1:0".into(),
            upstream_timeout: Duration::from_secs(600),
            ..Config::default()
        },
        Replication::new(Some(TOKEN.into()), Some(feed), follower),
    );
    let listener = router.bind().unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = router.serve_on(listener);
    });
    addr
}

/// What a standby router costs a request against the primary: one per
/// region forwards from the directory it follows, so a request through it
/// should cost what one through the primary does. The same request straight
/// to the node, through the primary and through the standby, in turns on
/// three keep-alive connections, so a drift of the machine's lands on all
/// three alike.
fn standby() {
    let (node_addr, node_dir) = node("sb");
    let files = std::env::temp_dir().join(format!("fenec-overhead-sb-dirs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&files);
    std::fs::create_dir_all(&files).unwrap();
    let primary = replicated_router(&files.join("primary.fenec"), None);
    let mut p = Client::new(&primary);
    let body = format!(r#"{{"addr":"{node_addr}","token":"adm","region":"eu","zone":"eu-a"}}"#);
    assert_eq!(
        p.send("PUT", "/_shard/nodes/n1", None, body.as_bytes()).0,
        201
    );
    let schema = r#"{"region":"eu","schema":"create collection docs (title text)"}"#;
    assert_eq!(
        p.send("PUT", "/_shard/tenants/acme", None, schema.as_bytes())
            .0,
        201
    );
    let put = r#"{"query":"put docs {title: \"probe\"}"}"#;
    assert_eq!(p.send("POST", "/t/acme/query", None, put.as_bytes()).0, 200);
    let standby = replicated_router(&files.join("standby.fenec"), Some(&primary));
    let mut s = Client::new(&standby);
    let deadline = Instant::now() + Duration::from_secs(30);
    while s
        .send("GET", "/_shard/tenants", None, b"")
        .1
        .windows(4)
        .all(|w| w != b"acme")
    {
        assert!(
            Instant::now() < deadline,
            "the standby's directory never arrived"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let target = "/t/acme/docs?limit=1&select=title";
    let mut d = Client::new(&node_addr);
    let mut clients = [&mut d, &mut p, &mut s];
    for c in clients.iter_mut() {
        for _ in 0..500 {
            let (status, _) = c.send("GET", target, None, b"");
            assert_eq!(status, 200);
        }
    }
    let mut times: [Vec<f64>; 3] = Default::default();
    for round in 0..ROUNDS {
        // The routers swap places every other round: asked always last, the
        // standby came out half a microsecond slower than the primary.
        let order = if round % 2 == 0 { [0, 1, 2] } else { [0, 2, 1] };
        for i in order {
            let at = Instant::now();
            let (status, _) = clients[i].send("GET", target, None, b"");
            times[i].push(at.elapsed().as_secs_f64() * 1e6);
            assert_eq!(status, 200);
        }
    }
    println!(
        "a standby router's forwarding, {ROUNDS} x GET {target} in turns over three \
         keep-alive connections"
    );
    println!("              p50       p95       p99");
    for t in times.iter_mut() {
        t.sort_by(|a, b| a.partial_cmp(b).unwrap());
    }
    for (label, v) in ["direct", "primary", "standby"].iter().zip(&times) {
        println!(
            "  {label:<8} {:>6.1} us {:>6.1} us {:>6.1} us",
            pct(v, 0.5),
            pct(v, 0.95),
            pct(v, 0.99)
        );
    }
    for (label, v) in [("primary", &times[1]), ("standby", &times[2])] {
        println!(
            "  added by the {label}: {:.1} us p50, {:.1} us p99",
            pct(v, 0.5) - pct(&times[0], 0.5),
            pct(v, 0.99) - pct(&times[0], 0.99)
        );
    }
    let _ = std::fs::remove_dir_all(node_dir);
    let _ = std::fs::remove_dir_all(files);
}

/// A read near a tenant's read replica against one sent home, through the
/// router of the replica's region: the node of each region reached through
/// a loopback proxy standing for the link to it. In turns over keep-alive
/// connections, each request the same `get`: sent home (unmarked), marked
/// for the replica, and marked after a write the replica already holds.
/// Then a client's write home and its read after it -- the replica waiting
/// for the write it names (`Fenec-After`) -- against a read sent home after
/// the same write: what a client reading its own writes pays for reading
/// near, the replication's catch-up in it.
fn reads() {
    let (eu1, eu_dir) = started("rr-eu1", None, true);
    let (us1, us_dir) = started("rr-us1", None, true);
    let (to_eu1, to_us1) = (Proxy::to(&eu1), Proxy::to(&us1));
    let router = Router::new(
        Directory::in_memory(),
        Config {
            addr: "127.0.0.1:0".into(),
            upstream_timeout: Duration::from_secs(600),
            near: Some("us".into()),
            ..Config::default()
        },
    );
    let listener = router.bind().unwrap();
    let raddr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = router.serve_on(listener);
    });
    let mut r = Client::new(&raddr);
    for (name, addr, region) in [("eu1", &to_eu1.addr, "eu"), ("us1", &to_us1.addr, "us")] {
        let body = format!(r#"{{"addr":"{addr}","token":"adm","region":"{region}"}}"#);
        let path = format!("/_shard/nodes/{name}");
        assert_eq!(r.send("PUT", &path, None, body.as_bytes()).0, 201);
    }
    let schema = r#"{"node":"eu1","schema":"create collection docs (title text)"}"#;
    assert_eq!(
        r.send("PUT", "/_shard/tenants/acme", None, schema.as_bytes())
            .0,
        201
    );
    let (status, body) = r.send("PUT", "/_shard/tenants/acme/read-replicas/us", None, b"");
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&body));

    let get = br#"{"query":"get docs select title limit 1"}"#;
    let put = br#"{"query":"put docs {title: \"probe\"}"}"#;
    let marked: &[(&str, &str)] = &[("Fenec-Read", "replica")];
    let (status, _, seq) = r.send_with("POST", "/t/acme/query", &[], put);
    assert_eq!(status, 200);
    let seq = seq.unwrap().to_string();
    let history = r.history.clone().expect("a write names its history");
    let after: &[(&str, &str)] = &[
        ("Fenec-Read", "replica"),
        ("Fenec-After", &seq),
        ("Fenec-History", &history),
    ];
    let kinds: [(&str, &[(&str, &str)]); 3] = [
        ("home", &[]),
        ("replica", marked),
        ("replica, after", after),
    ];
    let mut clients: Vec<Client> = kinds.iter().map(|_| Client::new(&raddr)).collect();
    for (c, (_, headers)) in clients.iter_mut().zip(&kinds) {
        for _ in 0..500 {
            assert_eq!(c.send_with("POST", "/t/acme/query", headers, get).0, 200);
        }
    }
    let mut times: Vec<Vec<f64>> = vec![Vec::new(); kinds.len()];
    for round in 0..ROUNDS {
        for k in 0..kinds.len() {
            let i = (k + round) % kinds.len();
            let at = Instant::now();
            let (status, _, _) = clients[i].send_with("POST", "/t/acme/query", kinds[i].1, get);
            times[i].push(at.elapsed().as_secs_f64() * 1e6);
            assert_eq!(status, 200);
        }
    }
    println!(
        "a read through the router of a read replica's region, {ROUNDS} x POST /t/acme/query in \
         turns over keep-alive connections, each node behind a loopback proxy"
    );
    println!("                    p50       p95       p99");
    for t in times.iter_mut() {
        t.sort_by(|a, b| a.partial_cmp(b).unwrap());
    }
    for ((label, _), v) in kinds.iter().zip(&times) {
        println!(
            "  {label:<15} {:>6.1} us {:>6.1} us {:>6.1} us",
            pct(v, 0.5),
            pct(v, 0.95),
            pct(v, 0.99)
        );
    }

    // A write home, then the client's read of it: near, waiting for it on
    // the replica, or home. In turns, a write before each read.
    const PAIRS: usize = 1_000;
    let mut w = Client::new(&raddr);
    let mut after_write: [Vec<f64>; 2] = Default::default();
    for round in 0..PAIRS {
        for k in 0..2 {
            let near = (k + round) % 2 == 0;
            let (status, _, seq) = w.send_with("POST", "/t/acme/query", &[], put);
            assert_eq!(status, 200);
            let seq = seq.unwrap().to_string();
            let history = w.history.clone().unwrap();
            let headers: &[(&str, &str)] = match near {
                true => &[
                    ("Fenec-Read", "replica"),
                    ("Fenec-After", &seq),
                    ("Fenec-History", &history),
                ],
                false => &[],
            };
            let at = Instant::now();
            let (status, _, _) = w.send_with("POST", "/t/acme/query", headers, get);
            after_write[near as usize].push(at.elapsed().as_secs_f64() * 1e6);
            assert_eq!(status, 200);
        }
    }
    println!("a client's read of the write it was just answered, {PAIRS} x each, in turns");
    for t in after_write.iter_mut() {
        t.sort_by(|a, b| a.partial_cmp(b).unwrap());
    }
    for (label, v) in [
        ("home", &after_write[0]),
        ("replica, after", &after_write[1]),
    ] {
        println!(
            "  {label:<15} {:>6.1} us {:>6.1} us {:>6.1} us",
            pct(v, 0.5),
            pct(v, 0.95),
            pct(v, 0.99)
        );
    }
    let _ = std::fs::remove_dir_all(eu_dir);
    let _ = std::fs::remove_dir_all(us_dir);
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

/// A region keeping its own directory, cut off from the global router, and
/// one of its three nodes cut off from its router: when the node stops
/// taking writes, and when its tenants take them again on their replicas
/// -- the failover its region's router makes with no word from the global
/// one -- at a lease of 1 s and of 5 s.
fn regions() {
    for term in [Duration::from_secs(1), Duration::from_secs(5)] {
        regional_failover(term);
    }
}

fn regional_failover(term: Duration) {
    use fenec_http::replication::{fresh_id, Follower, Replication};
    use std::path::Path;
    const TENANTS: usize = 30;
    const TOKEN: &str = "routers";
    const DIRS: &str = "directory";
    let files = std::env::temp_dir().join(format!(
        "fenec-overhead-regions-{}-{}",
        term.as_secs(),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&files);
    std::fs::create_dir_all(&files).unwrap();
    // A directory file: its own primary's, or following `follows`.
    let open = |path: &Path, follows: Option<String>| {
        let (mut db, feed) =
            fenec_http::replication::open(path.to_str().unwrap(), 8 << 20).unwrap();
        match &follows {
            Some(_) => {
                let lineage = db.history().lineage.clone();
                db.follow(lineage).unwrap();
            }
            None => {
                db.fork(fresh_id()).unwrap();
                db.sync().unwrap();
            }
        }
        let db = Arc::new(std::sync::RwLock::new(db));
        let follower = follows.map(|url| {
            let f = Follower::new(
                &url,
                DIRS.into(),
                Arc::clone(&db),
                Some(Arc::clone(&feed)),
                true,
            )
            .unwrap();
            f.start("fenec-follower".into()).unwrap();
            f
        });
        let dir = Directory::load(db).unwrap();
        (
            dir,
            Replication::new(Some(DIRS.into()), Some(feed), follower),
        )
    };
    let config = |region: Option<&str>, global: Vec<String>| Config {
        addr: "127.0.0.1:0".into(),
        token: Some(TOKEN.into()),
        upstream_timeout: Duration::from_secs(600),
        replicas: true,
        auto_failover: Some(term),
        region: region.map(str::to_string),
        global,
        ..Config::default()
    };
    let serve = |router: Arc<Router>| {
        let listener = router.bind().unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        router.start_leasing().unwrap();
        std::thread::spawn(move || {
            let _ = router.serve_on(listener);
        });
        addr
    };
    // The global router, and us's, each reaching the other through a proxy
    // that stands for the link between their regions.
    let (dir, repl) = open(&files.join("global.fenec"), None);
    let global = serve(Router::replicated(dir, config(None, Vec::new()), repl));
    let to_global = Proxy::to(&global);
    let (dir, repl) = open(&files.join("us.fenec"), None);
    let (copy, _) = open(
        &files.join("us.global.fenec"),
        Some(format!("http://{}", to_global.addr)),
    );
    let global_url = vec![format!("http://{}", to_global.addr)];
    let us =
        serve(Router::regional(dir, config(Some("us"), global_url), Some(repl), copy).unwrap());
    let to_us = Proxy::to(&us);
    let mut g = Client::new(&global);
    let body = format!(r#"{{"routers":["http://{}"]}}"#, to_us.addr);
    let (status, answer) = g.send("PUT", "/_shard/regions/us", Some(TOKEN), body.as_bytes());
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&answer));

    let nodes: Vec<(String, std::path::PathBuf)> = (1..=3)
        .map(|i| leased(&format!("r{}-{i}", term.as_secs())))
        .collect();
    let proxies: Vec<Proxy> = nodes.iter().map(|(addr, _)| Proxy::to(addr)).collect();
    for (i, p) in proxies.iter().enumerate() {
        let body = format!(
            r#"{{"addr":"{}","token":"adm","region":"us","zone":"us-{i}"}}"#,
            p.addr
        );
        let target = format!("/_shard/nodes/us{}", i + 1);
        let (status, answer) = g.send("PUT", &target, Some(TOKEN), body.as_bytes());
        assert_eq!(status, 201, "{}", String::from_utf8_lossy(&answer));
    }
    let query = |c: &mut Client, tenant: &str, sql: &str| -> u16 {
        let mut body = String::from("{\"query\":");
        fenec_core::json::escape_into(&mut body, sql);
        body.push('}');
        c.send("POST", &format!("/t/{tenant}/query"), None, body.as_bytes())
            .0
    };
    for i in 0..TENANTS {
        let body = format!(
            r#"{{"region":"us","node":"us{}","schema":"create collection notes (n int)"}}"#,
            i % 3 + 1
        );
        let (status, answer) = g.send(
            "PUT",
            &format!("/_shard/tenants/t{i}"),
            Some(TOKEN),
            body.as_bytes(),
        );
        assert_eq!(status, 201, "{}", String::from_utf8_lossy(&answer));
        assert_eq!(query(&mut g, &format!("t{i}"), "put notes {n: 0}"), 200);
    }
    // Every tenant's replica caught up before the cut.
    std::thread::sleep(Duration::from_millis(500));

    // What the relay costs: one request straight to the node, through us's
    // router, and through the global router, which relays it to us's, in
    // turns on three keep-alive connections (the proxies in the way of the
    // two routers' link and of the node, each a copy on loopback).
    if term == Duration::from_secs(1) {
        let target = "/t/t1/notes?limit=1";
        let mut clients = [
            Client::new(&nodes[1].0),
            Client::new(&us),
            Client::new(&global),
        ];
        for c in clients.iter_mut() {
            for _ in 0..500 {
                assert_eq!(c.send("GET", target, None, b"").0, 200);
            }
        }
        let mut times: [Vec<f64>; 3] = Default::default();
        for round in 0..ROUNDS {
            let order = if round % 2 == 0 { [0, 1, 2] } else { [0, 2, 1] };
            for i in order {
                let at = Instant::now();
                let (status, _) = clients[i].send("GET", target, None, b"");
                times[i].push(at.elapsed().as_secs_f64() * 1e6);
                assert_eq!(status, 200);
            }
        }
        for t in times.iter_mut() {
            t.sort_by(|a, b| a.partial_cmp(b).unwrap());
        }
        println!("\na request for a tenant of us, {ROUNDS} x GET {target} in turns");
        println!("              p50       p99");
        for (label, v) in ["node", "us router", "relayed"].iter().zip(&times) {
            println!(
                "  {label:<10} {:>6.1} us {:>6.1} us",
                pct(v, 0.5),
                pct(v, 0.99)
            );
        }
        println!(
            "  the relay through the global router added {:.1} us p50 to us's router's",
            pct(&times[2], 0.5) - pct(&times[1], 0.5)
        );
    }

    // us cut off from eu, then us1 from us's router.
    to_global.cut();
    to_us.cut();
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
        // A connection of its own each time: the router's to us1 was cut.
        loop {
            let mut c = Client::new(&us);
            if query(&mut c, t, "put notes {n: 2}") == 200 {
                back.push(cut.elapsed());
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    println!(
        "\nregion us cut off from the global router, a lease of {:.0} ms: us1 cut off from us's \
         router stopped taking writes {:.0} ms after, and its {} tenants took them again on their \
         replicas {:.0} to {:.0} ms after -- the lease and a tenth is {:.0} ms",
        ms(term),
        ms(fenced),
        held.len(),
        ms(*back.iter().min().unwrap()),
        ms(*back.iter().max().unwrap()),
        ms(term + term / 10)
    );
    for (_, dir) in &nodes {
        let _ = std::fs::remove_dir_all(dir);
    }
    let _ = std::fs::remove_dir_all(&files);
}
