//! `GET /_changes`: the writes on disk, one JSON object a line, from a
//! server in this process over a file of its own and real sockets.

use fenec_http::access::Access;
use fenec_http::replication::{self, Replication};
use fenec_http::{Config, Server};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const SECRET: &[u8] = b"thirty-two bytes and a few more, for HS256";
const ROOT: &str = "root-token";

fn file(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fenec-cdc-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.join("data.fenec")
}

struct Node {
    port: u16,
    access: Arc<Access>,
}

/// A server keeping `buffer` bytes of writes for `/_changes` alone, as
/// `fenec-pg --cdc` starts one, or none with `buffer` 0.
fn start(name: &str, buffer: usize) -> Node {
    let path = file(name);
    let access = Arc::new(Access::new(SECRET, "notes  read  where owner = $jwt.sub\n").unwrap());
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        // A write is on disk before it is answered: the feed hands over
        // only what an fsync has covered.
        sync_on_write: true,
        token: Some(ROOT.into()),
        access: Some(Arc::clone(&access)),
        ..Config::default()
    };
    let server = match buffer {
        0 => {
            let db = fenec_core::fs::open(path.to_str().unwrap()).unwrap();
            Server::new(Arc::new(RwLock::new(db)), cfg)
        }
        _ => {
            let (db, feed) = replication::open(path.to_str().unwrap(), buffer).unwrap();
            Server::new(Arc::new(RwLock::new(db)), cfg).with_replication(Replication::new(
                None,
                Some(feed),
                None,
            ))
        }
    };
    let listener = server.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    Node { port, access }
}

struct Answer {
    status: u16,
    next: Option<u64>,
    body: String,
}

impl Node {
    fn call(&self, token: Option<&str>, method: &str, target: &str, body: &str) -> Answer {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
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
        let (head, body) = out.split_once("\r\n\r\n").unwrap();
        let next = head
            .lines()
            .find_map(|l| l.strip_prefix("Fenec-Next: "))
            .map(|v| v.trim().parse().unwrap());
        Answer {
            status: head[9..12].parse().unwrap(),
            next,
            body: body.to_string(),
        }
    }

    fn run(&self, sql: &str) {
        let mut body = String::from("{\"query\":");
        fenec_core::json::escape_into(&mut body, sql);
        body.push('}');
        let a = self.call(Some(ROOT), "POST", "/query", &body);
        assert_eq!(a.status, 200, "{sql}: {}", a.body);
    }

    fn changes(&self, query: &str) -> Answer {
        self.call(Some(ROOT), "GET", &format!("/_changes?{query}"), "")
    }
}

/// The raw value of `"key":` in a line of JSON the server wrote: a number,
/// or a string's contents (these hold no escapes).
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let at = line.find(&format!("\"{key}\":"))? + key.len() + 3;
    let rest = &line[at..];
    match rest.strip_prefix('"') {
        Some(s) => s.split('"').next(),
        None => rest.split([',', '}']).next(),
    }
}

/// A line's `seq`, `op`, collection and `id`, and the document's `t`.
type Event = (u64, String, String, Option<u64>, Option<String>);

fn events(body: &str) -> Vec<Event> {
    body.lines()
        .map(|l| {
            let doc = l.split_once("\"doc\":").map(|(_, d)| d);
            (
                field(l, "seq").unwrap().parse().unwrap(),
                field(l, "op").unwrap().to_string(),
                field(l, "collection").unwrap_or_default().to_string(),
                field(l, "id").map(|v| v.parse().unwrap()),
                doc.and_then(|d| field(d, "t")).map(str::to_string),
            )
        })
        .collect()
}

#[test]
fn every_write_comes_once_in_order_and_a_cursor_inside_a_block_resumes_after_it() {
    let n = start("order", replication::DEFAULT_BUFFER);
    n.run("create collection a (n int, t text)");
    n.run("put a [{n: 1, t: \"x\"}, {n: 2, t: \"y\"}, {n: 3, t: \"w\"}]");
    n.run("set a {t: \"z\"} where n = 1");
    n.run("del a where n = 2");
    n.run("create collection b (t text)");
    n.run("drop collection b");
    let all = n.changes("since=0");
    assert_eq!(all.status, 200, "{}", all.body);
    assert_eq!(all.next, Some(8));
    let got = events(&all.body);
    let ops: Vec<_> = got
        .iter()
        .map(|e| (e.0, e.1.as_str(), e.2.as_str(), e.3))
        .collect();
    assert_eq!(
        ops,
        [
            (1, "create", "a", None),
            (2, "put", "a", Some(1)),
            (3, "put", "a", Some(2)),
            (4, "put", "a", Some(3)),
            (5, "put", "a", Some(1)),
            (6, "del", "a", Some(2)),
            (7, "create", "b", None),
            (8, "drop", "b", None),
        ]
    );
    assert_eq!(got[4].4.as_deref(), Some("z"));
    // Inside the put's block: from the write after it.
    let mid = n.changes("since=2");
    assert_eq!(events(&mid.body).first().map(|e| e.0), Some(3));
    // A page at a time, each from the last one's `Fenec-Next`, is the whole.
    let (mut since, mut paged) = (0, Vec::new());
    loop {
        let a = n.changes(&format!("since={since}&limit=3"));
        let page = events(&a.body);
        if page.is_empty() {
            break;
        }
        assert!(page.len() <= 3);
        paged.extend(page);
        since = a.next.unwrap();
    }
    assert_eq!(paged, got);
    // Nothing after the last: an empty answer that stays where it was.
    let none = n.changes("since=8");
    assert_eq!(
        (none.status, none.body.as_str(), none.next),
        (200, "", Some(8))
    );
}

#[test]
fn a_cursor_the_feed_no_longer_holds_is_refused_with_where_to_start() {
    let n = start("behind", 4096);
    n.run("create collection a (t text)");
    for i in 0..200 {
        n.run(&format!("put a {{t: \"{}\"}}", "x".repeat(40 + i % 7)));
    }
    let old = n.changes("since=0");
    assert_eq!(old.status, 410, "{}", old.body);
    let from: u64 = field(&old.body, "since").unwrap().parse().unwrap();
    assert!(from > 0 && from < 201, "{from}");
    let again = n.changes(&format!("since={from}&limit=10000"));
    assert_eq!(again.status, 200);
    let got = events(&again.body);
    assert_eq!(got.first().map(|e| e.0), Some(from + 1));
    assert_eq!(got.last().map(|e| e.0), Some(201));
    // Past the last write: someone else's cursor, or this database restored.
    assert_eq!(n.changes("since=9999").status, 409);
}

#[test]
fn a_wait_ends_when_a_write_is_on_disk() {
    let n = Arc::new(start("wait", replication::DEFAULT_BUFFER));
    n.run("create collection a (t text)");
    let writer = Arc::clone(&n);
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        writer.run("put a {t: \"late\"}");
    });
    let started = Instant::now();
    let a = n.changes("since=1&wait=10000");
    t.join().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        events(&a.body)
            .iter()
            .map(|e| e.4.clone())
            .collect::<Vec<_>>(),
        [Some("late".to_string())]
    );
    // Nothing comes: the wait's length, then an empty answer.
    let started = Instant::now();
    let idle = n.changes("since=2&wait=300");
    assert!(started.elapsed() >= Duration::from_millis(250));
    assert_eq!((idle.body.as_str(), idle.next), ("", Some(2)));
}

#[test]
fn what_is_refused() {
    let n = start("refused", replication::DEFAULT_BUFFER);
    n.run("create collection notes (owner text, t text)");
    // A scoped token's filter cannot hold back a deletion of a row it never
    // saw, so the stream is not for it.
    let scoped = n.access.mint("{\"sub\":\"alice\"}").unwrap();
    assert_eq!(n.call(Some(&scoped), "GET", "/_changes", "").status, 403);
    assert_eq!(n.call(None, "GET", "/_changes", "").status, 401);
    assert_eq!(n.changes("since=x").status, 400);
    assert_eq!(n.call(Some(ROOT), "POST", "/_changes", "").status, 404);
    // Without a feed there is nothing to read from.
    let plain = start("plain", 0);
    assert_eq!(plain.changes("since=0").status, 409);
    // A feed kept for `/_changes` alone feeds no replica: no token opens it,
    // an empty one least of all.
    assert_eq!(
        n.call(Some(""), "GET", "/_replication?since=0", "").status,
        401
    );
    assert_eq!(n.call(None, "GET", "/_replication?since=0", "").status, 401);
}

#[test]
fn a_consumer_reads_from_where_it_committed_and_again_what_it_did_not() {
    let n = start("consumer", replication::DEFAULT_BUFFER);
    n.run("create collection a (t text)");
    // Made first, at the last write on disk; unmade, it is not read.
    assert_eq!(n.changes("consumer=idx").status, 404);
    assert_eq!(
        n.call(Some(ROOT), "POST", "/_changes/consumers/idx", "")
            .status,
        200
    );
    assert_eq!(n.changes("consumer=idx").body, "");
    n.run("put a [{t: \"one\"}, {t: \"two\"}]");
    let first = n.changes("consumer=idx");
    let seen: Vec<_> = events(&first.body).into_iter().map(|e| e.4).collect();
    assert_eq!(seen, [Some("one".into()), Some("two".into())]);
    // Not committed: read again, as after a crash before the commit.
    assert_eq!(n.changes("consumer=idx").body, first.body);
    let next = first.next.unwrap();
    let c = n.call(
        Some(ROOT),
        "POST",
        "/_changes/consumers/idx",
        &format!("{{\"since\": {next}}}"),
    );
    assert_eq!(c.status, 200, "{}", c.body);
    assert_eq!(n.changes("consumer=idx").body, "");
    n.run("put a {t: \"three\"}");
    let later = n.changes("consumer=idx");
    assert_eq!(
        events(&later.body)
            .iter()
            .map(|e| e.4.clone())
            .collect::<Vec<_>>(),
        [Some("three".into())]
    );
    // A commit is a write, and the stream leaves the consumers' own out:
    // the cursor goes past them, and nothing of `_consumers` is read.
    let all = n.changes("since=0");
    assert!(!all.body.contains("_consumers"), "{}", all.body);
    assert_eq!(
        all.next,
        Some(n.changes("since=0&limit=10000").next.unwrap())
    );
    let list = n.call(Some(ROOT), "GET", "/_changes/consumers", "");
    assert_eq!(field(&list.body, "name"), Some("idx"));
    assert_eq!(field(&list.body, "since"), Some(next.to_string().as_str()));
    assert!(field(&list.body, "behind").unwrap().parse::<u64>().unwrap() >= 1);
    // Refused: past the last write on disk, a body without `since`.
    assert_eq!(
        n.call(
            Some(ROOT),
            "POST",
            "/_changes/consumers/idx",
            "{\"since\": 99999}"
        )
        .status,
        409
    );
    assert_eq!(
        n.call(
            Some(ROOT),
            "POST",
            "/_changes/consumers/idx",
            "{\"since\": \"x\"}"
        )
        .status,
        400
    );
    // Let go of, it is unseen again.
    assert_eq!(
        n.call(Some(ROOT), "DELETE", "/_changes/consumers/idx", "")
            .status,
        204
    );
    assert_eq!(
        n.call(Some(ROOT), "GET", "/_changes/consumers", "").body,
        "[]"
    );
}

#[test]
fn a_consumers_cursor_is_on_disk_before_it_is_answered() {
    let path = file("kept");
    let (db, feed) =
        replication::open(path.to_str().unwrap(), replication::DEFAULT_BUFFER).unwrap();
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        sync_on_write: true,
        ..Config::default()
    };
    let server = Server::new(Arc::new(RwLock::new(db)), cfg).with_replication(Replication::new(
        None,
        Some(feed),
        None,
    ));
    let listener = server.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    let n = Node {
        port,
        access: Arc::new(Access::new(SECRET, "").unwrap()),
    };
    n.run("create collection a (t text)");
    n.run("put a {t: \"x\"}");
    let c = n.call(None, "POST", "/_changes/consumers/idx", "{\"since\": 2}");
    assert_eq!(c.status, 200, "{}", c.body);
    // What the file holds now, read as a tool that only looks reads it.
    let on_disk = fenec_core::fs::open_read_only(path.to_str().unwrap()).unwrap();
    let rows = on_disk
        .query(
            &fenec_ql::parse_one("get _consumers select name, since").unwrap(),
            &[],
        )
        .unwrap();
    let fenec_core::query::Response::Rows(rs) = rows else {
        panic!()
    };
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(
        rs.rows[0].values,
        [
            fenec_core::value::Value::Text("idx".into()),
            fenec_core::value::Value::Int(2)
        ]
    );
}
