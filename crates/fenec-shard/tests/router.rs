//! The router end to end: two in-process `--dir` nodes and a router in
//! front of them, driven over raw TCP.

use fenec_http::tenants::Tenants;
use fenec_shard::directory::Directory;
use fenec_shard::{Config, Router};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Node {
    addr: String,
    dir: PathBuf,
    tenants: Arc<Tenants>,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn node(tag: &str) -> Node {
    let dir = std::env::temp_dir().join(format!(
        "fenec-shard-{tag}-{}-{:?}",
        std::process::id(),
        Instant::now()
    ));
    let tenants = Arc::new(Tenants::new(&dir).unwrap());
    let cfg = fenec_http::Config {
        addr: "127.0.0.1:0".into(),
        admin_token: Some(format!("adm-{tag}")),
        stream_keepalive: Duration::from_millis(80),
        ..fenec_http::Config::default()
    };
    let server = fenec_http::Server::with_tenants(Arc::clone(&tenants), cfg);
    let listener = server.bind().unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    Node { addr, dir, tenants }
}

struct Cluster {
    port: u16,
    nodes: Vec<Node>,
}

/// A router with `n` nodes registered as `n1`, `n2`, ...
fn cluster(tag: &str, n: usize, token: Option<&str>) -> Cluster {
    let nodes: Vec<Node> = (1..=n).map(|i| node(&format!("{tag}{i}"))).collect();
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        token: token.map(String::from),
        upstream_timeout: Duration::from_secs(5),
        ..Config::default()
    };
    let router = Router::new(Directory::in_memory(), cfg);
    let listener = router.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = router.serve_on(listener);
    });
    let c = Cluster { port, nodes };
    for (i, nd) in c.nodes.iter().enumerate() {
        let body = format!(r#"{{"addr":"{}","token":"adm-{tag}{}"}}"#, nd.addr, i + 1);
        let r = c.call("PUT", &format!("/_shard/nodes/n{}", i + 1), &body, token);
        assert_eq!(r.0, 201, "{}", r.1);
    }
    c
}

impl Cluster {
    fn call(&self, method: &str, target: &str, body: &str, auth: Option<&str>) -> (u16, String) {
        call(self.port, method, target, body, auth)
    }

    fn create(&self, tenant: &str, node: Option<&str>) -> String {
        let body = node
            .map(|n| format!(r#"{{"node":"{n}"}}"#))
            .unwrap_or_default();
        let r = self.call("PUT", &format!("/_shard/tenants/{tenant}"), &body, None);
        assert_eq!(r.0, 201, "{}", r.1);
        r.1
    }

    fn query(&self, tenant: &str, q: &str) -> (u16, String) {
        let mut body = String::from("{\"query\":");
        fenec_core::json::escape_into(&mut body, q);
        body.push('}');
        self.call("POST", &format!("/t/{tenant}/query"), &body, None)
    }
}

fn call(port: u16, method: &str, target: &str, body: &str, auth: Option<&str>) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut req = format!(
        "{method} {target} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(t) = auth {
        req.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let (head, body) = out.split_once("\r\n\r\n").unwrap_or((&out, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, format!("{head}\r\n\r\n{body}"))
}

fn body(r: &(u16, String)) -> &str {
    r.1.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("")
}

/// An open subscription through the router.
struct Stream {
    sock: TcpStream,
    buf: String,
}

impl Stream {
    fn open(port: u16, target: &str) -> Stream {
        let mut sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        sock.write_all(format!("GET {target} HTTP/1.1\r\nHost: t\r\n\r\n").as_bytes())
            .unwrap();
        Stream {
            sock,
            buf: String::new(),
        }
    }

    /// Reads until `needle` shows up; the text up to and including it.
    fn until(&mut self, needle: &str, budget: Duration) -> Option<String> {
        let deadline = Instant::now() + budget;
        loop {
            if let Some(i) = self.buf.find(needle) {
                let end = i + needle.len();
                let out = self.buf[..end].to_string();
                self.buf = self.buf[end..].to_string();
                return Some(out);
            }
            if Instant::now() > deadline {
                return None;
            }
            let mut chunk = [0u8; 4096];
            match self.sock.read(&mut chunk) {
                Ok(0) => return None,
                Ok(n) => self.buf.push_str(&String::from_utf8_lossy(&chunk[..n])),
                Err(_) => continue,
            }
        }
    }
}

#[test]
fn a_tenant_is_placed_created_and_served_through_the_router() {
    let c = cluster("serve", 2, None);
    c.create("acme", Some("n2"));
    assert_eq!(c.nodes[1].tenants.names(), ["acme"]);
    assert!(c.nodes[0].tenants.names().is_empty());

    assert_eq!(
        c.query("acme", "create collection notes (title text)").0,
        200
    );
    let r = c.call("POST", "/t/acme/notes", r#"{"title":"hello"}"#, None);
    assert_eq!(r.0, 201, "{}", r.1);
    let r = c.call("GET", "/t/acme/notes?select=title", "", None);
    assert_eq!(body(&r), r#"[{"title":"hello"}]"#);

    let r = c.call("GET", "/_shard/tenants", "", None);
    assert_eq!(
        body(&r),
        r#"[{"name":"acme","node":"n2","state":"active"}]"#
    );
    // The node tokens never leave the router.
    let r = c.call("GET", "/_shard/nodes", "", None);
    assert!(!r.1.contains("adm-"), "{}", r.1);
}

#[test]
fn placement_picks_the_node_with_the_least_on_disk() {
    let c = cluster("place", 2, None);
    c.create("big", Some("n1"));
    c.query("big", "create collection notes (title text)");
    for i in 0..50 {
        c.call(
            "POST",
            "/t/big/notes",
            &format!(r#"{{"title":"{i}"}}"#),
            None,
        );
    }
    // The node's writes are buffered until its syncer runs; this is what it
    // would have done by now.
    c.nodes[0].tenants.sync_dirty();
    let placed = c.create("small", None);
    assert!(placed.contains(r#""node":"n2""#), "{placed}");
}

#[test]
fn a_client_connection_is_kept_alive_across_forwarded_requests() {
    let c = cluster("alive", 1, None);
    c.create("acme", None);
    c.query("acme", "create collection notes (title text)");

    let mut s = TcpStream::connect(("127.0.0.1", c.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let one = "GET /t/acme/notes HTTP/1.1\r\nHost: t\r\n\r\n";
    s.write_all(one.as_bytes()).unwrap();
    s.write_all(one.as_bytes()).unwrap();
    let mut seen = String::new();
    let mut chunk = [0u8; 4096];
    while seen.matches("HTTP/1.1 200").count() < 2 {
        let n = s.read(&mut chunk).unwrap();
        assert!(n > 0, "closed after: {seen}");
        seen.push_str(&String::from_utf8_lossy(&chunk[..n]));
    }
    assert!(seen.contains("Connection: keep-alive"));
}

#[test]
fn a_head_answer_keeps_the_length_it_leaves_out() {
    let c = cluster("head", 1, None);
    c.create("acme", None);
    c.query("acme", "create collection notes (title text)");
    c.call("POST", "/t/acme/notes", r#"{"title":"hello"}"#, None);
    let get = c.call("GET", "/t/acme/notes", "", None);
    let head = c.call("HEAD", "/t/acme/notes", "", None);
    assert_eq!(head.0, 200);
    assert_eq!(body(&head), "");
    let length = format!("Content-Length: {}", body(&get).len());
    assert!(head.1.contains(&length), "{}", head.1);
}

#[test]
fn a_subscription_streams_through_the_router() {
    let c = cluster("sse", 1, None);
    c.create("acme", None);
    c.query("acme", "create collection notes (title text)");

    let mut s = Stream::open(c.port, "/t/acme/notes/changes");
    let seed = s
        .until("event: seed", Duration::from_secs(3))
        .expect("seed");
    assert!(seed.contains("text/event-stream"), "{seed}");

    c.call("POST", "/t/acme/notes", r#"{"title":"live"}"#, None);
    let change = s.until("\"live\"", Duration::from_secs(3));
    assert!(change.is_some(), "the change did not arrive: {}", s.buf);
}

#[test]
fn a_move_copies_the_tenant_flips_the_route_and_removes_the_source() {
    let c = cluster("move", 2, None);
    c.create("acme", Some("n1"));
    c.query(
        "acme",
        "create collection docs (title text, v vector<2> @hnsw(cosine))",
    );
    c.query(
        "acme",
        r#"put docs [{title: "x", v: [1, 0]}, {title: "y", v: [0, 1]}]"#,
    );
    let mut s = Stream::open(c.port, "/t/acme/docs/changes");
    s.until("event: seed", Duration::from_secs(3))
        .expect("seed");
    let seq = s.until(",", Duration::from_secs(3)).expect("seed data");
    let seq: u64 = seq
        .rsplit("\"seq\":")
        .next()
        .and_then(|v| v.trim_end_matches(',').parse().ok())
        .expect("seq in the seed");

    let r = c.call("POST", "/_shard/tenants/acme/move", r#"{"to":"n2"}"#, None);
    assert_eq!(r.0, 200, "{}", r.1);
    assert!(r.1.contains(r#""source_removed":true"#), "{}", r.1);

    assert_eq!(c.nodes[1].tenants.names(), ["acme"]);
    assert!(c.nodes[0].tenants.names().is_empty());
    let r = c.call("GET", "/_shard/tenants", "", None);
    assert_eq!(
        body(&r),
        r#"[{"name":"acme","node":"n2","state":"active"}]"#
    );

    // The open stream ended on the source; a new one lands on the target.
    assert!(s.until("event: error", Duration::from_secs(3)).is_some());

    // A subscriber that had seen everything resumes on the target from its
    // cursor: the sequence travels in the image and the source was frozen,
    // so nothing happened in between -- no reseed.
    let mut resumed = Stream::open(c.port, &format!("/t/acme/docs/changes?since={seq}"));
    resumed
        .until("\r\n\r\n", Duration::from_secs(3))
        .expect("stream head");
    c.call("POST", "/t/acme/docs", r#"{"title":"streamed"}"#, None);
    let got = resumed
        .until("\"streamed\"", Duration::from_secs(3))
        .expect("the write after the move streamed");
    assert!(!got.contains("event: seed"), "{got}");
    let r = c.call(
        "POST",
        "/t/acme/docs/near",
        r#"{"vector":[0,1],"limit":1,"select":["title"]}"#,
        None,
    );
    assert!(body(&r).contains("\"y\""), "{}", r.1);
    let r = c.call("POST", "/t/acme/docs", r#"{"title":"after"}"#, None);
    assert_eq!(r.0, 201, "{}", r.1);
    // Ids continue where the source left off: the counter moved with it.
    let r = c.call("GET", "/t/acme/docs?title=eq.after&select=id", "", None);
    assert_eq!(body(&r), r#"[{"id":4}]"#);
}

#[test]
fn a_move_replaces_a_stale_copy_left_on_the_target() {
    let c = cluster("stale", 2, None);
    c.create("acme", Some("n1"));
    c.query("acme", "create collection notes (title text)");
    c.call("POST", "/t/acme/notes", r#"{"title":"real"}"#, None);
    // What a move that died after the copy and before the flip leaves.
    c.nodes[1].tenants.create("acme").unwrap();

    let r = c.call("POST", "/_shard/tenants/acme/move", r#"{"to":"n2"}"#, None);
    assert_eq!(r.0, 200, "{}", r.1);
    let r = c.call("GET", "/t/acme/notes?select=title", "", None);
    assert_eq!(body(&r), r#"[{"title":"real"}]"#);
}

#[test]
fn writes_to_a_frozen_tenant_come_back_as_503_with_retry_after() {
    let c = cluster("frozen", 1, None);
    c.create("acme", None);
    c.query("acme", "create collection notes (title text)");
    c.nodes[0].tenants.freeze("acme").unwrap();

    let r = c.call("POST", "/t/acme/notes", r#"{"title":"x"}"#, None);
    assert_eq!(r.0, 503, "{}", r.1);
    assert!(r.1.contains("Retry-After: 1"), "{}", r.1);
    assert_eq!(c.call("GET", "/t/acme/notes", "", None).0, 200);
}

#[test]
fn deleting_a_tenant_removes_it_from_node_and_directory() {
    let c = cluster("delete", 1, None);
    c.create("acme", None);
    assert_eq!(c.call("DELETE", "/_shard/tenants/acme", "", None).0, 204);
    assert!(c.nodes[0].tenants.names().is_empty());
    assert_eq!(c.call("GET", "/t/acme/", "", None).0, 404);
    // Both the tenant's and the node's name are free again.
    assert_eq!(c.call("DELETE", "/_shard/nodes/n1", "", None).0, 204);
}

#[test]
fn refusals() {
    let c = cluster("refuse", 1, Some("root"));
    // /_shard/ needs the router token; the data path does not.
    assert_eq!(c.call("GET", "/_shard/tenants", "", None).0, 401);
    assert_eq!(c.call("GET", "/_shard/tenants", "", Some("root")).0, 200);
    assert_eq!(c.call("GET", "/t/nope/x", "", None).0, 404);
    assert_eq!(c.call("GET", "/elsewhere", "", None).0, 404);

    let r = c.call("PUT", "/_shard/tenants/acme", "", Some("root"));
    assert_eq!(r.0, 201, "{}", r.1);
    assert_eq!(
        c.call("PUT", "/_shard/tenants/acme", "", Some("root")).0,
        409
    );
    // A node that holds a tenant cannot be dropped from the directory.
    assert_eq!(
        c.call("DELETE", "/_shard/nodes/n1", "", Some("root")).0,
        409
    );
    // A node whose token is wrong is not added.
    let bad = format!(r#"{{"addr":"{}","token":"wrong"}}"#, c.nodes[0].addr);
    assert_eq!(c.call("PUT", "/_shard/nodes/n9", &bad, Some("root")).0, 502);
    // A move to where it already is, or to a node nobody knows.
    let r = c.call(
        "POST",
        "/_shard/tenants/acme/move",
        r#"{"to":"n1"}"#,
        Some("root"),
    );
    assert_eq!(r.0, 409);
    let r = c.call(
        "POST",
        "/_shard/tenants/acme/move",
        r#"{"to":"n7"}"#,
        Some("root"),
    );
    assert_eq!(r.0, 404);
}

#[test]
fn an_unreachable_node_is_a_502_not_a_hang() {
    // A directory that points at a node nothing listens on: what a crashed
    // node looks like from the router.
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = dead.local_addr().unwrap().to_string();
    drop(dead);
    let mut dir = Directory::in_memory();
    dir.set_node(
        "n1",
        fenec_shard::directory::Node {
            addr,
            token: "x".into(),
        },
    )
    .unwrap();
    dir.place("acme", "n1", fenec_shard::directory::State::Active)
        .unwrap();
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        upstream_timeout: Duration::from_secs(2),
        ..Config::default()
    };
    let router = Router::new(dir, cfg);
    let listener = router.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = router.serve_on(listener);
    });

    let started = Instant::now();
    let r = call(port, "GET", "/t/acme/", "", None);
    assert_eq!(r.0, 502, "{}", r.1);
    assert!(started.elapsed() < Duration::from_secs(2));
    // And a new tenant has nowhere to go.
    assert_eq!(call(port, "PUT", "/_shard/tenants/beta", "", None).0, 503);
}
