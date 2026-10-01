//! The audit log and the wait after a refusal, over a server in this
//! process. Apart from the other tests: the log and the count of failures
//! are the process's own.

use fenec_core::prelude::*;
use fenec_http::{Config, Server};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const ROOT: &str = "root-token";

fn call(port: u16, token: Option<&str>, method: &str, target: &str, body: &str) -> u16 {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let auth = token.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    write!(
        s,
        "{method} {target} HTTP/1.1\r\nHost: x\r\n{auth}Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out[9..12].parse().unwrap()
}

#[test]
fn refusals_wait_and_schema_changes_and_refusals_are_logged() {
    let dir = std::env::temp_dir().join(format!("fenec-audit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("audit.log");
    fenec_http::audit::open(&log).unwrap();
    fenec_http::audit::set_delay(50);

    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        token: Some(ROOT.into()),
        ..Config::default()
    };
    let server = Server::new(Arc::new(RwLock::new(Database::new())), cfg);
    let listener = server.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });

    // Each refusal waits twice as long as the one before.
    let mut waits = Vec::new();
    for _ in 0..3 {
        let t = Instant::now();
        assert_eq!(call(port, Some("wrong"), "GET", "/_schema", ""), 401);
        waits.push(t.elapsed());
    }
    assert!(waits[0] >= Duration::from_millis(50), "{waits:?}");
    assert!(waits[1] >= Duration::from_millis(100), "{waits:?}");
    assert!(waits[2] >= Duration::from_millis(200), "{waits:?}");
    // The right token starts the count again: the next refusal waits the
    // first wait, and the one after it twice that. (A clock bound on the
    // request itself fails on a slow runner.)
    let body = r#"{"query":"create collection notes (title text)"}"#;
    assert_eq!(call(port, Some(ROOT), "POST", "/query", body), 200);
    assert_eq!(call(port, None, "GET", "/_schema", ""), 401);
    let local = Some("127.0.0.1".parse().unwrap());
    assert_eq!(fenec_http::audit::failed(local), Duration::from_millis(100));

    // A write is no event; a schema change is, by its shape.
    let put = r#"{"query":"put notes {title: \"a secret title\"}"}"#;
    assert_eq!(call(port, Some(ROOT), "POST", "/query", put), 200);
    let drop = r#"{"query":"drop collection notes"}"#;
    assert_eq!(call(port, Some(ROOT), "POST", "/query", drop), 200);

    let text = std::fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    let events: Vec<&str> = lines
        .iter()
        .map(|l| {
            let at = l.find(r#""event":""#).unwrap() + 9;
            &l[at..at + l[at..].find('"').unwrap()]
        })
        .collect();
    assert_eq!(
        events,
        ["refused", "refused", "refused", "schema", "refused", "schema"],
        "{text}"
    );
    for l in &lines {
        let doc = fenec_core::json::parse_object(l).unwrap();
        let field = |k: &str| doc.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert!(
            matches!(field("at"), Some(Value::Text(t)) if t.ends_with('Z')),
            "{l}"
        );
        assert_eq!(field("proto"), Some(Value::Text("http".into())), "{l}");
        assert!(matches!(field("peer"), Some(Value::Text(p)) if p.starts_with("127.0.0.1:")));
    }
    assert!(lines[0].contains(r#""path":"/_schema""#), "{}", lines[0]);
    assert!(lines[3].contains(r#""statement":"create collection notes (title text)""#));
    assert!(lines[5].contains(r#""statement":"drop collection notes""#));
    assert!(!text.contains("a secret title"), "no write, no literal");
    std::fs::remove_dir_all(&dir).unwrap();
}
