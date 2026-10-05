//! The request id and the JSON lines a log collector reads, on a running
//! `fenec-server`: an id a client sent comes back and is in every line its
//! request wrote -- the audit log's and the slow statements' on stderr --
//! one it could not use is replaced, and one is made where none was sent.
//! And every attribute `monitoring/datadog/pipeline.json` remaps, and the
//! dashboard's log widgets show, is in the lines a server really writes:
//! a pipeline reading a name no line has does nothing, and says nothing.

use crate::support::{start, tmp};
use fenec_core::prelude::*;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const ADMIN: &str = "logs-admin";

type Line = Vec<(String, Value)>;

fn text<'a>(line: &'a Line, key: &str) -> Option<&'a str> {
    line.iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| match v {
            Value::Text(t) => Some(t.as_str()),
            _ => None,
        })
}

/// The JSON lines of `log`, the server's text lines left out.
fn json_lines(log: &str) -> Vec<Line> {
    log.lines()
        .filter(|l| l.starts_with('{'))
        .map(|l| fenec_core::json::parse_object(l).unwrap_or_else(|e| panic!("{l}: {e}")))
        .collect()
}

/// Every attribute a processor of the pipeline reads, and every `@name`
/// the dashboard's log widgets read; `message` is a text line's own.
fn attributes_read() -> BTreeSet<String> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../monitoring/datadog");
    let mut out = BTreeSet::new();
    let pipeline = std::fs::read_to_string(root.join("pipeline.json")).unwrap();
    let Value::Object(p) = fenec_core::json::parse_json(&pipeline).unwrap() else {
        panic!("pipeline.json is not an object");
    };
    let Some((_, Value::List(processors))) = p.iter().find(|(k, _)| k == "processors") else {
        panic!("pipeline.json has no processors");
    };
    for proc in processors {
        let Value::Object(fields) = proc else {
            panic!("{proc:?}")
        };
        for (k, v) in fields {
            match (k.as_str(), v) {
                ("sources", Value::List(s)) => out.extend(s.iter().filter_map(|s| match s {
                    Value::Text(t) => Some(t.clone()),
                    _ => None,
                })),
                ("source", Value::Text(t)) if t != "message" => {
                    out.insert(t.clone());
                }
                // `duration_ms * 1000000`: its names.
                ("expression", Value::Text(e)) => out.extend(
                    e.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                        .filter(|w| w.starts_with(|c: char| c.is_ascii_alphabetic()))
                        .map(String::from),
                ),
                _ => {}
            }
        }
    }
    let dashboard = std::fs::read_to_string(root.join("dashboard.json")).unwrap();
    for (i, _) in dashboard.match_indices('@') {
        let name: String = dashboard[i + 1..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        out.insert(name);
    }
    assert!(out.len() >= 10, "{out:?}");
    out
}

#[test]
fn a_requests_id_is_in_its_answer_and_every_line_it_wrote() {
    let dir = tmp("logs", "tenants");
    let audit = tmp("logs", "audit.log");
    let server = start(&[
        "--dir",
        dir.to_str().unwrap(),
        "--admin-token",
        ADMIN,
        "--audit",
        audit.to_str().unwrap(),
        "--auth-delay",
        "0",
        // Every statement: the pipeline is tried out on lines a server
        // writes now, as an operator tries one out.
        "--slow-ms",
        "0",
    ]);
    let mut c = server.http().with_token(ADMIN);

    // An admin request, with an id of the client's.
    let a = c
        .try_ask(
            "PUT",
            "/_admin/tenants/acme",
            "",
            &[("X-Request-Id", "deploy-7/acme")],
        )
        .unwrap();
    assert_eq!(a.status, 201, "{}", a.body);
    assert_eq!(a.header("x-request-id"), Some("deploy-7/acme"));

    // A schema change in the tenant: its audit line and its slow line.
    let body = r#"{"query":"create collection notes (title text)"}"#;
    let a = c
        .try_ask("POST", "/t/acme/query", body, &[("X-Request-Id", "req 42")])
        .unwrap();
    assert_eq!(a.status, 200, "{}", a.body);
    assert_eq!(a.header("x-request-id"), Some("req 42"));

    // An id too long, or not printable, is replaced by one of the server's.
    let long = "x".repeat(129);
    for bad in [long.as_str(), "caf\u{e9}"] {
        let a = c
            .try_ask("GET", "/t/acme/notes", "", &[("X-Request-Id", bad)])
            .unwrap();
        assert_eq!(a.status, 200, "{}", a.body);
        let id = a.header("x-request-id").unwrap();
        assert!(
            id.len() == 16 && id.bytes().all(|b| b.is_ascii_hexdigit()),
            "{id}"
        );
    }

    // None sent: one is made, and differs from the last.
    let first = c.ask("GET", "/_health", "");
    let second = c.ask("GET", "/_health", "");
    let made = first.header("x-request-id").unwrap().to_string();
    assert_eq!(made.len(), 16, "{made}");
    assert_ne!(Some(made.as_str()), second.header("x-request-id"));

    // A refusal, with the id it was made.
    c.token = Some("wrong".into());
    let refused = c.ask("GET", "/_admin/tenants", "");
    assert_eq!(refused.status, 401);
    let refused_id = refused.header("x-request-id").unwrap().to_string();

    // The slow lines go to stderr as the answer goes out: waited for.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !server
        .log
        .lock()
        .unwrap()
        .contains("\"request_id\":\"req 42\"")
    {
        assert!(Instant::now() < deadline, "no slow line for the request");
        std::thread::sleep(Duration::from_millis(20));
    }
    let slow = json_lines(&server.log.lock().unwrap());
    let audited = json_lines(&std::fs::read_to_string(&audit).unwrap());

    let of = |lines: &[Line], event: &str, id: &str| -> Line {
        lines
            .iter()
            .find(|l| text(l, "event") == Some(event) && text(l, "request_id") == Some(id))
            .unwrap_or_else(|| panic!("no {event} line for {id}: {lines:?}"))
            .clone()
    };
    let admin = of(&audited, "admin", "deploy-7/acme");
    assert_eq!(text(&admin, "path"), Some("/_admin/tenants/acme"));
    assert!(admin
        .iter()
        .any(|(k, v)| k == "http_status" && *v == Value::Int(201)));
    let schema = of(&audited, "schema", "req 42");
    assert_eq!(text(&schema, "tenant"), Some("acme"));
    let statement = of(&slow, "slow", "req 42");
    assert_eq!(text(&statement, "tenant"), Some("acme"));
    assert_eq!(text(&statement, "kind"), Some("write"));
    assert_eq!(text(&statement, "level"), Some("warn"));
    assert!(text(&statement, "statement")
        .unwrap()
        .contains("create collection notes"));
    assert!(matches!(
        statement.iter().find(|(k, _)| k == "duration_ms").map(|(_, v)| v),
        Some(Value::Float(ms)) if *ms >= 0.0
    ));
    let refusal = of(&audited, "refused", &refused_id);
    assert_eq!(text(&refusal, "level"), Some("warn"));
    // The health probe is no statement: no slow line, and the made id is
    // in no line.
    assert!(!slow
        .iter()
        .any(|l| text(l, "request_id") == Some(made.as_str())));

    let every: Vec<&Line> = slow.iter().chain(&audited).collect();
    for l in &every {
        assert!(text(l, "at").is_some_and(|t| t.ends_with('Z')), "{l:?}");
        assert!(text(l, "level").is_some(), "{l:?}");
        assert!(text(l, "request_id").is_some(), "{l:?}");
    }
    for attribute in attributes_read() {
        assert!(
            every.iter().any(|l| l.iter().any(|(k, _)| *k == attribute)),
            "the Datadog pipeline or dashboard reads `{attribute}`, which no line has:\n{every:?}"
        );
    }
}
