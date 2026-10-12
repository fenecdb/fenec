//! What a scoped token's rules cost: `make policy-bench`
//! (`cargo run --release -p fenec-http --example policy -- [ROUNDS]
//! [SIDE,SIDE...]`, `-- streams [N...]` or `-- beats [M...]`).
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
//!     which is held to the read rules as written;
//!   * `reach`: `update where courier in (get crews select courier where
//!     lead = $jwt.sub)`, a rule reading another collection, answered
//!     before the statement under its lock (`Scope::bound`).
//!
//! In process, the statement held to the token (`Scope::rewrite`) and run
//! under it (`access::within`) with the `Check` hook installed, as a
//! server runs one, ns a write; over HTTP on loopback, one server a side,
//! a `POST /query` each in turns over a keep-alive connection, p50 and p99.
//! Every figure is measured in rounds the sides take turns in, which goes
//! first changing every round, so a machine slowing down -- a fanless
//! laptop under minutes of load -- slows each alike. A side whose policy
//! the build cannot read (one from before transitions) is left out, so the
//! same program measures the build before beside this one; naming the
//! sides runs those alone.
//!
//! Then reads -- a row by id and a page of twenty, every field -- under the
//! server's token, a courier's two read rules (`plain`), and the same rules
//! with the first naming the fields it shows (`fields`), so that half the
//! rows answer two of their fields `null`, and one rule reading the
//! couriers out of `crews` (`reach`): in process, held to the token, run,
//! masked and written as JSON, and over HTTP as above.
//!
//! `streams` measures what a write costs beside scoped subscriptions: N
//! customers each following the courier carrying their order on
//! `positions`, under a rule that reads `orders` (`reach`) or one naming
//! the courier as a claim (`claim`), and none; a client writes `orders`
//! (a lease moved on, the order another customer's) and `positions` (a
//! courier's beat, a courier none of them follows) one after another,
//! and the process's CPU a write and the write's round trip are printed.
//! `beats` is examples/dispatch's beat beside M customers following their
//! couriers: on the order (`courier_loc`), or on `positions` through such
//! a rule.

use fenec_core::prelude::*;
use fenec_http::access::{self, Access, CheckPlugin, Who};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const SECRET: &[u8] = b"a secret of more than thirty-two bytes, for HS256";
const ROOT: &str = "root";
const ROWS: i64 = 1_000;
const SET: &str = "set orders {run_at: now() + 45000, n: n + 1} where id = $1";

/// A rule reading another collection: the couriers of the crews the token
/// leads, every order's.
const REACH: &str = "crews read where lead = $jwt.sub\n\
                     orders read,update where courier in (get crews select courier where lead = $jwt.sub)";

/// Each side: its name, its policy (`None`: the server's token), and
/// whether its write answers its row.
const SIDES: [(&str, Option<&str>, bool); 6] = [
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
    ("reach", Some(REACH), false),
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

/// The crews a rule reading another collection reads: both couriers led
/// by the token's subject, so that its rule admits the rows the other
/// sides' do.
const CREWS: &str = "create collection crews (courier text, lead text @hash)";
const CREWED: &str = r#"put crews [{courier: "c1", lead: "c1"}, {courier: "c2", lead: "c1"}]"#;

/// The orders, every one the courier's.
fn orders() -> Database {
    let mut db = Database::new();
    for sql in [
        "create collection orders (courier text, run_at timestamp, n int)".to_string(),
        CREWS.to_string(),
        CREWED.to_string(),
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
fn sides(only: &[String]) -> Vec<(&'static str, Option<Arc<Access>>, Who, bool)> {
    let transitions = Access::new(SECRET, "c read,update where new.x = 1").is_err();
    let mut out = Vec::new();
    for (name, policy, returning) in SIDES {
        if !only.is_empty() && !only.iter().any(|o| o == name) {
            continue;
        }
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

fn engine(rounds: usize, only: &[String]) {
    let sides = sides(only);
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
                if who.reaches_out() {
                    let (stmt, bound) = bound(who, &db, &stmts[side], &params);
                    access::within(&bound, || db.execute_with(&stmt, &params)).unwrap();
                    continue;
                }
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

/// The statement as `who`, whose rules read another collection, runs it
/// over `db`, and the token as bound -- as a server binds it, under the
/// lock it holds (`Scope::bound`) -- with what its answer must not show.
fn bound(who: &Who, db: &Database, stmt: &Statement, params: &[Value]) -> (Statement, Who) {
    let (stmt, who, _) = bound_masked(who, db, stmt, params);
    (stmt, who)
}

fn bound_masked(
    who: &Who,
    db: &Database,
    stmt: &Statement,
    params: &[Value],
) -> (Statement, Who, Option<access::Mask>) {
    let scope = who.scope().expect("a scoped token");
    let bound = Arc::new(scope.bound(db, &[stmt]).unwrap());
    let mut mask = None;
    let stmt = bound.rewrite_with(stmt.clone(), params, &mut mask).unwrap();
    (stmt, Who::Scoped(bound), mask)
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
        self.post_to("/query", body)
    }

    fn batch(&mut self, body: &[u8]) -> u16 {
        self.post_to("/batch", body)
    }

    fn post_to(&mut self, path: &str, body: &[u8]) -> u16 {
        let head = format!(
            "POST {path} HTTP/1.1\r\nHost: p\r\nAuthorization: Bearer {}\r\n\
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

fn http(rounds: usize, only: &[String]) {
    let sides = sides(only);
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

// ------------------------------------------------------------------ reads

/// The reads: a row by id, and a page of twenty, every field of each.
const READS: [(&str, &str); 2] = [
    ("a row by id", "get orders where id = $1"),
    ("a page of 20", "get orders where id > $1 limit 20"),
];

/// Each read side: its name and its policy (`None`: the server's token).
/// `plain` and `fields` admit the same rows, every one, by the same two
/// filters; under `fields` the first names what it shows, so the other
/// courier's half of the orders answers its customer and door `null`.
const READ_SIDES: [(&str, Option<&str>); 4] = [
    ("server token", None),
    (
        "plain",
        Some(
            "orders read where n >= 0\n\
             orders read where courier = $jwt.sub",
        ),
    ),
    (
        "fields",
        Some(
            "orders read(run_at, n) where n >= 0\n\
             orders read where courier = $jwt.sub",
        ),
    ),
    ("reach", Some(REACH)),
];

/// Orders of two couriers in turn, with what one shows the other.
fn shared_orders() -> Database {
    let mut db = Database::new();
    let rows: Vec<String> = (0..ROWS)
        .map(|i| {
            format!(
                "{{courier: \"c{}\", run_at: {i}, n: {i}, customer: \"k{i}\", dropoff: \"door {i}\"}}",
                1 + i % 2
            )
        })
        .collect();
    for sql in [
        "create collection orders (courier text, run_at timestamp, n int, customer text, \
         dropoff text)"
            .to_string(),
        CREWS.to_string(),
        CREWED.to_string(),
        format!("put orders [{}]", rows.join(", ")),
    ] {
        db.execute(&fenec_ql::parse_one(&sql).unwrap()).unwrap();
    }
    db
}

fn read_sides(only: &[String]) -> Vec<(&'static str, Option<Arc<Access>>, Who)> {
    let mut out = Vec::new();
    for (name, policy) in READ_SIDES {
        if !only.is_empty() && !only.iter().any(|o| o == name) {
            continue;
        }
        let Some(policy) = policy else {
            out.push((name, None, Who::Full));
            continue;
        };
        match Access::new(SECRET, policy) {
            Ok(a) => {
                let token = a.mint(r#"{"sub":"c1"}"#).unwrap();
                let scope = a.scope(&token, 1).unwrap();
                out.push((name, Some(Arc::new(a)), Who::Scoped(Arc::new(scope))));
            }
            Err(e) => println!("  ({name} left out: {e})"),
        }
    }
    out
}

/// A read as a server answers it, rendered: held to the token, run, its
/// rows shown as the token is shown them.
fn answered(who: &Who, db: &Database, stmt: &Statement, params: &[Value], out: &mut String) {
    let resp = match who.scope() {
        None => db.query(stmt, params).unwrap(),
        Some(_) if who.reaches_out() => {
            let (stmt, bound, mask) = bound_masked(who, db, stmt, params);
            let mut resp = db.query(&stmt, params).unwrap();
            if let (Some(m), Some(s)) = (&mask, bound.scope()) {
                s.shown(m, db, &mut resp);
            }
            resp
        }
        Some(s) => {
            let mut mask = None;
            let stmt = s.rewrite_with(stmt.clone(), params, &mut mask).unwrap();
            let mut resp = db.query(&stmt, params).unwrap();
            if let Some(m) = &mask {
                s.shown(m, db, &mut resp);
            }
            resp
        }
    };
    out.clear();
    if let Response::Rows(rs) = &resp {
        fenec_core::json::rows_array_into(out, rs);
    }
}

fn read_engine(rounds: usize, only: &[String]) {
    let sides = read_sides(only);
    let db = shared_orders();
    for (what, sql) in READS {
        let stmt = fenec_ql::parse_one(sql).unwrap();
        let mut times = vec![Vec::new(); sides.len()];
        let mut out = String::new();
        let mut id = 0;
        for round in 0..rounds {
            for k in 0..sides.len() {
                let side = (k + round) % sides.len();
                let t = Instant::now();
                for _ in 0..TURN {
                    id = (id + 7) % (ROWS - 25) + 1;
                    answered(&sides[side].2, &db, &stmt, &[Value::Int(id)], &mut out);
                }
                times[side].push(t.elapsed().as_nanos() as f64 / TURN as f64);
            }
        }
        println!(
            "\n{what} read in process and written as JSON, the median of {rounds} turns of {TURN}:"
        );
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
}

fn read_http(rounds: usize, only: &[String]) {
    let sides = read_sides(only);
    let mut clients = Vec::new();
    for (_, access, _) in &sides {
        let cfg = fenec_http::Config {
            addr: "127.0.0.1:0".into(),
            token: Some(ROOT.into()),
            access: access.clone(),
            ..fenec_http::Config::default()
        };
        let server = fenec_http::Server::new(Arc::new(RwLock::new(shared_orders())), cfg);
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
    for (what, sql) in READS {
        let mut lat = vec![Vec::new(); sides.len()];
        let mut id = 0;
        for round in 0..rounds {
            for k in 0..sides.len() {
                let side = (k + round) % sides.len();
                id = (id + 7) % (ROWS - 25) + 1;
                let mut body = String::from("{\"query\":");
                fenec_core::json::escape_into(&mut body, sql);
                body.push_str(&format!(",\"params\":[{id}]}}"));
                let start = Instant::now();
                let status = clients[side].post(body.as_bytes());
                lat[side].push(start.elapsed().as_secs_f64() * 1e6);
                assert_eq!(status, 200, "{}", sides[side].0);
            }
        }
        println!("\n{what} as a `POST /query` over HTTP on loopback, {rounds} a side in turns:");
        println!("                    p50        p99");
        for (k, s) in sides.iter().enumerate() {
            let (p50, p99) = (pct(&mut lat[k], 0.5), pct(&mut lat[k], 0.99));
            println!("  {:<14} {p50:>6.1} us  {p99:>6.1} us", s.0);
        }
    }
}

// ------------------------------------------------------- streams

/// A `POST /query`'s body: made before a request is timed.
fn body(sql: &str, params: &str) -> String {
    let mut body = String::from("{\"query\":");
    fenec_core::json::escape_into(&mut body, sql);
    body.push_str(&format!(",\"params\":{params}}}"));
    body
}

/// A server over `db` on loopback, `access` its policy, and its address.
fn serve(db: Database, access: Option<Arc<Access>>, streams: usize) -> String {
    let cfg = fenec_http::Config {
        addr: "127.0.0.1:0".into(),
        token: Some(ROOT.into()),
        access,
        max_streams: streams,
        // Every stream and every client a connection of its own: no
        // ceiling past the default's.
        max_connections: match streams > 64 {
            true => 0,
            false => fenec_http::Config::default().max_connections,
        },
        ..fenec_http::Config::default()
    };
    let server = fenec_http::Server::new(Arc::new(RwLock::new(db)), cfg);
    let listener = server.bind().unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    addr
}

// ---- writes beside scoped subscriptions

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Timeval {
    sec: i64,
    usec: i64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Rusage {
    utime: Timeval,
    stime: Timeval,
    rest: [i64; 14],
}

extern "C" {
    fn getrusage(who: i32, usage: *mut Rusage) -> i32;
}

/// The process's CPU time so far, us: the server's, the client's and the
/// streams' readers', which a turn of each side shares alike.
fn cpu_us() -> f64 {
    let mut r = Rusage::default();
    unsafe { getrusage(0, &mut r) };
    // macOS's microseconds are an `int` beside 4 bytes of padding.
    let us = |t: &Timeval| t.sec as f64 * 1e6 + (t.usec as i32) as f64;
    us(&r.utime) + us(&r.stime)
}

const FOLLOW_REACH: &str = "\
orders     read  where customer = $jwt.sub
positions  read  where courier in (get orders select courier where customer = $jwt.sub and (status = \"assigned\" or status = \"picked\"))
";
const FOLLOW_CLAIM: &str = "\
orders     read  where customer = $jwt.sub
positions  read  where courier = $jwt.courier
";

/// `n` customers, each with an order a courier of their own carries, and
/// every courier's position -- and as many couriers and orders again that
/// no stream follows, the ones written.
fn dispatch(n: usize) -> Database {
    let mut db = Database::new();
    let all = n + 64;
    let orders: Vec<String> = (0..all)
        .map(|i| format!(r#"{{customer: "u{i}", courier: "k{i}", status: "assigned", run_at: 0}}"#))
        .collect();
    let positions: Vec<String> = (0..all)
        .map(|i| format!(r#"{{courier: "k{i}", loc: {i}}}"#))
        .collect();
    for sql in [
        "create collection orders (customer text @hash, courier text @hash, status text, run_at timestamp)"
            .to_string(),
        "create collection positions (courier text @unique, loc int)".to_string(),
        format!("put orders [{}]", orders.join(", ")),
        format!("put positions [{}]", positions.join(", ")),
    ] {
        db.execute(&fenec_ql::parse_one(&sql).unwrap()).unwrap();
    }
    db
}

/// Opens a stream of `/positions/changes` as `token` and reads its seed;
/// a thread then reads it for good, as a client does.
fn follow(addr: &str, token: &str) {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(
        s,
        "GET /positions/changes HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\n\r\n"
    )
    .unwrap();
    let mut heard = Vec::new();
    let mut buf = [0u8; 4096];
    while !heard.windows(11).any(|w| w == b"event: seed") {
        let k = s.read(&mut buf).unwrap();
        assert!(k > 0, "the stream ended");
        heard.extend_from_slice(&buf[..k]);
    }
    std::thread::spawn(move || while matches!(s.read(&mut buf), Ok(k) if k > 0) {});
}

/// Writes a turn, of `orders` and of `positions` in turn.
const WRITES: usize = 200;

fn streams(counts: &[usize], rounds: usize) {
    println!(
        "a write beside N scoped streams following a courier each: the server's CPU a write \
         (us, the process's count) and its round trip p50 / p99 (us), {rounds} turns of \
         {WRITES} writes a side, each turn half `orders` (a lease moved on) and half \
         `positions` (a beat), neither of a followed row"
    );
    for &n in counts {
        // A side: its name, the policy its streams' tokens hold, a client
        // writing, and its laps.
        let mut sides = Vec::new();
        for (name, policy) in [
            ("none", None),
            ("claim", Some(FOLLOW_CLAIM)),
            ("reach", Some(FOLLOW_REACH)),
        ] {
            let access = policy.map(|p| Arc::new(Access::new(SECRET, p).unwrap()));
            let addr = serve(dispatch(n), access.clone(), n + 8);
            if let Some(a) = &access {
                for i in 0..n {
                    let token = a
                        .mint(&format!(r#"{{"sub":"u{i}","courier":"k{i}"}}"#))
                        .unwrap();
                    follow(&addr, &token);
                }
            }
            sides.push((
                name,
                Client::new(&addr, ROOT.into()),
                Vec::new(),
                Vec::new(),
            ));
        }
        let mut k = 0;
        for round in 0..rounds {
            for s in 0..sides.len() {
                let side = &mut sides[(s + round) % 3];
                let before = cpu_us();
                for w in 0..WRITES {
                    // Rows past the followed ones: what no stream holds.
                    let row = n + 1 + (k % 64);
                    k += 1;
                    let sql = match w % 2 {
                        0 => "set orders {run_at: now() + 45000} where id = $1",
                        _ => "set positions {loc: loc + 1} where id = $1",
                    };
                    let body = body(sql, &format!("[{row}]"));
                    let t = Instant::now();
                    let status = side.1.post(body.as_bytes());
                    side.3.push(t.elapsed().as_secs_f64() * 1e6);
                    assert_eq!(status, 200);
                }
                // The streams' looks after the last write, before the next
                // side's turn counts them.
                std::thread::sleep(Duration::from_millis(20));
                side.2.push((cpu_us() - before) / WRITES as f64);
            }
        }
        println!("  N = {n}:");
        for (name, _, cpu, lat) in &mut sides {
            println!(
                "    {name:<6} {:>7.1} us CPU   {:>6.1} / {:>6.1} us",
                pct(cpu, 0.5),
                pct(lat, 0.5),
                pct(lat, 0.99)
            );
        }
    }
}

// ---- examples/dispatch's beat, its customers following their couriers

/// The two ways a customer follows the courier carrying their order: the
/// courier's point written onto the order with each beat of its lease
/// (`order`), or the courier's own row of `positions`, which a rule
/// reading `orders` lets the customer read (`positions`).
const FOLLOW_ORDER: &str = "\
orders     read  where customer = $jwt.sub  for customer
orders     read  where courier = $jwt.sub  for courier
orders     update(run_at, courier_loc)  where courier = $jwt.sub and (status = \"assigned\" or status = \"picked\")  for courier
positions  read,insert,update(loc, seen, state, heading, job, vehicle)  where courier = $jwt.sub  for courier
";
const FOLLOW_POSITIONS: &str = "\
orders     read  where customer = $jwt.sub  for customer
orders     read  where courier = $jwt.sub  for courier
orders     update(run_at)  where courier = $jwt.sub and (status = \"assigned\" or status = \"picked\")  for courier
positions  read,insert,update(loc, seen, state, heading, job, vehicle)  where courier = $jwt.sub  for courier
positions  read  where courier in (get orders select courier where customer = $jwt.sub and (status = \"assigned\" or status = \"picked\"))  for customer
";
const BEAT_POSITION: &str = "put positions {courier: $1, loc: $2, seen: now(), state: $3, heading: $4, job: $5, vehicle: $6} \
                             if absent else set {loc: new.loc, seen: new.seen, state: new.state, heading: new.heading, job: new.job}";

/// `m` customers, each with an order picked up by a courier of their own,
/// and every courier's position, as Plover's company tenant holds them.
fn carried(m: usize) -> Database {
    let mut db = Database::new();
    let orders: Vec<String> = (0..m)
        .map(|i| {
            format!(
                r#"{{customer: "u{i}", courier: "k{i}", status: "picked", run_at: 0, claim: "c{i}", courier_loc: [29.0, 41.0]}}"#
            )
        })
        .collect();
    let positions: Vec<String> = (0..m)
        .map(|i| format!(r#"{{courier: "k{i}", loc: [29.0, 41.0], seen: 0, state: "dropoff", heading: 0, job: {}, vehicle: "bike"}}"#, i + 1))
        .collect();
    for sql in [
        "create collection orders (customer text @hash, courier text @hash, status text @hash, \
         run_at timestamp @sorted, claim text @hash, courier_loc geo)"
            .to_string(),
        "create collection positions (courier text @unique, loc geo @geo, seen timestamp @ttl(2m), \
         state text, heading float, job int, vehicle text)"
            .to_string(),
        format!("put orders [{}]", orders.join(", ")),
        format!("put positions [{}]", positions.join(", ")),
    ] {
        // A point read as written, not as a vector's `f32`s.
        for stmt in fenec_ql::parse_for(&db, &sql).unwrap() {
            db.execute(&stmt).unwrap();
        }
    }
    db
}

/// Opens a stream of `target` as `token`, reads its seed, and has a
/// thread read it for good, counting the bytes into `heard`.
fn hear(addr: &str, token: &str, target: &str, heard: &Arc<std::sync::atomic::AtomicUsize>) {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(
        s,
        "GET {target} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\n\r\n"
    )
    .unwrap();
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    while !seen.windows(11).any(|w| w == b"event: seed") {
        let k = s.read(&mut buf).unwrap();
        assert!(
            k > 0,
            "the stream ended: {}",
            String::from_utf8_lossy(&seen)
        );
        seen.extend_from_slice(&buf[..k]);
    }
    let heard = Arc::clone(heard);
    std::thread::spawn(move || {
        while let Ok(k) = s.read(&mut buf) {
            if k == 0 {
                break;
            }
            heard.fetch_add(k, std::sync::atomic::Ordering::Relaxed);
        }
    });
}

/// A courier's beat as `examples/dispatch` sends it (`heartbeat`), its
/// position and its order's lease in one `/batch`, beside `m` customers
/// following their couriers either way.
fn beats(counts: &[usize], rounds: usize) {
    println!(
        "a courier's beat carrying an order, M customers following theirs: the beat's round \
         trip p50 / p99 (us), the process's CPU a beat (us, the streams' readers among it) and \
         the bytes the customers' streams were sent a beat, {rounds} turns of {WRITES} beats a \
         side, each beat a courier's in turn"
    );
    use std::sync::atomic::{AtomicUsize, Ordering};
    for &m in counts {
        let mut sides = Vec::new();
        for (name, policy) in [("order", FOLLOW_ORDER), ("positions", FOLLOW_POSITIONS)] {
            let access = Arc::new(Access::new(SECRET, policy).unwrap());
            let addr = serve(carried(m), Some(Arc::clone(&access)), 2 * m + 8);
            let heard = Arc::new(AtomicUsize::new(0));
            for i in 0..m {
                let token = access
                    .mint(&format!(r#"{{"sub":"u{i}","role":"customer"}}"#))
                    .unwrap();
                hear(
                    &addr,
                    &token,
                    &format!("/orders/changes?id=eq.{}", i + 1),
                    &heard,
                );
                if name == "positions" {
                    hear(&addr, &token, "/positions/changes", &heard);
                }
            }
            let couriers: Vec<Client> = (0..m)
                .map(|i| {
                    let token = access
                        .mint(&format!(r#"{{"sub":"k{i}","role":"courier"}}"#))
                        .unwrap();
                    Client::new(&addr, token)
                })
                .collect();
            sides.push((name, couriers, heard, Vec::new(), Vec::new(), Vec::new()));
        }
        let mut k = 0usize;
        for round in 0..rounds {
            for s in 0..sides.len() {
                let (name, couriers, heard, lat, cpu, bytes) = &mut sides[(s + round) % 2];
                let before = (cpu_us(), heard.load(Ordering::Relaxed));
                for _ in 0..WRITES {
                    let i = k % m;
                    k += 1;
                    let at = format!("[{}, {}]", 29.0 + (k % 1000) as f64 * 1e-4, 41.0);
                    let lease = match *name {
                        "order" => format!(
                            "{{\"query\":\"set orders {{run_at: now() + $3, courier_loc: $4}} where id = $1 and claim = $2\",\
                             \"params\":[{}, \"c{i}\", 45000, {at}]}}",
                            i + 1
                        ),
                        _ => format!(
                            "{{\"query\":\"set orders {{run_at: now() + $3}} where id = $1 and claim = $2\",\
                             \"params\":[{}, \"c{i}\", 45000]}}",
                            i + 1
                        ),
                    };
                    let mut position = String::from("{\"query\":");
                    fenec_core::json::escape_into(&mut position, BEAT_POSITION);
                    position.push_str(&format!(
                        ",\"params\":[\"k{i}\", {at}, \"dropoff\", 90, {}, \"bike\"]}}",
                        i + 1
                    ));
                    let body = format!("{position}\n{lease}\n");
                    let t = Instant::now();
                    let status = couriers[i].batch(body.as_bytes());
                    lat.push(t.elapsed().as_secs_f64() * 1e6);
                    assert_eq!(status, 200, "{name}");
                }
                // The streams' looks after the last beat, before the next
                // side's turn counts them.
                std::thread::sleep(Duration::from_millis(20));
                cpu.push((cpu_us() - before.0) / WRITES as f64);
                bytes.push((heard.load(Ordering::Relaxed) - before.1) as f64 / WRITES as f64);
            }
        }
        println!("  M = {m}:");
        for (name, _, _, lat, cpu, bytes) in &mut sides {
            println!(
                "    {name:<9} {:>6.1} / {:>6.1} us   {:>6.1} us CPU   {:>6.0} bytes",
                pct(lat, 0.5),
                pct(lat, 0.99),
                pct(cpu, 0.5),
                pct(bytes, 0.5)
            );
        }
    }
}

fn main() {
    fenec_http::audit::set_delay(0);
    let args: Vec<String> = std::env::args().skip(1).collect();
    let counts = |default: &[usize]| -> Vec<usize> {
        let given: Vec<usize> = args[1..].iter().filter_map(|a| a.parse().ok()).collect();
        match given.is_empty() {
            true => default.to_vec(),
            false => given,
        }
    };
    match args.first().map(String::as_str) {
        Some("streams") => return streams(&counts(&[10, 100]), 20),
        Some("beats") => return beats(&counts(&[10, 50, 200]), 20),
        _ => {}
    }
    let rounds: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let only: Vec<String> = args
        .get(1)
        .map(|s| s.split(',').map(|n| n.trim().to_string()).collect())
        .unwrap_or_default();
    engine((rounds / 10).max(200), &only);
    http(rounds, &only);
    read_engine((rounds / 10).max(200), &only);
    read_http(rounds, &only);
}
