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
    db: Arc<RwLock<fenec_core::engine::Database>>,
}

/// A server keeping `buffer` bytes of writes for `/_changes` alone, as
/// `fenec-server --cdc` starts one, or none with `buffer` 0.
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
    let (server, db) = match buffer {
        0 => {
            let db = fenec_core::fs::open(path.to_str().unwrap()).unwrap();
            let db = Arc::new(RwLock::new(db));
            (Server::new(Arc::clone(&db), cfg), db)
        }
        _ => {
            let (db, feed) = replication::open(path.to_str().unwrap(), buffer).unwrap();
            let db = Arc::new(RwLock::new(db));
            let server = Server::new(Arc::clone(&db), cfg).with_replication(Replication::new(
                None,
                Some(feed),
                None,
            ));
            (server, db)
        }
    };
    let listener = server.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    Node { port, access, db }
}

struct Answer {
    status: u16,
    next: Option<u64>,
    /// `Fenec-Seq`: the last write the database holds.
    seq: Option<u64>,
    body: String,
}

impl Node {
    fn call(&self, token: Option<&str>, method: &str, target: &str, body: &str) -> Answer {
        self.call_with(token, method, target, body, "")
    }

    /// [`Self::call`] with `extra`, header lines each ended by `\r\n`.
    fn call_with(
        &self,
        token: Option<&str>,
        method: &str,
        target: &str,
        body: &str,
        extra: &str,
    ) -> Answer {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
        write!(
            s,
            "{method} {target} HTTP/1.1\r\nHost: x\r\n{auth}{extra}Content-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        let (head, body) = out.split_once("\r\n\r\n").unwrap();
        let header = |name: &str| {
            head.lines()
                .find_map(|l| l.strip_prefix(name))
                .map(|v| v.trim().parse().unwrap())
        };
        Answer {
            status: head[9..12].parse().unwrap(),
            next: header("Fenec-Next: "),
            seq: header("Fenec-Seq: "),
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

    /// `POST /_changes/consumers/<name>`: makes it, or answers where it is.
    fn make(&self, name: &str, body: &str) -> Answer {
        self.call(
            Some(ROOT),
            "POST",
            &format!("/_changes/consumers/{name}"),
            body,
        )
    }

    /// `PATCH /_changes/consumers/<name>` to `since`.
    fn commit(&self, name: &str, since: u64) -> Answer {
        self.call(
            Some(ROOT),
            "PATCH",
            &format!("/_changes/consumers/{name}"),
            &format!("{{\"since\": {since}}}"),
        )
    }

    /// The `t` of each write `name` reads from where it stands.
    fn read(&self, name: &str) -> (Vec<Option<String>>, Answer) {
        let a = self.changes(&format!("consumer={name}"));
        assert_eq!(a.status, 200, "{}", a.body);
        (events(&a.body).into_iter().map(|e| e.4).collect(), a)
    }

    fn seq(&self) -> u64 {
        self.db.read().unwrap().change_seq()
    }
}

/// An answer's `"since"`, as a `POST` or a `PATCH` of a consumer gives it.
fn since(a: &Answer) -> u64 {
    field(&a.body, "since")
        .unwrap_or_else(|| panic!("no since in {}", a.body))
        .parse()
        .unwrap()
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

/// An `alter collection` is a change of its own, and the documents after it
/// are read by the schema it made: a field renamed comes under its new
/// name, a dropped one not at all.
#[test]
fn an_alter_is_a_change_and_the_documents_after_it_read_by_it() {
    let n = start("alter", replication::DEFAULT_BUFFER);
    n.run("create collection a (n int, s text)");
    n.run("put a {n: 1, s: \"x\"}");
    n.run("alter collection a rename field s to t");
    n.run("alter collection a drop field n");
    n.run("put a {t: \"y\"}");
    let all = n.changes("since=0");
    assert_eq!(all.status, 200, "{}", all.body);
    let got = events(&all.body);
    let ops: Vec<_> = got
        .iter()
        .map(|e| (e.0, e.1.as_str(), e.4.as_deref()))
        .collect();
    assert_eq!(
        ops,
        [
            (1, "create", None),
            (2, "put", None),
            (3, "alter", None),
            (4, "alter", None),
            (5, "put", Some("y")),
        ]
    );
    let last = all.body.lines().last().unwrap();
    assert!(!last.contains("\"n\""), "{last}");
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

/// `Fenec-Seq` is the last write the database holds, so a reader knows at
/// once whether it has every write there is: an empty page whose
/// `Fenec-Next` is behind it has writes still to come -- here one not on
/// disk yet, which the stream hands over once an fsync covers it.
#[test]
fn an_answer_says_where_the_writes_end() {
    let n = start("seq", replication::DEFAULT_BUFFER);
    n.run("create collection a (t text)");
    n.run("put a [{t: \"1\"}, {t: \"2\"}]");
    let a = n.changes("since=0");
    assert_eq!((a.next, a.seq), (Some(3), Some(3)), "{}", a.body);
    // Caught up: an empty page at the end says so too.
    let a = n.changes("since=3");
    assert_eq!((a.body.as_str(), a.next, a.seq), ("", Some(3), Some(3)));
    // A write in the database, not yet on disk: not in the stream, and
    // not yet read.
    n.db.write()
        .unwrap()
        .execute(&fenec_ql::parse_one("put a {t: \"3\"}").unwrap())
        .unwrap();
    let a = n.changes("since=3");
    assert_eq!((a.body.as_str(), a.next, a.seq), ("", Some(3), Some(4)));
    n.db.write().unwrap().sync().unwrap();
    let a = n.changes("since=3");
    assert_eq!(events(&a.body).len(), 1, "{}", a.body);
    assert_eq!((a.next, a.seq), (Some(4), Some(4)));
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
    // Nor does a scoped token make, move, list or let go of a consumer:
    // the consumer reads what the stream holds.
    for (method, path, body) in [
        ("POST", "/_changes/consumers/idx", ""),
        ("PATCH", "/_changes/consumers/idx", "{\"since\": 1}"),
        ("DELETE", "/_changes/consumers/idx", ""),
        ("GET", "/_changes/consumers", ""),
        ("GET", "/_changes?consumer=idx", ""),
    ] {
        let a = n.call(Some(&scoped), method, path, body);
        assert_eq!(a.status, 403, "{method} {path}: {}", a.body);
    }
    assert_eq!(
        n.call(Some(ROOT), "GET", "/_changes/consumers", "").body,
        "[]"
    );
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
    let made = n.make("idx", "");
    assert_eq!(made.status, 201, "{}", made.body);
    assert_eq!(since(&made), 1);
    // Each answer names the change it left the database at, as a write's
    // does: a read sent after it (`Fenec-After`) finds the consumer so.
    assert_eq!(made.seq, Some(n.seq()));
    assert_eq!(n.changes("consumer=idx").body, "");
    n.run("put a [{t: \"one\"}, {t: \"two\"}]");
    let first = n.changes("consumer=idx");
    let seen: Vec<_> = events(&first.body).into_iter().map(|e| e.4).collect();
    assert_eq!(seen, [Some("one".into()), Some("two".into())]);
    // Not committed: read again, as after a crash before the commit.
    assert_eq!(n.changes("consumer=idx").body, first.body);
    let next = first.next.unwrap();
    let c = n.commit("idx", next);
    assert_eq!(c.status, 200, "{}", c.body);
    assert_eq!(since(&c), next);
    assert_eq!(c.seq, Some(n.seq()));
    assert!(c.seq > made.seq, "the commit is a write");
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
    // Refused: past the last write, a body without `since`.
    assert_eq!(n.commit("idx", 99999).status, 409);
    for body in ["{\"since\": \"x\"}", "{\"since\": -1}", "{}", ""] {
        let a = n.call(Some(ROOT), "PATCH", "/_changes/consumers/idx", body);
        assert_eq!(a.status, 400, "{body}: {}", a.body);
    }
    assert_eq!(n.make("idx", "{\"since\": \"x\"}").status, 400);
    // Let go of, it is unseen again.
    let gone = n.call(Some(ROOT), "DELETE", "/_changes/consumers/idx", "");
    assert_eq!((gone.status, gone.seq), (204, Some(n.seq())));
    assert_eq!(
        n.call(Some(ROOT), "GET", "/_changes/consumers", "").body,
        "[]"
    );
}

/// A commit sent as a `POST`, as every commit was before the `PATCH` --
/// an older client's, a recipe written then -- is refused (409), naming
/// where the consumer stands and what moves it. Answered 200 and left
/// where it was, the client read the same page for ever, told nothing. A
/// `POST` at or behind where it stands, or naming no start, is still the
/// make it is, 200.
#[test]
fn a_commit_sent_as_a_post_is_refused_and_moves_nothing() {
    let n = start("stale-commit", replication::DEFAULT_BUFFER);
    n.run("create collection a (t text)");
    let place = since(&n.make("sink", ""));
    n.run("put a {t: \"one\"}");
    let (seen, page) = n.read("sink");
    assert_eq!(seen, [Some("one".into())]);
    let next = page.next.unwrap();
    assert!(next > place);
    let before = n.seq();
    let stale = n.make("sink", &format!("{{\"since\": {next}}}"));
    assert_eq!(stale.status, 409, "{}", stale.body);
    assert!(
        stale.body.contains(&format!(
            "consumer sink is there, at {place}: a POST makes a consumer and moves none; \
             PATCH /_changes/consumers/sink moves it"
        )),
        "{}",
        stale.body
    );
    assert_eq!(since(&stale), place);
    assert_eq!(n.seq(), before, "a refused commit wrote nothing");
    assert_eq!(n.read("sink").0, [Some("one".into())], "nor moved it");
    for body in [
        String::new(),
        "{}".into(),
        "{\"since\": 0}".into(),
        format!("{{\"since\": {place}}}"),
    ] {
        let a = n.make("sink", &body);
        assert_eq!((a.status, since(&a)), (200, place), "{body}: {}", a.body);
    }
}

/// A 410 names the first `since` the feed still keeps, and the feed keeps
/// writes by their size, on disk or not: that `since` may be ahead of the
/// last write on disk. A consumer told to go on from there does, its
/// `PATCH` taking any `since` up to the last write the database holds; a
/// commit held to the disk was refused 409, and the consumer could not go
/// on at all.
#[test]
fn a_consumer_goes_on_from_a_410_past_the_disk() {
    use fenec_core::value::Value;
    let n = start("410-durable", 4096);
    n.run("create collection a (t text)");
    assert_eq!(n.make("idx", "{\"since\": 1}").status, 201);
    n.run("put a {t: \"on disk\"}");
    {
        let mut g = n.db.write().unwrap();
        let put = fenec_ql::parse_one("put a {t: $1}").unwrap();
        for i in 0..200 {
            g.execute_with(&put, &[Value::Text("x".repeat(40 + i % 7))])
                .unwrap();
        }
    }
    let old = n.changes("consumer=idx");
    assert_eq!(old.status, 410, "{}", old.body);
    let from: u64 = field(&old.body, "since").unwrap().parse().unwrap();
    let c = n.commit("idx", from);
    assert_eq!(c.status, 200, "{}", c.body);
    let got = events(&n.changes("consumer=idx&limit=10000").body);
    assert_eq!(got.first().map(|e| e.0), Some(from + 1));
    // Past every write the database holds is still refused.
    assert_eq!(n.commit("idx", n.seq() + 1).status, 409);
    assert_eq!(
        n.make("new", &format!("{{\"since\": {}}}", n.seq() + 1))
            .status,
        409
    );
}

/// A consumer's commit and its let-go are writes: marked as a read
/// (`Fenec-Read: replica`), as a router may send it to a replica, they are
/// refused as every marked write is, and the cursor stays where it was.
/// The change stream was served before the mark was read, and a marked
/// commit moved the cursor on whichever copy it reached.
#[test]
fn a_consumer_s_commit_marked_as_a_read_is_refused() {
    let n = start("marked-commit", replication::DEFAULT_BUFFER);
    n.run("create collection a (t text)");
    let marked = "Fenec-Read: replica\r\n";
    let made = n.call_with(Some(ROOT), "POST", "/_changes/consumers/idx", "", marked);
    assert_eq!(made.status, 403, "{}", made.body);
    assert!(
        made.body.contains("Fenec-Read: replica only reads"),
        "{}",
        made.body
    );
    assert_eq!(n.changes("consumer=idx").status, 404, "nothing was made");
    assert_eq!(n.make("idx", "").status, 201);
    // Made already, a marked make writes nothing, and is refused all the
    // same: it is a write's route.
    let again = n.call_with(Some(ROOT), "POST", "/_changes/consumers/idx", "", marked);
    assert_eq!(again.status, 403, "{}", again.body);
    n.run("put a {t: \"one\"}");
    let next = n.changes("consumer=idx").next.unwrap();
    let c = n.call_with(
        Some(ROOT),
        "PATCH",
        "/_changes/consumers/idx",
        &format!("{{\"since\": {next}}}"),
        marked,
    );
    assert_eq!(c.status, 403, "{}", c.body);
    let gone = n.call_with(Some(ROOT), "DELETE", "/_changes/consumers/idx", "", marked);
    assert_eq!(gone.status, 403, "{}", gone.body);
    // Still where it was, and the read is answered marked as any.
    let again = n.call_with(Some(ROOT), "GET", "/_changes?consumer=idx", "", marked);
    assert_eq!(again.status, 200, "{}", again.body);
    assert!(again.body.contains("one"), "{}", again.body);
}

/// A sink that commits after every answer, an empty one too, stays idle
/// while nothing else is written: its read waits past its own commits, and
/// a commit past nothing but cursors writes nothing. Each commit was a
/// write that woke the next read at once, which answered nothing past it
/// and was committed again -- 30 000 fsynced writes in a few idle minutes.
#[test]
fn a_sink_that_commits_every_answer_stays_idle() {
    let n = start("idle", replication::DEFAULT_BUFFER);
    n.run("create collection a (t text)");
    let made = n.make("sink", "");
    assert_eq!(made.status, 201, "{}", made.body);
    n.run("put a {t: \"one\"}");
    let seq = || n.seq();
    // Where the consumer stands after the commit, as its answer says.
    let commit = |next: u64| {
        let c = n.commit("sink", next);
        assert_eq!(c.status, 200, "{}", c.body);
        since(&c)
    };
    let first = n.changes("consumer=sink&wait=5000");
    assert_eq!(events(&first.body).len(), 1, "{}", first.body);
    let place = first.next.unwrap();
    assert_eq!(commit(place), place);
    let settled = seq();
    const WAIT: u64 = 300;
    for round in 0..4 {
        let t = Instant::now();
        let a = n.changes(&format!("consumer=sink&wait={WAIT}"));
        assert_eq!(a.status, 200, "{}", a.body);
        assert_eq!(a.body, "", "round {round}");
        assert!(
            t.elapsed() >= Duration::from_millis(WAIT),
            "round {round}: woken after {:?} by nothing but a commit",
            t.elapsed()
        );
        // Past nothing but its own commit, it stays where it stood, and is
        // answered so: where a list of the consumers and a later `POST`
        // find it, and where it reads the same writes from.
        let next = a.next.unwrap();
        assert!(next > place, "round {round}: past its own commit");
        assert_eq!(commit(next), place, "round {round}");
        assert_eq!(seq(), settled, "round {round}: a commit past nothing wrote");
        let list = n.call(Some(ROOT), "GET", "/_changes/consumers", "");
        assert_eq!(field(&list.body, "since"), Some(place.to_string().as_str()));
        let made = n.make("sink", &format!("{{\"since\": {place}}}"));
        assert_eq!((made.status, since(&made)), (200, place));
    }
    // A write wakes it, and the line comes once.
    n.run("put a {t: \"two\"}");
    let woken = n.changes("consumer=sink&wait=5000");
    let seen: Vec<_> = events(&woken.body).into_iter().map(|e| e.4).collect();
    assert_eq!(seen, [Some("two".into())]);
    let next = woken.next.unwrap();
    assert_eq!(commit(next), next);
    assert_eq!(n.changes("consumer=sink").body, "");
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
    let db = Arc::new(RwLock::new(db));
    let server = Server::new(Arc::clone(&db), cfg).with_replication(Replication::new(
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
        db,
    };
    n.run("create collection a (t text)");
    n.run("put a {t: \"x\"}");
    // What the file holds, read as a tool that only looks reads it.
    let on_disk = || {
        let db = fenec_core::fs::open_read_only(path.to_str().unwrap()).unwrap();
        let rows = db
            .query(
                &fenec_ql::parse_one("get _consumers select name, since").unwrap(),
                &[],
            )
            .unwrap();
        let fenec_core::query::Response::Rows(rs) = rows else {
            panic!()
        };
        rs.rows.into_iter().map(|r| r.values).collect::<Vec<_>>()
    };
    let row = |since| {
        vec![
            fenec_core::value::Value::Text("idx".into()),
            fenec_core::value::Value::Int(since),
        ]
    };
    let c = n.call(None, "POST", "/_changes/consumers/idx", "{\"since\": 1}");
    assert_eq!(c.status, 201, "{}", c.body);
    assert_eq!(on_disk(), [row(1)]);
    let c = n.call(None, "PATCH", "/_changes/consumers/idx", "{\"since\": 2}");
    assert_eq!(c.status, 200, "{}", c.body);
    assert_eq!(on_disk(), [row(2)]);
}

/// A relay or a follower makes its consumer each time it starts. Made
/// again, the consumer stays where it was committed -- whatever start the
/// `POST` names, which is a new consumer's -- and reads the writes made
/// while its relay was stopped. A `POST` of one that was there moved it to
/// the last write on disk, and the relay skipped every one of them.
#[test]
fn a_consumer_made_again_stays_where_it_was_and_reads_what_came_between() {
    let n = start("made-again", replication::DEFAULT_BUFFER);
    n.run("create collection a (t text)");
    let made = n.make("relay", "");
    assert_eq!(made.status, 201, "{}", made.body);
    n.run("put a {t: \"one\"}");
    let (seen, first) = n.read("relay");
    assert_eq!(seen, [Some("one".into())]);
    let committed = first.next.unwrap();
    assert_eq!(n.commit("relay", committed).status, 200);

    // The relay is stopped; writes go on.
    n.run("put a [{t: \"two\"}, {t: \"three\"}]");
    let before = n.seq();
    // It starts again, and makes its consumer as it did the first time --
    // with no body, with `{}`, and asking for the first write: each a
    // start for a consumer not there, none a move of this one.
    for body in [
        "",
        "{}",
        "{\"since\": null}",
        "{\"since\": 0}",
        "{\"since\": 1}",
    ] {
        let again = n.make("relay", body);
        assert_eq!(again.status, 200, "{body}: {}", again.body);
        assert_eq!(since(&again), committed, "{body}");
    }
    assert_eq!(n.seq(), before, "a consumer made again wrote nothing");
    let (seen, between) = n.read("relay");
    assert_eq!(seen, [Some("two".into()), Some("three".into())]);
    let list = n.call(Some(ROOT), "GET", "/_changes/consumers", "");
    assert_eq!(
        field(&list.body, "since"),
        Some(committed.to_string().as_str())
    );
    assert_eq!(n.commit("relay", between.next.unwrap()).status, 200);
    assert_eq!(n.read("relay").0, Vec::<Option<String>>::new());

    // Past the last write, a start is refused: another database's cursor,
    // or this one's before a restore -- and for one that is there, a
    // commit sent as a `POST`.
    assert_eq!(n.make("relay", "{\"since\": 99999}").status, 409);
    assert_eq!(n.make("other", "{\"since\": 99999}").status, 409);
    assert_eq!(n.changes("consumer=other").status, 404);
}

/// A consumer moves by a `PATCH` alone: on, once it has done with what it
/// read, or back, to read again. A `POST` naming a start makes a consumer
/// there; a `PATCH` makes none, even one let go a moment before.
#[test]
fn a_patch_moves_a_consumer_and_a_post_makes_one_where_it_says() {
    let n = start("moved", replication::DEFAULT_BUFFER);
    n.run("create collection a (t text)");
    assert_eq!(n.commit("idx", 1).status, 404);
    assert_eq!(n.changes("consumer=idx").status, 404, "a PATCH made none");
    // Made at the first write: every write is read.
    let made = n.make("idx", "{\"since\": 0}");
    assert_eq!((made.status, since(&made)), (201, 0), "{}", made.body);
    n.run("put a [{t: \"one\"}, {t: \"two\"}]");
    let (seen, all) = n.read("idx");
    assert_eq!(seen, [None, Some("one".into()), Some("two".into())]);
    let end = all.next.unwrap();
    assert_eq!(since(&n.commit("idx", end)), end);
    assert!(n.read("idx").0.is_empty());
    // Back to after the create: read again from there.
    let back = n.commit("idx", 1);
    assert_eq!((back.status, since(&back)), (200, 1), "{}", back.body);
    assert_eq!(n.read("idx").0, [Some("one".into()), Some("two".into())]);
    // Let go of, it is not moved back into being.
    assert_eq!(
        n.call(Some(ROOT), "DELETE", "/_changes/consumers/idx", "")
            .status,
        204
    );
    assert_eq!(n.commit("idx", end).status, 404);
    assert_eq!(n.changes("consumer=idx").status, 404);
}

/// A consumer further behind than the feed keeps is answered 410 with the
/// first `since` it does, as a cursor is, and a `PATCH` there goes on from
/// it. Made again in between, it stays as far behind: moved to the last
/// write on disk, it would have been answered as if nothing were missing.
#[test]
fn a_consumer_behind_the_feed_is_told_so_and_moves_on_by_a_patch() {
    let n = start("consumer-behind", 4096);
    n.run("create collection a (t text)");
    assert_eq!(n.make("idx", "{\"since\": 1}").status, 201);
    for i in 0..200 {
        n.run(&format!("put a {{t: \"{}\"}}", "x".repeat(40 + i % 7)));
    }
    let last = n.seq();
    let old = n.changes("consumer=idx");
    assert_eq!(old.status, 410, "{}", old.body);
    let from: u64 = field(&old.body, "since").unwrap().parse().unwrap();
    assert!(from > 1 && from < last, "{from}");
    let again = n.make("idx", "");
    assert_eq!((again.status, since(&again)), (200, 1), "{}", again.body);
    assert_eq!(n.changes("consumer=idx").status, 410);
    // A few writes on from the first the feed holds: the commit is a write
    // of its own, which may push that one out of a feed this small.
    let on = from + 3;
    assert_eq!(n.commit("idx", on).status, 200);
    let a = n.changes("consumer=idx&limit=10000");
    assert_eq!(a.status, 200, "{}", a.body);
    let got = events(&a.body);
    assert_eq!(got.first().map(|e| e.0), Some(on + 1));
    assert_eq!(got.last().map(|e| e.0), Some(last));
}

/// The sweep's deletes of rows past their time (`@ttl`) are ordinary
/// deletes on disk: the stream hands each over as one.
#[test]
fn a_sweep_comes_as_deletes() {
    let n = start("sweep", replication::DEFAULT_BUFFER);
    n.run("create collection s (t text, seen timestamp @ttl(1h))");
    n.run(
        "put s [{t: \"old\", seen: 1}, {t: \"live\", seen: \"2999-01-01\"}, {t: \"older\", seen: 2}]",
    );
    let before = n.changes("since=0");
    let last = events(&before.body).last().unwrap().0;
    assert_eq!(fenec_http::sweep::pass("test", &n.db), 2);
    let after = n.changes(&format!("since={last}"));
    assert_eq!(after.status, 200, "{}", after.body);
    let got: Vec<_> = events(&after.body)
        .into_iter()
        .map(|e| (e.1, e.2, e.3))
        .collect();
    assert_eq!(
        got,
        [
            ("del".to_string(), "s".to_string(), Some(1)),
            ("del".to_string(), "s".to_string(), Some(3)),
        ]
    );
}

/// The sweeper leaves a row a sweep's period past its time before it
/// deletes it, so a reaper coming by as often reads it with `expired()`
/// and gives back what it held; swept at the first pass after its time, a
/// row that lapsed a moment before went unseen.
#[test]
fn a_row_past_its_time_waits_a_sweep_for_its_reaper() {
    let n = start("grace", replication::DEFAULT_BUFFER);
    n.run("create collection holds (amount int, until timestamp @ttl(1ms))");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    n.run(&format!(
        "put holds [{{amount: 5, until: {}}}, {{amount: 7, until: 1}}]",
        now - 1000
    ));
    let grace = fenec_http::sweep::GRACE;
    assert_eq!(
        fenec_http::sweep::pass_after("test", &n.db, grace),
        1,
        "only the one past its time by a sweep's period"
    );
    let expired = |n: &Node| {
        let db = n.db.read().unwrap();
        let stmt = fenec_ql::parse_one("get holds select amount where expired()").unwrap();
        let r = db.query(&stmt, &[]).unwrap();
        r.rows()
            .unwrap()
            .rows
            .iter()
            .map(|r| r.values[0].clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(expired(&n), [fenec_core::value::Value::Int(5)]);
    assert_eq!(fenec_http::sweep::pass("test", &n.db), 1);
    assert!(expired(&n).is_empty());
}
