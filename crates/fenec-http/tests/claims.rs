//! A job queue over HTTP: workers claim with `set ... where run_at <=
//! now() order run_at limit 10 returning *`, one statement under the write
//! lock, through `/query` and through a `/batch` under an
//! `Idempotency-Key`, and ack with `del ... require 1`. Eight clients at
//! once never take the same job, every job is taken, and a claim sent again
//! under its key is answered the rows it took the first time, taking none.

use fenec_core::prelude::*;
use fenec_http::{Config, Server};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, RwLock};
use std::time::Duration;

const JOBS: i64 = 2_000;

const CLAIM: &str = "set jobs {owner: $1, run_at: now() + 60000, attempts: attempts + 1} \
                     where run_at <= now() order run_at limit 10 returning id, owner, attempts";
const ACK: &str = "del jobs where id = $1 and owner = $2 require 1";

fn start() -> u16 {
    let mut db = Database::new();
    db.execute(
        &fenec_ql::parse_one(
            "create collection jobs (run_at timestamp @sorted, owner text, attempts int, payload json)",
        )
        .unwrap(),
    )
    .unwrap();
    // Ready since 1970, the oldest first.
    for chunk in (1..=JOBS).collect::<Vec<_>>().chunks(500) {
        let docs: Vec<String> = chunk
            .iter()
            .map(|i| format!("{{run_at: {i}, attempts: 0, payload: {{\"n\": {i}}}}}"))
            .collect();
        let sql = format!("put jobs [{}]", docs.join(", "));
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

/// A keep-alive connection, as a worker's client keeps one.
struct Conn {
    r: BufReader<TcpStream>,
    w: TcpStream,
}

struct Answer {
    status: u16,
    replayed: bool,
    body: String,
}

impl Conn {
    fn open(port: u16) -> Conn {
        let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        Conn {
            r: BufReader::new(s.try_clone().unwrap()),
            w: s,
        }
    }

    fn post(&mut self, path: &str, body: &str, key: Option<&str>) -> Answer {
        let key = key.map_or(String::new(), |k| format!("Idempotency-Key: {k}\r\n"));
        write!(
            self.w,
            "POST {path} HTTP/1.1\r\nHost: t\r\n{key}Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut line = String::new();
        self.r.read_line(&mut line).unwrap();
        let status = line.split_whitespace().nth(1).unwrap().parse().unwrap();
        let (mut len, mut replayed) = (0, false);
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
                if k.eq_ignore_ascii_case("idempotent-replayed") {
                    replayed = v.trim() == "true";
                }
            }
        }
        let mut body = vec![0; len];
        self.r.read_exact(&mut body).unwrap();
        Answer {
            status,
            replayed,
            body: String::from_utf8(body).unwrap(),
        }
    }
}

/// One NDJSON line: a statement and its parameters, as JSON.
fn line(q: &str, params: &str) -> String {
    let mut out = String::from("{\"query\":");
    fenec_core::json::escape_into(&mut out, q);
    out.push_str(",\"params\":");
    out.push_str(params);
    out.push('}');
    out
}

/// The ids of a rows answer: the bare array `/query` answers, or the first
/// result of a `/batch`'s.
fn claimed(body: &str) -> Vec<i64> {
    /// An object's member `key`.
    fn member<'v>(v: &'v Value, key: &str) -> &'v Value {
        match v {
            Value::Object(m) => &m.iter().find(|(k, _)| k == key).unwrap().1,
            other => panic!("{other:?} is no object"),
        }
    }
    let v = fenec_core::json::parse_json(body).unwrap();
    let rows = match &v {
        Value::List(rows) => rows,
        batch => match member(batch, "results") {
            Value::List(results) => match member(&results[0], "rows") {
                Value::List(rows) => rows,
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        },
    };
    rows.iter()
        .map(|r| match member(r, "id") {
            Value::Int(id) => *id,
            other => panic!("{other:?}"),
        })
        .collect()
}

#[test]
fn eight_clients_claiming_over_http_never_take_the_same_job() {
    let port = start();
    let workers: Vec<_> = (0..8)
        .map(|w| {
            std::thread::spawn(move || {
                let mut c = Conn::open(port);
                let mut got = Vec::new();
                for n in 0.. {
                    let owner = format!("w{w}:{n}");
                    let params = format!("[\"{owner}\"]");
                    // Half the workers claim through `/query`, half through a
                    // `/batch` under a key, which every third time they send
                    // again: answered as the first time, and nothing taken.
                    let ids = if w % 2 == 0 {
                        let a = c.post("/query", &line(CLAIM, &params), None);
                        assert_eq!(a.status, 200, "{}", a.body);
                        claimed(&a.body)
                    } else {
                        let key = format!("claim-{owner}");
                        let a = c.post("/batch", &line(CLAIM, &params), Some(&key));
                        assert_eq!(a.status, 200, "{}", a.body);
                        assert!(!a.replayed);
                        if n % 3 == 0 {
                            let again = c.post("/batch", &line(CLAIM, &params), Some(&key));
                            assert_eq!(again.status, 200, "{}", again.body);
                            assert!(again.replayed, "{}", again.body);
                            assert_eq!(again.body, a.body);
                        }
                        claimed(&a.body)
                    };
                    if ids.is_empty() {
                        break;
                    }
                    for id in ids {
                        got.push(id);
                        let a = c.post("/query", &line(ACK, &format!("[{id},\"{owner}\"]")), None);
                        assert_eq!(a.status, 200, "{}", a.body);
                        assert_eq!(a.body, "{\"affected\":1}");
                    }
                }
                got
            })
        })
        .collect();
    let mut seen = HashSet::new();
    for w in workers {
        for id in w.join().unwrap() {
            assert!(seen.insert(id), "job {id} claimed twice");
        }
    }
    assert_eq!(seen.len() as i64, JOBS, "every job claimed and acked");
    let mut c = Conn::open(port);
    let a = c.post("/query", "{\"query\":\"get jobs count\"}", None);
    assert_eq!(a.body, "[{\"count\":0}]");
}

/// An ack by a worker whose job is not its own is 412, and a claim of one
/// or none (`limit 1 require 1`) over an empty queue is too.
#[test]
fn an_ack_by_another_worker_is_412() {
    let port = start();
    let mut c = Conn::open(port);
    let a = c.post("/query", &line(CLAIM, "[\"w1\"]"), None);
    let ids = claimed(&a.body);
    assert_eq!(ids.len(), 10);
    let a = c.post("/query", &line(ACK, &format!("[{},\"w2\"]", ids[0])), None);
    assert_eq!(a.status, 412, "{}", a.body);
    assert_eq!(
        a.body,
        r#"{"error":"`del jobs` wrote 0 rows, and requires 1"}"#
    );
    let a = c.post("/query", &line(ACK, &format!("[{},\"w1\"]", ids[0])), None);
    assert_eq!(a.status, 200, "{}", a.body);
    let one = "set jobs {owner: $1} where run_at <= now() and owner = \"nobody\" order run_at \
               limit 1 returning id require 1";
    let a = c.post("/query", &line(one, "[\"w3\"]"), None);
    assert_eq!(a.status, 412, "{}", a.body);
}
