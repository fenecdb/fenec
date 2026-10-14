//! Regions and zones: a node runs in a region and a datacenter (a zone), a
//! tenant lives in one region -- its node's -- and its replica is kept in
//! that region, in another zone where the region has one.
//!
//! What must hold: a tenant is placed in the region it names and refused
//! where the region has no node; its copy never leaves its region, so a
//! failover keeps it there; and it leaves its region only by a move that
//! names the region it goes to.

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
    name: &'static str,
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

/// A node that feeds and follows tenants, named `name` in the directory.
fn node(tag: &str, name: &'static str) -> Node {
    let dir =
        std::env::temp_dir().join(format!("fenec-regions-{tag}-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let tenants = Arc::new(Tenants::new(&dir).unwrap().with_replication(Replicated {
        token: REPL.into(),
        buffer: 8 << 20,
        upstream: None,
        sync_on_write: true,
    }));
    let cfg = fenec_http::Config {
        addr: "127.0.0.1:0".into(),
        admin_token: Some(format!("adm-{name}")),
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
        name,
        addr: addr.to_string(),
        port: addr.port(),
        dir,
        tenants,
    }
}

fn router(cfg: Config) -> u16 {
    let router = Router::new(
        Directory::in_memory(),
        Config {
            addr: "127.0.0.1:0".into(),
            upstream_timeout: Duration::from_secs(10),
            ..cfg
        },
    );
    let listener = router.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = router.serve_on(listener);
    });
    port
}

fn call(port: u16, method: &str, target: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    write!(
        s,
        "{method} {target} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n\
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

fn query(port: u16, tenant: &str, sql: &str) -> (u16, String) {
    let mut body = String::from("{\"query\":");
    fenec_core::json::escape_into(&mut body, sql);
    body.push('}');
    call(port, "POST", &format!("/t/{tenant}/query"), &body)
}

/// Waits until `sql` on `port` answers with `want` in it.
fn until(port: u16, tenant: &str, sql: &str, want: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (_, body) = query(port, tenant, sql);
        if body.contains(want) {
            return;
        }
        assert!(Instant::now() < deadline, "{tenant} {sql}: {body}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Records `nd` in `region` and `zone`.
fn register(port: u16, nd: &Node, region: &str, zone: &str) {
    let body = format!(
        r#"{{"addr":"{}","token":"adm-{}","region":"{region}","zone":"{zone}"}}"#,
        nd.addr, nd.name
    );
    let (status, body) = call(port, "PUT", &format!("/_shard/nodes/{}", nd.name), &body);
    assert_eq!(status, 201, "{body}");
}

/// The value of `key`, a text, in a JSON object's text.
fn text_of(json: &str, key: &str) -> Option<String> {
    let rest = json.split(&format!("\"{key}\":\"")).nth(1)?;
    Some(rest.split('"').next()?.to_string())
}

/// `GET /_shard/tenants/<t>`: its node, region and replica.
fn shown(port: u16, tenant: &str) -> (String, Option<String>, Option<String>) {
    let (status, body) = call(port, "GET", &format!("/_shard/tenants/{tenant}"), "");
    assert_eq!(status, 200, "{body}");
    (
        text_of(&body, "node").unwrap(),
        text_of(&body, "region"),
        text_of(&body, "replica"),
    )
}

/// The sample `line` in a scrape, if it has one.
fn sample(scrape: &str, line: &str) -> Option<String> {
    scrape
        .lines()
        .find_map(|l| l.strip_prefix(line)?.strip_prefix(' ').map(str::to_string))
}

#[test]
fn a_tenant_is_placed_in_the_region_it_names_and_refused_where_none_is() {
    let nodes = [
        node("place", "eu1"),
        node("place", "eu2"),
        node("place", "us1"),
    ];
    let port = router(Config {
        default_region: Some("us".into()),
        ..Config::default()
    });
    register(port, &nodes[0], "eu", "eu-a");
    register(port, &nodes[1], "eu", "eu-b");
    register(port, &nodes[2], "us", "us-a");
    let (_, list) = call(port, "GET", "/_shard/nodes", "");
    assert!(
        list.contains(r#"{"name":"eu2","addr":""#)
            && list.contains(r#""region":"eu","zone":"eu-b"}"#),
        "{list}"
    );

    // In the region named, on the node of the fewest bytes there.
    for t in ["a1", "a2", "a3"] {
        let (status, body) = call(
            port,
            "PUT",
            &format!("/_shard/tenants/{t}"),
            r#"{"region":"eu"}"#,
        );
        assert_eq!(status, 201, "{body}");
        assert!(body.contains(r#""region":"eu""#), "{body}");
        let (node, region, _) = shown(port, t);
        assert!(node.starts_with("eu"), "{t} on {node}");
        assert_eq!(region.as_deref(), Some("eu"));
    }
    // Naming neither region nor node, in the default region.
    let (status, body) = call(port, "PUT", "/_shard/tenants/b1", "");
    assert_eq!(status, 201, "{body}");
    assert_eq!(shown(port, "b1").0, "us1");
    // A node named is the node, the default region aside...
    let (status, body) = call(port, "PUT", "/_shard/tenants/b2", r#"{"node":"eu1"}"#);
    assert_eq!(status, 201, "{body}");
    // ... and refused beside a region it is not in.
    let (status, body) = call(
        port,
        "PUT",
        "/_shard/tenants/b3",
        r#"{"node":"us1","region":"eu"}"#,
    );
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("region `us`, not region `eu`"), "{body}");

    // A region no node is in is refused, and nothing is left behind.
    let (status, body) = call(port, "PUT", "/_shard/tenants/c1", r#"{"region":"asia"}"#);
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("no node in region `asia`"), "{body}");
    let (status, _) = call(port, "GET", "/_shard/tenants/c1", "");
    assert_eq!(status, 404);
    assert!(nodes
        .iter()
        .all(|n| !n.tenants.names().contains(&"c1".to_string())));
    for bad in [r#"{"region":"EU!"}"#, r#"{"region":7}"#] {
        let (status, body) = call(port, "PUT", "/_shard/tenants/c2", bad);
        assert_eq!(status, 400, "{bad}: {body}");
    }

    // The list carries each tenant's region, and gives one region's.
    let (_, list) = call(port, "GET", "/_shard/tenants?region=us", "");
    assert!(list.contains(r#""name":"b1""#), "{list}");
    assert!(!list.contains(r#""name":"a1""#), "{list}");
    let (_, list) = call(port, "GET", "/_shard/tenants", "");
    assert_eq!(list.matches(r#""region":"eu""#).count(), 4, "{list}");

    // A node holding tenants keeps its region; its zone and token change.
    let (status, body) = call(
        port,
        "PUT",
        "/_shard/nodes/us1",
        &format!(
            r#"{{"addr":"{}","token":"adm-us1","region":"eu"}}"#,
            nodes[2].addr
        ),
    );
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("holds tenants"), "{body}");
    let (status, body) = call(
        port,
        "PUT",
        "/_shard/nodes/us1",
        &format!(
            r#"{{"addr":"{}","token":"adm-us1","zone":"us-b"}}"#,
            nodes[2].addr
        ),
    );
    assert_eq!(status, 201, "{body}");
    let (_, list) = call(port, "GET", "/_shard/nodes", "");
    assert!(list.contains(r#""region":"us","zone":"us-b""#), "{list}");
    // A standby in another region than its node is refused.
    let (status, body) = call(
        port,
        "PUT",
        "/_shard/nodes/eu1",
        &format!(
            r#"{{"addr":"{}","token":"adm-eu1","standby":"us1"}}"#,
            nodes[0].addr
        ),
    );
    assert_eq!(status, 409, "{body}");
    assert!(
        body.contains("a standby is kept in its node's region"),
        "{body}"
    );
}

#[test]
fn a_replica_stays_in_its_region_in_another_zone_and_a_failover_keeps_it_there() {
    let nodes = [
        node("zone", "ea1"),
        node("zone", "ea2"),
        node("zone", "eb1"),
        node("zone", "ua1"),
        node("zone", "ua2"),
    ];
    let port = router(Config {
        replicas: true,
        ..Config::default()
    });
    for (nd, region, zone) in [
        (&nodes[0], "eu", "a"),
        (&nodes[1], "eu", "a"),
        (&nodes[2], "eu", "b"),
        (&nodes[3], "us", "a"),
        (&nodes[4], "us", "a"),
    ] {
        register(port, nd, region, zone);
    }

    // On ea1: each replica on eb1, the one node of another zone in eu --
    // past ea2, which holds fewer, and never in us.
    for t in ["t1", "t2", "t3"] {
        let (status, body) = call(
            port,
            "PUT",
            &format!("/_shard/tenants/{t}"),
            r#"{"node":"ea1"}"#,
        );
        assert_eq!(status, 201, "{body}");
        assert!(body.contains(r#""replica_on":"eb1""#), "{body}");
        assert_eq!(query(port, t, "create collection notes (n int)").0, 200);
        assert_eq!(query(port, t, "put notes {n: 1}").0, 200);
        until(nodes[2].port, t, "get notes count", "1");
    }
    // In us both nodes are in one zone: the replica goes there all the
    // same, and says so in the tenant and in the scrape.
    let (status, body) = call(port, "PUT", "/_shard/tenants/u1", r#"{"node":"ua1"}"#);
    assert_eq!(status, 201, "{body}");
    assert!(body.contains(r#""replica_on":"ua2""#), "{body}");
    let (_, one) = call(port, "GET", "/_shard/tenants/u1", "");
    assert!(one.contains(r#""same_zone":true"#), "{one}");
    let (_, one) = call(port, "GET", "/_shard/tenants/t1", "");
    assert!(!one.contains("same_zone"), "{one}");
    let (_, scrape) = call(port, "GET", "/_metrics", "");
    assert_eq!(
        sample(&scrape, r#"fenec_router_same_zone_copies{region="us"}"#).as_deref(),
        Some("1"),
        "{scrape}"
    );
    assert_eq!(
        sample(&scrape, r#"fenec_router_same_zone_copies{region="eu"}"#).as_deref(),
        Some("0")
    );
    assert_eq!(
        sample(&scrape, r#"fenec_router_tenants{node="ea1",region="eu"}"#).as_deref(),
        Some("3")
    );
    assert_eq!(
        sample(&scrape, r#"fenec_router_replicas{node="eb1",region="eu"}"#).as_deref(),
        Some("3")
    );

    // ea1 is lost: its tenants are promoted on eb1, in their region.
    let (status, body) = call(port, "POST", "/_shard/nodes/ea1/failover", "");
    assert_eq!(status, 200, "{body}");
    for t in ["t1", "t2", "t3"] {
        assert_eq!(shown(port, t), ("eb1".into(), Some("eu".into()), None));
        assert_eq!(query(port, t, "put notes {n: 2}").0, 200);
    }
    // The repair gives each a replica in another zone of eu again: ea1,
    // holding their copies, before ea2.
    let (status, body) = call(port, "POST", "/_shard/replicas", "");
    assert_eq!(status, 200, "{body}");
    for t in ["t1", "t2", "t3"] {
        assert_eq!(shown(port, t).2.as_deref(), Some("ea1"), "{body}");
        until(nodes[0].port, t, "get notes count", "2");
    }
    assert!(nodes[3].tenants.names().iter().all(|t| t == "u1"));
}

#[test]
fn a_tenant_leaves_its_region_by_a_move_that_names_the_one_it_goes_to() {
    let nodes = [
        node("move", "eu1"),
        node("move", "eu2"),
        node("move", "us1"),
        node("move", "us2"),
    ];
    let port = router(Config {
        replicas: true,
        ..Config::default()
    });
    register(port, &nodes[0], "eu", "a");
    register(port, &nodes[1], "eu", "b");
    register(port, &nodes[2], "us", "a");
    register(port, &nodes[3], "us", "b");
    let (status, body) = call(port, "PUT", "/_shard/tenants/acme", r#"{"node":"eu1"}"#);
    assert_eq!(status, 201, "{body}");
    assert!(body.contains(r#""replica_on":"eu2""#), "{body}");
    assert_eq!(
        query(port, "acme", "create collection notes (n int)").0,
        200
    );
    assert_eq!(query(port, "acme", "put notes {n: 1}").0, 200);

    // A node of another region alone does not take it there.
    let (status, body) = call(port, "POST", "/_shard/tenants/acme/move", r#"{"to":"us1"}"#);
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("names the region it goes to"), "{body}");
    let (status, body) = call(
        port,
        "POST",
        "/_shard/tenants/acme/move",
        r#"{"to":"us1","region":"eu"}"#,
    );
    assert_eq!(status, 409, "{body}");
    assert_eq!(shown(port, "acme").0, "eu1");

    // Named, it goes, and its region with it: its replica in eu is taken
    // off, and another placed in us, in the other zone.
    let (status, body) = call(
        port,
        "POST",
        "/_shard/tenants/acme/move",
        r#"{"to":"us1","region":"us"}"#,
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""region":"us""#), "{body}");
    assert!(body.contains(r#""replica_on":"us2""#), "{body}");
    assert_eq!(
        shown(port, "acme"),
        ("us1".into(), Some("us".into()), Some("us2".into()))
    );
    for eu in &nodes[..2] {
        assert!(
            !eu.tenants.names().contains(&"acme".to_string()),
            "{}",
            eu.name
        );
    }
    assert_eq!(query(port, "acme", "put notes {n: 2}").0, 200);
    let (_, body) = query(port, "acme", "get notes count");
    assert!(body.contains('2'), "{body}");
    until(nodes[3].port, "acme", "get notes count", "2");

    // A region alone: the node of the fewest bytes there, back in eu.
    let (status, body) = call(
        port,
        "POST",
        "/_shard/tenants/acme/move",
        r#"{"region":"eu"}"#,
    );
    assert_eq!(status, 200, "{body}");
    let (node, region, replica) = shown(port, "acme");
    assert!(node.starts_with("eu"), "{node}");
    assert_eq!(region.as_deref(), Some("eu"));
    assert!(replica.is_some_and(|r| r.starts_with("eu") && r != node));
    let (status, body) = call(
        port,
        "POST",
        "/_shard/tenants/acme/move",
        r#"{"region":"asia"}"#,
    );
    assert_eq!(status, 409, "{body}");
    let (status, body) = call(port, "POST", "/_shard/tenants/acme/move", "{}");
    assert_eq!(status, 400, "{body}");
}
