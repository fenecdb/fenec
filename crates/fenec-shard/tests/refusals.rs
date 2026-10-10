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

use fenec_http::access::Access;
use fenec_http::audit::would_wait;
use fenec_http::tenants::Tenants;
use fenec_shard::directory::{Directory, Node, State};
use fenec_shard::{Config, Router};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SECRET: &[u8] = b"thirty-two bytes and a few more, for HS256";

fn router(addr: &str, node: &str) -> String {
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
    fenec_http::audit::set_delay(50);
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
