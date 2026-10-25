//! A connection a server can make no thread for, through the router: the
//! node's -- answered 503 with `Retry-After`, which the router passes on
//! as an answer, the write never run -- the router's own, and a region's
//! router's to a router relaying to it, which asks the region's next one.
//! Closed with nothing said, the node's had the router answer a write 502,
//! "may have run": written whole, its connection ended before a byte of an
//! answer, as a node that ran it and went down looks.
//!
//! Each router stands straight in front of what it sends to, its directory
//! written before it starts, so nothing goes from it there but the test's
//! requests, and the first of them opens the connection the accept loop
//! there is made to fail (`Server::fail_spawns`, `Router::fail_spawns`).
//! The counts are `refused_counts.rs`'s, a binary of its own.

use fenec_http::accept::FailSpawns;
use fenec_http::tenants::Tenants;
use fenec_shard::directory::{Directory, Node, State};
use fenec_shard::{Config, Router};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Setup {
    router: String,
    node: String,
    node_fails: FailSpawns,
    router_fails: FailSpawns,
    dir: PathBuf,
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A node holding tenant `acme` with a collection `notes`: its address and
/// its spawns.
fn node(tag: &str) -> (String, FailSpawns, PathBuf) {
    fenec_http::audit::set_delay(0);
    let dir = std::env::temp_dir().join(format!(
        "fenec-shard-no-thread-{tag}-{}-{:?}",
        std::process::id(),
        Instant::now()
    ));
    let tenants = Arc::new(Tenants::new(&dir).unwrap());
    let acme = tenants.create("acme").unwrap();
    let create = fenec_ql::parse_one("create collection notes (title text)").unwrap();
    acme.db.write().unwrap().execute(&create).unwrap();
    drop(acme);
    let server = fenec_http::Server::with_tenants(
        Arc::clone(&tenants),
        fenec_http::Config {
            addr: "127.0.0.1:0".into(),
            admin_token: Some("adm".into()),
            ..fenec_http::Config::default()
        },
    );
    let fails = server.fail_spawns();
    let listener = server.bind().unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    (addr, fails, dir)
}

/// A directory placing `acme` on the node at `addr`, in `region`'s scope.
fn placing(addr: &str, region: Option<&str>) -> Directory {
    let mut d = Directory::in_memory();
    d.settle_scope(region).unwrap();
    let placed = Node {
        addr: addr.to_string(),
        token: "adm".into(),
        region: region.unwrap_or_default().to_string(),
        ..Node::default()
    };
    d.set_node("n1", placed).unwrap();
    d.place("acme", "n1", State::Active).unwrap();
    d
}

fn config(region: Option<&str>) -> Config {
    Config {
        addr: "127.0.0.1:0".into(),
        upstream_timeout: Duration::from_secs(10),
        region: region.map(String::from),
        ..Config::default()
    }
}

/// `router` serving: its address and its spawns.
fn serve(router: Arc<Router>) -> (String, FailSpawns) {
    let fails = router.fail_spawns();
    let listener = router.bind().unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = router.serve_on(listener);
    });
    (addr, fails)
}

/// A node holding tenant `acme`, and a router in front of it.
fn setup(tag: &str) -> Setup {
    let (node, node_fails, dir) = node(tag);
    let (router, router_fails) = serve(Router::new(placing(&node, None), config(None)));
    Setup {
        router,
        node,
        node_fails,
        router_fails,
        dir,
    }
}

/// The status, the head and the body of one request on a connection of
/// its own, the client's request sent whole before it reads.
fn call(addr: &str, method: &str, target: &str, body: &str) -> (u16, String, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let req = format!(
        "{method} {target} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let (head, body) = out.split_once("\r\n\r\n").unwrap_or((&out, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, head.to_string(), body.to_string())
}

fn put(addr: &str, title: &str) -> (u16, String, String) {
    let body = format!(r#"{{"query":"put notes {{title: \"{title}\"}}"}}"#);
    call(addr, "POST", "/t/acme/query", &body)
}

/// The titles the node at `node` holds, asked of the node itself.
fn titles(node: &str) -> Vec<String> {
    let (status, _, body) = call(
        node,
        "POST",
        "/t/acme/query",
        r#"{"query":"get notes select title order id"}"#,
    );
    assert_eq!(status, 200, "{body}");
    body.split("\"title\":\"")
        .skip(1)
        .map(|t| t.split('"').next().unwrap().to_string())
        .collect()
}

/// Whether `head` holds the header line `line`, its name in any case.
fn holds(head: &str, line: &str) -> bool {
    head.lines().any(|l| l.eq_ignore_ascii_case(line))
}

/// Whether `head` names the header `name` at all.
fn names(head: &str, name: &str) -> bool {
    head.lines().any(|l| {
        l.split_once(':')
            .is_some_and(|(k, _)| k.trim().eq_ignore_ascii_case(name))
    })
}

/// A write whose connection to its node got no thread there is answered
/// the node's 503, `Retry-After` and all, never 502 "may have run": the
/// node ran nothing, and says so. The node's mark of its connection is
/// not passed on -- the router's own was served. Sent again, it lands once.
#[test]
fn a_write_a_node_had_no_thread_for_is_answered_unrun_through_the_router() {
    let s = setup("node");
    s.node_fails.next(1);
    let (status, head, body) = put(&s.router, "first");
    assert_eq!(status, 503, "{head}\n{body}");
    assert!(holds(&head, "Retry-After: 1"), "{head}");
    assert!(!names(&head, "Fenec-Unserved"), "{head}");
    assert!(body.contains("no thread"), "{body}");
    assert!(body.ends_with(r#""retry_after":1}"#), "{body}");
    assert_eq!(
        titles(&s.node),
        Vec::<String>::new(),
        "a write refused unrun ran"
    );
    let (status, _, body) = put(&s.router, "first");
    assert_eq!(status, 200, "{body}");
    assert_eq!(titles(&s.node), ["first"]);
}

/// The router's own connection with no thread is answered the same, with
/// its mark and the mark of a connection served nothing: a 503 the router
/// wrote, the request never forwarded.
#[test]
fn a_connection_the_router_had_no_thread_for_is_answered_503() {
    let s = setup("router");
    s.router_fails.next(1);
    let (status, head, body) = put(&s.router, "first");
    assert_eq!(status, 503, "{head}\n{body}");
    assert!(holds(&head, "Retry-After: 1"), "{head}");
    assert!(holds(&head, "Fenec-Shard: 1"), "{head}");
    assert!(holds(&head, "Fenec-Unserved: no_thread"), "{head}");
    assert!(body.ends_with(r#""retry_after":1}"#), "{body}");
    assert_eq!(titles(&s.node), Vec::<String>::new());
    let (status, _, body) = put(&s.router, "second");
    assert_eq!(status, 200, "{body}");
    assert_eq!(titles(&s.node), ["second"]);
}

/// A router relaying to a region's routers -- here the global router, its
/// tenant at home in `us` -- asks the next where the first refused the
/// connection as it took it, a write as any request: it never ran there.
/// Taken for that router's answer, the write was its 503, though the
/// standby beside it was there to take it. Where every router of the region
/// refuses, the 503 is the answer, the relaying router's own connection
/// served.
#[test]
fn a_relay_asks_the_next_router_where_one_had_no_thread() {
    let (node, _, dir) = node("relay");
    // Two routers of `us`, each placing the tenant on the node.
    let us = || {
        let own = placing(&node, Some("us"));
        let router = Router::regional(own, config(Some("us")), None, Directory::in_memory());
        serve(router.unwrap())
    };
    let ((first, first_fails), (second, second_fails)) = (us(), us());
    let global = || {
        let mut d = Directory::in_memory();
        let home = [format!("http://{first}"), format!("http://{second}")];
        d.set_region("us", Some(&home)).unwrap();
        d.place_home("acme", "us").unwrap();
        serve(Router::new(d, config(None))).0
    };

    first_fails.next(1);
    let front = global();
    let (status, head, body) = put(&front, "first");
    assert_eq!(status, 200, "{head}\n{body}");
    assert_eq!(titles(&node), ["first"], "written once, by the second");

    // A new global router, so that no connection of its pool is there
    // already: each router of the region refuses the one made to it.
    first_fails.next(1);
    second_fails.next(1);
    let front = global();
    let (status, head, body) = put(&front, "second");
    assert_eq!(status, 503, "{head}\n{body}");
    assert!(holds(&head, "Retry-After: 1"), "{head}");
    assert!(!names(&head, "Fenec-Unserved"), "{head}");
    assert_eq!(titles(&node), ["first"], "the refused write never ran");
    let _ = std::fs::remove_dir_all(dir);
}
