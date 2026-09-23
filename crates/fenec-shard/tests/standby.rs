//! A standby router: the directory replicated from the primary.
//!
//! The standby follows the primary's directory file the way a replica
//! follows a database -- it is one -- so it knows the same placements and
//! forwards to the same nodes, refuses `/_shard/` changes while it follows,
//! and takes them once promoted. A router lost is then a promotion, not an
//! outage.

use fenec_http::replication::{fresh_id, Follower, Replication};
use fenec_http::tenants::Tenants;
use fenec_shard::directory::Directory;
use fenec_shard::{Config, Router};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const TOKEN: &str = "directory-secret";

struct Node {
    addr: String,
    dir: PathBuf,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn node(tag: &str) -> Node {
    let dir = std::env::temp_dir().join(format!("fenec-standby-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let tenants = Arc::new(Tenants::new(&dir).unwrap());
    let cfg = fenec_http::Config {
        addr: "127.0.0.1:0".into(),
        admin_token: Some("adm".into()),
        ..fenec_http::Config::default()
    };
    let server = fenec_http::Server::with_tenants(tenants, cfg);
    let listener = server.bind().unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    Node { addr, dir }
}

struct Files(PathBuf);

impl Drop for Files {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn dir_file(tag: &str, name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fenec-standby-dirs-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.join(name)
}

/// A router over `path`, following `upstream` when there is one.
fn router(path: &Path, upstream: Option<&str>) -> u16 {
    let (mut db, feed) =
        fenec_http::replication::open(path.to_str().unwrap(), 8 << 20).expect("open");
    match upstream {
        Some(_) => {
            let lineage = db.history().lineage.clone();
            db.follow(lineage).unwrap();
        }
        None => {
            if db.history().lineage.is_empty() {
                db.fork(fresh_id()).unwrap();
                db.sync().unwrap();
            }
        }
    }
    let db = Arc::new(RwLock::new(db));
    let dir = Directory::load(Arc::clone(&db)).expect("directory");
    let follower = upstream.map(|url| {
        let f = Follower::new(
            url,
            TOKEN.into(),
            Arc::clone(&db),
            Some(Arc::clone(&feed)),
            true,
        )
        .unwrap();
        let run = Arc::clone(&f);
        std::thread::spawn(move || run.run());
        f
    });
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        upstream_timeout: Duration::from_secs(5),
        ..Config::default()
    };
    let router = Router::replicated(
        dir,
        cfg,
        Replication::new(TOKEN.into(), Some(feed), follower),
    );
    let listener = router.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = router.serve_on(listener);
    });
    port
}

fn call(port: u16, method: &str, target: &str, body: &str, auth: Option<&str>) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let auth = auth.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    write!(
        s,
        "{method} {target} HTTP/1.1\r\nHost: x\r\n{auth}Content-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status = out[9..12].parse().unwrap();
    let body = out.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (status, body.to_string())
}

/// Waits for `target` on the standby to answer with `want` inside it.
fn until(port: u16, target: &str, want: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (_, body) = call(port, "GET", target, "", None);
        if body.contains(want) {
            return body;
        }
        assert!(Instant::now() < deadline, "{target}: {body}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_standby_forwards_from_the_primarys_directory_and_takes_over_when_promoted() {
    let n1 = node("n1");
    let files = Files(dir_file("main", "x").parent().unwrap().to_path_buf());
    let primary = router(&dir_file("main", "primary.fenec"), None);

    // The node and a tenant on it, placed through the primary.
    let (status, body) = call(
        primary,
        "PUT",
        "/_shard/nodes/n1",
        &format!(r#"{{"addr":"{}","token":"adm"}}"#, n1.addr),
        None,
    );
    assert_eq!(status, 201, "{body}");
    assert_eq!(
        call(primary, "PUT", "/_shard/tenants/acme", "", None).0,
        201
    );
    let (status, body) = call(
        primary,
        "POST",
        "/t/acme/query",
        r#"{"query":"create collection notes (title text)"}"#,
        None,
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        call(
            primary,
            "POST",
            "/t/acme/query",
            r#"{"query":"put notes {title: \"one\"}"}"#,
            None
        )
        .0,
        200
    );

    // The standby follows the directory, and forwards from the same
    // placements to the same node.
    let standby = router(
        &dir_file("main", "standby.fenec"),
        Some(&format!("http://127.0.0.1:{primary}")),
    );
    until(standby, "/_shard/tenants", "acme");
    let (status, body) = call(
        standby,
        "POST",
        "/t/acme/query",
        r#"{"query":"get notes select title"}"#,
        None,
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("one"), "{body}");

    // What it does not take is a change to the directory.
    let (status, body) = call(standby, "PUT", "/_shard/tenants/beta", "", None);
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("primary"), "{body}");
    let (status, body) = call(standby, "DELETE", "/_shard/nodes/n1", "", None);
    assert_eq!(status, 409, "{body}");

    // Promoted -- the primary is gone, say -- it takes them.
    let (status, body) = call(standby, "POST", "/_replication/promote", "", Some(TOKEN));
    assert_eq!(status, 200, "{body}");
    let (status, body) = call(standby, "PUT", "/_shard/tenants/beta", "", None);
    assert_eq!(status, 201, "{body}");
    let (status, body) = call(
        standby,
        "POST",
        "/t/beta/query",
        r#"{"query":"create collection notes (title text)"}"#,
        None,
    );
    assert_eq!(status, 200, "{body}");
    // The tenant the primary placed is still served, from the same node.
    let (status, body) = call(
        standby,
        "POST",
        "/t/acme/query",
        r#"{"query":"get notes count"}"#,
        None,
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.contains('1'), "{body}");
    drop(files);
}

#[test]
fn a_standbys_directory_file_does_not_open_as_a_primary() {
    let files = Files(dir_file("alone", "x").parent().unwrap().to_path_buf());
    let path = dir_file("alone", "standby.fenec");
    let mut db = fenec_core::fs::open(&path).unwrap();
    db.fork(fresh_id()).unwrap();
    let lineage = db.history().lineage.clone();
    db.follow(lineage).unwrap();
    db.sync().unwrap();
    drop(db);

    // A directory that follows refuses a write of its own, so a router over
    // it would answer every /_shard/ change with 409 -- which is what the
    // binary's --promote is for.
    let db = Arc::new(RwLock::new(fenec_core::fs::open(&path).unwrap()));
    let mut dir = Directory::load(Arc::clone(&db)).unwrap();
    assert!(dir.following());
    assert!(dir
        .set_node(
            "n1",
            fenec_shard::directory::Node {
                addr: "127.0.0.1:1".into(),
                token: "t".into(),
            }
        )
        .is_err());
    drop(files);
}

/// A router rejoining as the standby of the one promoted over it takes that
/// one's directory as an image -- and when the image lands on the change
/// its own maps were read at, it still reads them again. The counter alone
/// missed it: both routers had made one change since the promotion, and
/// the rejoined one went on routing from the placements it had made itself.
#[test]
fn a_standby_that_takes_an_image_at_the_same_change_reads_its_maps_again() {
    let n1 = node("rejoin");
    let files = Files(dir_file("rejoin", "x").parent().unwrap().to_path_buf());
    let a_path = dir_file("rejoin", "a.fenec");
    let a = router(&a_path, None);
    let (status, body) = call(
        a,
        "PUT",
        "/_shard/nodes/n1",
        &format!(r#"{{"addr":"{}","token":"adm"}}"#, n1.addr),
        None,
    );
    assert_eq!(status, 201, "{body}");
    let b = router(
        &dir_file("rejoin", "b.fenec"),
        Some(&format!("http://127.0.0.1:{a}")),
    );
    until(b, "/_shard/nodes", "n1");

    // b is promoted while a is still taking changes: one each.
    let (status, body) = call(b, "POST", "/_replication/promote", "", Some(TOKEN));
    assert_eq!(status, 200, "{body}");
    assert_eq!(call(a, "PUT", "/_shard/tenants/xa", "", None).0, 201);
    assert_eq!(call(b, "PUT", "/_shard/tenants/yb", "", None).0, 201);

    // a comes back as b's standby, from the file it had.
    let a2_path = dir_file("rejoin", "a2.fenec");
    std::fs::copy(&a_path, &a2_path).unwrap();
    let a2 = router(&a2_path, Some(&format!("http://127.0.0.1:{b}")));
    let tenants = until(a2, "/_shard/tenants", "yb");
    assert!(!tenants.contains("xa"), "{tenants}");
    drop(files);
}
