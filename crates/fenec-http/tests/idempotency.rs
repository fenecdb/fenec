//! `Idempotency-Key`: a write sent again is answered as the first time and
//! not made again, on every write path, per user, and only for its request.

use fenec_core::prelude::*;
use fenec_http::access::Access;
use fenec_http::{Config, Server};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, RwLock};
use std::time::Duration;

const SECRET: &[u8] = b"thirty-two bytes and a few more, for HS256";
const ROOT: &str = "root-token";

struct Node {
    port: u16,
    access: Arc<Access>,
    db: Arc<RwLock<Database>>,
}

fn start(ttl: Duration) -> Node {
    start_over(ttl, &[])
}

fn start_over(ttl: Duration, setup: &[&str]) -> Node {
    let access =
        Arc::new(Access::new(SECRET, "notes  read,write  where owner = $jwt.sub\n").unwrap());
    let mut db = Database::new();
    for sql in [
        "create collection notes (owner text, t text)",
        "create collection logs (t text)",
    ]
    .iter()
    .chain(setup)
    {
        db.execute(&fenec_ql::parse_one(sql).unwrap()).unwrap();
    }
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        token: Some(ROOT.into()),
        access: Some(Arc::clone(&access)),
        idempotency_ttl: ttl,
        ..Config::default()
    };
    let db = Arc::new(RwLock::new(db));
    let server = Server::new(Arc::clone(&db), cfg);
    let listener = server.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    Node { port, access, db }
}

struct Answer {
    status: u16,
    replayed: bool,
    body: String,
}

impl Node {
    fn call(
        &self,
        token: &str,
        key: Option<&str>,
        method: &str,
        target: &str,
        body: &str,
    ) -> Answer {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let key = key.map_or(String::new(), |k| format!("Idempotency-Key: {k}\r\n"));
        write!(
            s,
            "{method} {target} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\n{key}\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        let (head, body) = out.split_once("\r\n\r\n").unwrap();
        Answer {
            status: head[9..12].parse().unwrap(),
            replayed: head.contains("Idempotent-Replayed: true"),
            body: body.to_string(),
        }
    }

    fn query(&self, token: &str, key: Option<&str>, sql: &str) -> Answer {
        let mut body = String::from("{\"query\":");
        fenec_core::json::escape_into(&mut body, sql);
        body.push('}');
        self.call(token, key, "POST", "/query", &body)
    }

    fn count(&self, collection: &str) -> String {
        self.query(ROOT, None, &format!("get {collection} count"))
            .body
    }
}

#[test]
fn a_write_sent_again_is_answered_as_before_and_not_made_again() {
    let n = start(Duration::from_secs(3600));
    // /query: a put with no id makes a row each time it runs.
    let first = n.query(ROOT, Some("k1"), "put logs {t: \"once\"}");
    assert_eq!(
        (first.status, first.replayed),
        (200, false),
        "{}",
        first.body
    );
    let again = n.query(ROOT, Some("k1"), "put logs {t: \"once\"}");
    assert_eq!(
        (again.status, again.replayed, again.body.as_str()),
        (200, true, first.body.as_str())
    );
    assert_eq!(n.count("logs"), "[{\"count\":1}]");
    // Another key is another write.
    n.query(ROOT, Some("k2"), "put logs {t: \"once\"}");
    assert_eq!(n.count("logs"), "[{\"count\":2}]");
    // REST and /batch as well.
    for _ in 0..2 {
        assert_eq!(
            n.call(ROOT, Some("k3"), "POST", "/logs", "{\"t\": \"rest\"}")
                .status,
            201
        );
    }
    assert_eq!(n.count("logs"), "[{\"count\":3}]");
    let batch =
        "{\"query\":\"put logs {t: \\\"b1\\\"}\"}\n{\"query\":\"put logs {t: \\\"b2\\\"}\"}";
    let b1 = n.call(ROOT, Some("k4"), "POST", "/batch", batch);
    let b2 = n.call(ROOT, Some("k4"), "POST", "/batch", batch);
    assert_eq!(
        (b1.status, b2.body == b1.body, b2.replayed),
        (200, true, true),
        "{}",
        b1.body
    );
    assert_eq!(n.count("logs"), "[{\"count\":5}]");
}

#[test]
fn a_key_is_for_one_request_one_user_and_a_write_that_landed() {
    let n = start(Duration::from_secs(3600));
    n.query(ROOT, Some("k"), "put logs {t: \"a\"}");
    // The key with another request: refused, nothing made.
    assert_eq!(n.query(ROOT, Some("k"), "put logs {t: \"b\"}").status, 422);
    assert_eq!(n.count("logs"), "[{\"count\":1}]");
    // A failed write keeps no key: sent again, it runs again.
    assert_eq!(
        n.query(ROOT, Some("bad"), "put nowhere {t: \"x\"}").status,
        404
    );
    n.query(ROOT, None, "create collection nowhere (t text)");
    assert_eq!(
        n.query(ROOT, Some("bad"), "put nowhere {t: \"x\"}").status,
        200
    );
    // Two users' keys never meet: the same key is each one's own.
    let alice = n.access.mint("{\"sub\":\"alice\"}").unwrap();
    let bob = n.access.mint("{\"sub\":\"bob\"}").unwrap();
    let a = n.query(
        &alice,
        Some("same"),
        "put notes {owner: \"alice\", t: \"hers\"}",
    );
    let b = n.query(&bob, Some("same"), "put notes {owner: \"bob\", t: \"his\"}");
    assert_eq!(
        (a.status, b.status, b.replayed),
        (200, 200, false),
        "{} {}",
        a.body,
        b.body
    );
    assert_eq!(n.count("notes"), "[{\"count\":2}]");
    // A scoped token with no subject has no one to keep a key for.
    let nobody = n.access.mint("{\"role\":\"x\"}").unwrap();
    assert_eq!(
        n.query(&nobody, Some("k"), "put notes {owner: \"x\", t: \"y\"}")
            .status,
        400
    );
    // Keys are the server's own: a scoped user reads none of them, and is
    // not told they are there.
    let peek = n.query(&alice, None, "get _idempotency").status;
    assert!(matches!(peek, 403 | 404), "{peek}");
}

#[test]
fn a_key_past_its_time_is_a_new_request() {
    let n = start(Duration::from_millis(200));
    n.query(ROOT, Some("k"), "put logs {t: \"a\"}");
    std::thread::sleep(Duration::from_millis(400));
    let again = n.query(ROOT, Some("k"), "put logs {t: \"a\"}");
    assert_eq!((again.status, again.replayed), (200, false));
    assert_eq!(n.count("logs"), "[{\"count\":2}]");
}

/// The keys are `@ttl`: one past its time is out of every read at once and
/// the sweeper deletes it, where the write path once purged them itself.
#[test]
fn the_sweeper_deletes_the_keys_past_their_time() {
    let n = start(Duration::from_millis(200));
    n.query(ROOT, Some("k1"), "put logs {t: \"a\"}");
    n.query(ROOT, Some("k2"), "put logs {t: \"b\"}");
    let keys = |n: &Node| {
        let mut db = n.db.write().unwrap();
        // Every row the file holds, past its time or not.
        db.set_clock(Some(0));
        let r = db.query(&fenec_ql::parse_one("get _idempotency count").unwrap(), &[]);
        db.set_clock(None);
        r.unwrap().rows().unwrap().rows[0].values[0].clone()
    };
    assert_eq!(keys(&n), Value::Int(2));
    assert_eq!(
        fenec_http::sweep::pass("test", &n.db),
        0,
        "none past its time yet"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(fenec_http::sweep::pass("test", &n.db), 2);
    assert_eq!(keys(&n), Value::Int(0));
    let schema =
        n.db.read()
            .unwrap()
            .collection("_idempotency")
            .unwrap()
            .schema
            .clone();
    assert_eq!(schema.field("at").unwrap().index.ttl(), Some(200));
}

/// Keys kept before the expiry -- `at int @sorted`, purged by the write
/// path -- are made again as the expiry keeps them, and a server started
/// with another `--idempotency-ttl` moves it.
#[test]
fn the_keys_collection_follows_the_ttl_it_is_started_with() {
    let n = start_over(
        Duration::from_secs(60),
        &[
            "create collection _idempotency (key text @hash, request int, status int, \
             body text, at int @sorted)",
            "put _idempotency {key: \"x\", request: 1, status: 200, body: \"{}\", at: 1}",
        ],
    );
    let ttl = |n: &Node| {
        let db = n.db.read().unwrap();
        let f = db.collection("_idempotency").unwrap().schema.clone();
        let at = f.field("at").unwrap();
        (at.ty.clone(), at.index.ttl())
    };
    n.query(ROOT, Some("k"), "put logs {t: \"a\"}");
    assert_eq!(ttl(&n), (DataType::Timestamp, Some(60_000)));
    let again = n.query(ROOT, Some("k"), "put logs {t: \"a\"}");
    assert!(again.replayed);
    let db = Arc::clone(&n.db);
    let moved = Node {
        port: {
            let cfg = Config {
                addr: "127.0.0.1:0".into(),
                token: Some(ROOT.into()),
                idempotency_ttl: Duration::from_secs(3600),
                ..Config::default()
            };
            let server = Server::new(Arc::clone(&db), cfg);
            let listener = server.bind().unwrap();
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                let _ = server.serve_on(listener);
            });
            port
        },
        access: Arc::clone(&n.access),
        db,
    };
    let kept = moved.query(ROOT, Some("k"), "put logs {t: \"a\"}");
    assert!(kept.replayed, "a key kept is kept across the move");
    moved.query(ROOT, Some("k2"), "put logs {t: \"b\"}");
    assert_eq!(ttl(&moved), (DataType::Timestamp, Some(3_600_000)));
}
