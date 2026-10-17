//! A request refused for its token is refused at once: nothing it asks to
//! wait for -- a write to come (`Fenec-After`, on a read, a subscription or
//! `/_changes`), a claim held until it can write (`Fenec-Wait`),
//! `/_changes`'s `wait`, a subscription, the MCP endpoint -- is waited for
//! before its token is taken, on a single
//! database or under a tenant's prefix, where the tenant its token names is
//! asked first too. On a single database a `Fenec-After` wait ran before
//! the token was looked at: a guessed one held a connection thread for up
//! to 30 s and was told by the 504 where the database stood.
//!
//! Each refusal here is asked for with a wait of 30 s for a write that is
//! not made until every refusal is in: a refusal that waited would be
//! answered by the wait's end, as the wait answers (504, the database's
//! change), never 401 -- so the status is the test, with no bound on the
//! time. A good token's request still waits, which its wait's own answer
//! shows: 504 comes from the wait alone, and a held claim is counted held.

use fenec_core::engine::Database;
use fenec_http::access::Access;
use fenec_http::replication::{self, Replication};
use fenec_http::tenants::Tenants;
use fenec_http::{Config, Server};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const SECRET: &[u8] = b"thirty-two bytes and a few more, for HS256";
const ROOT: &str = "root-token";
const POLICY: &str = "notes  read,write  where owner = $jwt.sub\njobs  read,write\n";
/// The longest wait a request may ask, for a write that does not come
/// until the refusals are in.
const LONGEST: &str = "30000";
const CLAIM: &str = r#"{"query":"set jobs {owner: \"w\", run_at: now() + 60000} where run_at <= now() order run_at limit 1 returning id"}"#;
const MCP_CALL: &str = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;

/// A request's method, target, headers and body.
type Case<'a> = (&'a str, &'a str, &'a [(&'a str, &'a str)], String);

struct Answer {
    status: u16,
    head: String,
    body: String,
}

/// A request and its whole answer. The read timeout is past the longest
/// wait, so a request that waited is answered by its wait, not cut.
fn ask(
    port: u16,
    token: Option<&str>,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Answer {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let mut head = format!("{method} {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n");
    if let Some(t) = token {
        head.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    s.write_all(head.as_bytes()).unwrap();
    // Read to the end of the body its head says, not to the connection's:
    // a stream served where an answer was meant has no end, its keep-alive
    // lines coming faster than any read timeout, and the test would hang
    // rather than fail. A stream's head is its answer here.
    let mut out = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let text = String::from_utf8_lossy(&out);
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let length = head.lines().find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse::<usize>().ok())?
            });
            if length.is_none_or(|n| body.len() >= n) {
                break;
            }
        }
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => out.extend_from_slice(&chunk[..n]),
        }
    }
    let out = String::from_utf8_lossy(&out);
    let (head, body) = out.split_once("\r\n\r\n").unwrap_or((&out, ""));
    Answer {
        status: head.get(9..12).and_then(|s| s.parse().ok()).unwrap_or(0),
        head: head.to_string(),
        body: body.to_string(),
    }
}

fn query(text: &str) -> String {
    let mut body = String::from("{\"query\":");
    fenec_core::json::escape_into(&mut body, text);
    body.push('}');
    body
}

struct Single {
    port: u16,
    server: Arc<Server>,
    db: Arc<RwLock<Database>>,
    access: Arc<Access>,
}

/// A database over a file, keeping a change feed for `/_changes`, asking
/// the server's token or a JSON Web Token, with the MCP endpoint on.
fn single(name: &str) -> Single {
    fenec_http::audit::set_delay(0);
    let dir = std::env::temp_dir().join(format!(
        "fenec-token-first-{name}-{}-{:?}",
        std::process::id(),
        Instant::now()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("data.fenec");
    let (db, feed) =
        replication::open(path.to_str().unwrap(), replication::DEFAULT_BUFFER).unwrap();
    let db = Arc::new(RwLock::new(db));
    let access = Arc::new(Access::new(SECRET, POLICY).unwrap());
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        // On disk before it is answered: the feed hands over only that.
        sync_on_write: true,
        token: Some(ROOT.into()),
        access: Some(Arc::clone(&access)),
        mcp: Some(Arc::new(fenec_http::mcp::Options::default())),
        ..Config::default()
    };
    let server = Arc::new(
        Server::new(Arc::clone(&db), cfg).with_replication(Replication::new(
            None,
            Some(feed),
            None,
        )),
    );
    let listener = server.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    let s = Arc::clone(&server);
    std::thread::spawn(move || {
        let _ = s.serve_on(listener);
    });
    let n = Single {
        port,
        server,
        db,
        access,
    };
    for text in [
        "create collection notes (owner text, title text)",
        "create collection jobs (run_at timestamp @sorted, owner text)",
    ] {
        n.run(text);
    }
    n
}

impl Single {
    fn ask(
        &self,
        token: Option<&str>,
        method: &str,
        target: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Answer {
        ask(self.port, token, method, target, headers, body)
    }

    fn run(&self, text: &str) {
        let a = self.ask(Some(ROOT), "POST", "/query", &[], &query(text));
        assert_eq!(a.status, 200, "{text}: {}", a.body);
    }

    fn seq(&self) -> u64 {
        self.db.read().unwrap().change_seq()
    }

    /// Tokens the server refuses: none, a guess, a JSON Web Token's shape
    /// with no signature it made, and one it made that has expired.
    fn refused(&self) -> Vec<Option<String>> {
        vec![
            None,
            Some("guessed".into()),
            Some("a.b.c".into()),
            Some(self.access.mint(r#"{"sub":"alice","exp":1}"#).unwrap()),
        ]
    }
}

/// A 401 telling nothing of where the database stands.
fn refused(a: &Answer, what: &str) {
    assert_eq!(a.status, 401, "{what}: {} {}", a.head, a.body);
    assert!(!a.body.contains("\"seq\""), "{what}: {}", a.body);
    assert!(!a.head.contains("Fenec-Seq"), "{what}: {}", a.head);
}

#[test]
fn a_read_after_a_write_to_come_waits_only_once_its_token_is_taken() {
    let n = single("after");
    let seq = n.seq();
    let next = (seq + 1).to_string();
    let after = [("Fenec-After", next.as_str()), ("Fenec-Wait", LONGEST)];
    for token in n.refused() {
        for (method, target, body) in [
            ("POST", "/query", query("get notes")),
            ("POST", "/batch", query("get notes")),
            ("GET", "/notes", String::new()),
            ("GET", "/_schema", String::new()),
            ("POST", "/_mcp", MCP_CALL.to_string()),
        ] {
            let a = n.ask(token.as_deref(), method, target, &after, &body);
            refused(&a, &format!("{method} {target} with {token:?}"));
        }
    }
    // A page on another origin is refused by the MCP endpoint before its
    // token is asked, a good one or not: before the wait too.
    let a = n.ask(
        Some(ROOT),
        "POST",
        "/_mcp",
        &[("Origin", "https://elsewhere.example"), after[0], after[1]],
        MCP_CALL,
    );
    assert_eq!(a.status, 403, "{}", a.body);
    assert!(!a.body.contains("\"seq\""), "{}", a.body);

    // Only now is the write made that would have ended their waits.
    assert_eq!(n.seq(), seq, "a write was made before the refusals were in");
    n.run(r#"put notes {owner: "alice", title: "first"}"#);
    assert_eq!(n.seq(), seq + 1);

    // A good token waits as before: past the write, answered with it, and
    // for one that has not come, answered by its wait with where the
    // database stands -- the server's token, a scoped one, the MCP
    // endpoint's caller.
    let a = n.ask(Some(ROOT), "POST", "/query", &after, &query("get notes"));
    assert_eq!(a.status, 200, "{}", a.body);
    assert!(a.body.contains("first"), "{}", a.body);
    let far = (seq + 1000).to_string();
    let soon = [("Fenec-After", far.as_str()), ("Fenec-Wait", "100")];
    let alice = n.access.mint(r#"{"sub":"alice"}"#).unwrap();
    for (token, target, body) in [
        (ROOT, "/query", query("get notes")),
        (alice.as_str(), "/query", query("get notes")),
        (alice.as_str(), "/notes", String::new()),
        (ROOT, "/_mcp", MCP_CALL.to_string()),
    ] {
        let method = if target == "/notes" { "GET" } else { "POST" };
        let a = n.ask(Some(token), method, target, &soon, &body);
        assert_eq!(a.status, 504, "{target}: {}", a.body);
        assert!(
            a.body.contains(&format!("\"seq\":{}", seq + 1)),
            "{}",
            a.body
        );
    }
}

#[test]
fn a_claim_is_held_only_once_its_token_is_taken() {
    let n = single("claim");
    let wait = [("Fenec-Wait", LONGEST)];
    for token in n.refused() {
        for target in ["/query", "/batch"] {
            let a = n.ask(token.as_deref(), "POST", target, &wait, CLAIM);
            refused(&a, &format!("{target} with {token:?}"));
        }
    }
    // Not one of them was held, nor looked for a job.
    let w = n.server.waits();
    assert_eq!((w.held, w.looks, w.runs), (0, 0, 0), "{w:?}");

    // A good token's claim is held, and takes the job put after it.
    let port = n.port;
    let claim = std::thread::spawn(move || ask(port, Some(ROOT), "POST", "/query", &wait, CLAIM));
    let t = Instant::now();
    while n.server.waits().held < 1 {
        assert!(t.elapsed() < Duration::from_secs(20), "never held");
        std::thread::sleep(Duration::from_millis(1));
    }
    n.run("put jobs {run_at: 1}");
    let a = claim.join().unwrap();
    assert_eq!(a.status, 200, "{}", a.body);
    assert_eq!(a.body, r#"[{"id":1}]"#);
}

#[test]
fn a_wait_for_changes_or_a_subscription_is_refused_at_once_without_its_token() {
    let n = single("changes");
    let since = n.seq();
    let target = format!("/_changes?since={since}&wait={LONGEST}");
    // Each with and without a write to come (`Fenec-After`), which a
    // stream and `/_changes` wait for as a read does: past the token alone.
    let next = (since + 1).to_string();
    let after = [("Fenec-After", next.as_str()), ("Fenec-Wait", LONGEST)];
    let both: [&[(&str, &str)]; 2] = [&[], &after];
    for token in n.refused() {
        for headers in both {
            let a = n.ask(token.as_deref(), "GET", &target, headers, "");
            refused(&a, &format!("/_changes with {token:?}, {headers:?}"));
            // A subscription is refused before its head, and the
            // connection ends with the refusal.
            let a = n.ask(token.as_deref(), "GET", "/notes/changes", headers, "");
            refused(&a, &format!("a subscription with {token:?}, {headers:?}"));
        }
    }
    // A scoped token reads no change stream, and is told so at once.
    let alice = n.access.mint(r#"{"sub":"alice"}"#).unwrap();
    for headers in both {
        let a = n.ask(Some(&alice), "GET", &target, headers, "");
        assert_eq!(a.status, 403, "{}", a.body);
        assert!(!a.body.contains("\"seq\""), "{}", a.body);
    }

    assert_eq!(
        n.seq(),
        since,
        "a write was made before the refusals were in"
    );
    n.run(r#"put notes {owner: "alice", title: "first"}"#);
    for headers in both {
        let a = n.ask(Some(ROOT), "GET", &target, headers, "");
        assert_eq!(a.status, 200, "{}", a.body);
        assert!(a.body.contains("first"), "{}", a.body);
    }
    // A good token's stream and page wait for a write that has not come,
    // and are answered by the wait: 504, and where the database stands.
    let far = (since + 1000).to_string();
    let soon = [("Fenec-After", far.as_str()), ("Fenec-Wait", "100")];
    for target in [target.as_str(), "/notes/changes"] {
        let a = n.ask(Some(ROOT), "GET", target, &soon, "");
        assert_eq!(a.status, 504, "{target}: {}", a.body);
        assert!(
            a.body.contains(&format!("\"seq\":{}", since + 1)),
            "{target}: {}",
            a.body
        );
    }
}

#[test]
fn a_tenant_s_waits_come_after_its_token_and_its_tenant() {
    fenec_http::audit::set_delay(0);
    let dir = std::env::temp_dir().join(format!(
        "fenec-token-first-tenants-{}-{:?}",
        std::process::id(),
        Instant::now()
    ));
    let tenants = Arc::new(Tenants::new(&dir).unwrap());
    for name in ["acme", "globex"] {
        tenants.create(name).unwrap();
    }
    let access = Arc::new(Access::new(SECRET, POLICY).unwrap());
    let server = Server::with_tenants(
        Arc::clone(&tenants),
        Config {
            addr: "127.0.0.1:0".into(),
            token: Some(ROOT.into()),
            access: Some(Arc::clone(&access)),
            admin_token: Some("adm".into()),
            mcp: Some(Arc::new(fenec_http::mcp::Options::default())),
            ..Config::default()
        },
    );
    let listener = server.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    for text in [
        "create collection notes (owner text, title text)",
        "create collection jobs (run_at timestamp @sorted, owner text)",
    ] {
        let a = ask(port, Some(ROOT), "POST", "/t/acme/query", &[], &query(text));
        assert_eq!(a.status, 200, "{}", a.body);
    }
    let acme = tenants.get("acme").unwrap();
    let seq = acme.db.read().unwrap().change_seq();
    let next = (seq + 1).to_string();
    let after = [("Fenec-After", next.as_str()), ("Fenec-Wait", LONGEST)];
    let wait = [("Fenec-Wait", LONGEST)];
    let changes = format!("/t/acme/_changes?since={seq}&wait={LONGEST}");
    let globex = access.mint(r#"{"sub":"alice","tenant":"globex"}"#).unwrap();
    let cases: [(Option<&str>, u16); 5] = [
        (None, 401),
        (Some("guessed"), 401),
        (Some("a.b.c"), 401),
        // A good token, for another tenant.
        (Some(globex.as_str()), 403),
        // A good token for this one, from a page on another origin, at
        // the MCP endpoint alone (below).
        (Some(ROOT), 403),
    ];
    for (token, status) in cases {
        let mcp_only = token == Some(ROOT);
        let origin = [("Origin", "https://elsewhere.example"), after[0], after[1]];
        let requests: Vec<Case> = match mcp_only {
            true => vec![("POST", "/t/acme/_mcp", &origin, MCP_CALL.to_string())],
            false => vec![
                ("POST", "/t/acme/query", &after, query("get notes")),
                ("GET", "/t/acme/notes", &after, String::new()),
                ("POST", "/t/acme/query", &wait, CLAIM.to_string()),
                ("POST", "/t/acme/batch", &wait, CLAIM.to_string()),
                ("GET", &changes, &[], String::new()),
                ("GET", &changes, &after, String::new()),
                ("GET", "/t/acme/notes/changes", &[], String::new()),
                ("GET", "/t/acme/notes/changes", &after, String::new()),
                ("POST", "/t/acme/_mcp", &after, MCP_CALL.to_string()),
            ],
        };
        for (method, target, headers, body) in requests {
            let a = ask(port, token, method, target, headers, &body);
            assert_eq!(
                a.status, status,
                "{method} {target} with {token:?}: {}",
                a.body
            );
            assert!(!a.body.contains("\"seq\""), "{}", a.body);
            assert!(!a.head.contains("Fenec-Seq"), "{}", a.head);
        }
    }
    // A tenant that does not exist is no different to a guessed token.
    let a = ask(
        port,
        Some("guessed"),
        "POST",
        "/t/nosuch/query",
        &after,
        &query("get notes"),
    );
    assert_eq!(a.status, 401, "{}", a.body);
    let w = acme.hub.waits().stats();
    assert_eq!((w.held, w.looks, w.runs), (0, 0, 0), "{w:?}");

    // Only now is the write made that would have ended their waits; a
    // token for the tenant waits as before.
    assert_eq!(acme.db.read().unwrap().change_seq(), seq);
    let a = ask(
        port,
        Some(ROOT),
        "POST",
        "/t/acme/query",
        &[],
        &query(r#"put notes {owner: "alice", title: "first"}"#),
    );
    assert_eq!(a.status, 200, "{}", a.body);
    let alice = access.mint(r#"{"sub":"alice","tenant":"acme"}"#).unwrap();
    let a = ask(
        port,
        Some(&alice),
        "POST",
        "/t/acme/query",
        &after,
        &query("get notes"),
    );
    assert_eq!(a.status, 200, "{}", a.body);
    assert!(a.body.contains("first"), "{}", a.body);
    let far = (seq + 1000).to_string();
    let soon = [("Fenec-After", far.as_str()), ("Fenec-Wait", "100")];
    let a = ask(
        port,
        Some(&alice),
        "POST",
        "/t/acme/query",
        &soon,
        &query("get notes"),
    );
    assert_eq!(a.status, 504, "{}", a.body);
    assert!(
        a.body.contains(&format!("\"seq\":{}", seq + 1)),
        "{}",
        a.body
    );
    drop(acme);
    let _ = std::fs::remove_dir_all(&dir);
}
