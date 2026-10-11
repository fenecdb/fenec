//! The wait after a refused token, behind the router. Apart from the other
//! tests: the count of failures is the process's own, and the router and
//! the node here share it, as they would not apart -- which is what holds
//! the node to waiting none for what the router forwards.
//!
//! Two clients need two addresses: one reaches the router over IPv4's
//! loopback, the other a second router over the same node on IPv6's, `::1`.
//! Both routers reach the node from 127.0.0.1, the first client's address.
//!
//! Which count a refusal went to is read off the count (`would_wait`), and
//! a request's time is bounded from below only. Bounded from above, it
//! measured the system rather than the count: a macOS runner answered a
//! first refusal, a wait of 50 ms, in 256.6 ms against a bound of 200 --
//! macOS gives the timers of a process whose QoS it clamps that much
//! leeway, and under `taskpolicy -c background` the bound failed 41 runs of
//! 50, at 212 to 252 ms -- and in a container of three cores beside busy
//! loops the refusal took up to 287 ms (the sleep 90 of them, the node's
//! answer 75) and a good token 50.6 against a bound of 50. Every one of
//! them counted as it should.
//!
//! The tests take turns ([`turn`]): their clients' addresses are the same
//! two.

use fenec_http::access::Access;
use fenec_http::audit::{self, would_wait};
use fenec_http::tenants::Tenants;
use fenec_shard::directory::{Directory, Node, State};
use fenec_shard::{Config, Router};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const SECRET: &[u8] = b"thirty-two bytes and a few more, for HS256";
const V4: Option<IpAddr> = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));
const V6: Option<IpAddr> = Some(IpAddr::V6(Ipv6Addr::LOCALHOST));

static TURN: Mutex<()> = Mutex::new(());

/// This test's turn at the counts of both loopback addresses, which start
/// from nothing.
fn turn() -> MutexGuard<'static, ()> {
    let turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
    audit::set_delay(50);
    audit::succeeded(V4);
    audit::succeeded(V6);
    turn
}

fn router(addr: &str, node: &str) -> String {
    router_with(addr, node, None)
}

/// A router over the one node `node`, holding acme, with `token` for its
/// own `/_shard/` and `/_metrics`.
fn router_with(addr: &str, node: &str, token: Option<&str>) -> String {
    let mut dir = Directory::in_memory();
    dir.set_node(
        "n1",
        Node {
            addr: node.into(),
            token: "adm".into(),
        },
    )
    .unwrap();
    dir.place("acme", "n1", State::Active).unwrap();
    let router = Router::new(
        dir,
        Config {
            addr: addr.into(),
            token: token.map(str::to_string),
            upstream_timeout: Duration::from_secs(10),
            ..Config::default()
        },
    );
    let listener = router.bind().unwrap();
    let local = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = router.serve_on(listener);
    });
    local
}

/// A `get` of acme's notes through `router` with `token` and `extra`
/// headers: its status, and how long it took.
fn get(router: &str, token: &str, extra: &str) -> (u16, Duration) {
    let started = Instant::now();
    let mut s = TcpStream::connect(router).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    write!(
        s,
        "GET /t/acme/notes HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\n{extra}\
         Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    (out[9..12].parse().unwrap(), started.elapsed())
}

#[test]
fn a_refusal_through_the_router_waits_by_the_client_and_not_by_the_router() {
    let _turn = turn();
    let dir = std::env::temp_dir().join(format!("fenec-shard-refusals-{}", std::process::id()));
    let tenants = Arc::new(Tenants::new(&dir).unwrap());
    tenants.create("acme").unwrap();
    let access = Arc::new(Access::new(SECRET, "notes  read  where owner = $jwt.sub\n").unwrap());
    let cfg = fenec_http::Config {
        addr: "127.0.0.1:0".into(),
        admin_token: Some("adm".into()),
        token: Some("data".into()),
        access: Some(Arc::clone(&access)),
        ..fenec_http::Config::default()
    };
    let server = fenec_http::Server::with_tenants(Arc::clone(&tenants), cfg);
    let listener = server.bind().unwrap();
    let node = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    let q = r#"{"query":"create collection notes (owner text, title text)"}"#;
    let mut s = TcpStream::connect(&node).unwrap();
    write!(
        s,
        "POST /t/acme/query HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer data\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{q}",
        q.len()
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");

    let victim = router("127.0.0.1:0", &node);
    let attacker = match std::net::TcpListener::bind("[::1]:0") {
        Ok(_) => router("[::1]:0", &node),
        Err(e) => panic!("this test needs IPv6's loopback for a second address: {e}"),
    };
    let good = access.mint(r#"{"sub":"vera","tenant":"acme"}"#).unwrap();
    let forged = Access::new(b"another thirty-two bytes, and some more", "")
        .unwrap()
        .mint(r#"{"sub":"vera","tenant":"acme"}"#)
        .unwrap();
    // The attacker also names another address, and a mark of its own: the
    // router drops both, and the node believes neither.
    let spoof = "Fenec-Router: 203.0.113.7 guessed\r\n";
    let ms = Duration::from_millis;
    let (victim_ip, attacker_ip) = (
        Some(IpAddr::from(Ipv4Addr::LOCALHOST)),
        Some(IpAddr::from(Ipv6Addr::LOCALHOST)),
    );
    let spoofed = Some(IpAddr::from([203, 0, 113, 7]));

    // The attacker's refusals wait 50, 100, 200 ms: doubled, by its address.
    let waits: Vec<Duration> = (0..3)
        .map(|_| get(&attacker, &forged, spoof))
        .map(|(s, d)| {
            assert_eq!(s, 401);
            d
        })
        .collect();
    for (w, least) in waits.iter().zip([50, 100, 200]) {
        assert!(*w >= ms(least), "{waits:?}");
    }
    assert_eq!(would_wait(attacker_ip), ms(400));
    // Each reached the node from 127.0.0.1, which the node counted none of
    // -- nor the address the attacker named.
    assert_eq!(would_wait(victim_ip), ms(50));
    assert_eq!(would_wait(spoofed), ms(50));

    // Another client's first refusal waits the first wait, not the
    // attacker's fourth: counted by the router's address, it was 400 ms.
    let (status, took) = get(&victim, &forged, "");
    assert_eq!(status, 401);
    assert!(took >= ms(50), "{took:?}");
    // Counted once, by the router: counted by the node as well, by either
    // address, the next would wait 200 or 800.
    assert_eq!(would_wait(victim_ip), ms(100));
    assert_eq!(would_wait(attacker_ip), ms(400));

    // Its good token is served, and starts only its own count again.
    let (status, _) = get(&victim, &good, "");
    assert_eq!(status, 200);
    assert_eq!(would_wait(victim_ip), ms(50));
    assert_eq!(would_wait(attacker_ip), ms(400));
    let (status, took) = get(&attacker, &forged, spoof);
    assert_eq!(status, 401);
    assert!(took >= ms(400), "the attacker's count was reset: {took:?}");

    // Straight to the node, the headers are believed from no one: the
    // refusal waits by the address the connection came from.
    let mut s = TcpStream::connect(&node).unwrap();
    let started = Instant::now();
    write!(
        s,
        "GET /t/acme/notes HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {forged}\r\n{spoof}\
         Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    assert!(out.starts_with("HTTP/1.1 401"), "{out}");
    assert!(started.elapsed() >= ms(50));
    assert_eq!(would_wait(victim_ip), ms(100));
    assert_eq!(would_wait(spoofed), ms(50));
    let _ = std::fs::remove_dir_all(&dir);
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// A request to `addr` with `headers`: its status and the whole answer.
fn ask(addr: &str, method: &str, target: &str, headers: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    write!(
        s,
        "{method} {target} HTTP/1.1\r\nHost: x\r\n{headers}Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let head = out.split("\r\n\r\n").next().unwrap_or("");
    // Whether the node verified a token is the router's to know: a client
    // is never told.
    assert!(
        !head.to_ascii_lowercase().contains("fenec-verified"),
        "{out}"
    );
    (out[9..12].parse().unwrap(), out)
}

/// A case: a request's method, target, headers and body, and its status.
type Case<'a> = (&'a str, &'a str, &'a str, &'a str, u16);

/// Each request through `addr`, carrying `guessed`, answered as its case
/// says -- and the client's count (::1) left at `held`, the router's own
/// at the node (127.0.0.1) at `router`. Taken for a good token's, as every
/// answer but 401 to a request carrying `Authorization` once was, any of
/// them started the client's again.
fn verify_nothing(addr: &str, guessed: &str, cases: &[Case], held: Duration, router: Duration) {
    for &(method, target, headers, body, status) in cases {
        let (got, out) = ask(addr, method, target, &format!("{guessed}{headers}"), body);
        assert_eq!(got, status, "{method} {target}: {out}");
        assert_eq!(
            would_wait(V6),
            held,
            "{method} {target}, answered {got}, started the client's count again"
        );
        assert_eq!(would_wait(V4), router, "{method} {target}");
    }
}

/// Each request through `addr`, a good token in it, answered as its case
/// says -- and the client's count, grown first, started again, whatever
/// the answer says of the rest of the request; the router's own at the
/// node (127.0.0.1) left at `router`: the node verified the token of a
/// request the router forwarded, which is the client's.
fn verify(addr: &str, cases: &[Case], router: Duration) {
    for &(method, target, headers, body, status) in cases {
        audit::failed(V6);
        audit::failed(V6);
        assert!(would_wait(V6) >= ms(200));
        let (got, out) = ask(addr, method, target, headers, body);
        assert_eq!(got, status, "{method} {target}: {out}");
        assert_eq!(
            would_wait(V6),
            ms(50),
            "{method} {target}, answered {got}: its token was verified"
        );
        assert_eq!(would_wait(V4), router, "{method} {target}");
    }
}

#[test]
fn the_router_starts_a_count_again_on_a_token_verified_alone() {
    let _turn = turn();
    let dir = std::env::temp_dir().join(format!("fenec-shard-verified-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let tenants = Arc::new(Tenants::new(&dir).unwrap());
    tenants.create("acme").unwrap();
    let access = Arc::new(Access::new(SECRET, "notes  read  where owner = $jwt.sub\n").unwrap());
    let server = fenec_http::Server::with_tenants(
        Arc::clone(&tenants),
        fenec_http::Config {
            addr: "127.0.0.1:0".into(),
            admin_token: Some("adm".into()),
            token: Some("data".into()),
            access: Some(Arc::clone(&access)),
            cors: Some("*".into()),
            // A body past this and within the import's ceiling is read
            // whole, and refused before its token is looked at.
            max_body: 1024,
            max_import: 1 << 20,
            ..fenec_http::Config::default()
        },
    );
    let listener = server.bind().unwrap();
    let node = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    let data = "Authorization: Bearer data\r\n";
    let create = r#"{"query":"create collection notes (owner text, title text)"}"#;
    assert_eq!(ask(&node, "POST", "/t/acme/query", data, create).0, 200);

    // The client asks from ::1, and the routers reach the node from
    // 127.0.0.1: one count each.
    if let Err(e) = std::net::TcpListener::bind("[::1]:0") {
        panic!("this test needs IPv6's loopback for a second address: {e}");
    }
    let open = router("[::1]:0", &node);
    let guarded = router_with("[::1]:0", &node, Some("rt"));
    let forged = Access::new(b"another thirty-two bytes, and some more", "")
        .unwrap()
        .mint(r#"{"sub":"vera","tenant":"acme"}"#)
        .unwrap();
    let guessed = format!("Authorization: Bearer {forged}\r\n");
    for _ in 0..2 {
        assert_eq!(ask(&open, "GET", "/t/acme/notes", &guessed, "").0, 401);
    }
    let held = would_wait(V6);
    assert_eq!(held, ms(200));
    // The router's address at the node has a count of its own, which no
    // token of a client's starts again.
    audit::failed(V4);
    let router_held = would_wait(V4);
    assert_eq!(router_held, ms(100));

    let origin = "Origin: https://app.example\r\n";
    let big = format!(r#"{{"query":"get notes","pad":"{}"}}"#, "x".repeat(2048));
    verify_nothing(
        &open,
        &guessed,
        &[
            ("GET", "/nosuch", "", "", 404),
            ("GET", "/_studio/", "", "", 404),
            // A router with no `--token` of its own verifies none there.
            ("GET", "/_metrics", "", "", 200),
            ("GET", "/_shard/nodes", "", "", 200),
            ("GET", "/t/nobody/notes", "", "", 404),
            // Forwarded, and answered by the node before it looks at the
            // token: a preflight, a body past its ceiling.
            ("OPTIONS", "/t/acme/notes", origin, "", 204),
            ("POST", "/t/acme/query", "", &big, 413),
        ],
        held,
        router_held,
    );
    verify_nothing(
        &guarded,
        &guessed,
        &[("GET", "/nosuch", "", "", 404)],
        held,
        router_held,
    );

    let vera = access.mint(r#"{"sub":"vera","tenant":"acme"}"#).unwrap();
    let elsewhere = access.mint(r#"{"sub":"vera","tenant":"beta"}"#).unwrap();
    let (vera, elsewhere) = (
        format!("Authorization: Bearer {vera}\r\n"),
        format!("Authorization: Bearer {elsewhere}\r\n"),
    );
    let rt = "Authorization: Bearer rt\r\n";
    // The router's own token, and the node's verdict on the rest.
    verify(
        &guarded,
        &[
            ("GET", "/_metrics", rt, "", 200),
            ("GET", "/_shard/nosuch", rt, "", 404),
        ],
        router_held,
    );
    verify(
        &open,
        &[
            ("GET", "/t/acme/nothere", data, "", 404),
            ("GET", "/t/acme/notes", &vera, "", 200),
            // Verified, and bound to another tenant.
            ("GET", "/t/acme/notes", &elsewhere, "", 403),
            (
                "POST",
                "/t/acme/query",
                data,
                r#"{"query":"set notes {title: \"b\"} where id = 99 require 1"}"#,
                412,
            ),
        ],
        router_held,
    );

    // The node connection a verified request just used carries the next
    // request, which verifies nothing: its answer says nothing of the one
    // before.
    assert_eq!(ask(&open, "GET", "/t/acme/notes", data, "").0, 200);
    audit::failed(V6);
    audit::failed(V6);
    let held = would_wait(V6);
    verify_nothing(
        &open,
        &guessed,
        &[
            ("POST", "/t/acme/query", "", &big, 413),
            ("OPTIONS", "/t/acme/notes", origin, "", 204),
        ],
        held,
        router_held,
    );
    let _ = std::fs::remove_dir_all(&dir);
}
