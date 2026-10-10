//! A trace through the router into the node: the client's `traceparent`
//! is continued by the router, whose forward the node's server span hangs
//! under, so one request is one tree across both processes. Apart from the
//! other tests: tracing is turned on for the whole process, and the router
//! and the node here send their spans to the one receiver.

#[path = "../../fenec-server/tests/otlp.rs"]
mod otlp;

use fenec_http::tenants::Tenants;
use fenec_shard::directory::{Directory, Node, State};
use fenec_shard::{Config, Router};
use otlp::{by_id, children, Got, Receiver};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

const TRACE: &str = "0af7651916cd43dd8448eb211c80319c";
const CLIENT: &str = "b7ad6b7169203331";

/// A request through `addr`: its status.
fn call(addr: &str, target: &str, body: &str, extra: &str) -> u16 {
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
    out[9..12].parse().unwrap()
}

/// The server span of `trace` the process at `kind` answered: the router's
/// is the one with a `forward` under it.
fn servers(spans: &[Got], trace: &str) -> (Got, Got, Got) {
    let of: Vec<&Got> = spans.iter().filter(|g| g.trace == trace).collect();
    let forward = *of
        .iter()
        .find(|g| g.name == "forward")
        .unwrap_or_else(|| panic!("no forward in {of:#?}"));
    let router = by_id(spans, forward.parent.as_deref().unwrap()).clone();
    let node = (*of
        .iter()
        .find(|g| g.kind == 2 && g.parent.as_deref() == Some(forward.id.as_str()))
        .unwrap_or_else(|| panic!("no node span under the forward in {of:#?}")))
    .clone();
    (router, forward.clone(), node)
}

#[test]
fn a_request_through_the_router_is_one_trace_with_the_node() {
    let recv = Receiver::start();
    let settings = fenec_http::trace::Options {
        endpoint: Some(recv.url()),
        ..Default::default()
    }
    .settings("fenec-shard")
    .unwrap()
    .unwrap();
    fenec_http::trace::install(settings).unwrap();

    let dir = std::env::temp_dir().join(format!("fenec-shard-tracing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let tenants = Arc::new(Tenants::new(dir.join("node")).unwrap());
    tenants.create("acme").unwrap();
    let server = fenec_http::Server::with_tenants(
        tenants,
        fenec_http::Config {
            addr: "127.0.0.1:0".into(),
            admin_token: Some("adm".into()),
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

    // The client's context: the router continues it, and so does the node.
    let create = r#"{"query":"create collection notes (title text)"}"#;
    let status = call(
        &front,
        "/t/acme/query",
        create,
        &format!(
            "traceparent: 00-{TRACE}-{CLIENT}-01\r\ntracestate: rojo=00f067aa0ba902b7\r\n\
             X-Request-Id: order-17\r\n"
        ),
    );
    assert_eq!(status, 200);
    let spans = recv.until("the node's span of the client's trace", |s| {
        s.iter().filter(|g| g.trace == TRACE && g.kind == 2).count() == 2
    });
    let (router, forward, node_span) = servers(&spans, TRACE);
    assert_eq!(router.parent.as_deref(), Some(CLIENT));
    assert_eq!(router.name, "POST /t/{tenant}/query");
    assert_eq!(router.attr("fenec.tenant"), Some("acme"));
    assert_eq!(router.attr("fenec.node"), Some("n1"));
    assert_eq!(router.attr("http.response.status_code"), Some("200"));
    assert_eq!(forward.kind, 3, "a client span");
    assert_eq!(forward.attr("server.address"), Some("127.0.0.1"));
    let port = node.rsplit_once(':').unwrap().1;
    assert_eq!(forward.attr("server.port"), Some(port));
    assert_eq!(forward.attr("http.response.status_code"), Some("200"));
    assert_eq!(node_span.name, "POST /t/{tenant}/query");
    assert_eq!(node_span.attr("fenec.tenant"), Some("acme"));
    assert_eq!(node_span.attr("db.operation.name"), Some("create"));
    assert_eq!(
        node_span.attr("db.query.text"),
        Some("create collection notes (title text)")
    );
    // One id from end to end, on both server spans.
    assert_eq!(router.attr("fenec.request_id"), Some("order-17"));
    assert_eq!(node_span.attr("fenec.request_id"), Some("order-17"));
    for g in spans.iter().filter(|g| g.trace == TRACE) {
        assert_eq!(g.state.as_deref(), Some("rojo=00f067aa0ba902b7"), "{g:#?}");
    }
    assert!(forward.start >= router.start && forward.end <= router.end);
    assert!(node_span.start >= forward.start && node_span.end <= forward.end);
    let lock = children(&spans, &node_span, "lock.wait");
    assert_eq!(lock.len(), 1, "{spans:#?}");
    assert_eq!(children(&spans, &node_span, "execute").len(), 1);

    // No context sent: the router starts the trace, and the node joins it.
    let put = r#"{"query":"put notes {title: 'a'}"}"#;
    assert_eq!(
        call(&front, "/t/acme/query", put, "X-Request-Id: put-1\r\n"),
        200
    );
    let spans = recv.until("the router's own trace", |s| {
        s.iter()
            .filter(|g| g.attr("fenec.request_id") == Some("put-1"))
            .count()
            == 2
    });
    let started = spans
        .iter()
        .find(|g| g.attr("fenec.request_id") == Some("put-1") && g.parent.is_none())
        .expect("the router's root span");
    let (router, _, node_span) = servers(&spans, &started.trace);
    assert_eq!(router.id, started.id);
    assert_eq!(node_span.attr("fenec.request_id"), Some("put-1"));
    assert_eq!(node_span.state, None);

    // A tenant the directory does not hold: the router's span alone, a 4xx
    // and not an error.
    assert_eq!(
        call(&front, "/t/nobody/query", put, "X-Request-Id: miss-1\r\n"),
        404
    );
    let spans = recv.until("the 404", |s| {
        s.iter()
            .any(|g| g.attr("fenec.request_id") == Some("miss-1"))
    });
    let miss: Vec<&Got> = spans
        .iter()
        .filter(|g| g.attr("fenec.request_id") == Some("miss-1"))
        .collect();
    assert_eq!(miss.len(), 1);
    assert_eq!(miss[0].attr("http.response.status_code"), Some("404"));
    assert!(!miss[0].error);

    let scrape = {
        let mut s = TcpStream::connect(&front).unwrap();
        write!(
            s,
            "GET /_metrics HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    };
    assert!(scrape.contains("fenec_trace_spans_dropped_total{reason=\"queue_full\"} 0"));
    let _ = std::fs::remove_dir_all(&dir);
}
