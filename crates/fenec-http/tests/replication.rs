//! Replication end to end: a primary and its replicas as servers in this
//! process, each over its own file, talking over real sockets.

use fenec_core::prelude::*;
use fenec_http::replication::{self, fresh_id, Follower, Replication};
use fenec_http::{Config, Server};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const TOKEN: &str = "replication-secret";

struct Node {
    port: u16,
    db: Arc<RwLock<Database>>,
    follower: Option<Arc<Follower>>,
}

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fenec-repl-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn serve(db: Arc<RwLock<Database>>, repl: Arc<Replication>) -> u16 {
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        // Every write is fsynced before it is answered: what a replica is
        // sent is what an fsync covered.
        sync_on_write: true,
        ..Config::default()
    };
    let server = Server::new(db, cfg).with_replication(repl);
    let listener = server.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    port
}

/// A primary over `file`, as `fenec-pg --replication-token` starts one.
fn primary(file: &Path, buffer: usize) -> Node {
    let (mut db, feed) = replication::open(file.to_str().unwrap(), buffer).unwrap();
    if db.history().following {
        db.fork(fresh_id()).unwrap();
    }
    if db.history().lineage.is_empty() {
        db.fork(fresh_id()).unwrap();
    }
    let db = Arc::new(RwLock::new(db));
    let port = serve(
        Arc::clone(&db),
        Replication::new(TOKEN.into(), Some(feed), None),
    );
    Node {
        port,
        db,
        follower: None,
    }
}

/// A replica over `file` following the server on `upstream`, and feeding
/// replicas of its own.
fn replica(file: &Path, upstream: u16) -> Node {
    let (mut db, feed) =
        replication::open(file.to_str().unwrap(), replication::DEFAULT_BUFFER).unwrap();
    // As `fenec-pg --replica-of` does: no write of its own from the start,
    // before the primary has said a word.
    let lineage = db.history().lineage.clone();
    db.follow(lineage).unwrap();
    let db = Arc::new(RwLock::new(db));
    let follower = Follower::new(
        &format!("http://127.0.0.1:{upstream}"),
        TOKEN.into(),
        Arc::clone(&db),
        Some(Arc::clone(&feed)),
        true,
    )
    .unwrap();
    let f = Arc::clone(&follower);
    std::thread::spawn(move || f.run());
    let port = serve(
        Arc::clone(&db),
        Replication::new(TOKEN.into(), Some(feed), Some(Arc::clone(&follower))),
    );
    Node {
        port,
        db,
        follower: Some(follower),
    }
}

fn http(port: u16, method: &str, path: &str, token: Option<&str>, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: x\r\n{auth}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status = out[9..12].parse().unwrap();
    let body = out
        .split_once("\r\n\r\n")
        .map_or("", |(_, b)| b)
        .to_string();
    (status, body)
}

fn query(node: &Node, sql: &str) -> (u16, String) {
    let mut body = String::from("{\"query\":");
    fenec_core::json::escape_into(&mut body, sql);
    body.push('}');
    http(node.port, "POST", "/query", None, &body)
}

fn seq(node: &Node) -> u64 {
    node.db.read().unwrap().change_seq()
}

fn rows(node: &Node, sql: &str) -> Vec<(u64, Vec<Value>)> {
    let g = node.db.read().unwrap();
    let r = g.query(&fenec_ql::parse_one(sql).unwrap(), &[]).unwrap();
    r.rows()
        .unwrap()
        .rows
        .iter()
        .map(|r| (r.id, r.values.clone()))
        .collect()
}

fn caught_up(replica: &Node, primary: &Node) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while seq(replica) != seq(primary) {
        assert!(
            Instant::now() < deadline,
            "the replica stayed at {} while the primary is at {}",
            seq(replica),
            seq(primary)
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn write_some(node: &Node, from: usize, n: usize) {
    for i in from..from + n {
        let (status, body) = query(
            node,
            &format!("put items {{name: \"item {i}\", n: {i}, e: [{i}.0, 1.0, 0.5]}}"),
        );
        assert_eq!(status, 200, "{body}");
    }
}

const SCHEMA: &str =
    "create collection items (name text @text, n int @sorted, e vector<3> @hnsw(cosine))";

/// A primary that died in the middle of an append left its last record
/// cut short. It is cut off the file on the way back up -- no replica was
/// sent it, since no fsync covered it -- rather than read back later with
/// the first write after the restart as the rest of it.
#[test]
fn a_record_a_crash_cut_short_is_cut_off_before_the_next_write() {
    let d = dir("torn");
    let file = d.join("p.fenec");
    let path = file.to_str().unwrap();
    let run = |db: &mut Database, sql: &str| {
        db.execute(&fenec_ql::parse_one(sql).unwrap()).unwrap();
    };
    {
        let (mut db, _) = replication::open(path, replication::DEFAULT_BUFFER).unwrap();
        run(&mut db, "create collection t (x int)");
        run(&mut db, "put t {x: 1}");
        db.sync().unwrap();
    }
    let whole = std::fs::metadata(&file).unwrap().len();
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&file)
        .unwrap();
    f.write_all(&[3, 1, 100, 0, 1, 2]).unwrap();
    drop(f);
    {
        let (mut db, _) = replication::open(path, replication::DEFAULT_BUFFER).unwrap();
        assert_eq!(std::fs::metadata(&file).unwrap().len(), whole);
        run(&mut db, "put t {x: 2}");
        db.sync().unwrap();
    }
    let (db, _) = replication::open(path, replication::DEFAULT_BUFFER).unwrap();
    let r = db
        .query(&fenec_ql::parse_one("get t count").unwrap(), &[])
        .unwrap();
    assert_eq!(r.rows().unwrap().rows[0].values[0], Value::Int(2));
}

#[test]
fn a_replica_follows_and_a_restarted_one_goes_on_from_where_it_was() {
    let d = dir("follow");
    let p = primary(&d.join("p.fenec"), replication::DEFAULT_BUFFER);
    assert_eq!(query(&p, SCHEMA).0, 200);
    write_some(&p, 0, 20);

    let r = replica(&d.join("r.fenec"), p.port);
    caught_up(&r, &p);
    // It started empty, and an empty database continues anything.
    assert_eq!(r.follower.as_ref().unwrap().images(), 0);
    write_some(&p, 20, 30);
    assert_eq!(
        query(&p, "set items {name: \"renamed\"} where n < 5").0,
        200
    );
    assert_eq!(query(&p, "del items where n >= 45").0, 200);
    caught_up(&r, &p);
    for sql in [
        "get items",
        r#"get items select id match name "renamed""#,
        "get items select id, n where n > 10 order n desc limit 4",
        "get items select id near e [3.0, 1.0, 0.5] exact limit 3",
    ] {
        assert_eq!(rows(&r, sql), rows(&p, sql), "{sql}");
    }
    assert!(r.db.read().unwrap().history().following);
    assert_eq!(
        r.db.read().unwrap().history().lineage,
        p.db.read().unwrap().history().lineage
    );

    // A write sent to the replica is refused, as a standby refuses one.
    let (status, body) = query(&r, "put items {name: \"no\"}");
    assert_eq!(status, 403, "{body}");

    // Stopped and started again over its file, it asks for what it
    // missed rather than for an image.
    r.follower.as_ref().unwrap().halt();
    let at = seq(&r);
    write_some(&p, 50, 10);
    drop(r);
    let r = replica(&d.join("r.fenec"), p.port);
    assert_eq!(seq(&r), at);
    caught_up(&r, &p);
    assert_eq!(r.follower.as_ref().unwrap().images(), 0);
    assert_eq!(rows(&r, "get items"), rows(&p, "get items"));
}

#[test]
fn a_replica_behind_the_buffer_is_sent_an_image() {
    let d = dir("image");
    // A buffer this small keeps only the last few writes.
    let p = primary(&d.join("p.fenec"), 256);
    assert_eq!(query(&p, SCHEMA).0, 200);
    write_some(&p, 0, 40);

    let r = replica(&d.join("r.fenec"), p.port);
    caught_up(&r, &p);
    assert_eq!(r.follower.as_ref().unwrap().images(), 1);
    // After the image it follows write by write.
    write_some(&p, 40, 5);
    caught_up(&r, &p);
    assert_eq!(r.follower.as_ref().unwrap().images(), 1);
    assert_eq!(rows(&r, "get items"), rows(&p, "get items"));
    let near = "get items select id near e [7.0, 1.0, 0.5] exact limit 3";
    assert_eq!(rows(&r, near), rows(&p, near));
}

#[test]
fn a_replica_of_a_replica_is_fed_the_same_writes() {
    let d = dir("chain");
    let p = primary(&d.join("p.fenec"), replication::DEFAULT_BUFFER);
    assert_eq!(query(&p, SCHEMA).0, 200);
    let a = replica(&d.join("a.fenec"), p.port);
    let b = replica(&d.join("b.fenec"), a.port);
    write_some(&p, 0, 25);
    caught_up(&a, &p);
    caught_up(&b, &p);
    assert_eq!(rows(&b, "get items"), rows(&p, "get items"));
}

#[test]
fn a_promoted_replica_forks_and_the_old_primary_rejoins_under_it() {
    let d = dir("promote");
    let p = primary(&d.join("p.fenec"), replication::DEFAULT_BUFFER);
    assert_eq!(query(&p, SCHEMA).0, 200);
    write_some(&p, 0, 10);
    let r = replica(&d.join("r.fenec"), p.port);
    caught_up(&r, &p);

    // The token guards the replication endpoints.
    assert_eq!(
        http(r.port, "POST", "/_replication/promote", None, "").0,
        401
    );
    assert_eq!(
        http(r.port, "POST", "/_replication/promote", Some("wrong"), "").0,
        401
    );
    let (status, body) = http(r.port, "POST", "/_replication/promote", Some(TOKEN), "");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"promoted\":true"), "{body}");
    let promoted_at = seq(&r);
    assert!(!r.db.read().unwrap().history().following);
    assert_eq!(query(&r, "put items {name: \"after\", n: 100}").0, 200);

    // The old primary wrote on after the replica left it: its history went
    // past the fork, so it cannot be continued and takes an image.
    write_some(&p, 10, 3);
    assert!(seq(&p) > promoted_at);
    let old = p.db.read().unwrap().history().clone();
    let at = seq(&p);
    drop(p);
    let back = replica(&d.join("p.fenec"), r.port);
    caught_up(&back, &r);
    assert_eq!(back.follower.as_ref().unwrap().images(), 1);
    assert_eq!(rows(&back, "get items"), rows(&r, "get items"));
    assert_ne!(back.db.read().unwrap().history().lineage, old.lineage);
    assert!(rows(&back, "get items where n >= 10 and n < 13").is_empty());
    let _ = at;

    // Its status names what it follows.
    let (status, body) = http(back.port, "GET", "/_replication/status", Some(TOKEN), "");
    assert_eq!(status, 200);
    assert!(body.contains("\"role\":\"replica\""), "{body}");
    assert!(body.contains("\"connected\":true"), "{body}");
    let (_, body) = http(r.port, "GET", "/_replication/status", Some(TOKEN), "");
    assert!(body.contains("\"role\":\"primary\""), "{body}");
}

#[test]
fn a_replica_on_the_forked_side_of_nothing_goes_on_without_an_image() {
    let d = dir("prefix");
    let p = primary(&d.join("p.fenec"), replication::DEFAULT_BUFFER);
    assert_eq!(query(&p, SCHEMA).0, 200);
    write_some(&p, 0, 10);
    let a = replica(&d.join("a.fenec"), p.port);
    let b = replica(&d.join("b.fenec"), p.port);
    caught_up(&a, &p);
    caught_up(&b, &p);

    // The primary goes; `a` is promoted and `b` pointed at it. `b` holds
    // nothing `a` does not, so it goes on from where it was.
    b.follower.as_ref().unwrap().halt();
    drop(b);
    drop(p);
    let (status, _) = http(a.port, "POST", "/_replication/promote", Some(TOKEN), "");
    assert_eq!(status, 200);
    write_some(&a, 10, 5);
    let b = replica(&d.join("b.fenec"), a.port);
    caught_up(&b, &a);
    assert_eq!(b.follower.as_ref().unwrap().images(), 0);
    assert_eq!(rows(&b, "get items"), rows(&a, "get items"));
    assert_eq!(
        b.db.read().unwrap().history().lineage,
        a.db.read().unwrap().history().lineage
    );
}
