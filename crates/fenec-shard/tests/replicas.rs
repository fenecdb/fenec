//! Tenant replicas: a node whose tenants are followed by another, and a
//! failover that moves them there.
//!
//! Every tenant file has a feed of its own under
//! `/t/<tenant>/_replication`, so a replica node follows the tenant of the
//! same name on the node it is the standby of. The router creates and
//! deletes on both, and `POST /_shard/nodes/<n>/failover` promotes the
//! standby's copies and routes to them -- so a node lost is a promotion,
//! not an outage.

use fenec_http::tenants::{Replicated, Tenants};
use fenec_shard::directory::Directory;
use fenec_shard::{Config, Router};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const REPL: &str = "tenant-replication";

struct Node {
    addr: String,
    port: u16,
    dir: PathBuf,
    tenants: Arc<Tenants>,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A node; `follows` makes it the standby of the node at that address.
fn node(tag: &str, follows: Option<&str>) -> Node {
    node_over(tag, follows, |_| {})
}

/// [`node`], with `prepare` putting files in its directory before it starts.
fn node_over(tag: &str, follows: Option<&str>, prepare: impl FnOnce(&std::path::Path)) -> Node {
    let dir = std::env::temp_dir().join(format!("fenec-replicas-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    prepare(&dir);
    let tenants = Arc::new(Tenants::new(&dir).unwrap().with_replication(Replicated {
        token: REPL.into(),
        buffer: 8 << 20,
        upstream: follows.map(|a| format!("http://{a}")),
        sync_on_write: true,
    }));
    let cfg = fenec_http::Config {
        addr: "127.0.0.1:0".into(),
        admin_token: Some(format!("adm-{tag}")),
        // A replica is sent what an fsync covered, so a node that feeds one
        // syncs as it answers -- `fenec-pg --sync always` does this.
        sync_on_write: true,
        ..fenec_http::Config::default()
    };
    let server = fenec_http::Server::with_tenants(Arc::clone(&tenants), cfg);
    let listener = server.bind().unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    Node {
        addr: addr.to_string(),
        port: addr.port(),
        dir,
        tenants,
    }
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

fn query(port: u16, path: &str, sql: &str) -> (u16, String) {
    let mut body = String::from("{\"query\":");
    fenec_core::json::escape_into(&mut body, sql);
    body.push('}');
    call(port, "POST", path, &body, None)
}

/// Waits until `sql` on `port` answers with `want` in it.
fn until(port: u16, path: &str, sql: &str, want: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (_, body) = query(port, path, sql);
        if body.contains(want) {
            return;
        }
        assert!(Instant::now() < deadline, "{path} {sql}: {body}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_nodes_tenants_are_followed_by_its_standby_and_promoted_on_failover() {
    let n1 = node("n1", None);
    let n2 = node("n2", Some(&n1.addr));
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        upstream_timeout: Duration::from_secs(5),
        ..Config::default()
    };
    let router = Router::new(Directory::in_memory(), cfg);
    let listener = router.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = router.serve_on(listener);
    });

    // n2 is the standby of n1: every tenant placed on n1 gets a file on n2,
    // which follows it.
    for (name, nd) in [("n1", &n1), ("n2", &n2)] {
        let body = format!(r#"{{"addr":"{}","token":"adm-{name}"}}"#, nd.addr);
        assert_eq!(
            call(port, "PUT", &format!("/_shard/nodes/{name}"), &body, None).0,
            201
        );
    }
    let body = format!(
        r#"{{"addr":"{}","token":"adm-n1","standby":"n2"}}"#,
        n1.addr
    );
    assert_eq!(call(port, "PUT", "/_shard/nodes/n1", &body, None).0, 201);
    let (_, nodes) = call(port, "GET", "/_shard/nodes", "", None);
    assert!(nodes.contains("\"standby\":\"n2\""), "{nodes}");

    assert_eq!(call(port, "PUT", "/_shard/tenants/acme", "", None).0, 201);
    assert_eq!(
        query(
            port,
            "/t/acme/query",
            "create collection notes (title text)"
        )
        .0,
        200
    );
    assert_eq!(
        query(port, "/t/acme/query", r#"put notes {title: "one"}"#).0,
        200
    );

    // The row is on the replica, which took it from n1's feed.
    until(n2.port, "/t/acme/query", "get notes select title", "one");
    // And the replica refuses a write of its own while it follows.
    let (status, body) = query(n2.port, "/t/acme/query", r#"put notes {title: "no"}"#);
    assert_eq!(status, 403, "{body}");

    // n1 is lost. Its tenants are promoted on n2 and routed there.
    let (status, body) = call(port, "POST", "/_shard/nodes/n1/failover", "", None);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"promoted\":[\"acme\"]"), "{body}");
    let (_, tenants) = call(port, "GET", "/_shard/tenants", "", None);
    assert!(tenants.contains("\"node\":\"n2\""), "{tenants}");

    // Writes go on, through the router, on the node that has them now.
    let (status, body) = query(port, "/t/acme/query", r#"put notes {title: "after"}"#);
    assert_eq!(status, 200, "{body}");
    let (_, body) = query(port, "/t/acme/query", "get notes count");
    assert!(body.contains('2'), "{body}");
    // And the failover is over, the pair with it: n1 has no standby to
    // fail over to until it rejoins as n2's.
    let (status, body) = call(port, "POST", "/_shard/nodes/n1/failover", "", None);
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("no standby"), "{body}");

    drop(n1);
    drop(n2);
}

#[test]
fn a_tenant_deleted_through_the_router_goes_from_the_standby_as_well() {
    let n1 = node("d1", None);
    let n2 = node("d2", Some(&n1.addr));
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        upstream_timeout: Duration::from_secs(5),
        ..Config::default()
    };
    let router = Router::new(Directory::in_memory(), cfg);
    let listener = router.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = router.serve_on(listener);
    });
    for (name, nd) in [("n1", &n1), ("n2", &n2)] {
        let body = format!(
            r#"{{"addr":"{}","token":"adm-{}"}}"#,
            nd.addr,
            if name == "n1" { "d1" } else { "d2" }
        );
        assert_eq!(
            call(port, "PUT", &format!("/_shard/nodes/{name}"), &body, None).0,
            201
        );
    }
    let body = format!(
        r#"{{"addr":"{}","token":"adm-d1","standby":"n2"}}"#,
        n1.addr
    );
    assert_eq!(call(port, "PUT", "/_shard/nodes/n1", &body, None).0, 201);

    assert_eq!(call(port, "PUT", "/_shard/tenants/acme", "", None).0, 201);
    assert!(n2.tenants.names().contains(&"acme".to_string()));
    assert_eq!(
        call(port, "DELETE", "/_shard/tenants/acme", "", None).0,
        204
    );
    assert!(!n1.tenants.names().contains(&"acme".to_string()));
    assert!(!n2.tenants.names().contains(&"acme".to_string()));
}

/// A router with nodes `n1` and `n2`, `n2` the standby of `n1`, and the
/// tenant `acme` placed on `n1` with a row on both.
fn paired(tag: &str) -> (Node, Node, u16) {
    let n1 = node(&format!("{tag}1"), None);
    let n2 = node(&format!("{tag}2"), Some(&n1.addr));
    let port = router();
    register(port, "n1", &n1, &format!("adm-{tag}1"), None);
    register(port, "n2", &n2, &format!("adm-{tag}2"), None);
    register(port, "n1", &n1, &format!("adm-{tag}1"), Some("n2"));
    assert_eq!(
        call(
            port,
            "PUT",
            "/_shard/tenants/acme",
            r#"{"node":"n1"}"#,
            None
        )
        .0,
        201
    );
    assert_eq!(
        query(
            port,
            "/t/acme/query",
            "create collection notes (title text)"
        )
        .0,
        200
    );
    assert_eq!(
        query(port, "/t/acme/query", r#"put notes {title: "one"}"#).0,
        200
    );
    until(n2.port, "/t/acme/query", "get notes select title", "one");
    (n1, n2, port)
}

fn router() -> u16 {
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        upstream_timeout: Duration::from_secs(10),
        ..Config::default()
    };
    let router = Router::new(Directory::in_memory(), cfg);
    let listener = router.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = router.serve_on(listener);
    });
    port
}

fn register(port: u16, name: &str, nd: &Node, token: &str, standby: Option<&str>) -> String {
    let standby = standby.map_or(String::new(), |s| format!(r#","standby":"{s}""#));
    let body = format!(r#"{{"addr":"{}","token":"{token}"{standby}}}"#, nd.addr);
    let (status, body) = call(port, "PUT", &format!("/_shard/nodes/{name}"), &body, None);
    assert_eq!(status, 201, "{body}");
    body
}

fn is_open(nd: &Node, tenant: &str) -> bool {
    nd.tenants.stats().open.iter().any(|(n, _, _)| n == tenant)
}

/// A replica node's tenant is not closed as idle while it follows: its
/// follower holds the database, so closed, the tenant left it writing the
/// file and the next open put a second database over it. Once promoted it
/// closes like any other -- and opens again as the primary it became,
/// not as a replica of the node it failed over from.
#[test]
fn a_replica_tenant_stays_open_while_it_follows_and_a_primary_once_promoted() {
    let (_n1, n2, port) = paired("idle");
    assert_eq!(n2.tenants.close_idle(Duration::ZERO), 0);
    assert!(is_open(&n2, "acme"));
    assert_eq!(
        query(port, "/t/acme/query", r#"put notes {title: "two"}"#).0,
        200
    );
    until(n2.port, "/t/acme/query", "get notes select title", "two");

    let (status, body) = call(port, "POST", "/_shard/nodes/n1/failover", "", None);
    assert_eq!(status, 200, "{body}");
    // Every tenant moved: the pair is over, and n2 takes tenants again.
    let (_, nodes) = call(port, "GET", "/_shard/nodes", "", None);
    assert!(!nodes.contains("\"standby\":\"n2\""), "{nodes}");

    // Promoted, the tenant is closed as any idle one is, and opens again
    // as the primary it is now: writes go on, on n2.
    assert_eq!(n2.tenants.close_idle(Duration::ZERO), 1);
    assert!(!is_open(&n2, "acme"));
    let (status, body) = query(port, "/t/acme/query", r#"put notes {title: "three"}"#);
    assert_eq!(status, 200, "{body}");
    let (_, body) = query(port, "/t/acme/query", "get notes count");
    assert!(body.contains('3'), "{body}");
}

/// The old primary rejoins as the standby of the node its tenants failed
/// over to: recording the pair has the router give the rejoining node a
/// following copy of each tenant -- its own file, a primary's, told to
/// follow -- and the writes taken since the failover reach it.
#[test]
fn a_node_rejoining_as_the_standby_follows_the_new_primary() {
    let (n1, n2, port) = paired("rejoin");
    let (status, body) = call(port, "POST", "/_shard/nodes/n1/failover", "", None);
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        query(port, "/t/acme/query", r#"put notes {title: "after"}"#).0,
        200
    );

    // n1 comes back following n2, with the file it had: a primary's.
    let n1_file = n1.dir.join("acme.fenec");
    let n3 = node_over("rejoin3", Some(&n2.addr), |dir| {
        std::fs::copy(&n1_file, dir.join("acme.fenec")).unwrap();
    });
    let (status, body) = query(n3.port, "/t/acme/query", "get notes count");
    assert_eq!(status, 200, "{body}");
    register(port, "n3", &n3, "adm-rejoin3", None);
    let body = register(port, "n2", &n2, "adm-rejoin2", Some("n3"));
    assert!(body.contains("\"unreplicated\":[]"), "{body}");

    until(n3.port, "/t/acme/query", "get notes select title", "after");
    let (status, body) = query(n3.port, "/t/acme/query", r#"put notes {title: "no"}"#);
    assert_eq!(status, 403, "{body}");
    assert_eq!(
        query(port, "/t/acme/query", r#"put notes {title: "later"}"#).0,
        200
    );
    until(n3.port, "/t/acme/query", "get notes select title", "later");
}

/// A standby holds its primary's tenants as replicas, so no tenant is
/// placed on one or moved to one. It won the placement on a tie of empty
/// disks, the name sorting first, and a move onto a source's own standby
/// deleted the tenant from every node while answering "source_removed".
#[test]
fn a_tenant_is_never_placed_on_or_moved_to_a_standby() {
    let main = node("pmain", None);
    let backup = node("pbackup", Some(&main.addr));
    let other = node("pother", None);
    let port = router();
    register(port, "main", &main, "adm-pmain", None);
    register(port, "backup", &backup, "adm-pbackup", None);
    register(port, "main", &main, "adm-pmain", Some("backup"));
    for t in ["t1", "t2", "t3"] {
        let (status, body) = call(port, "PUT", &format!("/_shard/tenants/{t}"), "", None);
        assert_eq!(status, 201, "{body}");
        assert!(body.contains("\"node\":\"main\""), "{body}");
    }
    let (status, body) = call(
        port,
        "POST",
        "/_shard/tenants/t1/move",
        r#"{"to":"backup"}"#,
        None,
    );
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("standby"), "{body}");
    let (_, body) = call(port, "GET", "/_shard/tenants", "", None);
    assert!(!body.contains("\"node\":\"backup\""), "{body}");
    // A node that is nobody's standby takes a move as it always did.
    register(port, "other", &other, "adm-pother", None);
    let (status, body) = call(
        port,
        "POST",
        "/_shard/tenants/t1/move",
        r#"{"to":"other"}"#,
        None,
    );
    assert_eq!(status, 200, "{body}");
}

/// A delete that gives up with 409 leaves the tenant as it was: its
/// replicas go on being fed. It once closed the feed first and left it
/// closed, so the tenant took writes its standby was never sent.
#[test]
fn a_delete_that_gives_up_leaves_the_tenant_replicated() {
    let (n1, n2, port) = paired("keep");
    let held = n1.tenants.get("acme").unwrap();
    let (status, body) = call(port, "DELETE", "/_shard/tenants/acme", "", None);
    assert_eq!(status, 409, "{body}");
    drop(held);
    assert_eq!(
        query(port, "/t/acme/query", r#"put notes {title: "kept"}"#).0,
        200
    );
    until(n2.port, "/t/acme/query", "get notes select title", "kept");
}

/// A replica's file on a node that follows nobody -- started without
/// --replica-of after a failover left the tenant unpromoted -- refuses
/// writes, and is promoted where it stands. Nothing on the running node
/// could make it take writes before.
#[test]
fn a_replica_file_on_a_node_following_nobody_is_promoted_where_it_stands() {
    let n = node_over("orphan", None, |dir| {
        let mut db = fenec_core::fs::open(dir.join("acme.fenec")).unwrap();
        db.execute(&fenec_ql::parse_one("create collection notes (title text)").unwrap())
            .unwrap();
        db.fork(7).unwrap();
        let lineage = db.history().lineage.clone();
        db.follow(lineage).unwrap();
        db.sync().unwrap();
    });
    let (status, body) = query(n.port, "/t/acme/query", r#"put notes {title: "no"}"#);
    assert_eq!(status, 403, "{body}");
    let (status, body) = call(
        n.port,
        "POST",
        "/_admin/tenants/acme/promote",
        "",
        Some("adm-orphan"),
    );
    assert_eq!(status, 200, "{body}");
    let (status, body) = query(n.port, "/t/acme/query", r#"put notes {title: "yes"}"#);
    assert_eq!(status, 200, "{body}");
    let (status, body) = call(
        n.port,
        "POST",
        "/_admin/tenants/acme/promote",
        "",
        Some("adm-orphan"),
    );
    assert_eq!(status, 409, "{body}");
}
