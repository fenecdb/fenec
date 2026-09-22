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
    let dir = std::env::temp_dir().join(format!("fenec-replicas-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
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
    // And the failover is over: a second one has nothing to promote there.
    let (status, body) = call(port, "POST", "/_shard/nodes/n1/failover", "", None);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"promoted\":[]"), "{body}");

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
