//! A query across tenants (`POST /_shard/query`): two in-process `--dir`
//! nodes and a router, one node reached through a gate the test holds,
//! cuts or stops, so a failure, a timeout and a move under the query are
//! events the test makes rather than times it waits out.

use fenec_http::tenants::Tenants;
use fenec_shard::directory::Directory;
use fenec_shard::fanout::Limits;
use fenec_shard::{Config, Router};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

struct Node {
    addr: String,
    dir: PathBuf,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn node(tag: &str, access: Option<Arc<fenec_http::access::Access>>) -> Node {
    fenec_http::audit::set_delay(0);
    let dir = std::env::temp_dir().join(format!(
        "fenec-fanout-{tag}-{}-{:?}",
        std::process::id(),
        Instant::now()
    ));
    let tenants = Arc::new(Tenants::new(&dir).unwrap());
    let cfg = fenec_http::Config {
        addr: "127.0.0.1:0".into(),
        admin_token: Some(format!("adm-{tag}")),
        token: access.is_some().then(|| "data".to_string()),
        access,
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Bytes go through.
    Pass,
    /// Every connection is closed, open ones and new ones: the node gone.
    Cut,
    /// New connections are taken and never answered: the node hung.
    Hold,
}

#[derive(Default)]
struct GateState {
    mode: Option<Mode>,
    /// A request whose bytes hold this waits at the gate until released.
    marker: Option<String>,
    seen: bool,
    open: Vec<TcpStream>,
    held: Vec<TcpStream>,
}

/// A TCP relay in front of a node.
struct Gate {
    addr: String,
    state: Arc<(Mutex<GateState>, Condvar)>,
}

impl Gate {
    fn to(target: &str) -> Gate {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let state: Arc<(Mutex<GateState>, Condvar)> = Default::default();
        state.0.lock().unwrap().mode = Some(Mode::Pass);
        let (st, target) = (Arc::clone(&state), target.to_string());
        std::thread::spawn(move || {
            for client in listener.incoming() {
                let Ok(client) = client else { continue };
                let mut s = st.0.lock().unwrap();
                match s.mode {
                    Some(Mode::Cut) => {
                        let _ = client.shutdown(Shutdown::Both);
                        continue;
                    }
                    Some(Mode::Hold) => {
                        s.held.push(client);
                        continue;
                    }
                    _ => {}
                }
                let Ok(server) = TcpStream::connect(&target) else {
                    continue;
                };
                s.open.push(client.try_clone().unwrap());
                s.open.push(server.try_clone().unwrap());
                drop(s);
                let (mut c, mut sv) = (client.try_clone().unwrap(), server.try_clone().unwrap());
                std::thread::spawn(move || {
                    let _ = std::io::copy(&mut sv, &mut c);
                    let _ = c.shutdown(Shutdown::Write);
                });
                let st = Arc::clone(&st);
                std::thread::spawn(move || relay(client, server, st));
            }
        });
        Gate { addr, state }
    }

    fn set(&self, mode: Mode) {
        let mut s = self.state.0.lock().unwrap();
        s.mode = Some(mode);
        if mode != Mode::Pass {
            for c in s.open.drain(..) {
                let _ = c.shutdown(Shutdown::Both);
            }
        }
    }

    /// Requests holding `marker` wait at the gate from now on.
    fn arm(&self, marker: &str) {
        let mut s = self.state.0.lock().unwrap();
        s.marker = Some(marker.to_string());
        s.seen = false;
    }

    /// Waits until a request holding the marker has reached the gate.
    fn wait_seen(&self) {
        let (lock, cv) = &*self.state;
        let s = lock.lock().unwrap();
        let (s, timeout) = cv
            .wait_timeout_while(s, Duration::from_secs(30), |s| !s.seen)
            .unwrap();
        assert!(
            !timeout.timed_out() && s.seen,
            "no request reached the gate"
        );
    }

    fn release(&self) {
        let (lock, cv) = &*self.state;
        lock.lock().unwrap().marker = None;
        cv.notify_all();
    }
}

/// The client's bytes on to the node, a request holding the gate's marker
/// held until it is released.
fn relay(mut client: TcpStream, mut server: TcpStream, st: Arc<(Mutex<GateState>, Condvar)>) {
    let mut buf = [0u8; 16 << 10];
    let mut tail = String::new();
    loop {
        let n = match client.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        tail.push_str(&String::from_utf8_lossy(&buf[..n]));
        {
            let (lock, cv) = &*st;
            let mut s = lock.lock().unwrap();
            if s.marker.as_deref().is_some_and(|m| tail.contains(m)) {
                s.seen = true;
                cv.notify_all();
                drop(cv.wait_while(s, |s| s.marker.is_some()).unwrap());
                tail.clear();
            }
        }
        if tail.len() > 1024 {
            tail.drain(..tail.len() - 256);
        }
        if server.write_all(&buf[..n]).is_err() {
            break;
        }
    }
    let _ = server.shutdown(Shutdown::Write);
}

struct Cluster {
    port: u16,
    _nodes: Vec<Node>,
    gate: Gate,
}

#[derive(Default)]
struct Setup {
    token: Option<&'static str>,
    access: Option<Arc<fenec_http::access::Access>>,
    limits: Limits,
}

/// A router over `n1`, reached straight, and `n2`, reached through the
/// gate; acme and gamma on n1, beta on n2, each with a few users.
fn cluster(tag: &str, setup: Setup) -> Cluster {
    let nodes = vec![
        node(&format!("{tag}1"), setup.access.clone()),
        node(&format!("{tag}2"), setup.access.clone()),
    ];
    let gate = Gate::to(&nodes[1].addr);
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        token: setup.token.map(String::from),
        upstream_timeout: Duration::from_secs(10),
        fanout: setup.limits,
        ..Config::default()
    };
    let router = Router::new(Directory::in_memory(), cfg);
    let listener = router.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = router.serve_on(listener);
    });
    let c = Cluster {
        port,
        _nodes: nodes,
        gate,
    };
    let at = [c._nodes[0].addr.clone(), c.gate.addr.clone()];
    for (i, addr) in at.iter().enumerate() {
        let body = format!(r#"{{"addr":"{addr}","token":"adm-{tag}{}"}}"#, i + 1);
        let r = c.call(
            "PUT",
            &format!("/_shard/nodes/n{}", i + 1),
            &body,
            setup.token,
        );
        assert_eq!(r.0, 201, "{}", r.1);
    }
    let schema = "create collection users (email text, plan text, created timestamp @sorted)";
    for (t, n, users) in [
        (
            "acme",
            "n1",
            &[
                ("a1", "pro", "2026-10-01T00:00:00Z"),
                ("a2", "free", "2026-10-03T00:00:00.500Z"),
                ("a3", "pro", "2026-10-05T00:00:00Z"),
            ][..],
        ),
        (
            "beta",
            "n2",
            &[
                ("b1", "pro", "2026-10-02T00:00:00Z"),
                ("b2", "free", "2026-10-03T00:00:00Z"),
            ][..],
        ),
        ("gamma", "n1", &[("g1", "free", "2026-10-04T00:00:00Z")][..]),
    ] {
        let mut body = format!(r#"{{"node":"{n}","schema":"#);
        fenec_core::json::escape_into(&mut body, schema);
        body.push('}');
        let r = c.call("PUT", &format!("/_shard/tenants/{t}"), &body, setup.token);
        assert_eq!(r.0, 201, "{}", r.1);
        for (email, plan, at) in users {
            let q = format!(r#"put users {{email: "{email}", plan: "{plan}", created: "{at}"}}"#);
            let data = setup.access.is_some().then_some("data");
            let r = c.call("POST", &format!("/t/{t}/query"), &query(&q), data);
            assert_eq!(r.0, 200, "{}", r.1);
        }
    }
    c
}

impl Cluster {
    fn call(&self, method: &str, target: &str, body: &str, auth: Option<&str>) -> (u16, String) {
        call(self.port, method, target, body, auth)
    }

    /// `POST /_shard/query` of `body`: the status and the answer's body.
    fn fan(&self, body: &str, auth: Option<&str>) -> (u16, String) {
        let r = self.call("POST", "/_shard/query", body, auth);
        (r.0, body_of(&r.1).to_string())
    }
}

fn query(q: &str) -> String {
    let mut body = String::from("{\"query\":");
    fenec_core::json::escape_into(&mut body, q);
    body.push('}');
    body
}

fn call(port: u16, method: &str, target: &str, body: &str, auth: Option<&str>) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
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
    let status = out
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, out)
}

fn body_of(r: &str) -> &str {
    r.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("")
}

/// The rows of a merged answer, as their JSON text.
fn rows(answer: &str) -> &str {
    let start = answer.find("\"rows\":").expect("rows") + 7;
    let end = answer.find(",\"tenants\":").expect("tenants");
    &answer[start..end]
}

#[test]
fn a_query_across_tenants_merges_their_rows_across_nodes() {
    let c = cluster("merge", Setup::default());

    // Concatenated, tenants in name order, each one's rows in its own.
    let (status, a) = c.fan(r#"{"query":"get users select email order created"}"#, None);
    assert_eq!(status, 200, "{a}");
    assert_eq!(
        rows(&a),
        r#"[{"tenant":"acme","email":"a1"},{"tenant":"acme","email":"a2"},{"tenant":"acme","email":"a3"},{"tenant":"beta","email":"b1"},{"tenant":"beta","email":"b2"},{"tenant":"gamma","email":"g1"}]"#
    );
    assert!(a.contains(r#""tenants":3,"answered":3,"failed":[]"#), "{a}");

    // The newest four of all of them, from each one's newest four: a
    // timestamp's text leaves its milliseconds out where they are 0, and
    // is ordered as the time it is.
    let (status, a) = c.fan(
        r#"{"query":"get users select email, created order created desc limit 4",
            "merge":{"order":["created desc"],"limit":4},"column":"org"}"#,
        None,
    );
    assert_eq!(status, 200, "{a}");
    assert_eq!(
        rows(&a),
        r#"[{"org":"acme","email":"a3","created":"2026-10-05T00:00:00Z"},{"org":"gamma","email":"g1","created":"2026-10-04T00:00:00Z"},{"org":"acme","email":"a2","created":"2026-10-03T00:00:00.500Z"},{"org":"beta","email":"b2","created":"2026-10-03T00:00:00Z"}]"#
    );

    // Counts by plan, added across tenants; the first and the last.
    let (status, a) = c.fan(
        r#"{"query":"get users select plan, count() as n group plan",
            "merge":{"sum":["n"],"group":["plan"]}}"#,
        None,
    );
    assert_eq!(status, 200, "{a}");
    assert_eq!(rows(&a), r#"[{"plan":"free","n":3},{"plan":"pro","n":3}]"#);
    let (status, a) = c.fan(
        r#"{"query":"get users select count() as n, min(created) as first, max(created) as last",
            "merge":{"sum":["n"],"min":["first"],"max":["last"]}}"#,
        None,
    );
    assert_eq!(status, 200, "{a}");
    assert_eq!(
        rows(&a),
        r#"[{"n":6,"first":"2026-10-01T00:00:00Z","last":"2026-10-05T00:00:00Z"}]"#
    );

    // Narrowed by a prefix, or by names; parameters go as they came.
    let (status, a) = c.fan(
        r#"{"query":"get users select email where plan = $1","params":["pro"],"prefix":"ac"}"#,
        None,
    );
    assert_eq!(status, 200, "{a}");
    assert_eq!(
        rows(&a),
        r#"[{"tenant":"acme","email":"a1"},{"tenant":"acme","email":"a3"}]"#
    );
    let (status, a) = c.fan(
        r#"{"query":"get users select email where plan = $1","params":["free"],"tenants":["gamma","beta"]}"#,
        None,
    );
    assert_eq!(status, 200, "{a}");
    assert_eq!(
        rows(&a),
        r#"[{"tenant":"beta","email":"b2"},{"tenant":"gamma","email":"g1"}]"#
    );
    // A tenant the directory does not hold is named, nothing asked.
    let (status, a) = c.fan(r#"{"query":"get users","tenants":["acme","nobody"]}"#, None);
    assert_eq!(status, 404, "{a}");
    assert!(a.contains("`nobody`"), "{a}");

    // A merge that would drop a column, or rows not in its order, fails
    // the tenants it found so; nothing is merged wrong.
    let (status, a) = c.fan(
        r#"{"query":"get users select plan, email","merge":{"group":["plan"]}}"#,
        None,
    );
    assert_eq!(status, 400, "{a}");
    assert!(a.contains("neither groups by nor folds"), "{a}");
    let (status, a) = c.fan(
        r#"{"query":"get users select email, created order created","merge":{"order":["created desc"]}}"#,
        None,
    );
    assert_eq!(status, 400, "{a}");
    assert!(a.contains("not in the merge's order"), "{a}");
}

#[test]
fn a_tenant_that_fails_is_named_and_partial_answers_the_others() {
    let c = cluster("fail", Setup::default());
    let q = r#"{"query":"get users select email order created"}"#;
    c.gate.set(Mode::Cut);
    // beta's node is gone: the query fails naming it, and its node.
    let (status, a) = c.fan(q, None);
    assert_eq!(status, 502, "{a}");
    assert!(
        a.contains(r#""failed":[{"tenant":"beta","node":"n2","status":502"#),
        "{a}"
    );
    assert!(!a.contains("\"rows\""), "{a}");
    // Asked for, the others' rows, and beta named beside them.
    let partial = r#"{"query":"get users select email order created","partial":true}"#;
    let (status, a) = c.fan(partial, None);
    assert_eq!(status, 200, "{a}");
    assert_eq!(
        rows(&a),
        r#"[{"tenant":"acme","email":"a1"},{"tenant":"acme","email":"a2"},{"tenant":"acme","email":"a3"},{"tenant":"gamma","email":"g1"}]"#
    );
    assert!(
        a.contains(r#""tenants":3,"answered":2,"failed":[{"tenant":"beta""#),
        "{a}"
    );

    // A node that takes the call and never answers: the query's one
    // timeout bounds it, and beta is named as timed out.
    c.gate.set(Mode::Hold);
    let held = r#"{"query":"get users select email order created","timeout_ms":500}"#;
    let started = Instant::now();
    let (status, a) = c.fan(held, None);
    assert!(started.elapsed() >= Duration::from_millis(500));
    assert_eq!(status, 504, "{a}");
    assert!(
        a.contains(r#""tenant":"beta","node":"n2","status":504"#),
        "{a}"
    );
    let held =
        r#"{"query":"get users select email order created","timeout_ms":500,"partial":true}"#;
    let (status, a) = c.fan(held, None);
    assert_eq!(status, 200, "{a}");
    assert!(
        a.contains(r#""answered":2,"failed":[{"tenant":"beta""#),
        "{a}"
    );
    assert!(rows(&a).contains(r#""email":"g1""#), "{a}");

    // Back, and whole again.
    c.gate.set(Mode::Pass);
    let (status, a) = c.fan(q, None);
    assert_eq!(status, 200, "{a}");
    assert!(a.contains(r#""answered":3,"failed":[]"#), "{a}");

    // A tenant's `require` that says why: each failed tenant names the
    // reason its 412 carried, beside its words.
    let guarded = r#"{"query":"get users where email = \"a1\" limit 1 require 1 else 'no a1 here'","partial":true}"#;
    let (status, a) = c.fan(guarded, None);
    assert_eq!(status, 200, "{a}");
    assert!(rows(&a).contains(r#""email":"a1""#), "{a}");
    for t in ["beta", "gamma"] {
        let named = format!(r#""tenant":"{t}","node":"#);
        let at = a.find(&named).unwrap_or_else(|| panic!("{t}: {a}"));
        let entry = &a[at..at + a[at..].find('}').unwrap() + 1];
        assert!(entry.contains(r#""status":412"#), "{entry}");
        assert!(entry.ends_with(r#","reason":"no a1 here"}"#), "{entry}");
    }
}

/// Tenants are the boundary, and only the router's token crosses it: a
/// person's token bound to one tenant, and the nodes' data token, reach no
/// query across tenants -- at the router, or at a node's read it sends.
#[test]
fn a_token_bound_to_one_tenant_reaches_no_query_across_them() {
    let access = Arc::new(
        fenec_http::access::Access::new(
            b"thirty-two bytes and a few more, for HS256",
            "users  read  where email = $jwt.sub\n",
        )
        .unwrap(),
    );
    let c = cluster(
        "jwt",
        Setup {
            token: Some("rt"),
            access: Some(Arc::clone(&access)),
            ..Setup::default()
        },
    );
    let alice = access.mint(r#"{"sub":"a1","tenant":"acme"}"#).unwrap();
    // Her own tenant, through the router, as ever.
    let mine = c.call("GET", "/t/acme/users?select=email", "", Some(&alice));
    assert_eq!(body_of(&mine.1), r#"[{"email":"a1"}]"#);
    let q = r#"{"query":"get users select email"}"#;
    for token in [Some(alice.as_str()), Some("data"), None] {
        let (status, a) = c.fan(q, token);
        assert_eq!(status, 401, "{token:?}: {a}");
        assert!(!a.contains("a1") && !a.contains("b1"), "{a}");
    }
    let (status, a) = c.fan(q, Some("rt"));
    assert_eq!(status, 200, "{a}");
    assert!(a.contains(r#""answered":3"#), "{a}");
    // The node's read for it takes the admin token alone.
    let node = c._nodes[0]
        .addr
        .rsplit_once(':')
        .unwrap()
        .1
        .parse()
        .unwrap();
    for token in [alice.as_str(), "data"] {
        let r = call(node, "POST", "/_admin/tenants/acme/query", q, Some(token));
        assert_eq!(r.0, 401, "{}", r.1);
    }
    let r = call(
        node,
        "POST",
        "/_admin/tenants/acme/query",
        q,
        Some("adm-jwt1"),
    );
    assert_eq!(r.0, 200, "{}", r.1);
}

/// A query across tenants reads: a write is refused at each node before it
/// runs, the first refusal stops the asking, and nothing is written.
#[test]
fn a_write_across_tenants_is_refused_and_writes_nothing() {
    let c = cluster(
        "write",
        Setup {
            limits: Limits {
                concurrency: 1,
                ..Limits::default()
            },
            ..Setup::default()
        },
    );
    for (q, word) in [
        (r#"put users {email: "x", plan: "pro"}"#, "put"),
        (r#"set users {plan: "x"} where plan = "pro""#, "set"),
        (r#"del users where plan = "pro""#, "del"),
        ("create collection planted (a int)", "create"),
        ("drop collection users", "drop"),
    ] {
        let (status, a) = c.fan(&query(q), None);
        assert_eq!(status, 403, "{q}: {a}");
        assert!(a.contains(&format!("`{word}` writes")), "{a}");
        // One at a time, the first refusal stopped the rest.
        assert!(a.contains(r#""not_asked":2"#), "{a}");
    }
    let (status, a) = c.fan(
        r#"{"query":"get users select count() as n","merge":{"sum":["n"]}}"#,
        None,
    );
    assert_eq!(status, 200, "{a}");
    assert_eq!(rows(&a), r#"[{"n":6}]"#);
    let (_, a) = c.fan(r#"{"query":"collections"}"#, None);
    assert!(!a.contains("planted"), "{a}");
    // Straight to a node, the same.
    let node = c._nodes[0]
        .addr
        .rsplit_once(':')
        .unwrap()
        .1
        .parse()
        .unwrap();
    let r = call(
        node,
        "POST",
        "/_admin/tenants/acme/query",
        &query("del users"),
        Some("adm-write1"),
    );
    assert_eq!(r.0, 403, "{}", r.1);
}

/// The rows and bytes gathered are bounded, and a query past either is
/// refused, never answered with what fitted.
#[test]
fn a_query_past_its_bounds_is_refused_not_cut() {
    let c = cluster(
        "bounds",
        Setup {
            limits: Limits {
                max_rows: 5,
                max_bytes: 200,
                ..Limits::default()
            },
            ..Setup::default()
        },
    );
    let (status, a) = c.fan(r#"{"query":"get users select email"}"#, None);
    assert_eq!(status, 413, "{a}");
    assert!(
        a.contains("more than 5 rows") && a.contains("--fanout-max-rows"),
        "{a}"
    );
    assert!(!a.contains("\"rows\""), "{a}");
    let (status, a) = c.fan(r#"{"query":"get users","partial":true}"#, None);
    assert_eq!(status, 413, "{a}");
    assert!(
        a.contains("more than 200 bytes") && a.contains("--fanout-max-bytes"),
        "{a}"
    );
    // Within both, answered.
    let (status, a) = c.fan(
        r#"{"query":"get users select email order created desc limit 1","merge":{"order":["email desc"]}}"#,
        None,
    );
    assert_eq!(status, 200, "{a}");
    assert_eq!(
        rows(&a),
        r#"[{"tenant":"gamma","email":"g1"},{"tenant":"beta","email":"b2"},{"tenant":"acme","email":"a3"}]"#
    );
}

/// A tenant moved while the query runs -- after the router read where it
/// was, before its node was asked -- is asked where it went; one deleted
/// meanwhile is named. Neither is ever left out.
#[test]
fn a_tenant_moved_or_deleted_under_the_query_is_answered_or_named() {
    let c = cluster("moved", Setup::default());
    let q = r#"{"query":"get users select email order created"}"#;
    // beta's read is held at its node's gate, the directory read already.
    c.gate.arm("/_admin/tenants/beta/query");
    let port = c.port;
    let fan = std::thread::spawn(move || call(port, "POST", "/_shard/query", q, None));
    c.gate.wait_seen();
    // beta moves to n1 meanwhile: its copy on n2 is deleted on the way.
    let r = c.call("POST", "/_shard/tenants/beta/move", r#"{"to":"n1"}"#, None);
    assert_eq!(r.0, 200, "{}", r.1);
    assert!(r.1.contains(r#""source_removed":true"#), "{}", r.1);
    c.gate.release();
    let (status, a) = fan.join().unwrap();
    let a = body_of(&a);
    assert_eq!(status, 200, "{a}");
    assert_eq!(
        rows(a),
        r#"[{"tenant":"acme","email":"a1"},{"tenant":"acme","email":"a2"},{"tenant":"acme","email":"a3"},{"tenant":"beta","email":"b1"},{"tenant":"beta","email":"b2"},{"tenant":"gamma","email":"g1"}]"#
    );
    assert!(a.contains(r#""answered":3,"failed":[]"#), "{a}");

    // gamma moves to n2, then is deleted under a query that read it there.
    let r = c.call("POST", "/_shard/tenants/gamma/move", r#"{"to":"n2"}"#, None);
    assert_eq!(r.0, 200, "{}", r.1);
    for partial in [false, true] {
        let r = c.call("PUT", "/_shard/tenants/gamma", r#"{"node":"n2"}"#, None);
        assert!(matches!(r.0, 201 | 409), "{}", r.1);
        c.gate.arm("/_admin/tenants/gamma/query");
        let body =
            format!(r#"{{"query":"get users select email order created","partial":{partial}}}"#);
        let fan = std::thread::spawn(move || call(port, "POST", "/_shard/query", &body, None));
        c.gate.wait_seen();
        let r = c.call("DELETE", "/_shard/tenants/gamma", "", None);
        assert_eq!(r.0, 204, "{}", r.1);
        c.gate.release();
        let (status, a) = fan.join().unwrap();
        let a = body_of(&a);
        let named = r#""failed":[{"tenant":"gamma","node":"n2","status":404"#;
        assert!(a.contains(named), "{a}");
        assert!(
            a.contains("deleted from the directory while the query ran"),
            "{a}"
        );
        match partial {
            false => assert_eq!(status, 404, "{a}"),
            true => {
                assert_eq!(status, 200, "{a}");
                assert!(a.contains(r#""tenants":3,"answered":2"#), "{a}");
                assert!(rows(a).contains(r#""email":"b2""#), "{a}");
            }
        }
    }
}

/// A text column the tenants order by a collation or by a `nocase` field's
/// fold comes in an order that is not its bytes': told that order, the
/// merge puts the rows of all of them as one tenant holding them would --
/// the collation over the folds, texts folding to one tied and so in
/// tenant order -- and not told, it fails the tenants rather than merge
/// them wrong.
#[test]
fn a_collated_or_nocase_order_merges_as_one_tenant_would_order() {
    let c = cluster("collate", Setup::default());
    let schema = "create collection people (name text collate tr, email text nocase, \
                  tag text collate tr nocase, plain text)";
    for (t, people) in [
        (
            "acme",
            &[
                ("Ali", "ada@x", "ılık", "Ömer"),
                ("Çiğdem", "Ada@x", "İnce", "Ali"),
                ("Ömer", "bob@x", "ISTANBUL", "Zeynep"),
                ("Zeynep", "Carol@x", "zeytin", "Çiğdem"),
            ][..],
        ),
        (
            "beta",
            &[
                ("Can", "ALAN@x", "IRMAK", "Can"),
                ("Ilgaz", "Bea@x", "inci", "Şule"),
                ("İpek", "cem@x", "Ilık", "Ilgaz"),
                ("Şule", "dora@x", "ufuk", "İpek"),
            ][..],
        ),
        (
            "gamma",
            &[
                ("Deniz", "ADA@x", "çay", "Deniz"),
                ("Ufuk", "aysel@x", "İNCE", "Ufuk"),
                ("Ümit", "BOB@x", "Ünal", "Ümit"),
            ][..],
        ),
    ] {
        let r = c.call("POST", &format!("/t/{t}/query"), &query(schema), None);
        assert_eq!(r.0, 200, "{}", r.1);
        for (name, email, tag, plain) in people {
            let q = format!(
                r#"put people {{name: "{name}", email: "{email}", tag: "{tag}", plain: "{plain}"}}"#
            );
            let r = c.call("POST", &format!("/t/{t}/query"), &query(&q), None);
            assert_eq!(r.0, 200, "{}", r.1);
        }
    }
    let fan = |q: &str, merge: &str| {
        let mut body = String::from("{\"query\":");
        fenec_core::json::escape_into(&mut body, q);
        body.push_str(&format!(",\"merge\":{merge}}}"));
        c.fan(&body, None)
    };
    // Each merged row's tenant and its `name` column, as `tenant:value`.
    let column = |a: &str, name: &str| -> Vec<String> {
        let rows = rows(a);
        let key = format!("\"{name}\":\"");
        rows.match_indices(&key)
            .map(|(at, _)| {
                let rest = &rows[at + key.len()..];
                let tenant = rows[..at].rsplit("\"tenant\":\"").next().unwrap();
                let tenant = &tenant[..tenant.find('"').unwrap()];
                format!("{tenant}:{}", &rest[..rest.find('"').unwrap()])
            })
            .collect()
    };

    // A field declared `collate tr`: Turkish's order, `Ç` after `C`, `I`
    // (dotless) before `İ`, `Ö` after `O`, where the bytes put every one
    // of them after `Z`.
    let q = "get people select name order name limit 6";
    let (status, a) = fan(
        q,
        r#"{"order":[{"column":"name","collate":"tr"}],"limit":6}"#,
    );
    assert_eq!(status, 200, "{a}");
    assert_eq!(
        column(&a, "name"),
        [
            "acme:Ali",
            "beta:Can",
            "acme:Çiğdem",
            "gamma:Deniz",
            "beta:Ilgaz",
            "beta:İpek"
        ]
    );
    let q = "get people select name order name desc limit 4";
    let (status, a) = fan(
        q,
        r#"{"order":[{"column":"name","collate":"TR","desc":true}],"limit":4}"#,
    );
    assert_eq!(status, 200, "{a}");
    assert_eq!(
        column(&a, "name"),
        ["acme:Zeynep", "gamma:Ümit", "gamma:Ufuk", "beta:Şule"]
    );
    // A collation the query names over a plain field.
    let q = "get people select plain order plain collate tr limit 5";
    let (status, a) = fan(
        q,
        r#"{"order":[{"column":"plain","collate":"tr"}],"limit":5}"#,
    );
    assert_eq!(status, 200, "{a}");
    assert_eq!(
        column(&a, "plain"),
        [
            "acme:Ali",
            "beta:Can",
            "acme:Çiğdem",
            "gamma:Deniz",
            "beta:Ilgaz"
        ]
    );

    // A `nocase` field: by its fold, texts folding to one tied -- in id
    // order within a tenant, in tenant order across them. Tied by their
    // bytes, acme's `ada@x` before its `Ada@x` would have failed it.
    let q = "get people select email order email limit 5";
    let (status, a) = fan(
        q,
        r#"{"order":[{"column":"email","nocase":true}],"limit":5}"#,
    );
    assert_eq!(status, 200, "{a}");
    assert_eq!(
        column(&a, "email"),
        [
            "acme:ada@x",
            "acme:Ada@x",
            "gamma:ADA@x",
            "beta:ALAN@x",
            "gamma:aysel@x"
        ]
    );

    // `collate tr nocase`: the collation over Turkish's folds, `I` the
    // capital of `ı` and `İ` of `i`; a key after it as plain as ever.
    let q = "get people select id, tag order tag, id limit 7";
    let (status, a) = fan(
        q,
        r#"{"order":[{"column":"tag","collate":"tr","nocase":true},"id"],"limit":7}"#,
    );
    assert_eq!(status, 200, "{a}");
    assert_eq!(
        column(&a, "tag"),
        [
            "gamma:çay",
            "acme:ılık",
            "beta:Ilık",
            "beta:IRMAK",
            "acme:ISTANBUL",
            "acme:İnce",
            "gamma:İNCE"
        ]
    );

    // A fold the same way: each tenant's least and greatest in the
    // collation over the folds, and the least and greatest of those -- by
    // the bytes, `Ilık` and `Ünal`; and a `nocase` group one for every
    // text folding to it, under the first tenant's text.
    let q = "get people select min(tag) as first, max(tag) as last";
    let (status, a) = fan(
        q,
        r#"{"min":[{"column":"first","collate":"tr","nocase":true}],
            "max":[{"column":"last","collate":"tr","nocase":true}]}"#,
    );
    assert_eq!(status, 200, "{a}");
    assert_eq!(rows(&a), r#"[{"first":"çay","last":"zeytin"}]"#);
    let q = "get people select email, count() as n group email";
    let (status, a) = fan(
        q,
        r#"{"sum":["n"],"group":[{"column":"email","nocase":true}]}"#,
    );
    assert_eq!(status, 200, "{a}");
    assert_eq!(
        rows(&a),
        r#"[{"email":"ada@x","n":3},{"email":"ALAN@x","n":1},{"email":"aysel@x","n":1},{"email":"Bea@x","n":1},{"email":"bob@x","n":2},{"email":"Carol@x","n":1},{"email":"cem@x","n":1},{"email":"dora@x","n":1}]"#
    );
    // `desc` is an order's alone.
    let (status, a) = fan(
        q,
        r#"{"sum":["n"],"group":[{"column":"email","desc":true}]}"#,
    );
    assert_eq!(status, 400, "{a}");

    // Not told, the router compares bytes: the tenants whose rows are not
    // in that order fail, named, and nothing is merged wrong.
    for (q, key) in [
        ("get people select name order name limit 6", "name"),
        ("get people select email order email limit 6", "email"),
    ] {
        let (status, a) = fan(q, &format!(r#"{{"order":["{key}"],"limit":6}}"#));
        assert_eq!(status, 400, "{a}");
        assert!(a.contains("not in the merge's order"), "{a}");
        assert!(!a.contains("\"rows\""), "{a}");
    }

    // A collation the router does not know is refused, named, nothing
    // asked; so is a key it cannot read.
    let (status, a) = fan(
        "get people select name order name",
        r#"{"order":[{"column":"name","collate":"de"}]}"#,
    );
    assert_eq!(status, 400, "{a}");
    assert!(a.contains("`de`"), "{a}");
    for bad in [
        r#"{"order":[{"collate":"tr"}]}"#,
        r#"{"order":[{"column":"name","nocase":"de"}]}"#,
        r#"{"order":[{"column":"name","desc":"yes"}]}"#,
        r#"{"order":[{"column":"name","colate":"tr"}]}"#,
    ] {
        let (status, a) = fan("get people select name order name", bad);
        assert_eq!(status, 400, "{bad}: {a}");
        assert!(!a.contains("\"rows\""), "{bad}: {a}");
    }
}
