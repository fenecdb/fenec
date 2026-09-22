//! Scoped access: two users of one collection, each with a JSON Web Token,
//! may not see each other's rows -- not through any route, not through raw
//! FenecQL, and not as changed ids on a subscription -- nor write outside
//! their rules.

use fenec_core::prelude::*;
use fenec_http::access::Access;
use fenec_http::{Config, Server};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const SECRET: &[u8] = b"thirty-two bytes and a few more, for HS256";
const ROOT: &str = "root-token";
const POLICY: &str = "\
notes  read,write  where owner = $jwt.sub
board  read
board  write                                  for moderator
";

struct Node {
    port: u16,
    access: Arc<Access>,
}

fn start() -> Node {
    let access = Arc::new(Access::new(SECRET, POLICY).unwrap());
    let mut db = Database::new();
    for sql in [
        "create collection notes (owner text @hash, title text)",
        "create collection board (msg text)",
        "create collection secrets (x int)",
        "put secrets {x: 42}",
    ] {
        db.execute(&fenec_ql::parse_one(sql).unwrap()).unwrap();
    }
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
    fn token(&self, claims: &str) -> String {
        self.access.mint(claims).unwrap()
    }

    fn call(&self, token: Option<&str>, method: &str, target: &str, body: &str) -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
        write!(
            s,
            "{method} {target} HTTP/1.1\r\nHost: x\r\n{auth}Content-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
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
        let mut body = String::from("{\"query\":");
        fenec_core::json::escape_into(&mut body, sql);
        body.push('}');
        self.call(Some(token), "POST", "/query", &body)
    }
}

/// Reads the stream into `heard` until `until` holds or `budget` passes.
fn listen(s: &mut TcpStream, heard: &mut String, until: &dyn Fn(&str) -> bool, budget: Duration) {
    let deadline = Instant::now() + budget;
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline && !until(heard) {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(k) => heard.push_str(&String::from_utf8_lossy(&buf[..k])),
            Err(_) => {}
        }
    }
}

/// How many times `needle` appears in `hay`.
fn count(hay: &str, needle: &str) -> usize {
    hay.matches(needle).count()
}

#[test]
fn two_users_see_and_change_only_their_own_rows() {
    let n = start();
    let (alice, bob) = (n.token(r#"{"sub":"alice"}"#), n.token(r#"{"sub":"bob"}"#));
    // A put that leaves `owner` out gets its token's subject.
    assert_eq!(
        n.call(Some(&alice), "POST", "/notes", r#"{"title":"a1"}"#)
            .0,
        201
    );
    assert_eq!(
        n.call(Some(&alice), "POST", "/notes", r#"{"title":"a2"}"#)
            .0,
        201
    );
    assert_eq!(
        n.call(Some(&bob), "POST", "/notes", r#"{"title":"b1"}"#).0,
        201
    );

    let (status, rows) = n.call(Some(&alice), "GET", "/notes", "");
    assert_eq!(status, 200);
    assert_eq!(
        (count(&rows, "\"alice\""), count(&rows, "\"bob\"")),
        (2, 0),
        "{rows}"
    );
    let (_, rows) = n.call(Some(&bob), "GET", "/notes", "");
    assert_eq!(
        (count(&rows, "\"alice\""), count(&rows, "\"bob\"")),
        (0, 1),
        "{rows}"
    );
    let (_, rows) = n.call(Some(ROOT), "GET", "/notes", "");
    assert_eq!(
        (count(&rows, "\"alice\""), count(&rows, "\"bob\"")),
        (2, 1),
        "{rows}"
    );

    // Raw FenecQL is held to the same rows, whatever it asks for.
    let (_, body) = n.query(&alice, "get notes count");
    assert!(body.contains("\"count\":2"), "{body}");
    let (_, body) = n.query(&alice, r#"get notes where owner = "bob""#);
    assert_eq!(count(&body, "\"bob\""), 0, "{body}");
    let (_, body) = n.query(&alice, r#"explain get notes where owner = "bob""#);
    assert!(!body.is_empty());

    // A set or del reaches only the token's own rows.
    let (status, body) = n.call(Some(&alice), "PATCH", "/notes/all", r#"{"title":"mine"}"#);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"updated\":2"), "{body}");
    let (_, rows) = n.call(Some(ROOT), "GET", "/notes?owner=eq.bob", "");
    assert!(rows.contains("\"b1\""), "{rows}");
    let (_, body) = n.query(&bob, "del notes");
    assert!(body.contains("\"affected\":1"), "{body}");
    let (_, rows) = n.call(Some(ROOT), "GET", "/notes", "");
    assert_eq!(
        (count(&rows, "\"alice\""), count(&rows, "\"bob\"")),
        (2, 0),
        "{rows}"
    );
}

#[test]
fn a_write_outside_the_rules_is_refused() {
    let n = start();
    let alice = n.token(r#"{"sub":"alice"}"#);
    let bob = n.token(r#"{"sub":"bob"}"#);
    assert_eq!(
        n.call(Some(&bob), "POST", "/notes", r#"{"title":"b1"}"#).0,
        201
    );
    assert_eq!(
        n.call(Some(&alice), "POST", "/notes", r#"{"title":"a1"}"#)
            .0,
        201
    );

    // Writing someone else's row: as its owner, over its id, or by moving
    // one's own over.
    let (status, body) = n.call(
        Some(&alice),
        "POST",
        "/notes",
        r#"{"owner":"bob","title":"spoof"}"#,
    );
    assert_eq!(status, 403, "{body}");
    assert_eq!(
        n.query(&alice, r#"put notes {id: 1, title: "mine now"}"#).0,
        403
    );
    assert_eq!(n.query(&alice, r#"set notes {owner: "bob"}"#).0, 403);
    let (_, rows) = n.call(Some(ROOT), "GET", "/notes?owner=eq.bob", "");
    assert_eq!(count(&rows, "\"title\""), 1, "{rows}");
    assert!(
        !rows.contains("spoof") && !rows.contains("mine now"),
        "{rows}"
    );

    // No schema, no maintenance.
    for sql in [
        "create collection mine (x int)",
        "drop collection notes",
        "create index on notes (title) @sorted",
        "compact",
    ] {
        assert_eq!(n.query(&alice, sql).0, 403, "{sql}");
    }

    // Read but not write, for all but one role.
    assert_eq!(n.call(Some(&alice), "GET", "/board", "").0, 200);
    assert_eq!(
        n.call(Some(&alice), "POST", "/board", r#"{"msg":"hi"}"#).0,
        403
    );
    let moderator = n.token(r#"{"sub":"m","role":"moderator"}"#);
    assert_eq!(
        n.call(Some(&moderator), "POST", "/board", r#"{"msg":"hi"}"#)
            .0,
        201
    );

    // A collection no rule lets it read does not exist for the token.
    assert_eq!(n.call(Some(&alice), "GET", "/secrets", "").0, 404);
    assert_eq!(n.query(&alice, "get secrets").0, 404);
    assert_eq!(n.query(&alice, "get notes lookup secrets on x = id").0, 404);
    let (_, list) = n.call(Some(&alice), "GET", "/collections", "");
    assert!(
        list.contains("notes") && !list.contains("secrets"),
        "{list}"
    );
    let (_, list) = n.query(&alice, "collections");
    assert!(!list.contains("secrets"), "{list}");
}

#[test]
fn tokens_that_do_not_verify_are_refused() {
    let n = start();
    let expired = n.token(r#"{"sub":"alice","exp":1000}"#);
    let (status, body) = n.call(Some(&expired), "GET", "/notes", "");
    assert_eq!(status, 401);
    assert!(body.contains("expired"), "{body}");
    let other = Access::new(&[9u8; 40], POLICY).unwrap();
    let forged = other.mint(r#"{"sub":"alice"}"#).unwrap();
    assert_eq!(n.call(Some(&forged), "GET", "/notes", "").0, 401);
    assert_eq!(n.call(None, "GET", "/notes", "").0, 401);
    assert_eq!(n.call(Some("guess"), "GET", "/notes", "").0, 401);
    // The server's own token is everything.
    assert_eq!(n.call(Some(ROOT), "GET", "/secrets", "").0, 200);
}

/// A subscription to the notes, as alice: the rows other users write are
/// neither sent nor mentioned -- not even as deletions.
#[test]
fn a_subscription_hears_nothing_of_other_users_rows() {
    let n = start();
    let alice = n.token(r#"{"sub":"alice"}"#);
    let bob = n.token(r#"{"sub":"bob"}"#);
    assert_eq!(
        n.call(Some(&alice), "POST", "/notes", r#"{"title":"a1"}"#)
            .0,
        201
    );

    let mut s = TcpStream::connect(("127.0.0.1", n.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    write!(
        s,
        "GET /notes/changes HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {alice}\r\n\r\n"
    )
    .unwrap();
    let mut heard = String::new();
    listen(
        &mut s,
        &mut heard,
        &|h| h.contains("event: seed"),
        Duration::from_secs(3),
    );
    assert!(heard.contains("\"a1\""), "{heard}");

    // Bob writes three notes and deletes one; then alice writes hers.
    for t in ["b1", "b2", "b3"] {
        let body = format!(r#"{{"title":"{t}"}}"#);
        assert_eq!(n.call(Some(&bob), "POST", "/notes", &body).0, 201);
    }
    assert_eq!(n.query(&bob, r#"del notes where title = "b2""#).0, 200);
    assert_eq!(
        n.call(Some(&alice), "POST", "/notes", r#"{"title":"a2"}"#)
            .0,
        201
    );
    listen(
        &mut s,
        &mut heard,
        &|h| h.contains("\"a2\""),
        Duration::from_secs(3),
    );
    // A moment more, for anything that would have followed.
    listen(&mut s, &mut heard, &|_| false, Duration::from_millis(300));

    assert!(heard.contains("\"a2\""), "{heard}");
    // Bob's rows had ids 2, 3 and 4: no event names them, as a row or as a
    // deletion.
    let events: Vec<&str> = heard.split("event: ").skip(1).collect();
    for e in &events {
        assert!(!e.contains("\"bob\"") && !e.contains("\"b2\""), "{e}");
        if let Some(dels) = e.split("\"dels\":[").nth(1) {
            let dels = dels.split(']').next().unwrap();
            assert!(dels.is_empty(), "a deletion of someone else's row: {e}");
        }
    }
}
