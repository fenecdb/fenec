//! What a scoped write's rules cost: `make policy-bench`
//! (`cargo run --release -p fenec-http --example policy -- [ROUNDS]`).
//!
//! The same `set` -- a lease moved on and a counter counted, a row of the
//! token's own -- under the server's token and under a courier's, whose
//! policy grants it its own orders:
//!
//!   * `plain`: `update where courier = $jwt.sub`, the filter asked of the
//!     row before and the row after;
//!   * `fields`: `update(run_at, n) where courier = $jwt.sub`, the fields
//!     the write changes compared besides;
//!   * `transition`: `update(run_at, n) where courier = $jwt.sub and new.n
//!     = n + 1`, the rule asked of the two rows together;
//!   * `returning`: the `fields` policy, the write answering its row,
//!     which is held to the read rules as written.
//!
//! In process, the statement held to the token (`Scope::rewrite`) and run
//! under it (`access::within`) with the `Check` hook installed, as a
//! server runs one, ns a write; over HTTP on loopback, one server a side,
//! a `POST /query` each in turns over a keep-alive connection, p50 and p99.
//! Every figure is measured in rounds the sides take turns in, which goes
//! first changing every round, so a machine slowing down -- a fanless
//! laptop under minutes of load -- slows each alike. A side whose policy
//! the build cannot read (one from before transitions) is left out, so the
//! same program measures the build before beside this one.

use fenec_core::prelude::*;
use fenec_http::access::{self, Access, CheckPlugin, Who};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, RwLock};
use std::time::Instant;

const SECRET: &[u8] = b"a secret of more than thirty-two bytes, for HS256";
const ROOT: &str = "root";
const ROWS: i64 = 1_000;
const SET: &str = "set orders {run_at: now() + 45000, n: n + 1} where id = $1";

/// Each side: its name, its policy (`None`: the server's token), and
/// whether its write answers its row.
const SIDES: [(&str, Option<&str>, bool); 5] = [
    ("server token", None, false),
    (
        "plain",
        Some("orders read,update where courier = $jwt.sub"),
        false,
    ),
    (
        "fields",
        Some(
            "orders read where courier = $jwt.sub\n\
             orders update(run_at, n) where courier = $jwt.sub",
        ),
        false,
    ),
    (
        "transition",
        Some(
            "orders read where courier = $jwt.sub\n\
             orders update(run_at, n) where courier = $jwt.sub and new.n = n + 1",
        ),
        false,
    ),
    (
        "returning",
        Some(
            "orders read where courier = $jwt.sub\n\
             orders update(run_at, n) where courier = $jwt.sub",
        ),
        true,
    ),
];

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 - 1.0) * p).round() as usize]
}

fn statement(returning: bool) -> String {
    match returning {
        true => format!("{SET} returning n"),
        false => SET.to_string(),
    }
}

/// The orders, every one the courier's.
fn orders() -> Database {
    let mut db = Database::new();
    for sql in [
        "create collection orders (courier text, run_at timestamp, n int)".to_string(),
        format!(
            "put orders [{}]",
            (0..ROWS)
                .map(|_| "{courier: \"c1\", run_at: 0, n: 0}")
                .collect::<Vec<_>>()
                .join(", ")
        ),
    ] {
        db.execute(&fenec_ql::parse_one(&sql).unwrap()).unwrap();
    }
    db
}

/// The sides this build can run: a transition is left out of a build that
/// takes `new.` for a json field's path -- one that refuses no rule naming
/// it beside a read.
fn sides() -> Vec<(&'static str, Option<Arc<Access>>, Who, bool)> {
    let transitions = Access::new(SECRET, "c read,update where new.x = 1").is_err();
    let mut out = Vec::new();
    for (name, policy, returning) in SIDES {
        let Some(policy) = policy else {
            out.push((name, None, Who::Full, returning));
            continue;
        };
        if policy.contains("new.") && !transitions {
            println!("  ({name} left out: this build has no transitions)");
            continue;
        }
        match Access::new(SECRET, policy) {
            Ok(a) => {
                let token = a.mint(r#"{"sub":"c1"}"#).unwrap();
                let scope = a.scope(&token, 1).unwrap();
                out.push((
                    name,
                    Some(Arc::new(a)),
                    Who::Scoped(Arc::new(scope)),
                    returning,
                ));
            }
            Err(e) => println!("  ({name} left out: {e})"),
        }
    }
    out
}

/// Writes a side makes in a turn, in process.
const TURN: usize = 100;

fn engine(rounds: usize) {
    let sides = sides();
    // One database for every side: each turn writes the same store, grown
    // by every side alike.
    let mut db = orders();
    db.install_plugin(&CheckPlugin).unwrap();
    let stmts: Vec<Statement> = sides
        .iter()
        .map(|s| fenec_ql::parse_one(&statement(s.3)).unwrap())
        .collect();
    let mut times = vec![Vec::new(); sides.len()];
    let mut id = 0;
    for round in 0..rounds {
        for k in 0..sides.len() {
            let side = (k + round) % sides.len();
            let who = &sides[side].2;
            let t = Instant::now();
            for _ in 0..TURN {
                id = id % ROWS + 1;
                let params = [Value::Int(id)];
                let stmt = match who.scope() {
                    None => stmts[side].clone(),
                    Some(s) => s.rewrite(stmts[side].clone()).unwrap(),
                };
                access::within(who, || db.execute_with(&stmt, &params)).unwrap();
            }
            times[side].push(t.elapsed().as_nanos() as f64 / TURN as f64);
        }
    }
    println!("a `set` of one row in process, the median of {rounds} turns of {TURN}:");
    let base = pct(&mut times[0], 0.5);
    for (k, s) in sides.iter().enumerate() {
        let f = pct(&mut times[k], 0.5);
        println!(
            "  {:<14} {f:>7.0} ns   ({:+.0} ns against the server's token)",
            s.0,
            f - base
        );
    }
}

/// A keep-alive client: one connection, requests one after another.
struct Client {
    reader: BufReader<TcpStream>,
    out: TcpStream,
    token: String,
}

impl Client {
    fn new(addr: &str, token: String) -> Client {
        let s = TcpStream::connect(addr).unwrap();
        s.set_nodelay(true).unwrap();
        Client {
            out: s.try_clone().unwrap(),
            reader: BufReader::new(s),
            token,
        }
    }

    fn post(&mut self, body: &[u8]) -> u16 {
        let head = format!(
            "POST /query HTTP/1.1\r\nHost: p\r\nAuthorization: Bearer {}\r\n\
             Content-Length: {}\r\n\r\n",
            self.token,
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

fn http(rounds: usize) {
    let sides = sides();
    let mut clients = Vec::new();
    for (_, access, _, _) in &sides {
        let cfg = fenec_http::Config {
            addr: "127.0.0.1:0".into(),
            token: Some(ROOT.into()),
            access: access.clone(),
            ..fenec_http::Config::default()
        };
        let server = fenec_http::Server::new(Arc::new(RwLock::new(orders())), cfg);
        let listener = server.bind().unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            let _ = server.serve_on(listener);
        });
        let token = match access {
            None => ROOT.to_string(),
            Some(a) => a.mint(r#"{"sub":"c1"}"#).unwrap(),
        };
        clients.push(Client::new(&addr, token));
    }
    let mut lat = vec![Vec::new(); sides.len()];
    let mut id = 0;
    for round in 0..rounds {
        for k in 0..sides.len() {
            let side = (k + round) % sides.len();
            id = id % ROWS + 1;
            let mut body = String::from("{\"query\":");
            fenec_core::json::escape_into(&mut body, &statement(sides[side].3));
            body.push_str(&format!(",\"params\":[{id}]}}"));
            let start = Instant::now();
            let status = clients[side].post(body.as_bytes());
            lat[side].push(start.elapsed().as_secs_f64() * 1e6);
            assert_eq!(status, 200, "{}", sides[side].0);
        }
    }
    println!("\na `POST /query` of it over HTTP on loopback, {rounds} a side in turns:");
    println!("                    p50        p99");
    for (k, s) in sides.iter().enumerate() {
        let (p50, p99) = (pct(&mut lat[k], 0.5), pct(&mut lat[k], 0.99));
        println!("  {:<14} {p50:>6.1} us  {p99:>6.1} us", s.0);
    }
}

fn main() {
    fenec_http::audit::set_delay(0);
    let rounds: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000);
    engine((rounds / 10).max(200));
    http(rounds);
}
