//! A replica whose attach fails is tried again, and a file the router left
//! on a node is taken off.
//!
//! With `--replicas` a create, a move and a repair each give a tenant a
//! replica on another node: the file made there (`PUT`) and made to follow
//! the primary (`POST .../follow`). An attach that failed -- the node
//! refused, or took the follow past the router's timeout -- was logged and
//! named in the answer's `unreplicated`, and nothing tried it again: the
//! tenant had no copy to fail over to until a node failed over answered
//! again or an operator asked, and a follow that landed after the router
//! gave it up left a copy following the primary that no directory named.
//! And the `DELETE`s that take a create back ignored their failure, so the
//! file stayed on its node and every create of the name there met it, 409.
//!
//! Each node the router reaches here through a proxy the test arms: it
//! refuses a request whose line names what it is armed with (503), or sends
//! it on and swallows the answer, the connection ended with nothing said --
//! the node ran it, the router heard nothing. A test waits for what must
//! happen, a repair's tries 2, 4 and 8 s apart, bounded by [`WITHIN`].

use fenec_http::tenants::{Replicated, Tenants};
use fenec_shard::directory::Directory;
use fenec_shard::{Config, Router};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const REPL: &str = "tenant-replication";

/// The longest a test waits for what must happen: a repair's first tries
/// come 2, 4, 8 and 16 s after the failure, each a few calls, under a
/// machine's load from other suites besides.
const WITHIN: Duration = Duration::from_secs(120);

struct Node {
    port: u16,
    dir: PathBuf,
    tenants: Arc<Tenants>,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Node {
    fn holds(&self, tenant: &str) -> bool {
        self.tenants.names().iter().any(|t| t == tenant)
    }
}

fn node(tag: &str) -> Node {
    let dir = std::env::temp_dir().join(format!("fenec-retries-{tag}-{}", std::process::id()));
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
        admin_token: Some(format!("adm-{tag}")),
        sync_on_write: true,
        ..fenec_http::Config::default()
    };
    let server = fenec_http::Server::with_tenants(Arc::clone(&tenants), cfg);
    let listener = server.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    Node { port, dir, tenants }
}

/// What the proxy does with a request whose line holds a rule's text.
#[derive(Clone, Copy)]
enum Act {
    /// Answers 503 itself: the node never sees the request.
    Refuse,
    /// Sends it on and takes the node's answer, then -- once the test lets
    /// it ([`Proxy::release`]) -- ends the connection with nothing said.
    Swallow,
    /// Holds it, not sent on, until the test lets it go, then sends it on
    /// and the answer back.
    Hold,
}

struct Rule {
    what: &'static str,
    act: Act,
    /// How many more requests it takes; `usize::MAX`, every one.
    times: usize,
}

#[derive(Default)]
struct Rules {
    list: Vec<Rule>,
    /// Every request line it read.
    seen: Vec<String>,
    /// The answers swallowed and held until released.
    holding: usize,
    released: bool,
}

type Shared = Arc<(Mutex<Rules>, Condvar)>;

/// A proxy in front of a node, a request a connection: each is sent on with
/// `Connection: close`, and the node's answer, which says so, passed back,
/// so the router keeps none of its connections to it.
struct Proxy {
    addr: String,
    rules: Shared,
}

impl Proxy {
    fn to(port: u16) -> Proxy {
        let target = format!("127.0.0.1:{port}");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let rules: Shared = Arc::default();
        let shared = Arc::clone(&rules);
        std::thread::spawn(move || {
            for client in listener.incoming() {
                let Ok(client) = client else { continue };
                let (target, rules) = (target.clone(), Arc::clone(&shared));
                std::thread::spawn(move || serve(client, &target, &rules));
            }
        });
        Proxy { addr, rules }
    }

    fn arm(&self, what: &'static str, act: Act, times: usize) {
        self.rules
            .0
            .lock()
            .unwrap()
            .list
            .push(Rule { what, act, times });
    }

    fn disarm(&self, what: &str) {
        self.rules.0.lock().unwrap().list.retain(|r| r.what != what);
    }

    /// How many requests whose line holds `what` it read.
    fn seen(&self, what: &str) -> usize {
        let r = self.rules.0.lock().unwrap();
        r.seen.iter().filter(|l| l.contains(what)).count()
    }

    /// Waits until an answer is swallowed and held.
    fn wait_holding(&self) {
        let (m, cv) = &*self.rules;
        let deadline = Instant::now() + WITHIN;
        let mut r = m.lock().unwrap();
        while r.holding == 0 {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "no answer was swallowed");
            r = cv.wait_timeout(r, left).unwrap().0;
        }
    }

    /// Lets the answers it holds go, unsaid.
    fn release(&self) {
        self.rules.0.lock().unwrap().released = true;
        self.rules.1.notify_all();
    }
}

fn serve(mut client: TcpStream, target: &str, rules: &Shared) {
    let Some(bytes) = request_of(&client) else {
        return;
    };
    let line = String::from_utf8_lossy(&bytes)
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    let act = {
        let mut r = rules.0.lock().unwrap();
        r.seen.push(line.clone());
        let rule = r
            .list
            .iter_mut()
            .find(|rule| rule.times > 0 && line.contains(rule.what));
        rule.map(|rule| {
            rule.times = rule.times.saturating_sub(1);
            rule.act
        })
    };
    match act {
        Some(Act::Refuse) => {
            let body = r#"{"error":"refused by the test's proxy"}"#;
            let _ = write!(
                client,
                "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
        Some(Act::Swallow) => {
            let _ = asked_of(target, &bytes);
            held_until_released(rules);
        }
        Some(Act::Hold) => {
            held_until_released(rules);
            if let Some(answer) = asked_of(target, &bytes) {
                let _ = client.write_all(&answer);
            }
        }
        None => {
            if let Some(answer) = asked_of(target, &bytes) {
                let _ = client.write_all(&answer);
            }
        }
    }
    let _ = client.shutdown(Shutdown::Both);
}

/// Counted as held, until the test lets every held one go.
fn held_until_released(rules: &Shared) {
    let (m, cv) = &**rules;
    let mut r = m.lock().unwrap();
    r.holding += 1;
    cv.notify_all();
    while !r.released {
        r = cv.wait(r).unwrap();
    }
}

/// The first request a client sends on a connection, whole: its head and
/// the body its `Content-Length` names.
fn request_of(client: &TcpStream) -> Option<Vec<u8>> {
    let mut client = client.try_clone().ok()?;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = client.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..n]);
        let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
        let length: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        if bytes.len() >= end + 4 + length {
            return Some(bytes);
        }
    }
}

/// The node's whole answer to the request `bytes`, asked to close after it.
fn asked_of(target: &str, bytes: &[u8]) -> Option<Vec<u8>> {
    let mut server = TcpStream::connect(target).ok()?;
    let line = bytes
        .windows(2)
        .position(|w| w == b"\r\n")
        .map_or(bytes.len(), |n| n + 2);
    let mut sent = bytes[..line].to_vec();
    sent.extend_from_slice(b"Connection: close\r\n");
    sent.extend_from_slice(&bytes[line..]);
    server.write_all(&sent).ok()?;
    let mut answer = Vec::new();
    server.read_to_end(&mut answer).ok()?;
    Some(answer)
}

fn call(port: u16, method: &str, target: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(WITHIN)).unwrap();
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

fn query(port: u16, path: &str, sql: &str) -> (u16, String) {
    let mut body = String::from("{\"query\":");
    fenec_core::json::escape_into(&mut body, sql);
    body.push('}');
    call(port, "POST", path, &body)
}

/// A router giving each tenant a replica of its own (`--replicas`), with
/// fenec-shard's own upstream timeout: nothing here waits one out.
fn router() -> u16 {
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        replicas: true,
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

/// `name` recorded at `addr`, in region `eu` and `zone`.
fn put_node(port: u16, name: &str, addr: &str, tag: &str, zone: &str) {
    let body = format!(r#"{{"addr":"{addr}","token":"adm-{tag}","region":"eu","zone":"{zone}"}}"#);
    let (status, body) = call(port, "PUT", &format!("/_shard/nodes/{name}"), &body);
    assert!(status == 200 || status == 201, "{body}");
}

/// Where the directory keeps `tenant`: its node, and its replica's.
fn placed(port: u16, tenant: &str) -> (String, Option<String>) {
    let (status, body) = call(port, "GET", &format!("/_shard/tenants/{tenant}"), "");
    assert_eq!(status, 200, "{body}");
    let value = |key: &str| {
        let rest = body.split(&format!("\"{key}\":\"")).nth(1)?;
        Some(rest.split('"').next()?.to_string())
    };
    (value("node").unwrap(), value("replica"))
}

/// Waits until `check` holds, [`WITHIN`] at most.
fn until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + WITHIN;
    while !check() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Waits until the node at `port` reads `want` in `sql` on `tenant`.
fn reads(port: u16, tenant: &str, sql: &str, want: &str) {
    let path = format!("/t/{tenant}/query");
    until(&format!("{tenant} on :{port} never read {want}"), || {
        query(port, &path, sql).1.contains(want)
    });
}

/// `acme` made on n1 with a row in it, through the router: the create's
/// answer.
fn create_acme(port: u16) -> String {
    let (status, body) = call(port, "PUT", "/_shard/tenants/acme", r#"{"node":"n1"}"#);
    assert_eq!(status, 201, "{body}");
    let path = "/t/acme/query";
    assert_eq!(
        query(port, path, "create collection notes (title text)").0,
        200
    );
    assert_eq!(query(port, path, r#"put notes {title: "one"}"#).0, 200);
    body
}

/// The gauge of tenants without a copy, as the router's scrape has it for
/// region `eu`.
fn unreplicated(port: u16) -> Option<u64> {
    let (status, text) = call(port, "GET", "/_metrics", "");
    assert_eq!(status, 200, "{text}");
    text.lines()
        .find_map(|l| l.strip_prefix("fenec_router_tenants_unreplicated{region=\"eu\"} "))
        .map(|v| v.trim().parse().unwrap())
}

/// A create whose replica's node refused the follow once gets its replica
/// all the same, with no operator asking: a repair tries again a couple of
/// seconds later, on the node holding the copy the first try made. Left as
/// it was, the tenant had none until a node failed over answered again or
/// `POST /_shard/replicas`.
#[test]
fn a_create_whose_follow_was_refused_gets_its_replica_from_a_repair() {
    let (n1, n2) = (node("cr1"), node("cr2"));
    let p2 = Proxy::to(n2.port);
    p2.arm("/acme/follow", Act::Refuse, 1);
    let port = router();
    put_node(port, "n1", &format!("127.0.0.1:{}", n1.port), "cr1", "");
    put_node(port, "n2", &p2.addr, "cr2", "");
    let body = create_acme(port);
    assert!(body.contains("\"unreplicated\":"), "{body}");
    assert!(body.contains("a repair tries again"), "{body}");
    until("acme never got its replica", || {
        placed(port, "acme").1.as_deref() == Some("n2")
    });
    reads(n2.port, "acme", "get notes select title", "\"one\"");
    // The refused follow, and the repair's, once.
    assert_eq!(p2.seen("/acme/follow"), 2);
}

/// A repair's attach that failed is tried again, as a create's is: it
/// answered 502 and let the tenant be. The `PUT` is what n2 refuses, so the
/// try leaves no file there for the repair to wait on.
#[test]
fn a_repair_whose_attach_failed_tries_again() {
    let (n1, n2) = (node("rp1"), node("rp2"));
    let p2 = Proxy::to(n2.port);
    let port = router();
    put_node(port, "n1", &format!("127.0.0.1:{}", n1.port), "rp1", "");
    // Made while n1 is the only node: no other node to keep a replica,
    // which nothing tries again.
    let body = create_acme(port);
    assert!(body.contains("no other node"), "{body}");
    put_node(port, "n2", &p2.addr, "rp2", "");
    assert_eq!(placed(port, "acme").1, None);
    p2.arm("PUT /_admin/tenants/acme ", Act::Refuse, 1);
    let (status, body) = call(port, "POST", "/_shard/replicas", "");
    assert_eq!(status, 502, "{body}");
    assert!(body.contains("answered 503"), "{body}");
    until("the repair's failed attach was never tried again", || {
        placed(port, "acme").1.as_deref() == Some("n2")
    });
    reads(n2.port, "acme", "get notes select title", "\"one\"");
}

/// A follow that landed on its node after the router gave it up -- a
/// rename slower than the router's timeout, or an answer lost on the way --
/// left a copy following the primary that no directory named. The repair
/// that tries again adopts it: the node holding the copy is picked first
/// among those of the zone it would pick, so no second copy is placed --
/// here n0, of the same zone as n2 by then and first by name, which a try
/// blind to the copy picks.
#[test]
fn a_follow_landing_after_the_router_gave_it_up_is_adopted_not_doubled() {
    let (n0, n1, n2) = (node("ad0"), node("ad1"), node("ad2"));
    let p2 = Proxy::to(n2.port);
    p2.arm("/acme/follow", Act::Swallow, 1);
    let port = router();
    put_node(port, "n1", &format!("127.0.0.1:{}", n1.port), "ad1", "a");
    put_node(port, "n2", &p2.addr, "ad2", "b");
    // In n1's zone: the create's try picks n2, of another.
    put_node(port, "n0", &format!("127.0.0.1:{}", n0.port), "ad0", "a");
    let made = std::thread::spawn(move || create_acme(port));
    // n2 has followed n1; the router has not heard it yet.
    p2.wait_holding();
    // n0 is in n2's zone from now: ranked by load and name alone, n0 comes
    // first.
    put_node(port, "n0", &format!("127.0.0.1:{}", n0.port), "ad0", "b");
    p2.release();
    let body = made.join().unwrap();
    assert!(body.contains("\"unreplicated\":"), "{body}");
    until("acme's copy on n2 was never adopted", || {
        placed(port, "acme").1.is_some()
    });
    assert_eq!(placed(port, "acme"), ("n1".into(), Some("n2".into())));
    assert!(!n0.holds("acme"), "a second copy was placed on n0");
    reads(n2.port, "acme", "get notes select title", "\"one\"");
}

/// Two repairs at once -- an operator's beside the router's own try -- give
/// a tenant one replica: the one that comes to it second asks again under
/// its claim and finds it given. Asked by the list it read before, it gave
/// `bbb` another on n3, the node with fewer of n1's replicas by then, the
/// copy on n2 left following unrecorded.
#[test]
fn two_repairs_at_once_place_a_tenants_replica_once() {
    let (n1, n2, n3) = (node("tw1"), node("tw2"), node("tw3"));
    let p2 = Proxy::to(n2.port);
    let port = router();
    put_node(port, "n1", &format!("127.0.0.1:{}", n1.port), "tw1", "");
    // Made while n1 is the only node: neither has a replica, and nothing
    // tries again.
    for t in ["aaa", "bbb"] {
        let (status, body) = call(
            port,
            "PUT",
            &format!("/_shard/tenants/{t}"),
            r#"{"node":"n1"}"#,
        );
        assert_eq!(status, 201, "{body}");
    }
    put_node(port, "n2", &p2.addr, "tw2", "");
    put_node(port, "n3", &format!("127.0.0.1:{}", n3.port), "tw3", "");
    // The first repair, `aaa` before `bbb`, held at aaa's follow on n2.
    p2.arm("/aaa/follow", Act::Hold, 1);
    let first = std::thread::spawn(move || call(port, "POST", "/_shard/replicas", ""));
    p2.wait_holding();
    // The second passes over `aaa`, held, and gives `bbb` its replica.
    let (status, body) = call(port, "POST", "/_shard/replicas", "");
    assert_eq!(status, 502, "{body}");
    assert!(
        body.contains(r#"{"tenant":"bbb","replica":"n2"}"#),
        "{body}"
    );
    p2.release();
    let (status, body) = first.join().unwrap();
    assert_eq!(status, 200, "{body}");
    assert!(!body.contains("\"bbb\""), "{body}");
    assert_eq!(placed(port, "aaa").1.as_deref(), Some("n2"));
    assert_eq!(placed(port, "bbb").1.as_deref(), Some("n2"));
    assert!(!n3.holds("bbb"), "bbb was given a second copy, on n3");
}

/// A node that did not answer the repair's look is given no replica: one
/// that answers is, though the silent one comes first by name. Tried, its
/// copy would follow nothing from a node that stays silent, and each
/// repair's try at it waited out the upstream timeout.
#[test]
fn a_node_silent_at_the_repairs_look_is_given_no_replica() {
    let (n1, n2, n3) = (node("sl1"), node("sl2"), node("sl3"));
    let p2 = Proxy::to(n2.port);
    let port = router();
    put_node(port, "n1", &format!("127.0.0.1:{}", n1.port), "sl1", "");
    create_acme(port);
    put_node(port, "n2", &p2.addr, "sl2", "");
    put_node(port, "n3", &format!("127.0.0.1:{}", n3.port), "sl3", "");
    p2.arm("GET /_admin/tenants HTTP", Act::Refuse, usize::MAX);
    let (status, body) = call(port, "POST", "/_shard/replicas", "");
    assert_eq!(status, 200, "{body}");
    assert_eq!(placed(port, "acme").1.as_deref(), Some("n3"), "{body}");
    assert!(!n2.holds("acme"));
}

/// A deleted tenant's replica its node would not take off is taken off by
/// the repair after: left, it stayed on the node for good, and a create of
/// the name placed there met it, 409.
#[test]
fn a_deleted_tenants_replica_that_stayed_is_taken_off() {
    let (n1, n2) = (node("dr1"), node("dr2"));
    let p2 = Proxy::to(n2.port);
    let port = router();
    put_node(port, "n1", &format!("127.0.0.1:{}", n1.port), "dr1", "");
    put_node(port, "n2", &p2.addr, "dr2", "");
    create_acme(port);
    assert_eq!(placed(port, "acme").1.as_deref(), Some("n2"));
    p2.arm("DELETE /_admin/tenants/acme ", Act::Refuse, 1);
    let (status, body) = call(port, "DELETE", "/_shard/tenants/acme", "");
    assert_eq!(status, 204, "{body}");
    assert!(
        n2.holds("acme"),
        "the refused DELETE took the copy off after all"
    );
    until(
        "the deleted tenant's copy on n2 was never taken off",
        || !n2.holds("acme"),
    );
}

/// A move undone -- its target refused the tenant's quota -- whose `DELETE`
/// the target did not take left the image there, an orphan beside the
/// tenant still on its source; the repair after takes it off.
#[test]
fn a_move_undone_leaves_no_copy_on_its_target() {
    let (n1, n2, n3) = (node("mv1"), node("mv2"), node("mv3"));
    let p3 = Proxy::to(n3.port);
    let port = router();
    put_node(port, "n1", &format!("127.0.0.1:{}", n1.port), "mv1", "");
    put_node(port, "n2", &format!("127.0.0.1:{}", n2.port), "mv2", "");
    put_node(port, "n3", &p3.addr, "mv3", "");
    create_acme(port);
    assert_eq!(placed(port, "acme"), ("n1".into(), Some("n2".into())));
    let (status, body) = call(
        port,
        "PUT",
        "/_shard/tenants/acme/quota",
        r#"{"rows":1000}"#,
    );
    assert_eq!(status, 200, "{body}");
    p3.arm("PUT /_admin/tenants/acme/quota ", Act::Refuse, 1);
    p3.arm("DELETE /_admin/tenants/acme ", Act::Refuse, 1);
    let (status, body) = call(port, "POST", "/_shard/tenants/acme/move", r#"{"to":"n3"}"#);
    assert_eq!(status, 502, "{body}");
    assert_eq!(placed(port, "acme"), ("n1".into(), Some("n2".into())));
    assert!(
        n3.holds("acme"),
        "the refused DELETE took the copy off after all"
    );
    until("the copy the move left on n3 was never taken off", || {
        !n3.holds("acme")
    });
    assert_eq!(placed(port, "acme"), ("n1".into(), Some("n2".into())));
}

/// A copy a failed attach made is taken off where the try after places the
/// replica elsewhere: a node of another zone than the primary's came free
/// meanwhile, which the rule puts first. Left, it went on following the
/// primary beside the replica the directory names.
#[test]
fn a_copy_left_where_the_replica_went_elsewhere_is_taken_off() {
    let (n0, n1, n2) = (node("to0"), node("to1"), node("to2"));
    let p2 = Proxy::to(n2.port);
    p2.arm("/acme/follow", Act::Swallow, 1);
    let port = router();
    put_node(port, "n1", &format!("127.0.0.1:{}", n1.port), "to1", "a");
    put_node(port, "n2", &p2.addr, "to2", "b");
    put_node(port, "n0", &format!("127.0.0.1:{}", n0.port), "to0", "a");
    let made = std::thread::spawn(move || create_acme(port));
    p2.wait_holding();
    // The zones turn: n0 is the one of another zone than n1's now.
    put_node(port, "n2", &p2.addr, "to2", "a");
    put_node(port, "n0", &format!("127.0.0.1:{}", n0.port), "to0", "b");
    p2.release();
    let body = made.join().unwrap();
    assert!(body.contains("\"unreplicated\":"), "{body}");
    until("acme never got its replica on n0", || {
        placed(port, "acme").1.as_deref() == Some("n0")
    });
    reads(n0.port, "acme", "get notes select title", "\"one\"");
    until("the copy left on n2 was never taken off", || {
        !n2.holds("acme")
    });
}

/// A create taken back -- its schema refused -- whose `DELETE` its node did
/// not take left the file there, an orphan; the router takes it off itself
/// in a while, and the name is made there again.
#[test]
fn a_create_taken_back_leaves_no_orphan_on_its_node() {
    let n1 = node("tb1");
    let p1 = Proxy::to(n1.port);
    p1.arm("/acme/schema", Act::Refuse, 1);
    p1.arm("DELETE /_admin/tenants/acme ", Act::Refuse, 1);
    let port = router();
    put_node(port, "n1", &p1.addr, "tb1", "");
    let (status, body) = call(
        port,
        "PUT",
        "/_shard/tenants/acme",
        r#"{"node":"n1","schema":"create collection notes (title text)"}"#,
    );
    assert_eq!(status, 503, "{body}");
    assert_eq!(call(port, "GET", "/_shard/tenants/acme", "").0, 404);
    assert!(
        n1.holds("acme"),
        "the refused DELETE took the file off after all"
    );
    until(
        "the file a create taken back left was never taken off",
        || !n1.holds("acme"),
    );
    create_acme(port);
}

/// A create of the name meeting the file a create taken back left there
/// takes it off and makes the tenant: on main every create of the name on
/// that node was refused, 409. The repair that would take it off on its
/// own is kept from it here, the node's look refused.
#[test]
fn a_create_meeting_the_file_a_create_taken_back_left_takes_it_off() {
    let n1 = node("mt1");
    let p1 = Proxy::to(n1.port);
    p1.arm("GET /_admin/tenants HTTP", Act::Refuse, usize::MAX);
    p1.arm("/acme/schema", Act::Refuse, 1);
    p1.arm("DELETE /_admin/tenants/acme ", Act::Refuse, usize::MAX);
    let port = router();
    put_node(port, "n1", &p1.addr, "mt1", "");
    let (status, body) = call(
        port,
        "PUT",
        "/_shard/tenants/acme",
        r#"{"node":"n1","schema":"create collection notes (title text)"}"#,
    );
    assert_eq!(status, 503, "{body}");
    assert!(n1.holds("acme"));
    p1.disarm("DELETE /_admin/tenants/acme ");
    let body = create_acme(port);
    assert!(body.contains("\"node\":\"n1\""), "{body}");
    assert_eq!(placed(port, "acme").0, "n1");
}

/// `fenec_router_tenants_unreplicated{region}` counts the tenants with no
/// copy a failover could promote, and falls as a repair's try gives one a
/// replica.
#[test]
fn the_unreplicated_gauge_counts_tenants_without_a_copy_and_clears() {
    let (n1, n2) = (node("gg1"), node("gg2"));
    let p2 = Proxy::to(n2.port);
    p2.arm("/acme/follow", Act::Refuse, usize::MAX);
    let port = router();
    assert_eq!(unreplicated(port), None, "no node names a region yet");
    put_node(port, "n1", &format!("127.0.0.1:{}", n1.port), "gg1", "");
    put_node(port, "n2", &p2.addr, "gg2", "");
    assert_eq!(unreplicated(port), Some(0));
    create_acme(port);
    assert_eq!(unreplicated(port), Some(1));
    p2.disarm("/acme/follow");
    until("the gauge never cleared", || unreplicated(port) == Some(0));
    assert_eq!(placed(port, "acme").1.as_deref(), Some("n2"));
}
