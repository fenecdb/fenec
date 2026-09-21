//! What the router costs: `cargo run --release -p fenec-shard --example overhead -- [N] [DIM]`
//!
//! What is measured, all on loopback with node and router in one process
//!   * one keep-alive client, the same request straight to the node and
//!     through the router: p50 / p95 / p99 of each and the difference
//!   * a tenant move: N documents with a DIM vector and an HNSW index,
//!     image size and wall time, and the first query on the target (the
//!     graph travels in the image, so it is not rebuilt)

use fenec_http::tenants::Tenants;
use fenec_shard::directory::Directory;
use fenec_shard::{Config, Router};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

const ROUNDS: usize = 5_000;

fn node(tag: &str) -> (String, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("fenec-overhead-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let tenants = Arc::new(Tenants::new(&dir).unwrap());
    let cfg = fenec_http::Config {
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
