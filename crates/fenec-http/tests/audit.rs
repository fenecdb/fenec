//! The audit log and the wait after a refusal, over a server in this
//! process. Apart from the other tests: the log and the count of failures
//! are the process's own.
//!
//! Every request here comes from 127.0.0.1, so the tests take turns
//! ([`turn`]), and which count a request moved is read off the count
//! (`would_wait`), a request's time bounded from below only: the sleep is
//! the system's to stretch.

use fenec_core::prelude::*;
use fenec_http::access::Access;
use fenec_http::audit::{self, would_wait};
use fenec_http::replication::Replication;
use fenec_http::tenants::Tenants;
use fenec_http::{Config, Server};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};

const ROOT: &str = "root-token";
const LOCAL: Option<IpAddr> = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));
/// A token guessed, sent with every request that must verify nothing.
const GUESSED: &str = "Authorization: Bearer guessed\r\n";
const ORIGIN: &str = "Origin: https://app.example\r\n";
const SECRET: &[u8] = b"thirty-two bytes and a few more, for HS256";

static TURN: Mutex<()> = Mutex::new(());

/// This test's turn at the count of 127.0.0.1, which starts from nothing.
fn turn() -> MutexGuard<'static, ()> {
    let turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
    audit::set_delay(50);
    audit::succeeded(LOCAL);
    turn
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn serve(server: Server) -> u16 {
    let listener = server.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    port
}

fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("fenec-audit-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A request with `headers`: its status and the whole answer.
fn ask(port: u16, method: &str, target: &str, headers: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
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
    (out[9..12].parse().unwrap(), out)
}

/// A case: a request's method, target, headers and body, and its status.
type Case<'a> = (&'a str, &'a str, &'a str, &'a str, u16);

/// Each request, a guessed token in it, answered as its case says -- and
/// the count of the guesses before them left at `held`. Taken for a good
/// token's, as every answer but 401 to a request carrying `Authorization`
/// once was, any of them started it again.
fn verify_nothing(port: u16, cases: &[Case], held: Duration) {
    for &(method, target, headers, body, status) in cases {
        let (got, out) = ask(port, method, target, &format!("{GUESSED}{headers}"), body);
        assert_eq!(got, status, "{method} {target}: {out}");
        assert_eq!(
            would_wait(LOCAL),
            held,
            "{method} {target}, answered {got}, started the count again"
        );
        let head = out.split("\r\n\r\n").next().unwrap_or("");
        assert!(
            !head.to_ascii_lowercase().contains("fenec-verified"),
            "{out}"
        );
    }
}

/// Each request, a good token in it, answered as its case says -- and the
/// count, grown first, started again: the token was verified, whatever the
/// answer says of the rest of the request.
fn verify(port: u16, cases: &[Case]) {
    for &(method, target, headers, body, status) in cases {
        audit::failed(LOCAL);
        audit::failed(LOCAL);
        assert!(would_wait(LOCAL) >= ms(200));
        let (got, out) = ask(port, method, target, headers, body);
        assert_eq!(got, status, "{method} {target}: {out}");
        assert_eq!(
            would_wait(LOCAL),
            ms(50),
            "{method} {target}, answered {got}: its token was verified"
        );
    }
}

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
    let _turn = turn();
    let dir = scratch("log");
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("audit.log");
    fenec_http::audit::open(&log).unwrap();
    // The log is the process's from here: what a test before wrote was
    // written nowhere, and one after writes once this one has read it.

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

#[test]
fn an_answer_that_verified_no_token_starts_no_count_again() {
    let _turn = turn();
    let db = Arc::new(RwLock::new(Database::new()));
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        token: Some(ROOT.into()),
        cors: Some("*".into()),
        ..Config::default()
    };
    let port = serve(Server::new(Arc::clone(&db), cfg.clone()));
    let scrape = serve(Server::metrics_only(Arc::clone(&db), None, cfg));

    for _ in 0..2 {
        assert_eq!(ask(port, "GET", "/_schema", GUESSED, "").0, 401);
    }
    let held = would_wait(LOCAL);
    assert_eq!(held, ms(200));
    verify_nothing(
        port,
        &[
            ("GET", "/_health", "", "", 200),
            ("OPTIONS", "/query", ORIGIN, "", 204),
            ("GET", "/_studio/", "", "", 404),
            // Its method is refused before its token is looked at.
            ("POST", "/_whoami", "", "", 405),
        ],
        held,
    );
    // A listener of `/_metrics` alone answers the rest 404, unasked.
    verify_nothing(scrape, &[("GET", "/query", "", "", 404)], held);
    // A read waiting for a write is refused for its token before it
    // waits, and counted as any refusal: held first, it was answered 504
    // by the wait's end and counted nothing.
    let after = format!("{GUESSED}Fenec-After: 99\r\nFenec-Wait: 30000\r\n");
    let (got, out) = ask(port, "GET", "/notes", &after, "");
    assert_eq!(got, 401, "{out}");
    assert_eq!(would_wait(LOCAL), held * 2);
    let root = format!("Authorization: Bearer {ROOT}\r\n");
    verify(scrape, &[("GET", "/_metrics", &root, "", 200)]);

    // The server's token is verified whatever the statement then meets.
    let create = r#"{"query":"create collection notes (title text)"}"#;
    assert_eq!(ask(port, "POST", "/query", &root, create).0, 200);
    let insert = r#"{"query":"insert notes {id: 1, title: \"a\"}"}"#;
    assert_eq!(ask(port, "POST", "/query", &root, insert).0, 200);
    verify(
        port,
        &[
            ("POST", "/query", &root, r#"{"query":"get nothere"}"#, 404),
            ("POST", "/query", &root, insert, 409),
            (
                "POST",
                "/query",
                &root,
                r#"{"query":"set notes {title: \"b\"} where id = 99 require 1"}"#,
                412,
            ),
            (
                "POST",
                "/query",
                &root,
                r#"{"query":"get notes where"}"#,
                400,
            ),
            ("GET", "/_whoami", &root, "", 200),
        ],
    );
}

#[test]
fn a_server_that_asks_for_no_token_verifies_none() {
    let _turn = turn();
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        ..Config::default()
    };
    // Open to everyone, but for its replication token: that one guessed,
    // the answers to everything else verified nothing of it.
    let port = serve(
        Server::new(Arc::new(RwLock::new(Database::new())), cfg)
            .with_replication(Replication::new(Some("repl".into()), None, None)),
    );
    for _ in 0..2 {
        assert_eq!(ask(port, "GET", "/_replication/status", GUESSED, "").0, 401);
    }
    let held = would_wait(LOCAL);
    assert_eq!(held, ms(200));
    verify_nothing(
        port,
        &[
            (
                "POST",
                "/query",
                "",
                r#"{"query":"create collection notes (title text)"}"#,
                200,
            ),
            ("GET", "/notes", "", "", 200),
            ("GET", "/_schema", "", "", 200),
            ("GET", "/_whoami", "", "", 200),
            ("GET", "/_metrics", "", "", 200),
            ("GET", "/_stats/statements", "", "", 200),
            ("GET", "/_changes", "", "", 409),
        ],
        held,
    );
    verify(
        port,
        &[(
            "GET",
            "/_replication/status",
            "Authorization: Bearer repl\r\n",
            "",
            200,
        )],
    );
}

#[test]
fn a_tenant_node_starts_a_count_again_on_a_token_it_verified_alone() {
    let _turn = turn();
    let dir = scratch("node");
    let tenants = Arc::new(Tenants::new(&dir).unwrap());
    tenants.create("acme").unwrap();
    let access = Arc::new(Access::new(SECRET, "notes  read  where owner = $jwt.sub\n").unwrap());
    let port = serve(Server::with_tenants(
        tenants,
        Config {
            addr: "127.0.0.1:0".into(),
            admin_token: Some("adm".into()),
            token: Some("data".into()),
            access: Some(Arc::clone(&access)),
            cors: Some("*".into()),
            // A body past this and within the import's ceiling is read
            // whole, and refused before its token is looked at.
            max_body: 1024,
            max_import: 1 << 20,
            ..Config::default()
        },
    ));
    // A node with no admin token: `/_admin/` is off, and says so unasked.
    let bare_dir = scratch("bare");
    let bare = serve(Server::with_tenants(
        Arc::new(Tenants::new(&bare_dir).unwrap()),
        Config {
            addr: "127.0.0.1:0".into(),
            token: Some("data".into()),
            ..Config::default()
        },
    ));
    let data = "Authorization: Bearer data\r\n";
    let create = r#"{"query":"create collection notes (owner text, title text)"}"#;
    assert_eq!(ask(port, "POST", "/t/acme/query", data, create).0, 200);

    for _ in 0..2 {
        assert_eq!(ask(port, "GET", "/t/acme/notes", GUESSED, "").0, 401);
    }
    let held = would_wait(LOCAL);
    assert_eq!(held, ms(200));
    let big = format!(r#"{{"query":"get notes","pad":"{}"}}"#, "x".repeat(2048));
    verify_nothing(
        port,
        &[
            ("GET", "/_health", "", "", 200),
            ("OPTIONS", "/t/acme/notes", ORIGIN, "", 204),
            ("GET", "/nosuch", "", "", 404),
            ("GET", "/_studio/", "", "", 404),
            ("POST", "/_whoami", "", "", 405),
            ("POST", "/t/acme/query", "", &big, 413),
        ],
        held,
    );
    verify_nothing(bare, &[("GET", "/_admin/stats", "", "", 404)], held);

    let vera = access.mint(r#"{"sub":"vera","tenant":"acme"}"#).unwrap();
    let elsewhere = access.mint(r#"{"sub":"vera","tenant":"beta"}"#).unwrap();
    let (vera, elsewhere) = (
        format!("Authorization: Bearer {vera}\r\n"),
        format!("Authorization: Bearer {elsewhere}\r\n"),
    );
    let adm = "Authorization: Bearer adm\r\n";
    verify(
        port,
        &[
            ("GET", "/t/acme/nothere", data, "", 404),
            ("GET", "/t/nobody/notes", data, "", 404),
            ("GET", "/t/acme/notes", &vera, "", 200),
            // Verified, and bound to another tenant.
            ("GET", "/t/acme/notes", &elsewhere, "", 403),
            ("GET", "/_admin/stats", adm, "", 200),
            ("GET", "/_metrics", adm, "", 200),
        ],
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&bare_dir);
}
