//! `GET /_metrics` on the router: what it answered by route and status
//! class, the nodes' time over what it forwarded, a node it could not
//! reach, its moves done and failed, and what the directory holds.
//!
//! The counters are the process's, as a router is one to a process, so
//! this file holds one test: in a binary of its own nothing else counts.

use fenec_http::tenants::Tenants;
use fenec_shard::directory::{Directory, Node, State};
use fenec_shard::{Config, Router};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// An in-process `--dir` node; its address and its directory.
fn node(tag: &str) -> (String, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "fenec-shard-metrics-{tag}-{}-{:?}",
        std::process::id(),
        Instant::now()
    ));
    let tenants = Arc::new(Tenants::new(&dir).unwrap());
    let cfg = fenec_http::Config {
        addr: "127.0.0.1:0".into(),
        admin_token: Some(format!("adm-{tag}")),
        ..fenec_http::Config::default()
    };
    let server = fenec_http::Server::with_tenants(tenants, cfg);
    let listener = server.bind().unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    (addr, dir)
}

fn call(port: u16, method: &str, target: &str, body: &str, auth: Option<&str>) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let auth = auth.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    write!(
        s,
        "{method} {target} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n{auth}\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let status = out
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, out)
}

/// The value of the sample `line` names, as the exposition writes it.
fn sample<'a>(text: &'a str, line: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|l| l.strip_prefix(line)?.strip_prefix(' '))
}

#[test]
fn the_router_counts_what_it_answered_forwarded_and_moved() {
    let (a1, d1) = node("n1");
    let (a2, d2) = node("n2");
    // A node nothing listens on: what a crashed one looks like from here.
    let gone = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = gone.local_addr().unwrap().to_string();
    drop(gone);
    let mut dir = Directory::in_memory();
    for (name, addr, token) in [
        ("n1", a1, "adm-n1"),
        ("n2", a2, "adm-n2"),
        ("dead", dead, "x"),
    ] {
        let token = token.into();
        dir.set_node(name, Node { addr, token }).unwrap();
    }
    dir.place("lost", "dead", State::Active).unwrap();
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        token: Some("root".into()),
        upstream_timeout: Duration::from_secs(2),
        ..Config::default()
    };
    let router = Router::new(dir, cfg);
    let listener = router.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = router.serve_on(listener);
    });
    let root = Some("root");

    let r = call(
        port,
        "PUT",
        "/_shard/tenants/acme",
        r#"{"node":"n1"}"#,
        root,
    );
    assert_eq!(r.0, 201, "{}", r.1);
    for _ in 0..3 {
        let r = call(
            port,
            "POST",
            "/t/acme/query",
            r#"{"query":"collections"}"#,
            None,
        );
        assert_eq!(r.0, 200, "{}", r.1);
    }
    assert_eq!(call(port, "GET", "/t/nosuch/", "", None).0, 404);
    assert_eq!(call(port, "GET", "/t/lost/", "", None).0, 502);
    assert_eq!(call(port, "GET", "/elsewhere", "", None).0, 404);
    let r = call(
        port,
        "POST",
        "/_shard/tenants/acme/move",
        r#"{"to":"n2"}"#,
        root,
    );
    assert_eq!(r.0, 200, "{}", r.1);
    // Frozen on a node that does not answer: begun, failed, thawed.
    let r = call(
        port,
        "POST",
        "/_shard/tenants/lost/move",
        r#"{"to":"n1"}"#,
        root,
    );
    assert_eq!(r.0, 502, "{}", r.1);
    // Refused before it began: no move.
    let r = call(
        port,
        "POST",
        "/_shard/tenants/acme/move",
        r#"{"to":"n9"}"#,
        root,
    );
    assert_eq!(r.0, 404, "{}", r.1);

    // The router's token, as /_shard/ takes it: the scrape names the nodes.
    assert_eq!(call(port, "GET", "/_metrics", "", None).0, 401);
    let (status, text) = call(port, "GET", "/_metrics", "", root);
    assert_eq!(status, 200, "{text}");
    assert!(
        text.contains("Content-Type: text/plain; version=0.0.4"),
        "{text}"
    );
    for (line, want) in [
        (
            "fenec_router_requests_total{route=\"tenant\",code=\"2xx\"}",
            "3",
        ),
        (
            "fenec_router_requests_total{route=\"tenant\",code=\"4xx\"}",
            "1",
        ),
        (
            "fenec_router_requests_total{route=\"tenant\",code=\"5xx\"}",
            "1",
        ),
        (
            "fenec_router_requests_total{route=\"other\",code=\"4xx\"}",
            "1",
        ),
        (
            "fenec_router_requests_total{route=\"shard\",code=\"2xx\"}",
            "2",
        ),
        (
            "fenec_router_requests_total{route=\"shard\",code=\"4xx\"}",
            "1",
        ),
        (
            "fenec_router_requests_total{route=\"shard\",code=\"5xx\"}",
            "1",
        ),
        (
            "fenec_router_requests_total{route=\"metrics\",code=\"4xx\"}",
            "1",
        ),
        (
            "fenec_router_request_duration_seconds_count{route=\"tenant\"}",
            "5",
        ),
        ("fenec_router_upstream_duration_seconds_count", "3"),
        ("fenec_router_upstream_errors_total{node=\"dead\"}", "1"),
        ("fenec_router_moves_total{outcome=\"done\"}", "1"),
        ("fenec_router_moves_total{outcome=\"failed\"}", "1"),
        ("fenec_router_move_duration_seconds_count", "1"),
        (
            "fenec_router_move_duration_seconds_bucket{le=\"+Inf\"}",
            "1",
        ),
        ("fenec_router_nodes", "3"),
        ("fenec_router_tenants{node=\"n1\"}", "0"),
        ("fenec_router_tenants{node=\"n2\"}", "1"),
        ("fenec_router_tenants{node=\"dead\"}", "1"),
        ("fenec_router_tenants_moving", "0"),
        ("fenec_router_following", "0"),
    ] {
        assert_eq!(sample(&text, line), Some(want), "{line}\n{text}");
    }
    // A bucket holds what is at or under its bound: every forwarded answer
    // took under ten seconds.
    assert_eq!(
        sample(
            &text,
            "fenec_router_request_duration_seconds_bucket{route=\"tenant\",le=\"10\"}"
        ),
        Some("5")
    );
    assert!(text.contains("fenec_build_info{version="), "{text}");
    let _ = std::fs::remove_dir_all(d1);
    let _ = std::fs::remove_dir_all(d2);
}
