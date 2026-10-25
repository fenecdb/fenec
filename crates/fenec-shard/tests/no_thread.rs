//! A connection a server can make no thread for, through the router: the
//! node's -- answered 503 with `Retry-After`, which the router passes on
//! as an answer, the write never run -- and the router's own. Closed with
//! nothing said, the node's had the router answer a write 502, "may have
//! run": written whole, its connection ended before a byte of an answer,
//! as a node that ran it and went down looks.
//!
//! The router stands straight in front of the node, the directory written
//! before it starts, so nothing goes from the router to the node but the
//! test's requests and the first of them opens the connection the node's
//! accept loop is made to fail ([`fenec_http::Server::fail_spawns`]).

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

/// A node holding tenant `acme` with a collection `notes`, and a router in
/// front of it.
fn setup(tag: &str) -> Setup {
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
    let node_fails = server.fail_spawns();
    let listener = server.bind().unwrap();
    let node = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    let mut d = Directory::in_memory();
    let placed = Node {
        addr: node.clone(),
        token: "adm".into(),
        ..Node::default()
    };
    d.set_node("n1", placed).unwrap();
    d.place("acme", "n1", State::Active).unwrap();
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        upstream_timeout: Duration::from_secs(10),
        ..Config::default()
    };
    let router = Router::new(d, cfg);
    let router_fails = router.fail_spawns();
    let listener = router.bind().unwrap();
    let front = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = router.serve_on(listener);
    });
    Setup {
        router: front,
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

/// The titles the node holds, asked of the node itself.
fn titles(s: &Setup) -> Vec<String> {
    let (status, _, body) = call(
        &s.node,
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

/// A write whose connection to its node got no thread there is answered
/// the node's 503, `Retry-After` and all, never 502 "may have run": the
/// node ran nothing, and says so. Sent again, it lands once.
#[test]
fn a_write_a_node_had_no_thread_for_is_answered_unrun_through_the_router() {
    let s = setup("node");
    s.node_fails.next(1);
    let (status, head, body) = put(&s.router, "first");
    assert_eq!(status, 503, "{head}\n{body}");
    assert!(holds(&head, "Retry-After: 1"), "{head}");
    assert!(body.contains("no thread"), "{body}");
    assert!(body.ends_with(r#""retry_after":1}"#), "{body}");
    assert_eq!(
        titles(&s),
        Vec::<String>::new(),
        "a write refused unrun ran"
    );
    let (status, _, body) = put(&s.router, "first");
    assert_eq!(status, 200, "{body}");
    assert_eq!(titles(&s), ["first"]);
}

/// The router's own connection with no thread is answered the same, with
/// its mark: a 503 the router wrote, the request never forwarded.
#[test]
fn a_connection_the_router_had_no_thread_for_is_answered_503() {
    let s = setup("router");
    s.router_fails.next(1);
    let (status, head, body) = put(&s.router, "first");
    assert_eq!(status, 503, "{head}\n{body}");
    assert!(holds(&head, "Retry-After: 1"), "{head}");
    assert!(holds(&head, "Fenec-Shard: 1"), "{head}");
    assert!(body.ends_with(r#""retry_after":1}"#), "{body}");
    assert_eq!(titles(&s), Vec::<String>::new());
    let (status, _, body) = put(&s.router, "second");
    assert_eq!(status, 200, "{body}");
    assert_eq!(titles(&s), ["second"]);
    let (status, _, scrape) = call(&s.router, "GET", "/_metrics", "");
    assert_eq!(status, 200, "{scrape}");
    let refused = scrape
        .lines()
        .find_map(|l| {
            l.strip_prefix(r#"fenec_router_connections_refused_total{reason="no_thread"} "#)
        })
        .and_then(|n| n.parse::<u64>().ok());
    // At least this one: the count is the process's, and the other tests
    // here add to it, never take from it.
    assert!(refused >= Some(1), "{scrape}");
}
