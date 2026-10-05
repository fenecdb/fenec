//! One id for a request through the router, end to end: the client's, or
//! one the router makes, is what the node is sent, answers with and logs,
//! and the client gets it back once. Apart from the other tests: the audit
//! log is the process's own, and the router and the node here write theirs
//! into the same file -- which is what shows they logged the same id.

use fenec_http::tenants::Tenants;
use fenec_shard::directory::{Directory, Node, State};
use fenec_shard::{Config, Router};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

/// A request through `addr`: its status and the `X-Request-Id`s answered.
fn call(addr: &str, target: &str, body: &str, extra: &str) -> (u16, Vec<String>) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "POST {target} HTTP/1.1\r\nHost: x\r\n{extra}Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let head = out.split("\r\n\r\n").next().unwrap();
    let ids = head
        .lines()
        .filter_map(|l| l.split_once(':'))
        .filter(|(k, _)| k.eq_ignore_ascii_case("x-request-id"))
        .map(|(_, v)| v.trim().to_string())
        .collect();
    (out[9..12].parse().unwrap(), ids)
}

#[test]
fn a_request_through_the_router_has_one_id_in_both_logs() {
    let dir = std::env::temp_dir().join(format!("fenec-shard-request-id-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let log = dir.join("audit.log");
    let tenants = Arc::new(Tenants::new(dir.join("node")).unwrap());
    tenants.create("acme").unwrap();
    fenec_http::audit::open(&log).unwrap();
    fenec_http::audit::set_delay(0);
    let server = fenec_http::Server::with_tenants(
        tenants,
        fenec_http::Config {
            addr: "127.0.0.1:0".into(),
            admin_token: Some("adm".into()),
            token: Some("data".into()),
            ..fenec_http::Config::default()
        },
    );
    let listener = server.bind().unwrap();
    let node = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    let mut d = Directory::in_memory();
    let (addr, token) = (node.clone(), "adm".into());
    d.set_node("n1", Node { addr, token }).unwrap();
    d.place("acme", "n1", State::Active).unwrap();
    let router = Router::new(
        d,
        Config {
            addr: "127.0.0.1:0".into(),
            upstream_timeout: Duration::from_secs(10),
            ..Config::default()
        },
    );
    let listener = router.bind().unwrap();
    let front = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = router.serve_on(listener);
    });
    let router = front;

    let create = r#"{"query":"create collection notes (title text)"}"#;
    let auth = "Authorization: Bearer data\r\n";

    // The client's id: the node is sent it and answers with it, once.
    let (status, ids) = call(
        &router,
        "/t/acme/query",
        create,
        &format!("{auth}X-Request-Id: order-17\r\n"),
    );
    assert_eq!(status, 200);
    assert_eq!(ids, ["order-17"]);

    // None sent: the router's, which the node logs as its own.
    let drop = r#"{"query":"drop collection notes"}"#;
    let (status, ids) = call(&router, "/t/acme/query", drop, auth);
    assert_eq!(status, 200);
    let [made] = ids.as_slice() else {
        panic!("{ids:?}")
    };
    assert_eq!(made.len(), 16, "{made}");

    // One the router could not use is replaced before the node sees it.
    let (status, ids) = call(
        &router,
        "/t/acme/query",
        r#"{"query":"collections"}"#,
        &format!("{auth}X-Request-Id: {}\r\n", "z".repeat(200)),
    );
    assert_eq!(status, 200);
    assert_eq!(ids.len(), 1);
    assert_eq!(ids[0].len(), 16, "{ids:?}");

    // A refusal: the router logs it and so does the node, by one id.
    let (status, ids) = call(
        &router,
        "/t/acme/query",
        create,
        "Authorization: Bearer wrong\r\nX-Request-Id: probe-3\r\n",
    );
    assert_eq!(status, 401);
    assert_eq!(ids, ["probe-3"]);

    let text = std::fs::read_to_string(&log).unwrap();
    let with = |id: &str, event: &str| {
        text.lines()
            .filter(|l| {
                l.contains(&format!(r#""request_id":"{id}""#))
                    && l.contains(&format!(r#""event":"{event}""#))
            })
            .count()
    };
    assert_eq!(with("order-17", "schema"), 1, "{text}");
    assert_eq!(with(made, "schema"), 1, "{text}");
    assert_eq!(
        with("probe-3", "refused"),
        2,
        "the router's line and the node's:\n{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
