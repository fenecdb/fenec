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

/// A primary over `file`, as `fenec-server --replication-token` starts one.
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
        Replication::new(Some(TOKEN.into()), Some(feed), None),
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
    // As `fenec-server --replica-of` does: no write of its own from the start,
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
        Replication::new(Some(TOKEN.into()), Some(feed), Some(Arc::clone(&follower))),
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

/// A primary's writes made durable through the sync log (`fenec_core::fs`,
/// Linux's, kept here by the test thread) -- through its `Tee`, which sends
/// a write once its durability ran -- are its file's again after the
/// machine lost the file's tail, and a replica started after is sent every
/// one of them.
#[cfg(unix)]
#[test]
fn a_primary_back_from_a_power_loss_holds_what_its_log_made_durable() {
    let d = dir("power");
    let file = d.join("p.fenec");
    let path = file.to_str().unwrap();
    fenec_core::fs::keep_sync_log(true);
    let (mut db, feed) = replication::open(path, replication::DEFAULT_BUFFER).unwrap();
    fenec_core::fs::keep_sync_log(false);
    db.fork(fresh_id()).unwrap();
    let durable = |db: &mut Database, sql: &str| {
        db.execute(&fenec_ql::parse_one(sql).unwrap()).unwrap();
        if let Some(d) = db.flush().unwrap() {
            d().unwrap();
        }
    };
    durable(&mut db, SCHEMA);
    for i in 0..30 {
        durable(
            &mut db,
            &format!("put items {{name: \"item {i}\", n: {i}, e: [{i}.0, 1.0, 0.5]}}"),
        );
    }
    let at_crash = db.change_seq();
    let log = std::fs::read(fenec_core::fs::beside(&file, "sync")).unwrap();
    assert_eq!(&log[..8], b"FENECSYN");
    let synced = u64::from_le_bytes(log[24..32].try_into().unwrap());
    std::mem::forget(db);
    std::mem::forget(feed);
    let f = std::fs::OpenOptions::new().write(true).open(&file).unwrap();
    f.set_len(synced).unwrap();
    drop(f);

    let p = primary(&file, replication::DEFAULT_BUFFER);
    assert_eq!(seq(&p), at_crash);
    assert_eq!(rows(&p, "get items").len(), 30);
    let r = replica(&d.join("r.fenec"), p.port);
    caught_up(&r, &p);
    assert_eq!(rows(&r, "get items"), rows(&p, "get items"));
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
    // missed rather than for an image. The file is looked at before the
    // follower starts: read after, the follower had often fetched the
    // writes it missed already.
    r.follower.as_ref().unwrap().halt();
    let at = seq(&r);
    write_some(&p, 50, 10);
    drop(r);
    let file = d.join("r.fenec");
    let (db, _) = replication::open(file.to_str().unwrap(), replication::DEFAULT_BUFFER).unwrap();
    assert_eq!(db.change_seq(), at);
    drop(db);
    let r = replica(&file, p.port);
    caught_up(&r, &p);
    assert_eq!(r.follower.as_ref().unwrap().images(), 0);
    assert_eq!(rows(&r, "get items"), rows(&p, "get items"));
}

/// A primary's `compact` writes its file beside the database, through the
/// feed as a server without replicas does, and the writes made while it
/// does reach the replica as any others: numbered on, and in the new file
/// when the primary opens it again.
#[test]
fn a_primary_compacts_beside_the_database_while_a_replica_follows() {
    let d = dir("compact");
    let file = d.join("p.fenec");
    let p = primary(&file, replication::DEFAULT_BUFFER);
    assert_eq!(query(&p, SCHEMA).0, 200);
    write_some(&p, 0, 40);
    assert_eq!(query(&p, "del items where n < 10").0, 200);
    let r = replica(&d.join("r.fenec"), p.port);
    caught_up(&r, &p);

    let compact = fenec_ql::parse_one("compact").unwrap();
    let mut calls = 0;
    Database::maintain_with(&p.db, &compact, &mut || {
        calls += 1;
        // The second copy is the file's: its side file is being written.
        assert_eq!(file.with_extension("fenec.beside").exists(), calls == 2);
        write_some(&p, 100 * calls, 5);
        assert_eq!(
            query(&p, &format!("del items where n = {}", 10 + calls)).0,
            200
        );
    })
    .unwrap()
    .unwrap();
    assert_eq!(calls, 2);
    assert!(!file.with_extension("fenec.beside").exists());
    write_some(&p, 300, 5);
    caught_up(&r, &p);
    assert_eq!(r.follower.as_ref().unwrap().images(), 0);
    for sql in [
        "get items",
        "get items select id near e [3.0, 1.0, 0.5] exact limit 3",
    ] {
        assert_eq!(rows(&r, sql), rows(&p, sql), "{sql}");
    }

    let (seq_was, rows_were) = (seq(&p), rows(&p, "get items"));
    r.follower.as_ref().unwrap().halt();
    drop(p);
    let back = fenec_core::fs::open(&file).unwrap();
    assert_eq!(back.change_seq(), seq_was);
    let g = back
        .query(&fenec_ql::parse_one("get items").unwrap(), &[])
        .unwrap();
    let got: Vec<(u64, Vec<Value>)> = g
        .rows()
        .unwrap()
        .rows
        .iter()
        .map(|r| (r.id, r.values.clone()))
        .collect();
    assert_eq!(got, rows_were);
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

/// A request as written, and the answer's status, head and body.
fn raw(port: u16, method: &str, path: &str, headers: &str, body: &str) -> (u16, String, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: x\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let (head, body) = out.split_once("\r\n\r\n").unwrap();
    (
        head[9..12].parse().unwrap(),
        head.to_string(),
        body.to_string(),
    )
}

/// A client that wrote on the primary reads its write on a replica: the
/// write's answer names the change it made (`Fenec-Seq`), and a read sent
/// with `Fenec-After` waits until the replica holds it -- never answered
/// from before it.
#[test]
fn a_read_sent_after_a_write_waits_for_it_on_a_replica() {
    let d = dir("ryw");
    let p = primary(&d.join("p.fenec"), replication::DEFAULT_BUFFER);
    let r = replica(&d.join("r.fenec"), p.port);
    query(&p, SCHEMA);
    caught_up(&r, &p);
    for i in 0..50 {
        let put = format!(
            "{{\"query\":\"put items {{name: \\\"ryw {i}\\\", n: {i}, e: [1.0, 0.5, {i}.0]}}\"}}"
        );
        let (status, head, body) = raw(p.port, "POST", "/query", "", &put);
        assert_eq!(status, 200, "{body}");
        let seq: u64 = head
            .lines()
            .find_map(|l| l.strip_prefix("Fenec-Seq: "))
            .expect("a write names its change")
            .trim()
            .parse()
            .unwrap();
        let get = format!("{{\"query\":\"get items where n = {i}\"}}");
        let (status, _, body) = raw(
            r.port,
            "POST",
            "/query",
            &format!("Fenec-After: {seq}\r\n"),
            &get,
        );
        assert_eq!(status, 200, "{body}");
        assert!(
            body.contains(&format!("ryw {i}")),
            "read {i} on the replica missed its write: {body}"
        );
    }
    // A replica held back: without the header a read is answered from
    // where it stands, and with it waits until the write is there.
    let f = r.follower.as_ref().unwrap();
    f.halt();
    let put = "{\"query\":\"put items {name: \\\"held back\\\", n: 999, e: [1.0, 0.5, 2.0]}\"}";
    let (_, head, _) = raw(p.port, "POST", "/query", "", put);
    let written: u64 = head
        .lines()
        .find_map(|l| l.strip_prefix("Fenec-Seq: "))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let get = "{\"query\":\"get items where n = 999\"}";
    let (_, _, body) = raw(r.port, "POST", "/query", "", get);
    assert!(!body.contains("held back"), "{body}");
    let port = r.port;
    let reader = std::thread::spawn(move || {
        let started = Instant::now();
        let answer = raw(
            port,
            "POST",
            "/query",
            &format!("Fenec-After: {written}\r\n"),
            get,
        );
        (started.elapsed(), answer)
    });
    std::thread::sleep(Duration::from_millis(300));
    f.start("replica".into()).unwrap();
    let (took, (status, _, body)) = reader.join().unwrap();
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("held back"), "{body}");
    assert!(took >= Duration::from_millis(250), "{took:?}");
    // A change that has not come: the wait, then 504 and where it stands.
    let started = Instant::now();
    let (status, _, body) = raw(
        r.port,
        "POST",
        "/query",
        &format!("Fenec-After: {}\r\nFenec-Wait: 300\r\n", seq(&p) + 1000),
        "{\"query\":\"get items count\"}",
    );
    assert_eq!(status, 504, "{body}");
    assert!(started.elapsed() >= Duration::from_millis(250));
    assert!(body.contains("\"seq\":"), "{body}");
    let (status, _, _) = raw(
        r.port,
        "POST",
        "/query",
        "Fenec-After: soon\r\n",
        "{\"query\":\"get items count\"}",
    );
    assert_eq!(status, 400);
}

/// The primary sweeps the rows past their time (`@ttl`) and its replica
/// applies the deletes as any it is sent; the replica itself never sweeps,
/// its writes being its primary's.
#[test]
fn a_sweep_on_the_primary_reaches_its_replica() {
    let d = dir("sweep");
    let primary = primary(&d.join("p.fenec"), replication::DEFAULT_BUFFER);
    let replica = replica(&d.join("r.fenec"), primary.port);
    for sql in [
        "create collection s (t text, seen timestamp @ttl(1h))",
        r#"put s [{t: "old", seen: 1}, {t: "live", seen: "2999-01-01"}, {t: "older", seen: 2}, {t: "none"}]"#,
    ] {
        let (status, body) = query(&primary, sql);
        assert_eq!(status, 200, "{body}");
    }
    caught_up(&replica, &primary);
    // Out of every read on both at once, before anything is deleted.
    let alive = |n: &Node| rows(n, "get s select t order id");
    let want = vec![
        (2, vec![Value::Text("live".into())]),
        (4, vec![Value::Text("none".into())]),
    ];
    assert_eq!(alive(&primary), want);
    assert_eq!(alive(&replica), want);
    // A replica sweeps nothing.
    assert_eq!(fenec_http::sweep::pass("replica", &replica.db), 0);
    let before = seq(&primary);
    assert_eq!(fenec_http::sweep::pass("primary", &primary.db), 2);
    assert_eq!(seq(&primary), before + 2, "a delete a row");
    caught_up(&replica, &primary);
    // Gone from both, at any time a read is answered at.
    for n in [&primary, &replica] {
        n.db.write().unwrap().set_clock(Some(0));
        assert_eq!(rows(n, "get s count")[0].1, vec![Value::Int(2)]);
        assert_eq!(alive(n), want);
    }
}

/// A primary and its replica each compact their file on their own, again
/// and again, while the primary takes updates: the replica follows across
/// every one -- the writes made during a compact reach it numbered on --
/// holds what the primary holds, and opened again over its compacted file
/// it asks for what it missed rather than for an image.
#[test]
fn a_primary_and_its_replica_compact_on_their_own_while_writes_go_on() {
    use fenec_core::engine::{CompactPolicy, Compactor};
    let d = dir("autocompact");
    let p = primary(&d.join("p.fenec"), replication::DEFAULT_BUFFER);
    assert_eq!(query(&p, SCHEMA).0, 200);
    write_some(&p, 0, 40);
    let rfile = d.join("r.fenec");
    let r = replica(&rfile, p.port);
    caught_up(&r, &p);

    let small = CompactPolicy {
        ratio: 0.5,
        floor: 16 << 10,
    };
    let every = Duration::from_millis(3);
    let compactors = [
        Compactor::start_every(&p.db, small, every).unwrap(),
        Compactor::start_every(&r.db, small, every).unwrap(),
    ];
    let pad = "y".repeat(300);
    for round in 0..40 {
        let (status, body) = query(
            &p,
            &format!("set items {{name: \"round {round} {pad}\", e: [{round}.0, 2.0, 1.0]}} where n < 30"),
        );
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            query(&p, &format!("del items where n = {}", 30 + round % 10)).0,
            200
        );
        write_some(&p, 100 + round * 2, 2);
    }
    drop(compactors);
    caught_up(&r, &p);
    let compacted = |n: &Node| n.db.read().unwrap().compactions();
    assert!(
        compacted(&p) >= 2,
        "the primary compacted {} times",
        compacted(&p)
    );
    assert!(
        compacted(&r) >= 2,
        "the replica compacted {} times",
        compacted(&r)
    );
    for sql in [
        "get items",
        "get items select id, n match name \"round\" limit 1000",
    ] {
        assert_eq!(rows(&r, sql), rows(&p, sql), "{sql}");
    }
    let near = |n: &Node| {
        let mut v = rows(
            n,
            "get items select id near e [3.0, 2.0, 1.0] exact limit 10000",
        );
        v.sort_by_key(|r| r.0);
        v
    };
    assert_eq!(near(&r), near(&p));

    // Opened again over the file it compacted, it goes on from where it was.
    let images = r.follower.as_ref().unwrap().images();
    r.follower.as_ref().unwrap().halt();
    let at = seq(&r);
    drop(r);
    write_some(&p, 500, 5);
    let (db, _) = replication::open(rfile.to_str().unwrap(), replication::DEFAULT_BUFFER).unwrap();
    assert_eq!(db.change_seq(), at);
    drop(db);
    let r = replica(&rfile, p.port);
    caught_up(&r, &p);
    assert_eq!(r.follower.as_ref().unwrap().images(), 0, "{images} before");
    assert_eq!(rows(&r, "get items"), rows(&p, "get items"));
}

/// The writes a raw replication stream has been sent: the last one, after
/// reading whatever the socket holds now and waiting for nothing more.
struct RawStream {
    s: TcpStream,
    buf: Vec<u8>,
    head: bool,
    last: u64,
}

impl RawStream {
    fn open(port: u16) -> RawStream {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            s,
            "GET /_replication?since=0 HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\n\r\n"
        )
        .unwrap();
        RawStream {
            s,
            buf: Vec::new(),
            head: false,
            last: 0,
        }
    }

    /// Reads what has arrived, blocking for none of it.
    fn drain(&mut self) {
        self.s.set_nonblocking(true).unwrap();
        let mut chunk = [0u8; 1 << 16];
        loop {
            match self.s.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("{e}"),
            }
        }
        self.s.set_nonblocking(false).unwrap();
        if !self.head {
            let Some(end) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                return;
            };
            assert!(self.buf.starts_with(b"HTTP/1.1 200"));
            self.buf.drain(..end + 4);
            self.head = true;
        }
        while self.buf.len() >= 9 {
            let len = u64::from_le_bytes(self.buf[1..9].try_into().unwrap()) as usize;
            if self.buf.len() < 9 + len {
                break;
            }
            let at = |i: usize| u64::from_le_bytes(self.buf[9 + i..17 + i].try_into().unwrap());
            match self.buf[0] {
                // [version][image][seq]...
                b'H' => self.last = self.last.max(at(2)),
                // [first][n]...
                b'W' => self.last = self.last.max(at(0) + at(8) - 1),
                _ => {}
            }
            self.buf.drain(..9 + len);
        }
    }
}

/// A durable write is answered once the stream to each replica has sent it:
/// sent after the answer, a write the client was told about was lost when
/// the primary died between the two, and its replica was promoted without
/// it (Trellis's node killed under eight writers, 3 runs in 60). Bytes in
/// the stream's socket reach the replica after the process is gone, so each
/// answered write must be in the stream already, with nothing waited for.
#[test]
fn a_durable_write_is_answered_once_its_replicas_streams_have_sent_it() {
    let d = dir("sent-first");
    let p = primary(&d.join("p.fenec"), replication::DEFAULT_BUFFER);
    assert_eq!(query(&p, SCHEMA).0, 200);
    let mut stream = RawStream::open(p.port);
    // Once the stream has its first write it is sending, and every write
    // after that waits for it.
    write_some(&p, 0, 1);
    let deadline = Instant::now() + Duration::from_secs(10);
    while stream.last < seq(&p) {
        assert!(
            Instant::now() < deadline,
            "the stream never sent the first write"
        );
        stream.drain();
        std::thread::sleep(Duration::from_millis(1));
    }
    for i in 1..1000 {
        write_some(&p, i, 1);
        let answered = seq(&p);
        stream.drain();
        assert!(
            stream.last >= answered,
            "write {answered} was answered before its stream sent it (sent {})",
            stream.last
        );
    }
}

/// A claim is a write, so a replica refuses it as it refuses any (403) --
/// at once, held or not: what a held claim waits for is the primary's.
#[test]
fn a_held_claim_on_a_replica_is_refused_at_once() {
    let d = dir("held");
    let p = primary(&d.join("p.fenec"), replication::DEFAULT_BUFFER);
    let r = replica(&d.join("r.fenec"), p.port);
    query(&p, SCHEMA);
    caught_up(&r, &p);
    let started = Instant::now();
    let (status, _, body) = raw(
        r.port,
        "POST",
        "/query",
        "Fenec-Wait: 5000\r\n",
        "{\"query\":\"set items {n: 1} where n = 999 order n limit 1 returning id\"}",
    );
    assert_eq!(status, 403, "{body}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}
