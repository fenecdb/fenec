//! `require <n>` over HTTP: a write that misses its count is 412, a
//! `/batch` holding it is put back whole and says which statement stopped
//! it, and transfers sent as `/batch`es from eight clients at once neither
//! make nor lose money.

use fenec_core::prelude::*;
use fenec_http::{Config, Server};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, RwLock};
use std::time::Duration;

fn start() -> u16 {
    let mut db = Database::new();
    for sql in [
        "create collection accounts (name text @unique, balance int)",
        "create collection journal (tx int @hash, account text @hash, amount int)",
    ] {
        db.execute(&fenec_ql::parse_one(sql).unwrap()).unwrap();
    }
    for a in 0..ACCOUNTS {
        let sql = format!("put accounts {{name: \"a{a}\", balance: {START}}}");
        db.execute(&fenec_ql::parse_one(&sql).unwrap()).unwrap();
    }
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
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

const ACCOUNTS: i64 = 5;
const START: i64 = 100;

/// A keep-alive connection, as a client's pool keeps one.
struct Conn {
    r: BufReader<TcpStream>,
    w: TcpStream,
}

impl Conn {
    fn open(port: u16) -> Conn {
        let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        Conn {
            r: BufReader::new(s.try_clone().unwrap()),
            w: s,
        }
    }

    fn post(&mut self, path: &str, body: &str) -> (u16, String) {
        write!(
            self.w,
            "POST {path} HTTP/1.1\r\nHost: t\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut line = String::new();
        self.r.read_line(&mut line).unwrap();
        let status = line.split_whitespace().nth(1).unwrap().parse().unwrap();
        let mut len = 0;
        loop {
            let mut h = String::new();
            self.r.read_line(&mut h).unwrap();
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                if k.eq_ignore_ascii_case("content-length") {
                    len = v.trim().parse().unwrap();
                }
            }
        }
        let mut body = vec![0; len];
        self.r.read_exact(&mut body).unwrap();
        (status, String::from_utf8(body).unwrap())
    }

    fn query(&mut self, q: &str) -> (u16, String) {
        self.post("/query", &line(q, ""))
    }

    /// The one number in a one-row, one-column answer.
    fn number(&mut self, q: &str) -> i64 {
        let (status, body) = self.query(q);
        assert_eq!(status, 200, "{body}");
        let digits: String = body
            .rsplit(':')
            .next()
            .unwrap()
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '-')
            .collect();
        digits.parse().unwrap_or(0)
    }
}

/// One NDJSON line: a statement and its parameters.
fn line(q: &str, params: &str) -> String {
    let mut out = String::from("{\"query\":");
    fenec_core::json::escape_into(&mut out, q);
    if !params.is_empty() {
        out.push_str(",\"params\":");
        out.push_str(params);
    }
    out.push('}');
    out
}

/// A transfer as one `/batch`: the debit where the money is, the credit
/// where the account is, each required to write its row, and the journal.
fn transfer(from: &str, to: &str, amount: i64, tx: i64) -> String {
    let p = format!(r#"["{from}","{to}",{amount},{tx}]"#);
    [
        "set accounts {balance: balance - $3} where name = $1 and balance >= $3 require 1",
        "set accounts {balance: balance + $3} where name = $2 require 1",
        "insert journal [{tx: $4, account: $1, amount: 0 - $3}, {tx: $4, account: $2, amount: $3}]",
    ]
    .iter()
    .map(|q| line(q, &p))
    .collect::<Vec<_>>()
    .join("\n")
}

#[test]
fn a_write_that_misses_its_count_is_412_and_a_batch_names_where_it_stopped() {
    let port = start();
    let mut c = Conn::open(port);
    let (status, body) = c.query("set accounts {balance: 0} where name = \"nobody\" require 1");
    assert_eq!(status, 412, "{body}");
    assert_eq!(
        body,
        r#"{"error":"`set accounts` wrote 0 rows, and requires 1"}"#
    );
    // A credit to an account that is not there: the debit before it is put
    // back, and the answer names the credit.
    let (status, body) = c.post("/batch", &transfer("a0", "ghost", 10, 1));
    assert_eq!(status, 412, "{body}");
    assert_eq!(
        body,
        r#"{"error":"unmet: `set accounts` wrote 0 rows, and requires 1","completed":0,"at":1}"#
    );
    assert_eq!(
        c.number("get accounts select balance where name = \"a0\""),
        START
    );
    assert_eq!(c.number("get journal count"), 0);
    // Met, the batch lands.
    let (status, body) = c.post("/batch", &transfer("a0", "a1", 10, 2));
    assert_eq!(status, 200, "{body}");
    assert_eq!(c.number("get journal count"), 2);
}

/// `get ... require <n>` guards a read in a `/batch`: a balance that is no
/// longer what the client saw stops the batch at the read, 412 and `at`,
/// and the writes after it never land. On its own it is a 412 as well.
#[test]
fn a_get_that_misses_its_count_stops_the_batch_at_the_read() {
    let port = start();
    let mut c = Conn::open(port);
    let (status, body) = c.query("get accounts where name = \"nobody\" require 1");
    assert_eq!(status, 412, "{body}");
    assert_eq!(
        body,
        r#"{"error":"`get accounts` answered 0 rows, and requires 1"}"#
    );
    let (status, body) = c.query("get accounts limit 1 require 1");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"a0\""), "{body}");

    let guarded = |seen: i64| {
        [
            line(
                "set accounts {balance: balance + 1} where name = \"a1\" require 1",
                "",
            ),
            line(
                "get accounts select balance where name = \"a0\" and balance = $1 require 1",
                &format!("[{seen}]"),
            ),
            line("insert journal {tx: 9, account: \"a0\", amount: 1}", ""),
        ]
        .join("\n")
    };
    let (status, body) = c.post("/batch", &guarded(START - 1));
    assert_eq!(status, 412, "{body}");
    assert_eq!(
        body,
        r#"{"error":"unmet: `get accounts` answered 0 rows, and requires 1","completed":0,"at":1}"#
    );
    assert_eq!(
        c.number("get accounts select balance where name = \"a1\""),
        START
    );
    assert_eq!(c.number("get journal count"), 0);
    let (status, body) = c.post("/batch", &guarded(START));
    assert_eq!(status, 200, "{body}");
    assert_eq!(c.number("get journal count"), 1);
}

/// Eight clients sending transfers as `/batch`es at random among few
/// accounts, overdrafts and missing accounts common: the sum stays, no
/// balance goes below zero, and the journal accounts for every balance.
#[test]
fn concurrent_batches_neither_make_nor_lose_money() {
    const CLIENTS: u64 = 8;
    const EACH: u64 = 300;
    let port = start();
    let clients: Vec<_> = (0..CLIENTS)
        .map(|t| {
            std::thread::spawn(move || {
                let mut c = Conn::open(port);
                let mut seed = 0x2545_f491_4f6c_dd1du64 ^ (t + 1);
                let mut next = |n: u64| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    seed % n
                };
                let (mut landed, mut refused) = (0u64, 0u64);
                for i in 0..EACH {
                    let from = format!("a{}", next(ACCOUNTS as u64));
                    let to = match next(8) {
                        0 => "ghost".to_string(),
                        _ => format!("a{}", next(ACCOUNTS as u64)),
                    };
                    let amount = 1 + next(60) as i64;
                    let batch = transfer(&from, &to, amount, (t * EACH + i) as i64);
                    match c.post("/batch", &batch) {
                        (200, _) => landed += 1,
                        (412, _) => refused += 1,
                        (status, body) => panic!("{status} {body}"),
                    }
                }
                (landed, refused)
            })
        })
        .collect();
    let (mut landed, mut refused) = (0, 0);
    for h in clients {
        let (l, r) = h.join().unwrap();
        landed += l;
        refused += r;
    }
    assert!(
        landed > 0 && refused > 0,
        "{landed} landed, {refused} refused"
    );
    let mut c = Conn::open(port);
    assert_eq!(
        c.number("get accounts select sum(balance)"),
        ACCOUNTS * START
    );
    assert_eq!(c.number("get accounts where balance < 0 count"), 0);
    assert_eq!(c.number("get journal count") as u64, 2 * landed);
    for a in 0..ACCOUNTS {
        let balance = c.number(&format!(
            "get accounts select balance where name = \"a{a}\""
        ));
        let moved = c.number(&format!(
            "get journal select sum(amount) where account = \"a{a}\""
        ));
        assert_eq!(balance, START + moved, "a{a}");
    }
}
