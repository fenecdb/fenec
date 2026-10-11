//! What a tenant's quota costs: `make quota-bench`
//! (`cargo run --release -p fenec-http --example quotas -- [ROUNDS]`).
//!
//!   * In process: a lone `put` into a database with no limit, and into one
//!     held to the limit a tenant's bytes, rows and bytes-a-second quota
//!     install (`quota::limit`), in turns, ns a put.
//!   * Over HTTP on loopback: one node, two tenants -- `free` with no quota,
//!     `held` with every quota set, each high enough never to refuse -- a
//!     read (`GET /docs?limit=1`) and a write (`POST /docs`) asked of each
//!     in turns over a keep-alive connection a tenant, p50 and p99 of each.
//!
//! Every pair of figures is measured in the same rounds, the two sides
//! taking turns -- which goes first changing every round -- so a machine
//! slowing down, as a fanless laptop does under minutes of load, slows
//! both alike.

use fenec_http::quota::{self, Held, Kind, Quota};
use fenec_http::tenants::Tenants;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Instant;

/// Every quota, high enough that nothing here is refused.
fn generous() -> Quota {
    Quota::default()
        .with(Kind::Bytes, 1 << 40)
        .with(Kind::Rows, 1 << 40)
        .with(Kind::Streams, 1 << 20)
        .with(Kind::Requests, 1 << 20)
        .with(Kind::Writes, 1 << 30)
        .with(Kind::WriteBytes, 1 << 40)
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 - 1.0) * p).round() as usize]
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

    fn send(&mut self, method: &str, target: &str, body: &[u8]) -> u16 {
        let head = format!(
            "{method} {target} HTTP/1.1\r\nHost: q\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
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
            if let Some((_, v)) = l
                .split_once(':')
                .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            {
                len = v.trim().parse().unwrap();
            }
        }
        let mut body = vec![0u8; len];
        self.reader.read_exact(&mut body).unwrap();
        status
    }
}

fn engine(rounds: usize) {
    let put = fenec_ql::parse_one("put docs {title: \"a row of some length\", n: 7}").unwrap();
    let make = |q: Option<Quota>| {
        let mut db = fenec_core::prelude::Database::new();
        db.execute(&fenec_ql::parse_one("create collection docs (title text, n int)").unwrap())
            .unwrap();
        if let Some(q) = q {
            db.set_limit(quota::limit(&Held::new(q)));
        }
        db
    };
    // No limit; bytes and rows, which the landing asks; and the bytes a
    // second besides, whose bucket each landing is charged to.
    let sizes = Quota::default()
        .with(Kind::Bytes, 1 << 40)
        .with(Kind::Rows, 1 << 40);
    let mut dbs = [make(None), make(Some(sizes)), make(Some(generous()))];
    let mut times = [Vec::new(), Vec::new(), Vec::new()];
    for round in 0..rounds {
        for k in 0..3 {
            let side = (k + round) % 3;
            let t = Instant::now();
            for _ in 0..1000 {
                dbs[side].execute(&put).unwrap();
            }
            times[side].push(t.elapsed().as_nanos() as f64 / 1000.0);
        }
    }
    let [a, b, c] = &mut times;
    let (fa, fb, fc) = (pct(a, 0.5), pct(b, 0.5), pct(c, 0.5));
    println!("a lone put in process, the median of {rounds} rounds of 1 000:");
    println!("  no limit                    {fa:>7.0} ns");
    for (label, f) in [("bytes and rows", fb), ("and bytes a second", fc)] {
        println!(
            "  {label:<27} {f:>7.0} ns   ({:+.0} ns, {:+.1}%)",
            f - fa,
            (f / fa - 1.0) * 100.0
        );
    }
}

fn http(rounds: usize) {
    let dir = std::env::temp_dir().join(format!("fenec-quota-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let tenants = Arc::new(Tenants::new(&dir).unwrap());
    let cfg = fenec_http::Config {
        addr: "127.0.0.1:0".into(),
        admin_token: Some("adm".into()),
        ..fenec_http::Config::default()
    };
    let server = fenec_http::Server::with_tenants(Arc::clone(&tenants), cfg);
    let listener = server.bind().unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    for t in ["free", "held"] {
        tenants.create(t).unwrap();
        let mut c = Client::new(&addr);
        let body = br#"{"query":"create collection docs (title text, n int)"}"#;
        assert_eq!(c.send("POST", &format!("/t/{t}/query"), body), 200);
        assert_eq!(
            c.send("POST", &format!("/t/{t}/docs"), br#"{"title":"x","n":0}"#),
            201
        );
    }
    let held = generous();
    let mut set = quota::Set::default();
    for k in quota::KINDS {
        set = set.with(k, Some(held.get(k)));
    }
    tenants.set_quota("held", set).unwrap();

    let mut clients = [Client::new(&addr), Client::new(&addr)];
    let cases: [(&str, &str, &[u8], u16); 2] = [
        ("read ", "GET", b"", 200),
        (
            "write",
            "POST",
            br#"{"title":"a row of some length","n":7}"#,
            201,
        ),
    ];
    println!("\none tenant node on loopback, {rounds} requests a case and tenant, in turns:");
    println!("               p50        p99");
    for (name, method, body, want) in cases {
        let mut lat = [Vec::new(), Vec::new()];
        for round in 0..rounds {
            for k in 0..2 {
                let side = (k + round) % 2;
                let t = ["free", "held"][side];
                let target = match method {
                    "GET" => format!("/t/{t}/docs?limit=1"),
                    _ => format!("/t/{t}/docs"),
                };
                let start = Instant::now();
                let status = clients[side].send(method, &target, body);
                lat[side].push(start.elapsed().as_secs_f64() * 1e6);
                assert_eq!(status, want, "{t} {method}");
            }
        }
        let [free, held] = &mut lat;
        let (f50, f99, h50, h99) = (
            pct(free, 0.5),
            pct(free, 0.99),
            pct(held, 0.5),
            pct(held, 0.99),
        );
        println!("  {name} free  {f50:>6.1} us  {f99:>6.1} us");
        println!(
            "  {name} held  {h50:>6.1} us  {h99:>6.1} us   ({:+.1} us p50)",
            h50 - f50
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

fn main() {
    let rounds: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000);
    engine((rounds / 100).max(20));
    http(rounds);
}
