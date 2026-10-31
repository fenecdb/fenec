//! `fenec-shard --http-cors`: what a page on another origin is told by the
//! router. Its own answers -- an unknown tenant's 404, a node's 502, a
//! standby's 503, its connection ceiling's -- carried no CORS header while
//! the nodes' answers through it carried the nodes', so a page read every
//! one of them as the network failing; and a preflight was forwarded, so
//! the request after it was never sent where the router could not reach a
//! node.

use fenec_core::prelude::Database;
use fenec_http::tenants::Tenants;
use fenec_shard::directory::{Directory, Node as Placed, State};
use fenec_shard::{Config, Router};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const APP: &str = "https://app.example";
const EVIL: &str = "https://evil.example";

struct Node {
    addr: String,
    dir: PathBuf,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A `--dir` node answering browsers from `cors`.
fn node(tag: &str, cors: Option<&str>) -> Node {
    // A refusal's wait is the process's; see router.rs.
    fenec_http::audit::set_delay(0);
    let dir = std::env::temp_dir().join(format!(
        "fenec-shard-cors-{tag}-{}-{:?}",
        std::process::id(),
        Instant::now()
    ));
    let tenants = Arc::new(Tenants::new(&dir).unwrap());
    let cfg = fenec_http::Config {
        addr: "127.0.0.1:0".into(),
        admin_token: Some("adm".into()),
        stream_keepalive: Duration::from_millis(80),
        cors: cors.map(String::from),
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

/// A router over `dir` answering browsers from `cors`, its port. Its
/// upstream timeout is the default minute: it bounds every answer a node
/// sends through it, and at 2 s a node under load could miss it, while the
/// node it cannot reach refuses at once.
fn router(dir: Directory, cors: Option<&str>, max_connections: usize) -> u16 {
    fenec_http::audit::set_delay(0);
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        token: Some("root".into()),
        max_connections,
        cors: cors.map(String::from),
        ..Config::default()
    };
    let router = Router::new(dir, cfg);
    let listener = router.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = router.serve_on(listener);
    });
    port
}

/// A directory placing `acme` on `addr`, as the router's admin would.
fn placed_at(addr: &str) -> Directory {
    let mut dir = Directory::in_memory();
    dir.set_node(
        "n1",
        Placed {
            addr: addr.into(),
            token: "adm".into(),
            ..Placed::default()
        },
    )
    .unwrap();
    dir.place("acme", "n1", State::Active).unwrap();
    dir
}

/// A router in front of `n`, which holds `acme`: placed and made as an
/// operator would, through `/_shard/`.
fn in_front_of(n: &Node, cors: Option<&str>) -> u16 {
    let port = router(Directory::in_memory(), cors, 0);
    let root = "Authorization: Bearer root\r\n";
    let body = format!(r#"{{"addr":"{}","token":"adm"}}"#, n.addr);
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        s,
        "PUT /_shard/nodes/n1 HTTP/1.1\r\nHost: t\r\n{root}Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    assert_eq!(read_answer(&mut s).status, 201);
    let made = ask(port, "PUT", "/_shard/tenants/acme", None, root);
    assert_eq!(made.status, 201, "{}", made.body);
    port
}

/// An address nothing listens on: a crashed node, seen from the router.
/// Port 1, which no test's server takes: a port the system picked and that
/// was let go of was taken again by another test's router running beside
/// this one, and the request forwarded there was answered its 404, no
/// tenant `acme` in its directory (once in 50 runs of the file, three cores
/// beside six busy loops).
fn dead_addr() -> String {
    "127.0.0.1:1".into()
}

struct Answer {
    status: u16,
    head: String,
    body: String,
}

impl Answer {
    /// Every value of the header `name`, in order.
    fn all(&self, name: &str) -> Vec<&str> {
        self.head
            .lines()
            .skip(1)
            .filter_map(|l| l.split_once(':'))
            .filter(|(k, _)| k.trim().eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim())
            .collect()
    }

    fn one(&self, name: &str) -> Option<&str> {
        let all = self.all(name);
        assert!(all.len() <= 1, "{name} twice: {}", self.head);
        all.first().copied()
    }

    /// The CORS headers a node's are, held to the page's needs: one
    /// origin allowed, the headers a replica and `FenecHttp` read exposed,
    /// the ones they send allowed in.
    fn tells(&self, origin: &str) {
        assert_eq!(
            self.one("access-control-allow-origin"),
            Some(origin),
            "{}",
            self.head
        );
        let exposed = self.one("access-control-expose-headers").unwrap_or("");
        for h in ["retry-after", "fenec-seq", "x-request-id"] {
            assert!(exposed.contains(h), "{h} not exposed: {}", self.head);
        }
        let allowed = self.one("access-control-allow-headers").unwrap_or("");
        for h in ["idempotency-key", "authorization", "content-type"] {
            assert!(allowed.contains(h), "{h} not allowed in: {}", self.head);
        }
        assert_eq!(self.one("vary"), Some("Origin"), "{}", self.head);
    }

    fn tells_nothing(&self) {
        assert!(
            !self.head.to_ascii_lowercase().contains("access-control-"),
            "{}",
            self.head
        );
    }
}

/// One request on a connection of its own, `Origin` sent when given.
fn ask(port: u16, method: &str, target: &str, origin: Option<&str>, extra: &str) -> Answer {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let origin = origin.map_or(String::new(), |o| format!("Origin: {o}\r\n"));
    write!(
        s,
        "{method} {target} HTTP/1.1\r\nHost: t\r\n{origin}{extra}Content-Length: 0\r\n\
         Connection: close\r\n\r\n"
    )
    .unwrap();
    read_answer(&mut s)
}

fn read_answer(s: &mut TcpStream) -> Answer {
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let (head, body) = out.split_once("\r\n\r\n").unwrap_or((&out, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    Answer {
        status,
        head: head.to_string(),
        body: body.to_string(),
    }
}

const PREFLIGHT: &str = "Access-Control-Request-Method: POST\r\n\
                         Access-Control-Request-Headers: authorization, content-type, idempotency-key\r\n";

#[test]
fn the_routers_own_answers_tell_an_allowed_page() {
    let port = router(Directory::in_memory(), Some(APP), 0);

    // A tenant the directory does not hold.
    let r = ask(port, "POST", "/t/nosuch/query", Some(APP), "");
    assert_eq!(r.status, 404, "{}", r.body);
    r.tells(APP);
    assert!(r.one("x-request-id").is_some(), "{}", r.head);
    // A path the router does not serve, and /_shard/ without its token.
    let r = ask(port, "GET", "/nosuch", Some(APP), "");
    assert_eq!(r.status, 404);
    r.tells(APP);
    let r = ask(port, "GET", "/_shard/tenants", Some(APP), "");
    assert_eq!(r.status, 401);
    r.tells(APP);
    // With it: an answer a page reads, as the studio's admin view does.
    let r = ask(
        port,
        "GET",
        "/_shard/tenants",
        Some(APP),
        "Authorization: Bearer root\r\n",
    );
    assert_eq!(r.status, 200, "{}", r.body);
    r.tells(APP);

    // A page on another origin is never named: the browser compares the
    // origin allowed with its own and keeps the answer from it.
    let r = ask(port, "POST", "/t/nosuch/query", Some(EVIL), "");
    assert_eq!(r.status, 404);
    assert_eq!(r.one("access-control-allow-origin"), Some(APP));
    assert!(!r.head.contains(EVIL), "{}", r.head);
}

#[test]
fn a_node_the_router_cannot_reach_is_a_502_a_page_reads() {
    let port = router(placed_at(&dead_addr()), Some(APP), 0);
    let r = ask(port, "POST", "/t/acme/query", Some(APP), "");
    assert_eq!(r.status, 502, "{}", r.body);
    r.tells(APP);
    // The preflight before it never reached the node: the router says
    // what its answers say, and the page sends the request.
    let r = ask(port, "OPTIONS", "/t/acme/query", Some(APP), PREFLIGHT);
    assert_eq!(r.status, 204, "{}", r.body);
    r.tells(APP);
}

/// A standby whose directory has not arrived asks to be asked again: a page
/// reads the 503 and its wait, in the header and in the body.
#[test]
fn a_standby_without_its_directory_tells_a_page_when_to_come_back() {
    let mut db = Database::new();
    db.follow(Vec::new()).unwrap();
    let dir = Directory::load(Arc::new(RwLock::new(db))).unwrap();
    assert!(!dir.arrived());
    let port = router(dir, Some(APP), 0);
    let r = ask(port, "GET", "/t/acme/collections", Some(APP), "");
    assert_eq!(r.status, 503, "{}", r.body);
    r.tells(APP);
    assert_eq!(r.one("retry-after"), Some("1"));
    assert!(r.body.contains(r#""retry_after":1"#), "{}", r.body);
    let r = ask(port, "OPTIONS", "/t/acme/collections", Some(APP), PREFLIGHT);
    assert_eq!(r.status, 204);
    r.tells(APP);
}

#[test]
fn the_connection_ceiling_tells_a_page_when_to_come_back() {
    let port = router(Directory::in_memory(), Some(APP), 1);
    // One connection held open, answered once so it is surely counted.
    let mut held = TcpStream::connect(("127.0.0.1", port)).unwrap();
    held.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(held, "GET /nosuch HTTP/1.1\r\nHost: t\r\n\r\n").unwrap();
    let mut first = [0u8; 12];
    held.read_exact(&mut first).unwrap();
    assert_eq!(&first, b"HTTP/1.1 404");

    // The refusal is written as the connection is taken, before a request
    // is read; closed with it, a request written after it met the closed
    // socket (`EPIPE`) one run in about sixty, and is read off now
    // (`a_request_written_after_the_routers_refusal_...`).
    let r = ask(port, "GET", "/nosuch", Some(APP), "");
    assert_eq!(r.status, 503, "{}", r.body);
    r.tells(APP);
    assert_eq!(r.one("retry-after"), Some("1"));
    assert!(r.body.contains(r#""retry_after":1"#), "{}", r.body);
    drop(held);
}

/// A client past the router's ceiling writing its request as the answer
/// comes -- a request sent after its answer and end included, head and
/// body apart as Python's `http.client` writes them -- is read off and its
/// connection ended with no reset. Closed with its answer, the router's
/// socket reset the head and the body's write failed `EPIPE`, the 503
/// unread: 300 such clients writing through a router at a ceiling of 100
/// read 2 939 503s and 22 957 broken pipes of 25 921 refusals (Linux). The
/// body is larger than a send buffer -- macOS's 128 KiB, Linux's first
/// 16 KiB -- so written to a socket that is gone, its write waits for room
/// that never comes and the reset ends it every time.
#[test]
fn a_request_written_after_the_routers_refusal_is_read_off_not_reset() {
    const BODY: usize = 512 << 10;
    let port = router(Directory::in_memory(), None, 1);
    let mut held = TcpStream::connect(("127.0.0.1", port)).unwrap();
    held.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(held, "GET /nosuch HTTP/1.1\r\nHost: t\r\n\r\n").unwrap();
    let mut first = [0u8; 12];
    held.read_exact(&mut first).unwrap();
    assert_eq!(&first, b"HTTP/1.1 404");
    for i in 0..3 {
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        // The answer and its end: where the refusal closed the socket, it
        // had closed it by then.
        let r = read_answer(&mut c);
        assert_eq!(r.status, 503, "{}", r.body);
        assert_eq!(r.one("retry-after"), Some("1"));
        let head =
            format!("POST /t/acme/query HTTP/1.1\r\nHost: t\r\nContent-Length: {BODY}\r\n\r\n");
        c.write_all(head.as_bytes())
            .unwrap_or_else(|e| panic!("{i}: the head: {e}"));
        c.write_all(&vec![b' '; BODY])
            .unwrap_or_else(|e| panic!("{i}: the body: {e}"));
        c.shutdown(std::net::Shutdown::Write)
            .unwrap_or_else(|e| panic!("{i}: the end: {e}"));
        // Over either way -- the end acknowledged, or reset -- a
        // connection has no peer.
        let by = Instant::now() + Duration::from_secs(30);
        while c.peer_addr().is_ok() {
            assert!(Instant::now() < by, "the connection never ended");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(c.take_error().unwrap().is_none(), "{i}: reset");
    }
    drop(held);
}

/// The router answers a preflight itself, before the directory is read: a
/// tenant it does not hold is the request's 404, not the preflight's.
#[test]
fn a_preflight_is_the_routers_to_answer() {
    let port = router(Directory::in_memory(), Some(APP), 0);
    let r = ask(port, "OPTIONS", "/t/nosuch/query", Some(APP), PREFLIGHT);
    assert_eq!(r.status, 204, "{}", r.body);
    r.tells(APP);
    assert!(r.one("x-request-id").is_some(), "{}", r.head);
    let r = ask(port, "OPTIONS", "/_shard/tenants", Some(APP), PREFLIGHT);
    assert_eq!(r.status, 204);
    r.tells(APP);

    // Another origin's goes where it went before: the router lets it in
    // no further than its answers do.
    let r = ask(port, "OPTIONS", "/t/nosuch/query", Some(EVIL), PREFLIGHT);
    assert_eq!(r.status, 404);
    assert_eq!(r.one("access-control-allow-origin"), Some(APP));
    let r = ask(port, "OPTIONS", "/_shard/tenants", Some(EVIL), PREFLIGHT);
    assert_eq!(r.status, 401);
    assert_eq!(r.one("access-control-allow-origin"), Some(APP));

    // `*` lets every origin's in.
    let port = router(Directory::in_memory(), Some("*"), 0);
    let r = ask(port, "OPTIONS", "/t/nosuch/query", Some(EVIL), PREFLIGHT);
    assert_eq!(r.status, 204);
    r.tells("*");
}

/// A node allowing `*` behind a router allowing one origin: both kept,
/// the answer named two origins, and a browser refuses an answer with two
/// `Access-Control-Allow-Origin`s whole. The router's own goes out in
/// place of the node's, on an answer and on a stream's head.
#[test]
fn a_forwarded_answer_tells_the_routers_rule_once() {
    let n = node("fwd", Some("*"));
    let port = in_front_of(&n, Some(APP));
    let r = ask(port, "GET", "/t/acme/collections", Some(APP), "");
    assert_eq!(r.status, 200, "{}", r.body);
    r.tells(APP);

    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let create = "create collection notes (title text)";
    let body = format!("{{\"query\":\"{create}\"}}");
    write!(
        s,
        "POST /t/acme/query HTTP/1.1\r\nHost: t\r\nOrigin: {APP}\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let r = read_answer(&mut s);
    assert_eq!(r.status, 200, "{}", r.body);
    r.tells(APP);

    // A subscription's head, copied from the node's.
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "GET /t/acme/notes/changes HTTP/1.1\r\nHost: t\r\nOrigin: {APP}\r\n\r\n"
    )
    .unwrap();
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        s.read_exact(&mut byte).unwrap();
        head.push(byte[0]);
    }
    let r = Answer {
        status: 200,
        head: String::from_utf8(head).unwrap(),
        body: String::new(),
    };
    assert!(r.head.starts_with("HTTP/1.1 200"), "{}", r.head);
    assert!(r.head.contains("text/event-stream"), "{}", r.head);
    r.tells(APP);
}

/// Without `--http-cors` the router is what it was: none of its own, and a
/// node's passed through as the node wrote them.
#[test]
fn without_http_cors_the_nodes_headers_pass_through() {
    let n = node("off", Some("*"));
    let port = in_front_of(&n, None);
    let r = ask(port, "GET", "/t/acme/collections", Some(APP), "");
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.one("access-control-allow-origin"), Some("*"));
    let r = ask(port, "GET", "/t/nosuch/collections", Some(APP), "");
    assert_eq!(r.status, 404);
    r.tells_nothing();
    // The preflight goes to the node, which answers it.
    let r = ask(port, "OPTIONS", "/t/acme/query", Some(APP), PREFLIGHT);
    assert_eq!(r.status, 204, "{}", r.body);
    assert_eq!(r.one("access-control-allow-origin"), Some("*"));
    let r = ask(port, "OPTIONS", "/t/nosuch/query", Some(APP), PREFLIGHT);
    assert_eq!(r.status, 404);
    r.tells_nothing();
}
