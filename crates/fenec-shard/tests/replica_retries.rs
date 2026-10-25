//! A replica whose attach fails is tried again, and a file the router may
//! have left on a node is reported, never taken off.
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
//! file stayed on its node, unsaid, and every create of the name there met
//! it, 409. Such a file is noted now and reported -- every repair's `left`,
//! `fenec_router_files_left` -- for an operator, and nothing deletes it:
//! taken back, the router lost a tenant moved back onto the node and one
//! made again there.
//!
//! Each node the router reaches here through a proxy the test arms: it
//! refuses a request whose line names what it is armed with (503), holds
//! it, or sends it on and swallows the answer, the connection ended with
//! nothing said -- the node ran it, the router heard nothing. A test waits
//! for what must happen, a repair's tries 2, 4 and 8 s apart, bounded by
//! [`WITHIN`].

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
    node_of(tag, None)
}

/// A node; `follows` makes it the standby of the node at that address.
fn node_of(tag: &str, follows: Option<u16>) -> Node {
    let dir = std::env::temp_dir().join(format!("fenec-retries-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let tenants = Arc::new(Tenants::new(&dir).unwrap().with_replication(Replicated {
        token: REPL.into(),
        buffer: 8 << 20,
        upstream: follows.map(|p| format!("http://127.0.0.1:{p}")),
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
        // Carried on as it comes, so a stream -- a replica following the
        // node through it -- flows; the node asked to close after, so no
        // second request rides the connection past the rules.
        None => {
            let Ok(mut server) = TcpStream::connect(target) else {
                let _ = client.shutdown(Shutdown::Both);
                return;
            };
            if server.write_all(&closing(&bytes)).is_err() {
                let _ = client.shutdown(Shutdown::Both);
                return;
            }
            let _ = std::io::copy(&mut server, &mut client);
        }
    }
    let _ = client.shutdown(Shutdown::Both);
}

/// The request `bytes` with `Connection: close` asked for first.
fn closing(bytes: &[u8]) -> Vec<u8> {
    let line = bytes
        .windows(2)
        .position(|w| w == b"\r\n")
        .map_or(bytes.len(), |n| n + 2);
    let mut sent = bytes[..line].to_vec();
    sent.extend_from_slice(b"Connection: close\r\n");
    sent.extend_from_slice(&bytes[line..]);
    sent
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
    server.write_all(&closing(bytes)).ok()?;
    let mut answer = Vec::new();
    server.read_to_end(&mut answer).ok()?;
    Some(answer)
}

fn call(port: u16, method: &str, target: &str, body: &str) -> (u16, String) {
    call_as(port, method, target, body, None)
}

/// [`call`] with a bearer token: a node's admin calls, made by hand.
fn call_as(
    port: u16,
    method: &str,
    target: &str,
    body: &str,
    token: Option<&str>,
) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(WITHIN)).unwrap();
    let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
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
    call(port, "POST", path, &body)
}

/// A router giving each tenant a replica of its own (`--replicas`), with
/// fenec-shard's own upstream timeout: nothing here waits one out.
fn router() -> u16 {
    router_over(Directory::in_memory(), true).0
}

/// A router over `dir`, giving tenants replicas of their own where
/// `replicas`: its port, and the router.
fn router_over(dir: Directory, replicas: bool) -> (u16, Arc<Router>) {
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        replicas,
        ..Config::default()
    };
    let router = Router::new(dir, cfg);
    let listener = router.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    let served = Arc::clone(&router);
    std::thread::spawn(move || {
        let _ = served.serve_on(listener);
    });
    (port, router)
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

/// A gauge of the router's scrape, the sample `line` begins.
fn gauge(port: u16, line: &str) -> Option<u64> {
    let (status, text) = call(port, "GET", "/_metrics", "");
    assert_eq!(status, 200, "{text}");
    text.lines()
        .find_map(|l| l.strip_prefix(line))
        .map(|v| v.trim().parse().unwrap())
}

/// The gauge of tenants without a copy, as the router's scrape has it for
/// region `eu`.
fn unreplicated(port: u16) -> Option<u64> {
    gauge(port, "fenec_router_tenants_unreplicated{region=\"eu\"} ")
}

/// The gauge of files the router may have left on nodes.
fn files_left(port: u16) -> u64 {
    gauge(port, "fenec_router_files_left ").unwrap()
}

/// A repair asked of the router at `port`: its status and answer.
fn repair(port: u16) -> (u16, String) {
    call(port, "POST", "/_shard/replicas", "")
}

/// Whether a repair's answer reports `tenant`'s file on `node` as left
/// there, and why holding `why`.
fn reports(body: &str, tenant: &str, node: &str, why: &str) -> bool {
    let entry = format!(r#"{{"tenant":"{tenant}","node":"{node}","why":""#);
    let Some(rest) = body.split(&entry).nth(1) else {
        return false;
    };
    // The JSON string, to its closing quote: an escaped one goes on.
    let mut text = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => text.extend(chars.next()),
            '"' => break,
            c => text.push(c),
        }
    }
    text.contains(why)
}

/// A create whose replica's node refused the follow once gets its replica
/// all the same, with no operator asking: a repair tries again a couple of
/// seconds later, on the node holding the copy the first try made, and the
/// note of that copy as left goes as it is recorded. Left as it was, the
/// tenant had none until a node failed over answered again or `POST
/// /_shard/replicas`.
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
    assert_eq!(files_left(port), 0);
}

/// A repair's attach that failed is tried again, as a create's is: it
/// answered 502 and let the tenant be. The `PUT` is what n2 refuses, so the
/// try leaves no file there.
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
    let (status, body) = repair(port);
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
    assert_eq!(files_left(port), 0);
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
    let first = std::thread::spawn(move || repair(port));
    p2.wait_holding();
    // The second passes over `aaa`, held, and gives `bbb` its replica.
    let (status, body) = repair(port);
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
    let (status, body) = repair(port);
    assert_eq!(status, 200, "{body}");
    assert_eq!(placed(port, "acme").1.as_deref(), Some("n3"), "{body}");
    assert!(!n2.holds("acme"));
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

/// `tenant` created on n1 through `port` with its schema refused at the
/// proxy `p` and its take-back's `DELETE` refused too, `schema` and
/// `delete` naming those requests: the file stays on n1, an orphan the
/// router notes as left.
fn create_taken_back(
    port: u16,
    p: &Proxy,
    tenant: &str,
    schema: &'static str,
    delete: &'static str,
) {
    p.arm(schema, Act::Refuse, 1);
    p.arm(delete, Act::Refuse, 1);
    let (status, body) = call(
        port,
        "PUT",
        &format!("/_shard/tenants/{tenant}"),
        r#"{"node":"n1","schema":"create collection notes (title text)"}"#,
    );
    assert_eq!(status, 503, "{body}");
    assert_eq!(
        call(port, "GET", &format!("/_shard/tenants/{tenant}"), "").0,
        404
    );
}

/// `tenant` made through `port` on `node`, a row written in it.
fn create_on(port: u16, tenant: &str, node: &str) {
    let (status, body) = call(
        port,
        "PUT",
        &format!("/_shard/tenants/{tenant}"),
        &format!(r#"{{"node":"{node}"}}"#),
    );
    assert_eq!(status, 201, "{body}");
    let path = format!("/t/{tenant}/query");
    assert_eq!(
        query(port, &path, "create collection notes (title text)").0,
        200
    );
    assert_eq!(query(port, &path, r#"put notes {title: "one"}"#).0, 200);
}

/// The node at `port`, admin token `adm-{tag}`, asked to delete `tenant`
/// by hand, as an operator does.
fn delete_by_hand(port: u16, tag: &str, tenant: &str) {
    let token = format!("adm-{tag}");
    let target = format!("/_admin/tenants/{tenant}");
    let (status, body) = call_as(port, "DELETE", &target, "", Some(&token));
    assert!(status == 204 || status == 404, "{body}");
}

/// A tenant made by hand on the node at `port`, admin token `adm-{tag}`, a
/// row written in it: a file put under the name with writes no router left.
fn made_by_hand(port: u16, tag: &str, tenant: &str) {
    delete_by_hand(port, tag, tenant);
    let target = format!("/_admin/tenants/{tenant}");
    let token = format!("adm-{tag}");
    let (status, body) = call_as(port, "PUT", &target, "", Some(&token));
    assert_eq!(status, 201, "{body}");
    let path = format!("/t/{tenant}/query");
    assert_eq!(
        query(port, &path, "create collection notes (title text)").0,
        200
    );
    assert_eq!(query(port, &path, r#"put notes {title: "by hand"}"#).0, 200);
}

/// A create taken back -- its schema refused -- whose `DELETE` its node did
/// not take leaves the file there, an orphan: noted, reported by every
/// repair and counted, never taken off, and a create of the name there met
/// as 409, as on main. Once an operator removes the file by hand, the
/// repair after finds it gone and forgets it, and the name is made there.
/// Unnoted, the orphan was there for good with nothing to say so.
#[test]
fn a_create_taken_back_leaves_its_file_reported_until_it_is_removed_by_hand() {
    let n1 = node("tb1");
    let p1 = Proxy::to(n1.port);
    let (port, router) = router_over(Directory::in_memory(), true);
    put_node(port, "n1", &p1.addr, "tb1", "");
    create_taken_back(
        port,
        &p1,
        "acme",
        "/acme/schema",
        "DELETE /_admin/tenants/acme ",
    );
    assert!(
        n1.holds("acme"),
        "the refused DELETE took the file off after all"
    );
    assert!(!router.repair_due(), "a file left scheduled a repair");
    assert_eq!(files_left(port), 1);
    let (status, body) = repair(port);
    assert_eq!(status, 200, "{body}");
    assert!(
        reports(
            &body,
            "acme",
            "n1",
            "a create taken back, its schema not applied"
        ),
        "{body}"
    );
    assert!(n1.holds("acme"), "the repair took the file off");
    let (status, body) = call(port, "PUT", "/_shard/tenants/acme", r#"{"node":"n1"}"#);
    assert_eq!(status, 409, "{body}");
    assert!(n1.holds("acme"), "the create took the file off");
    // The one `DELETE` the create's take-back sent: none since.
    assert_eq!(p1.seen("DELETE /_admin/tenants/acme "), 1);
    // Removed by hand: the repair after finds it gone.
    delete_by_hand(n1.port, "tb1", "acme");
    let (status, body) = repair(port);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""left":[]"#), "{body}");
    assert_eq!(files_left(port), 0);
    create_acme(port);
}

/// A create whose `PUT` went unanswered -- the node made the file all the
/// same -- leaves it noted and reported, not taken off.
#[test]
fn a_create_whose_put_went_unanswered_is_reported() {
    let n1 = node("pu1");
    let p1 = Proxy::to(n1.port);
    p1.release();
    p1.arm("PUT /_admin/tenants/acme ", Act::Swallow, 1);
    let port = router();
    put_node(port, "n1", &p1.addr, "pu1", "");
    let (status, body) = call(port, "PUT", "/_shard/tenants/acme", r#"{"node":"n1"}"#);
    assert_eq!(status, 502, "{body}");
    assert!(n1.holds("acme"));
    let (status, body) = repair(port);
    assert_eq!(status, 200, "{body}");
    assert!(
        reports(&body, "acme", "n1", "a create whose PUT went unanswered"),
        "{body}"
    );
    assert!(n1.holds("acme"), "the repair took the file off");
    assert_eq!(files_left(port), 1);
}

/// A deleted tenant's replica its node would not take off is noted and
/// reported, and stays on the node for an operator: unnoted, it stayed for
/// good, unsaid, and a create of the name placed there met it, 409.
#[test]
fn a_deleted_tenants_replica_that_stayed_is_reported() {
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
    let (status, body) = repair(port);
    assert_eq!(status, 200, "{body}");
    assert!(
        reports(
            &body,
            "acme",
            "n2",
            "a deleted tenant's replica that stayed"
        ),
        "{body}"
    );
    assert!(n2.holds("acme"), "the repair took the copy off");
    assert_eq!(p2.seen("DELETE /_admin/tenants/acme "), 1);
}

/// A move undone -- its target refused the tenant's quota -- whose `DELETE`
/// the target did not take leaves the image there, beside the tenant still
/// on its source: reported, not taken off.
#[test]
fn a_move_undone_leaves_its_targets_copy_reported() {
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
    let (status, body) = repair(port);
    assert_eq!(status, 200, "{body}");
    assert!(
        reports(&body, "acme", "n3", "a move undone, its quota not taken"),
        "{body}"
    );
    assert!(n3.holds("acme"), "the repair took the copy off");
    reads(n1.port, "acme", "get notes select title", "\"one\"");
}

/// A move whose source would not let go of its copy once the move was done
/// -- its `DELETE` refused -- leaves the copy noted and reported: unnoted,
/// it stayed on the source, frozen, for good, unsaid.
#[test]
fn a_moves_source_that_would_not_go_is_reported() {
    let (n1, n2, n3) = (node("ms1"), node("ms2"), node("ms3"));
    let p1 = Proxy::to(n1.port);
    let port = router();
    put_node(port, "n1", &p1.addr, "ms1", "");
    put_node(port, "n2", &format!("127.0.0.1:{}", n2.port), "ms2", "");
    put_node(port, "n3", &format!("127.0.0.1:{}", n3.port), "ms3", "");
    create_acme(port);
    assert_eq!(placed(port, "acme"), ("n1".into(), Some("n2".into())));
    p1.arm("DELETE /_admin/tenants/acme ", Act::Refuse, 1);
    let (status, body) = call(port, "POST", "/_shard/tenants/acme/move", r#"{"to":"n3"}"#);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""source_removed":false"#), "{body}");
    let (status, body) = repair(port);
    assert_eq!(status, 200, "{body}");
    assert!(reports(&body, "acme", "n1", "a move's source"), "{body}");
    assert!(n1.holds("acme"), "the repair took the source off");
    assert_eq!(placed(port, "acme"), ("n3".into(), Some("n2".into())));
    reads(n3.port, "acme", "get notes select title", "\"one\"");
}

/// A copy a failed attach made, which the try after did not adopt -- a
/// node of another zone than the primary's came free meanwhile, which the
/// rule puts first -- is reported: unnoted, it went on following the
/// primary beside the replica the directory names, unsaid.
#[test]
fn a_copy_left_where_the_replica_went_elsewhere_is_reported() {
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
    let (status, body) = repair(port);
    assert_eq!(status, 200, "{body}");
    assert!(
        reports(
            &body,
            "acme",
            "n2",
            "made by an attach of its replica that failed"
        ),
        "{body}"
    );
    assert!(n2.holds("acme"), "the repair took the copy off");
}

/// A replica detached that its node would not take off -- the move's
/// target in the copy's zone, where a node of another is free -- is noted
/// and reported once the tenant has its replica again: unnoted, the copy
/// went on following beside the replica the directory names, unsaid.
#[test]
fn a_detached_replica_that_stayed_is_reported() {
    let (n1, n2, n3) = (node("dt1"), node("dt2"), node("dt3"));
    let p2 = Proxy::to(n2.port);
    let port = router();
    put_node(port, "n1", &format!("127.0.0.1:{}", n1.port), "dt1", "a");
    put_node(port, "n2", &p2.addr, "dt2", "b");
    put_node(port, "n3", &format!("127.0.0.1:{}", n3.port), "dt3", "b");
    create_acme(port);
    assert_eq!(placed(port, "acme"), ("n1".into(), Some("n2".into())));
    reads(n2.port, "acme", "get notes select title", "\"one\"");
    p2.arm("DELETE /_admin/tenants/acme ", Act::Refuse, 1);
    let (status, body) = call(port, "POST", "/_shard/tenants/acme/move", r#"{"to":"n3"}"#);
    assert_eq!(status, 200, "{body}");
    until("acme never got its replica on n1", || {
        placed(port, "acme") == ("n3".into(), Some("n1".into()))
    });
    reads(n1.port, "acme", "get notes select title", "\"one\"");
    let (status, body) = repair(port);
    assert_eq!(status, 200, "{body}");
    assert!(
        reports(&body, "acme", "n2", "a replica taken off its record"),
        "{body}"
    );
    assert!(n2.holds("acme"), "the repair took the copy off");
}

/// An attach that found a file of the name on its node (409) and failed
/// after notes it too: here one put there by hand, which the replica
/// placed on another node -- of another zone by then -- leaves reported,
/// its row where it was. Noted only where the `PUT` made the file (201),
/// it went unsaid.
#[test]
fn a_file_an_attach_found_and_failed_on_is_reported() {
    let (n1, n2, n3) = (node("fd1"), node("fd2"), node("fd3"));
    let p2 = Proxy::to(n2.port);
    let port = router();
    put_node(port, "n1", &format!("127.0.0.1:{}", n1.port), "fd1", "a");
    create_acme(port);
    made_by_hand(n2.port, "fd2", "acme");
    put_node(port, "n2", &p2.addr, "fd2", "b");
    put_node(port, "n3", &format!("127.0.0.1:{}", n3.port), "fd3", "a");
    p2.arm("/acme/follow", Act::Refuse, usize::MAX);
    let (status, body) = repair(port);
    assert_eq!(status, 502, "{body}");
    assert!(p2.seen("PUT /_admin/tenants/acme ") >= 1);
    // The zones turn: n3 is the one of another zone than n1's now.
    put_node(port, "n2", &p2.addr, "fd2", "a");
    put_node(port, "n3", &format!("127.0.0.1:{}", n3.port), "fd3", "b");
    until("acme never got its replica on n3", || {
        placed(port, "acme").1.as_deref() == Some("n3")
    });
    let (status, body) = repair(port);
    assert_eq!(status, 200, "{body}");
    assert!(
        reports(&body, "acme", "n2", "found there by an attach"),
        "{body}"
    );
    reads(n2.port, "acme", "get notes select title", "\"by hand\"");
}

/// A tenant failed over, given its replica while its old primary -- the
/// node the failover took it off -- did not answer the repair's look,
/// leaves that node's file noted rather than unnoted, and the repair after,
/// the node answering, reports it with its rows in place. Answering the
/// look, the node would have kept the replica, its file made to follow.
#[test]
fn an_old_primary_silent_at_the_look_is_reported() {
    let (n1, n2, n3) = (node("op1"), node("op2"), node("op3"));
    let p1 = Proxy::to(n1.port);
    let port = router();
    put_node(port, "n1", &p1.addr, "op1", "");
    put_node(port, "n2", &format!("127.0.0.1:{}", n2.port), "op2", "");
    put_node(port, "n3", &format!("127.0.0.1:{}", n3.port), "op3", "");
    create_acme(port);
    assert_eq!(placed(port, "acme"), ("n1".into(), Some("n2".into())));
    reads(n2.port, "acme", "get notes select title", "\"one\"");
    let (status, body) = call(port, "POST", "/_shard/nodes/n1/failover", "");
    assert_eq!(status, 200, "{body}");
    assert_eq!(placed(port, "acme"), ("n2".into(), None));
    p1.arm("GET /_admin/tenants HTTP", Act::Refuse, usize::MAX);
    let (status, body) = repair(port);
    assert_eq!(status, 200, "{body}");
    assert_eq!(placed(port, "acme").1.as_deref(), Some("n3"), "{body}");
    p1.disarm("GET /_admin/tenants HTTP");
    let (status, body) = repair(port);
    assert_eq!(status, 200, "{body}");
    assert!(
        reports(&body, "acme", "n1", "the tenant's old primary"),
        "{body}"
    );
    reads(n1.port, "acme", "get notes select title", "\"one\"");
}

/// A file left on a node that does not answer the repair's look is
/// reported, said so, and fails no repair: only a tenant without its
/// replica answers 502. Its node silent, the look cannot say it is gone,
/// and it is not forgotten.
#[test]
fn a_file_left_on_a_silent_node_fails_no_repair() {
    let n1 = node("st1");
    let p1 = Proxy::to(n1.port);
    let port = router();
    put_node(port, "n1", &p1.addr, "st1", "");
    p1.arm("GET /_admin/tenants HTTP", Act::Refuse, usize::MAX);
    create_taken_back(
        port,
        &p1,
        "acme",
        "/acme/schema",
        "DELETE /_admin/tenants/acme ",
    );
    delete_by_hand(n1.port, "st1", "acme");
    let (status, body) = repair(port);
    assert_eq!(status, 200, "{body}");
    assert!(
        reports(&body, "acme", "n1", "did not answer this repair's look"),
        "{body}"
    );
    assert_eq!(files_left(port), 1);
    p1.disarm("GET /_admin/tenants HTTP");
    let (status, body) = repair(port);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""left":[]"#), "{body}");
}

/// The review's first loss: a move undone, its `DELETE` refused, noted the
/// target's copy; the tenant was moved back onto that node later and its
/// copy elsewhere deleted, so the node held the tenant's one file; and a
/// standby router promoted behind its primary -- its directory taken as
/// the copy was noted, the tenant still on its first node there -- took
/// that file back, the node finding it exactly as the router had left it,
/// lineage and change. Nothing takes a file off now: the standby's repair
/// reports it, and the tenant stands with its rows.
#[test]
fn a_tenant_moved_back_where_its_copy_was_noted_stays() {
    let (n1, n2, n3) = (node("mb1"), node("mb2"), node("mb3"));
    let p2 = Proxy::to(n2.port);
    let scratch = std::env::temp_dir().join(format!("fenec-retries-mb-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();
    let (primary, standby) = (scratch.join("r1.fenec"), scratch.join("r2.fenec"));
    let (r1, _) = router_over(Directory::open(&primary).unwrap(), true);
    put_node(r1, "n1", &format!("127.0.0.1:{}", n1.port), "mb1", "");
    put_node(r1, "n3", &format!("127.0.0.1:{}", n3.port), "mb3", "");
    create_acme(r1);
    assert_eq!(placed(r1, "acme"), ("n1".into(), Some("n3".into())));
    put_node(r1, "n2", &p2.addr, "mb2", "");
    let (status, body) = call(r1, "PUT", "/_shard/tenants/acme/quota", r#"{"rows":1000}"#);
    assert_eq!(status, 200, "{body}");
    // The move to n2 undone, its copy there left.
    p2.arm("PUT /_admin/tenants/acme/quota ", Act::Refuse, 1);
    p2.arm("DELETE /_admin/tenants/acme ", Act::Refuse, 1);
    let (status, body) = call(r1, "POST", "/_shard/tenants/acme/move", r#"{"to":"n2"}"#);
    assert_eq!(status, 502, "{body}");
    assert!(n2.holds("acme"));
    // The standby's directory, as it stood then.
    std::fs::copy(&primary, &standby).unwrap();
    // Moved back onto n2: its copy there taken over, n1's deleted.
    let (status, body) = call(r1, "POST", "/_shard/tenants/acme/move", r#"{"to":"n2"}"#);
    assert_eq!(status, 200, "{body}");
    assert!(!n1.holds("acme"));
    reads(n2.port, "acme", "get notes select title", "\"one\"");
    // The standby promoted: acme on n1 there, its copy on n2 noted as left.
    let (r2, _) = router_over(Directory::open(&standby).unwrap(), true);
    let (status, body) = repair(r2);
    assert_eq!(status, 200, "{body}");
    assert!(reports(&body, "acme", "n2", "a move undone"), "{body}");
    assert!(
        n2.holds("acme"),
        "the standby took the tenant's one file off"
    );
    reads(n2.port, "acme", "get notes select title", "\"one\"");
    // The primary's own look finds the tenant there, and forgets its note.
    let (status, body) = repair(r1);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""left":[]"#), "{body}");
    let _ = std::fs::remove_dir_all(&scratch);
}

/// A node without `--replication-token`, whose files have no lineage.
fn plain_node(tag: &str) -> Node {
    let dir = std::env::temp_dir().join(format!("fenec-retries-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let tenants = Arc::new(Tenants::new(&dir).unwrap());
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

/// The review's second loss, on nodes without `--replication-token`, where
/// every lineage is none: a move's source noted as left, the tenant then
/// deleted, its name made again on that node with one row -- by hand here,
/// so the directory places it nowhere -- was taken back as a file of no
/// write, the row of a tenant made since lost. Reported now, and kept.
#[test]
fn a_tenant_made_again_where_a_file_of_no_lineage_was_noted_stays() {
    let (n1, n2) = (plain_node("pl1"), plain_node("pl2"));
    let p1 = Proxy::to(n1.port);
    let (port, _) = router_over(Directory::in_memory(), false);
    put_node(port, "n1", &p1.addr, "pl1", "");
    put_node(port, "n2", &format!("127.0.0.1:{}", n2.port), "pl2", "");
    create_acme(port);
    p1.arm("DELETE /_admin/tenants/acme ", Act::Refuse, 1);
    let (status, body) = call(port, "POST", "/_shard/tenants/acme/move", r#"{"to":"n2"}"#);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""source_removed":false"#), "{body}");
    let (status, body) = call(port, "DELETE", "/_shard/tenants/acme", "");
    assert_eq!(status, 204, "{body}");
    made_by_hand(n1.port, "pl1", "acme");
    let (status, body) = repair(port);
    assert_eq!(status, 200, "{body}");
    assert!(reports(&body, "acme", "n1", "a move's source"), "{body}");
    reads(n1.port, "acme", "get notes select title", "\"by hand\"");
    assert_eq!(files_left(port), 1);
}

/// A router without `--replicas` places no replica unasked: a file a
/// create taken back left schedules no repair, and `aaa`, a tenant in no
/// pair with no copy, stays as it is. Where a file left had a repair run
/// to take it off, that repair gave every such tenant a replica, 2 s after.
#[test]
fn a_router_without_replicas_places_none_unasked() {
    let (n1, n2) = (node("nr1"), node("nr2"));
    let p1 = Proxy::to(n1.port);
    let (port, router) = router_over(Directory::in_memory(), false);
    put_node(port, "n1", &p1.addr, "nr1", "");
    put_node(port, "n2", &format!("127.0.0.1:{}", n2.port), "nr2", "");
    create_on(port, "aaa", "n1");
    create_taken_back(
        port,
        &p1,
        "bbb",
        "/bbb/schema",
        "DELETE /_admin/tenants/bbb ",
    );
    assert_eq!(files_left(port), 1);
    assert!(!router.repair_due(), "a file left scheduled a repair");
    assert_eq!(placed(port, "aaa").1, None);
    assert!(!n2.holds("aaa"), "aaa was given a replica unasked");
    assert!(n1.holds("bbb"));
}

/// Pairs alone, no `--replicas`: a file a create taken back left on a node
/// in no pair is noted, counted and kept, schedules no repair, and its
/// tenant, made again on another node in no pair, is given no replica, nor
/// is the pair's tenant touched.
#[test]
fn pairs_alone_note_files_and_place_no_replica() {
    let n1 = node("pa1");
    let n2 = node_of("pa2", Some(n1.port));
    let (n3, n4) = (node("pa3"), node("pa4"));
    let p4 = Proxy::to(n4.port);
    let (port, router) = router_over(Directory::in_memory(), false);
    put_node(port, "n1", &format!("127.0.0.1:{}", n1.port), "pa1", "");
    put_node(port, "n2", &format!("127.0.0.1:{}", n2.port), "pa2", "");
    let body = format!(
        r#"{{"addr":"127.0.0.1:{}","token":"adm-pa1","region":"eu","standby":"n2"}}"#,
        n1.port
    );
    let (status, body) = call(port, "PUT", "/_shard/nodes/n1", &body);
    assert!(status == 200 || status == 201, "{body}");
    put_node(port, "n3", &format!("127.0.0.1:{}", n3.port), "pa3", "");
    put_node(port, "n4", &p4.addr, "pa4", "");
    create_on(port, "ddd", "n1");
    reads(n2.port, "ddd", "get notes select title", "\"one\"");
    // `ccc` taken back off n4, then made on n3: no copy, by design.
    p4.arm("/ccc/schema", Act::Refuse, 1);
    p4.arm("DELETE /_admin/tenants/ccc ", Act::Refuse, 1);
    let (status, body) = call(
        port,
        "PUT",
        "/_shard/tenants/ccc",
        r#"{"node":"n4","schema":"create collection notes (title text)"}"#,
    );
    assert_eq!(status, 503, "{body}");
    create_on(port, "ccc", "n3");
    assert_eq!(files_left(port), 1);
    assert!(!router.repair_due(), "a file left scheduled a repair");
    assert_eq!(placed(port, "ccc").1, None);
    assert!(n4.holds("ccc"));
    for nd in [&n1, &n2] {
        assert!(!nd.holds("ccc"));
    }
    assert!(n2.holds("ddd"));
}

/// A paired node's tenant deleted, its standby refusing the `DELETE` of its
/// copy, leaves that copy noted and reported: unnoted, it stayed on the
/// standby for good, unsaid.
#[test]
fn a_deleted_tenants_standby_copy_that_stayed_is_reported() {
    let n1 = node("ds1");
    let n2 = node_of("ds2", Some(n1.port));
    let p2 = Proxy::to(n2.port);
    let (port, _) = router_over(Directory::in_memory(), false);
    put_node(port, "n1", &format!("127.0.0.1:{}", n1.port), "ds1", "");
    put_node(port, "n2", &p2.addr, "ds2", "");
    let body = format!(
        r#"{{"addr":"127.0.0.1:{}","token":"adm-ds1","region":"eu","standby":"n2"}}"#,
        n1.port
    );
    let (status, body) = call(port, "PUT", "/_shard/nodes/n1", &body);
    assert!(status == 200 || status == 201, "{body}");
    create_on(port, "acme", "n1");
    reads(n2.port, "acme", "get notes select title", "\"one\"");
    p2.arm("DELETE /_admin/tenants/acme ", Act::Refuse, 1);
    let (status, body) = call(port, "DELETE", "/_shard/tenants/acme", "");
    assert_eq!(status, 204, "{body}");
    let (status, body) = repair(port);
    assert_eq!(status, 200, "{body}");
    assert!(
        reports(
            &body,
            "acme",
            "n2",
            "a deleted tenant's standby copy that stayed"
        ),
        "{body}"
    );
    assert!(n2.holds("acme"), "the repair took the copy off");
}
