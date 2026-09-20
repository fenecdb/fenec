//! The subscription endpoint (`GET /<name>/changes`) and the batch endpoint
//! (`POST /batch`).
//!
//! The client is raw TCP: the assertions are on the real SSE byte stream,
//! not on a library's leniency.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use fenec_core::prelude::*;
use fenec_http::{Config, Server};

fn seeded() -> Database {
    let mut db = Database::new();
    for sql in [
        "create collection tasks (key text @hash, title text, status text @hash)",
        r#"put tasks [
             {key: "a", title: "one",   status: "open"},
             {key: "b", title: "two",   status: "open"},
             {key: "c", title: "three", status: "closed"}
           ]"#,
    ] {
        for stmt in fenec_ql::parse(sql).expect("parse") {
            db.execute(&stmt).expect("execute");
        }
    }
    db
}

struct Harness {
    port: u16,
    db: Arc<RwLock<Database>>,
}

fn start(mut cfg: Config) -> Harness {
    cfg.addr = "127.0.0.1:0".into();
    // We do not want to wait for a keep-alive line in the tests.
    cfg.stream_keepalive = Duration::from_millis(80);
    let db = Arc::new(RwLock::new(seeded()));
    let server = Server::new(Arc::clone(&db), cfg);
    let listener = server.bind().expect("bind");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    Harness { port, db }
}

// ------------------------------------------------------------------- SSE

struct Sub {
    sock: TcpStream,
    buf: String,
    status: u16,
    head: String,
}

impl Sub {
    fn open(port: u16, target: &str) -> Sub {
        Sub::open_with(port, target, &[])
    }

    fn open_with(port: u16, target: &str, headers: &[(&str, &str)]) -> Sub {
        let mut sock = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        sock.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        let mut req = format!("GET {target} HTTP/1.1\r\nHost: t\r\nAccept: text/event-stream\r\n");
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        sock.write_all(req.as_bytes()).unwrap();

        let mut s = Sub {
            sock,
            buf: String::new(),
            status: 0,
            head: String::new(),
        };
        let head = s
            .read_until("\r\n\r\n", Duration::from_secs(3))
            .expect("header");
        s.status = head
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        s.head = head;
        s
    }

    fn head_line(&self) -> &str {
        &self.head
    }

    fn read_until(&mut self, sep: &str, budget: Duration) -> Option<String> {
        let deadline = Instant::now() + budget;
        loop {
            if let Some(i) = self.buf.find(sep) {
                let out = self.buf[..i].to_string();
                self.buf = self.buf[i + sep.len()..].to_string();
                return Some(out);
            }
            if Instant::now() > deadline {
                return None;
            }
            let mut chunk = [0u8; 4096];
            match self.sock.read(&mut chunk) {
                Ok(0) => return None,
                Ok(n) => self.buf.push_str(&String::from_utf8_lossy(&chunk[..n])),
                Err(_) => continue, // read timeout: keep trying until the budget runs out
            }
        }
    }

    /// The next event; keep-alive comments are skipped.
    fn next(&mut self) -> Option<(String, String)> {
        self.next_within(Duration::from_secs(3))
    }

    fn next_within(&mut self, budget: Duration) -> Option<(String, String)> {
        let deadline = Instant::now() + budget;
        loop {
            let block = self.read_until("\n\n", deadline.saturating_duration_since(Instant::now()))?;
            let block = block.trim_start_matches(['\r', '\n']);
            if block.starts_with(':') {
                continue; // keep-alive
            }
            let mut name = String::new();
            let mut data = String::new();
            for line in block.lines() {
                if let Some(v) = line.strip_prefix("event: ") {
                    name = v.trim().to_string();
                } else if let Some(v) = line.strip_prefix("data: ") {
                    data = v.trim().to_string();
                }
            }
            if !name.is_empty() {
                return Some((name, data));
            }
        }
    }
}

fn post(port: u16, target: &str, body: &str) -> (u16, String) {
    call(port, "POST", target, Some(body), &[])
}

fn call(
    port: u16,
    method: &str,
    target: &str,
    body: Option<&str>,
    headers: &[(&str, &str)],
) -> (u16, String) {
    let mut sock = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut req = format!("{method} {target} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    match body {
        Some(b) => req.push_str(&format!("Content-Length: {}\r\n\r\n{b}", b.len())),
        None => req.push_str("\r\n"),
    }
    sock.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = sock.read_to_string(&mut out);
    let (head, tail) = out.split_once("\r\n\r\n").unwrap_or((out.as_str(), ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, tail.to_string())
}

/// Number of elements of a JSON array field inside `data`. It counts the
/// top-level commas; nested objects, arrays and strings are skipped.
fn count(data: &str, key: &str) -> usize {
    let pat = format!("\"{key}\":[");
    let at = data
        .find(&pat)
        .unwrap_or_else(|| panic!("`{key}` missing: {data}"));
    // *After* the opening bracket: is the array empty, and if not how many items.
    let body = &data[at + pat.len()..];
    let mut depth = 0i32;
    let mut commas = 0usize;
    let mut any = false;
    let mut in_str = false;
    let mut esc = false;
    for c in body.chars() {
        if esc {
            esc = false;
            continue;
        }
        if in_str {
            match c {
                '\\' => esc = true,
                '"' => in_str = false,
                _ => {}
            }
            any = true;
            continue;
        }
        match c {
            ']' | '}' if depth == 0 => break,
            '"' => {
                in_str = true;
                any = true;
            }
            '[' | '{' => {
                depth += 1;
                any = true;
            }
            ']' | '}' => depth -= 1,
            ',' if depth == 0 => commas += 1,
            c if !c.is_whitespace() => any = true,
            _ => {}
        }
    }
    if any {
        commas + 1
    } else {
        0
    }
}

// ------------------------------------------------------------------ tests

#[test]
fn seed_carries_the_whole_shape() {
    let h = start(Config::default());
    let mut s = Sub::open(h.port, "/tasks/changes");
    assert_eq!(s.status, 200);
    assert!(s.head_line().contains("text/event-stream"), "{}", s.head_line());
    let (name, data) = s.next().expect("event");
    assert_eq!(name, "seed");
    assert_eq!(count(&data, "rows"), 3);
}

#[test]
fn shape_filter_narrows_the_seed() {
    let h = start(Config::default());
    let mut s = Sub::open(h.port, "/tasks/changes?status=eq.open");
    let (name, data) = s.next().expect("event");
    assert_eq!(name, "seed");
    assert_eq!(count(&data, "rows"), 2);
}

#[test]
fn a_write_arrives_as_a_change() {
    let h = start(Config::default());
    let mut s = Sub::open(h.port, "/tasks/changes");
    assert_eq!(s.next().expect("seed").0, "seed");

    let (st, _) = post(
        h.port,
        "/tasks",
        r#"{"key":"d","title":"four","status":"open"}"#,
    );
    assert_eq!(st, 201);

    let (name, data) = s.next().expect("change");
    assert_eq!(name, "change");
    assert_eq!(count(&data, "puts"), 1);
    assert!(data.contains("\"dels\":[]"), "{data}");
    assert!(data.contains("four"), "{data}");
}

#[test]
fn leaving_the_shape_arrives_as_a_delete() {
    let h = start(Config::default());
    let mut s = Sub::open(h.port, "/tasks/changes?status=eq.open");
    assert_eq!(s.next().expect("seed").0, "seed");

    let (st, _) = call(
        h.port,
        "PATCH",
        "/tasks?key=eq.a",
        Some(r#"{"status":"closed"}"#),
        &[],
    );
    assert_eq!(st, 200);

    let (name, data) = s.next().expect("change");
    assert_eq!(name, "change");
    assert_eq!(count(&data, "puts"), 0, "{data}");
    assert!(!data.contains("\"dels\":[]"), "leaving the shape must be a delete: {data}");
}

#[test]
fn a_write_outside_the_shape_is_not_pushed() {
    let h = start(Config::default());
    let mut s = Sub::open(h.port, "/tasks/changes?status=eq.closed");
    assert_eq!(s.next().expect("seed").0, "seed");

    // A write to another collection: the subscriber wakes but no empty batch is sent.
    let (st, _) = post(
        h.port,
        "/query",
        r#"{"query":"create collection notes (text text)"}"#,
    );
    assert_eq!(st, 200);
    // Only a schema event should arrive (another collection -> nothing).
    assert!(
        s.next_within(Duration::from_millis(500)).is_none(),
        "an unrelated write must not reach the subscriber"
    );
}

#[test]
fn resume_from_cursor_skips_the_seed() {
    let h = start(Config::default());
    let cursor = h.db.read().unwrap().change_seq();
    let mut s = Sub::open(h.port, &format!("/tasks/changes?since={cursor}"));
    assert_eq!(s.status, 200);

    post(
        h.port,
        "/tasks",
        r#"{"key":"d","title":"four","status":"open"}"#,
    );
    let (name, data) = s.next().expect("event");
    assert_eq!(name, "change", "a resuming subscriber must not get a seed");
    assert_eq!(count(&data, "puts"), 1);
}

#[test]
fn a_stale_cursor_is_reseeded() {
    let mut cfg = Config::default();
    cfg.change_capacity = 2;
    let h = start(cfg);

    // Overflow the ring.
    for k in ["x", "y", "z"] {
        post(
            h.port,
            "/tasks",
            &format!(r#"{{"key":"{k}","title":"{k}","status":"open"}}"#),
        );
    }
    let mut s = Sub::open(h.port, "/tasks/changes?since=1");
    let (name, data) = s.next().expect("event");
    assert_eq!(name, "seed", "a stale cursor must be reseeded");
    assert_eq!(count(&data, "rows"), 6);
}

#[test]
fn window_clauses_are_rejected() {
    let h = start(Config::default());
    for target in [
        "/tasks/changes?limit=10",
        "/tasks/changes?order=title.asc",
        "/tasks/changes?offset=1",
        "/tasks/changes?count",
    ] {
        let s = Sub::open(h.port, target);
        assert_eq!(s.status, 400, "{target}");
    }
}

#[test]
fn unknown_collection_is_404() {
    let h = start(Config::default());
    let s = Sub::open(h.port, "/nosuch/changes");
    assert_eq!(s.status, 404);
}

#[test]
fn projection_is_honoured_and_keeps_id() {
    let h = start(Config::default());
    let mut s = Sub::open(h.port, "/tasks/changes?select=title");
    let (_, data) = s.next().expect("seed");
    assert!(data.contains("\"id\""), "the id must always be carried: {data}");
    assert!(!data.contains("\"status\""), "{data}");
}

#[test]
fn the_stream_needs_a_token_too() {
    let mut cfg = Config::default();
    cfg.token = Some("secret".into());
    let h = start(cfg);

    assert_eq!(Sub::open(h.port, "/tasks/changes").status, 401);
    let s = Sub::open_with(
        h.port,
        "/tasks/changes",
        &[("Authorization", "Bearer secret")],
    );
    assert_eq!(s.status, 200);
}

#[test]
fn stream_cap_is_separate_from_connection_cap() {
    let mut cfg = Config::default();
    cfg.max_streams = 1;
    let h = start(cfg);

    let mut first = Sub::open(h.port, "/tasks/changes");
    assert_eq!(first.next().expect("seed").0, "seed");
    let second = Sub::open(h.port, "/tasks/changes");
    assert_eq!(second.status, 503);

    // Ordinary requests are unaffected.
    let (st, _) = call(h.port, "GET", "/tasks", None, &[]);
    assert_eq!(st, 200);
}

// ------------------------------------------------------------------- batch

#[test]
fn batch_runs_every_statement() {
    let h = start(Config::default());
    let body = concat!(
        r#"{"query":"put tasks {key: $1, title: $2, status: $3}","params":["d","four","open"]}"#,
        "\n",
        r#"{"query":"set tasks {title: $1} where key = $2","params":["ONE","a"]}"#,
    );
    let (st, out) = post(h.port, "/batch", body);
    assert_eq!(st, 200, "{out}");
    assert!(out.contains("\"ok\":2"), "{out}");

    let (_, rows) = call(h.port, "GET", "/tasks?key=eq.a", None, &[]);
    assert!(rows.contains("ONE"), "{rows}");
}

#[test]
fn batch_stops_at_the_first_error_and_says_how_far_it_got() {
    let h = start(Config::default());
    let body = concat!(
        r#"{"query":"put tasks {key: $1, title: $2, status: $3}","params":["d","four","open"]}"#,
        "\n",
        r#"{"query":"put tasks {nofield: $1}","params":[1]}"#,
        "\n",
        r#"{"query":"put tasks {key: $1, title: $2, status: $3}","params":["e","five","open"]}"#,
    );
    let (st, out) = post(h.port, "/batch", body);
    assert_eq!(st, 404, "{out}");
    assert!(out.contains("\"completed\":1"), "{out}");

    // The first was applied and the third never ran: no rollback, reported
    // exactly as it happened.
    let (_, rows) = call(h.port, "GET", "/tasks?key=eq.d", None, &[]);
    assert!(rows.contains("four"), "{rows}");
    let (_, rows) = call(h.port, "GET", "/tasks?key=eq.e", None, &[]);
    assert_eq!(rows.trim(), "[]");
}

#[test]
fn batch_respects_read_only() {
    let mut cfg = Config::default();
    cfg.read_only = true;
    let h = start(cfg);
    let (st, _) = post(
        h.port,
        "/batch",
        r#"{"query":"put tasks {key: $1}","params":["z"]}"#,
    );
    assert_eq!(st, 403);
}

#[test]
fn empty_batch_is_rejected() {
    let h = start(Config::default());
    let (st, out) = post(h.port, "/batch", "\n\n");
    assert_eq!(st, 400, "{out}");
}

#[test]
fn batch_wakes_subscribers_once_per_statement() {
    let h = start(Config::default());
    let mut s = Sub::open(h.port, "/tasks/changes");
    assert_eq!(s.next().expect("seed").0, "seed");

    let body = concat!(
        r#"{"query":"put tasks {key: $1, title: $2, status: $3}","params":["d","four","open"]}"#,
        "\n",
        r#"{"query":"put tasks {key: $1, title: $2, status: $3}","params":["e","five","open"]}"#,
    );
    post(h.port, "/batch", body);

    // The two writes may arrive as a single batch or separately: seeing two
    // rows in total is enough.
    let mut seen = 0;
    while seen < 2 {
        let Some((name, data)) = s.next_within(Duration::from_secs(2)) else {
            break;
        };
        assert_eq!(name, "change");
        seen += count(&data, "puts");
    }
    assert_eq!(seen, 2);
}

#[test]
fn a_quiet_collection_keeps_its_cursor_fresh() {
    // The counter and the ring are shared across all collections. If the
    // subscriber of a quiet collection did not wake on other people's
    // writes, its cursor would stay put and it would be reseeded once the
    // ring overflowed. This test sets up exactly that scenario: a 4-entry
    // ring and 12 writes into a neighbouring collection.
    let mut cfg = Config::default();
    cfg.change_capacity = 4;
    let h = start(cfg);

    let mut s = Sub::open(h.port, "/tasks/changes");
    assert_eq!(s.next().expect("seed").0, "seed");

    post(
        h.port,
        "/query",
        r#"{"query":"create collection notes (text text)"}"#,
    );
    for i in 0..12 {
        post(h.port, "/notes", &format!(r#"{{"text":"n{i}"}}"#));
        // Let the subscriber wake and move its cursor.
        std::thread::sleep(Duration::from_millis(10));
    }

    post(
        h.port,
        "/tasks",
        r#"{"key":"z","title":"zed","status":"open"}"#,
    );
    let (name, data) = s.next().expect("change");
    assert_eq!(
        name, "change",
        "the cursor must stay current: there must be no reseed"
    );
    assert_eq!(count(&data, "puts"), 1);
    assert!(data.contains("zed"), "{data}");
}
