//! OpenTelemetry tracing (`--otlp-endpoint`) from a running server: the
//! spans a collector is sent, with their parents and attributes; what a
//! ratio of 0 sends; and a collector that hangs or is down, which no
//! request may wait for.

use crate::otlp::{by_id, children, closed, silent, Got, Receiver};
use crate::support::{start_env, tmp, Http};
use std::time::{Duration, Instant};

const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const PARENT: &str = "00f067aa0ba902b7";

fn traceparent(trace: &str, flags: &str) -> String {
    format!("00-{trace}-{PARENT}-{flags}")
}

/// `name`'s value in a scrape, the labels as written.
fn scraped(http: &mut Http, name: &str) -> f64 {
    let a = http.ask("GET", "/_metrics", "");
    assert_eq!(a.status, 200);
    a.body
        .lines()
        .find_map(|l| l.strip_prefix(name)?.strip_prefix(' ')?.parse().ok())
        .unwrap_or_else(|| panic!("no {name} in\n{}", a.body))
}

/// Polls the scrape until `name` is past 0: an exporter's count moves on
/// its own thread, after the requests that made it.
fn until_counted(http: &mut Http, name: &str) -> f64 {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let n = scraped(http, name);
        if n > 0.0 {
            return n;
        }
        assert!(Instant::now() < deadline, "{name} stayed 0");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn within(child: &Got, parent: &Got) {
    assert_eq!(child.trace, parent.trace);
    assert!(
        child.start >= parent.start && child.end <= parent.end,
        "{} [{}, {}] outside {} [{}, {}]",
        child.name,
        child.start,
        child.end,
        parent.name,
        parent.start,
        parent.end
    );
}

#[test]
fn a_durable_write_is_traced_through_the_lock_the_disk_and_the_replicas() {
    let recv = Receiver::start();
    let path = tmp("trace", "spans.fenec");
    let url = recv.url();
    let server = start_env(
        &[
            "--file",
            path.to_str().unwrap(),
            "--sync",
            "always",
            "--replication-token",
            "r",
            "--otlp-endpoint",
            &url,
            "--otlp-header",
            "x-api-key=k1",
        ],
        &[("OTEL_SERVICE_NAME", "orders-db")],
    );
    let mut http = server.http();
    http.run("create collection notes (title text)");
    let tp = traceparent(TRACE, "01");
    let a = http.ask_with(
        "POST",
        "/query",
        r#"{"query": "put notes {title: 'secret'}"}"#,
        &[
            ("traceparent", &tp),
            ("tracestate", "vendor=x,other=y"),
            ("X-Request-Id", "req-7"),
        ],
    );
    assert_eq!(a.status, 200, "{}", a.body);

    // A trace goes out whole, so its server span brings the rest.
    let spans = recv.until("the put's server span", |s| {
        s.iter().any(|g| g.trace == TRACE && g.kind == 2)
    });
    let root = spans
        .iter()
        .find(|g| g.trace == TRACE && g.kind == 2)
        .unwrap();
    assert_eq!(root.parent.as_deref(), Some(PARENT), "the client's span");
    assert_eq!(root.state.as_deref(), Some("vendor=x,other=y"));
    assert_eq!(root.service, "orders-db");
    assert_eq!(root.name, "POST /query");
    for (k, v) in [
        ("http.request.method", "POST"),
        ("http.route", "/query"),
        ("http.response.status_code", "200"),
        ("db.system", "fenecdb"),
        ("db.operation.name", "put"),
        ("db.query.text", "put notes {title: $1}"),
        ("fenec.request_id", "req-7"),
        ("server.address", "127.0.0.1"),
        ("server.port", &server.port.to_string()),
    ] {
        assert_eq!(root.attr(k), Some(v), "{k} in {root:#?}");
    }
    assert!(!root.error);
    let trace: Vec<Got> = spans.iter().filter(|g| g.trace == TRACE).cloned().collect();
    assert!(
        trace
            .iter()
            .all(|g| !format!("{:?}", g.attrs).contains("secret")),
        "a literal reached a span: {trace:#?}"
    );
    let lock = children(&trace, root, "lock.wait");
    assert_eq!(lock.len(), 1, "{trace:#?}");
    assert_eq!(lock[0].attr("fenec.lock"), Some("write"));
    let [execute] = children(&trace, root, "execute")[..] else {
        panic!("{trace:#?}")
    };
    let [durability] = children(&trace, root, "durability")[..] else {
        panic!("{trace:#?}")
    };
    let [fsync] = children(&trace, durability, "fsync")[..] else {
        panic!("{trace:#?}")
    };
    let [sent] = children(&trace, durability, "replication.wait_sent")[..] else {
        panic!("{trace:#?}")
    };
    for child in [lock[0], execute, durability] {
        within(child, root);
        assert_eq!(child.kind, 1);
    }
    within(fsync, durability);
    within(sent, durability);
    assert!(fsync.end <= sent.start, "the disk, then the replicas");
    assert!(
        execute.end <= durability.start,
        "the fsync waits for no lock"
    );
    assert_eq!(
        by_id(&trace, &fsync.parent.clone().unwrap()).name,
        "durability"
    );

    // No traceparent: a trace of its own, sampled at the default of 1.
    let a = http.ask_with("GET", "/notes", "", &[("X-Request-Id", "req-8")]);
    assert_eq!(a.status, 200);
    let spans = recv.until("the read's server span", |s| {
        s.iter()
            .any(|g| g.attr("fenec.request_id") == Some("req-8"))
    });
    let read = spans
        .iter()
        .find(|g| g.attr("fenec.request_id") == Some("req-8"))
        .unwrap();
    assert_ne!(read.trace, TRACE);
    assert_eq!(read.parent, None);
    assert_eq!(read.name, "GET /{collection}");
    assert_eq!(read.attr("db.operation.name"), Some("get"));
    assert_eq!(
        read.attr("db.query.text"),
        None,
        "a REST target holds values"
    );
    let lock = children(&spans, read, "lock.wait");
    assert_eq!(lock[0].attr("fenec.lock"), Some("read"));
    assert_eq!(children(&spans, read, "execute").len(), 1);

    // A refusal is the client's, not an error of the server's span.
    let a = http.ask_with("GET", "/missing", "", &[("X-Request-Id", "req-9")]);
    assert_eq!(a.status, 404);
    let spans = recv.until("the 404's span", |s| {
        s.iter()
            .any(|g| g.attr("fenec.request_id") == Some("req-9"))
    });
    let missing = spans
        .iter()
        .find(|g| g.attr("fenec.request_id") == Some("req-9"))
        .unwrap();
    assert_eq!(missing.attr("http.response.status_code"), Some("404"));
    assert!(!missing.error);

    for post in recv.posts() {
        assert_eq!(post.path, "/v1/traces");
        let header = |k: &str| {
            post.headers
                .iter()
                .find(|(h, _)| h == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(header("content-type"), Some("application/json"));
        assert_eq!(header("x-api-key"), Some("k1"));
    }
    // Health checks and scrapes are no spans of their own.
    assert_eq!(http.ask("GET", "/_health", "").status, 200);
    until_counted(&mut http, "fenec_trace_spans_exported_total");
    assert!(recv
        .spans()
        .iter()
        .all(|g| !g.name.contains("_health") && !g.name.contains("_metrics")));
}

#[test]
fn sampling_at_zero_sends_nothing() {
    let recv = Receiver::start();
    let url = recv.url();
    let server = start_env(
        &["--otlp-endpoint", &url, "--trace-sample", "0"],
        &[("OTEL_BSP_SCHEDULE_DELAY", "50")],
    );
    let mut http = server.http();
    http.run("create collection notes (title text)");
    let unsampled = traceparent("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "00");
    for i in 0..30 {
        let body = format!(r#"{{"query": "put notes {{title: 't{i}'}}"}}"#);
        assert_eq!(http.ask("POST", "/query", &body).status, 200);
        let a = http.ask_with("GET", "/notes", "", &[("traceparent", &unsampled)]);
        assert_eq!(a.status, 200);
    }
    // A sampled parent is followed whatever the ratio, and its trace goes
    // out after everything before it: once it is in, nothing else came.
    let marker = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let tp = traceparent(marker, "01");
    assert_eq!(
        http.ask_with("GET", "/notes", "", &[("traceparent", &tp)])
            .status,
        200
    );
    let spans = recv.until("the sampled request", |s| {
        s.iter().any(|g| g.trace == marker)
    });
    assert!(
        spans.iter().all(|g| g.trace == marker),
        "a ratio of 0 sent {spans:#?}"
    );
    assert_eq!(
        scraped(
            &mut http,
            "fenec_trace_spans_dropped_total{reason=\"queue_full\"}"
        ),
        0.0
    );
}

#[test]
fn a_collector_that_hangs_or_is_down_costs_the_requests_nothing() {
    // It takes the connection and never answers: every post waits out its
    // 20 s, while the queue holds 16 spans.
    let (_held, url) = silent();
    let env = [
        ("OTEL_BSP_MAX_QUEUE_SIZE", "16"),
        ("OTEL_BSP_MAX_EXPORT_BATCH_SIZE", "4"),
        ("OTEL_EXPORTER_OTLP_TIMEOUT", "20000"),
    ];
    let server = start_env(&["--otlp-endpoint", &url], &env);
    let mut http = server.http();
    http.run("create collection notes (title text)");
    http.run("put notes {title: 'a'}");
    let began = Instant::now();
    for _ in 0..300 {
        assert_eq!(http.ask("GET", "/notes?id=eq.1", "").status, 200);
    }
    // Waiting on the post even once would have taken its 20 s.
    let took = began.elapsed();
    assert!(took < Duration::from_secs(10), "300 reads took {took:?}");
    let dropped = until_counted(
        &mut http,
        "fenec_trace_spans_dropped_total{reason=\"queue_full\"}",
    );
    assert!(dropped > 0.0);

    // Down: every post refused at once, its spans counted out.
    let url = closed();
    let server = start_env(&["--otlp-endpoint", &url], &env[..2]);
    let mut http = server.http();
    http.run("create collection notes (title text)");
    let began = Instant::now();
    for i in 0..100 {
        let body = format!(r#"{{"query": "put notes {{title: 't{i}'}}"}}"#);
        assert_eq!(http.ask("POST", "/query", &body).status, 200);
    }
    let took = began.elapsed();
    assert!(took < Duration::from_secs(10), "100 writes took {took:?}");
    until_counted(
        &mut http,
        "fenec_trace_spans_dropped_total{reason=\"export_failed\"}",
    );
    assert!(server.logged("tracing: could not post", Duration::from_secs(10)));
}
