//! Named statements over HTTP, before anything runs them: every
//! definition, drop and call is refused in the words of its own (400),
//! for the server's token and a scoped one alike, and nothing of a
//! `/batch` holding one lands. `statements` lists none, and `describe
//! statement` finds none.

use fenec_core::named::NOT_BUILT;
use fenec_core::prelude::*;
use fenec_http::access::Access;
use fenec_http::{Config, Server};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, RwLock};
use std::time::Duration;

const SECRET: &[u8] = b"thirty-two bytes and a few more, for HS256";
const ROOT: &str = "root-token";
const POLICY: &str = "notes  read,write  where owner = $jwt.sub\n";

struct Node {
    port: u16,
    access: Arc<Access>,
}

fn start() -> Node {
    fenec_http::audit::set_delay(0);
    let access = Arc::new(Access::new(SECRET, POLICY).unwrap());
    let mut db = Database::new();
    db.execute(
        &fenec_ql::parse_one("create collection notes (owner text @hash, body text)").unwrap(),
    )
    .unwrap();
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        token: Some(ROOT.into()),
        access: Some(Arc::clone(&access)),
        ..Config::default()
    };
    let server = Server::new(Arc::new(RwLock::new(db)), cfg);
    let listener = server.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    Node { port, access }
}

impl Node {
    fn post(&self, token: &str, target: &str, body: &str) -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        write!(
            s,
            "POST {target} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        let body = out
            .split_once("\r\n\r\n")
            .map_or("", |(_, b)| b)
            .to_string();
        (out[9..12].parse().unwrap(), body)
    }

    fn query(&self, token: &str, sql: &str) -> (u16, String) {
        self.post(token, "/query", &line(sql))
    }

    fn notes(&self) -> String {
        let (status, body) = self.query(ROOT, "get notes count");
        assert_eq!(status, 200, "{body}");
        body
    }
}

fn line(sql: &str) -> String {
    let mut body = String::from("{\"query\":");
    fenec_core::json::escape_into(&mut body, sql);
    body.push('}');
    body
}

const NAMED: [&str; 7] = [
    "create statement mine() claims (sub text) as get notes select body where owner = $jwt.sub",
    "create or replace statement mine() claims (sub text) as begin get notes select body where \
     owner = $jwt.sub; end",
    "drop statement mine",
    "drop statement if exists mine",
    "call mine",
    "call mine {limit: 3} as {sub: 'alice'}",
    "CALL mine(3)",
];

#[test]
fn the_server_s_token_is_refused_every_definition_and_call() {
    let node = start();
    let before = node.notes();
    for text in NAMED {
        let (status, body) = node.query(ROOT, text);
        assert_eq!(status, 400, "{text}: {body}");
        assert!(body.contains(NOT_BUILT), "{text}: {body}");
    }
    // A `/batch` holding one is refused before its first statement runs.
    let batch = [
        line("put notes {owner: \"alice\", body: \"a\"}"),
        line("call mine"),
    ]
    .join("\n");
    let (status, body) = node.post(ROOT, "/batch", &batch);
    assert_eq!(status, 400, "{body}");
    assert!(
        body.contains(NOT_BUILT) && body.contains("\"at\":1"),
        "{body}"
    );
    assert_eq!(node.notes(), before);
    // A key is kept by no refused write: sent again, refused again.
    let keyed = |n: &Node| {
        let mut s = TcpStream::connect(("127.0.0.1", n.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let body = line("call mine");
        write!(
            s,
            "POST /query HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {ROOT}\r\n\
             Idempotency-Key: k1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    };
    for _ in 0..2 {
        let out = keyed(&node);
        assert!(out.starts_with("HTTP/1.1 400"), "{out}");
        assert!(out.contains(NOT_BUILT), "{out}");
    }
    // The reads answer for a database that holds none.
    let (status, body) = node.query(ROOT, "statements");
    assert_eq!((status, body.as_str()), (200, "[]"));
    let (status, body) = node.query(ROOT, "describe statement mine");
    assert_eq!(status, 404, "{body}");
    assert!(body.contains("statement `mine`"), "{body}");
    // A name after `$` outside a definition is refused where it stands.
    let (status, body) = node.query(ROOT, "get notes where owner = $owner");
    assert_eq!(status, 400, "{body}");
    assert!(
        body.contains("`$owner` names a named statement's parameter"),
        "{body}"
    );
}

#[test]
fn a_scoped_token_is_refused_them_too() {
    let node = start();
    let alice = node.access.mint(r#"{"sub":"alice"}"#).unwrap();
    // A call is refused as not built, before its rules are asked.
    for text in ["call mine", "call mine {limit: 3}", "CALL mine(3)"] {
        let (status, body) = node.query(&alice, text);
        assert_eq!(status, 400, "{text}: {body}");
        assert!(body.contains(NOT_BUILT), "{text}: {body}");
    }
    // A definition and a drop are schema changes, a scoped token's never.
    for text in &NAMED[..4] {
        let (status, body) = node.query(&alice, text);
        assert_eq!(status, 403, "{text}: {body}");
    }
    let (status, body) = node.query(&alice, "statements");
    assert_eq!((status, body.as_str()), (200, "[]"));
    let (status, _) = node.query(&alice, "describe statement mine");
    assert_eq!(status, 404);
    // Nor a `/batch` with one: its own write before it never lands.
    let batch = [
        line("put notes {owner: \"alice\", body: \"a\"}"),
        line("call mine"),
    ]
    .join("\n");
    let (status, body) = node.post(&alice, "/batch", &batch);
    assert_eq!(status, 400, "{body}");
    assert!(body.contains(NOT_BUILT), "{body}");
    let (_, body) = node.query(ROOT, "get notes count");
    assert!(body.contains("[0]") || body.contains(":0"), "{body}");
}
