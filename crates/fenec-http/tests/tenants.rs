//! Tenant mode: one file per tenant under `/t/<tenant>/`, and `/_admin/`.
//!
//! The server runs in-process over a scratch directory; the client is raw
//! TCP, as in the other suites.

use fenec_http::tenants::Tenants;
use fenec_http::{Config, Server};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const ADMIN: &str = "Bearer adm";

struct Node {
    port: u16,
    tenants: Arc<Tenants>,
    dir: PathBuf,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "fenec-tenants-{tag}-{}-{:?}",
        std::process::id(),
        Instant::now()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn start(tag: &str, cfg: Config) -> Node {
    start_with(tag, cfg, |t| t)
}

fn start_with(tag: &str, mut cfg: Config, build: impl FnOnce(Tenants) -> Tenants) -> Node {
    let dir = scratch(tag);
    let tenants = Arc::new(build(Tenants::new(&dir).expect("dir")));
    cfg.addr = "127.0.0.1:0".into();
    cfg.stream_keepalive = Duration::from_millis(80);
    let server = Server::with_tenants(Arc::clone(&tenants), cfg);
    let listener = server.bind().expect("bind");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    Node { port, tenants, dir }
}

fn admin_cfg() -> Config {
    Config {
        admin_token: Some("adm".into()),
        ..Config::default()
    }
}

struct Res {
    status: u16,
    head: String,
    body: Vec<u8>,
}

impl Res {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

fn call(port: u16, method: &str, target: &str, body: &[u8], auth: Option<&str>) -> Res {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut req = format!(
        "{method} {target} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(a) = auth {
        req.push_str(&format!("Authorization: {a}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).unwrap();
    s.write_all(body).unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let split = out
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response head");
    let head = String::from_utf8_lossy(&out[..split]).into_owned();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    Res {
        status,
        head,
        body: out[split + 4..].to_vec(),
    }
}

fn query(port: u16, tenant: &str, q: &str) -> Res {
    let mut body = String::from("{\"query\":");
    fenec_core::json::escape_into(&mut body, q);
    body.push('}');
    call(
        port,
        "POST",
        &format!("/t/{tenant}/query"),
        body.as_bytes(),
        None,
    )
}

fn create(port: u16, tenant: &str) {
    let r = call(
        port,
        "PUT",
        &format!("/_admin/tenants/{tenant}"),
        b"",
        Some(ADMIN),
    );
    assert_eq!(r.status, 201, "{}", r.text());
}

#[test]
fn tenants_do_not_see_each_other() {
    let n = start("isolation", admin_cfg());
    create(n.port, "acme");
    create(n.port, "beta");
    for t in ["acme", "beta"] {
        let r = query(n.port, t, "create collection notes (title text)");
        assert_eq!(r.status, 200, "{}", r.text());
    }
    let r = call(
        n.port,
        "POST",
        "/t/acme/notes",
        br#"{"title":"acme only"}"#,
        None,
    );
    assert_eq!(r.status, 201, "{}", r.text());

    let acme = call(n.port, "GET", "/t/acme/notes", b"", None);
    assert_eq!(acme.text(), r#"[{"id":1,"title":"acme only"}]"#);
    let beta = call(n.port, "GET", "/t/beta/notes", b"", None);
    assert_eq!(beta.text(), "[]");

    // Empty segments do not shift what the prefix is taken to be.
    let again = call(n.port, "GET", "//t//acme//notes", b"", None);
    assert_eq!(again.text(), acme.text());

    // Ids are counted per file: beta's first document is 1 as well.
    call(
        n.port,
        "POST",
        "/t/beta/notes",
        br#"{"title":"beta"}"#,
        None,
    );
    let beta = call(n.port, "GET", "/t/beta/notes", b"", None);
    assert_eq!(beta.text(), r#"[{"id":1,"title":"beta"}]"#);
}

#[test]
fn the_metrics_are_the_nodes() {
    let n = start("metrics", admin_cfg());
    create(n.port, "acme");
    create(n.port, "beta");
    let r = query(n.port, "acme", "create collection notes (title text)");
    assert_eq!(r.status, 200, "{}", r.text());
    // The node has an admin token, so a scrape needs it.
    assert_eq!(call(n.port, "GET", "/_metrics", b"", None).status, 401);
    let r = call(n.port, "GET", "/_metrics", b"", Some(ADMIN));
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(r.head.contains("text/plain; version=0.0.4"), "{}", r.head);
    let text = r.text();
    assert!(text.contains("\nfenec_tenants 2\n"), "{text}");
    // Creating a tenant opens it.
    assert!(text.contains("\nfenec_tenants_open 2\n"), "{text}");
    // Per tenant it counts, not what a tenant holds: its collections are
    // not the node's to publish.
    assert!(!text.contains("notes"), "{text}");
}

#[test]
fn unknown_and_invalid_tenants_are_refused() {
    let n = start("refusals", admin_cfg());
    assert_eq!(call(n.port, "GET", "/t/nope/x", b"", None).status, 404);
    assert_eq!(call(n.port, "GET", "/t/Bad/x", b"", None).status, 400);
    assert_eq!(call(n.port, "GET", "/t/../x", b"", None).status, 400);
    assert_eq!(call(n.port, "GET", "/notes", b"", None).status, 404);
    // A request never brings a tenant into being.
    assert!(!n.dir.join("nope.fenec").exists());
}

#[test]
fn the_data_token_is_checked_before_the_tenant_is_looked_up() {
    let cfg = Config {
        token: Some("data".into()),
        ..admin_cfg()
    };
    let n = start("token", cfg);
    create(n.port, "acme");
    // Same status for a tenant that exists and one that does not: the
    // answer must not tell an unauthenticated caller which names are taken.
    assert_eq!(call(n.port, "GET", "/t/acme/", b"", None).status, 401);
    assert_eq!(call(n.port, "GET", "/t/nope/", b"", None).status, 401);
    assert_eq!(
        call(n.port, "GET", "/t/acme/", b"", Some("Bearer data")).status,
        200
    );
    // The data token is not an admin token.
    let r = call(n.port, "PUT", "/_admin/tenants/x", b"", Some("Bearer data"));
    assert_eq!(r.status, 401);
}

#[test]
fn admin_is_off_without_its_token() {
    let n = start("no-admin", Config::default());
    let r = call(n.port, "PUT", "/_admin/tenants/acme", b"", Some(ADMIN));
    assert_eq!(r.status, 404);
    assert!(n.tenants.names().is_empty());
}

#[test]
fn a_frozen_tenant_refuses_writes_with_retry_after_and_keeps_reading() {
    let n = start("freeze", admin_cfg());
    create(n.port, "acme");
    query(n.port, "acme", "create collection notes (title text)");
    call(n.port, "POST", "/t/acme/notes", br#"{"title":"a"}"#, None);

    let r = call(
        n.port,
        "POST",
        "/_admin/tenants/acme/freeze",
        b"",
        Some(ADMIN),
    );
    assert_eq!(r.status, 200, "{}", r.text());

    for (method, target, body) in [
        ("POST", "/t/acme/notes", &br#"{"title":"b"}"#[..]),
        ("PATCH", "/t/acme/notes/all", &br#"{"title":"c"}"#[..]),
        ("POST", "/t/acme/query", &br#"{"query":"del notes"}"#[..]),
        (
            "POST",
            "/t/acme/batch",
            &br#"{"query":"put notes {title: \"d\"}"}"#[..],
        ),
    ] {
        let r = call(n.port, method, target, body, None);
        assert_eq!(r.status, 503, "{method} {target}: {}", r.text());
        assert!(r.head.contains("Retry-After: 1"), "{}", r.head);
    }
    // Reads, including a read through /query, still work.
    let r = query(n.port, "acme", "get notes");
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(r.text().contains("\"a\""));

    call(
        n.port,
        "POST",
        "/_admin/tenants/acme/thaw",
        b"",
        Some(ADMIN),
    );
    let r = call(n.port, "POST", "/t/acme/notes", br#"{"title":"b"}"#, None);
    assert_eq!(r.status, 201, "{}", r.text());
}

#[test]
fn an_exported_image_imports_as_a_working_tenant() {
    let n = start("image", admin_cfg());
    create(n.port, "acme");
    query(
        n.port,
        "acme",
        "create collection docs (title text, v vector<2> @hnsw(cosine))",
    );
    query(
        n.port,
        "acme",
        r#"put docs [{title: "x", v: [1, 0]}, {title: "y", v: [0, 1]}]"#,
    );

    let image = call(n.port, "GET", "/_admin/tenants/acme/file", b"", Some(ADMIN));
    assert_eq!(image.status, 200);
    assert!(image.head.contains("application/octet-stream"));

    let r = call(
        n.port,
        "PUT",
        "/_admin/tenants/copy/file",
        &image.body,
        Some(ADMIN),
    );
    assert_eq!(r.status, 201, "{}", r.text());
    let r = call(
        n.port,
        "POST",
        "/t/copy/docs/near",
        br#"{"vector":[0,1],"limit":1,"select":["title"]}"#,
        None,
    );
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(r.text().contains("\"y\""), "{}", r.text());

    // A name that is taken is not overwritten, and a truncated image never
    // becomes a tenant.
    let r = call(
        n.port,
        "PUT",
        "/_admin/tenants/copy/file",
        &image.body,
        Some(ADMIN),
    );
    assert_eq!(r.status, 409);
    let cut = &image.body[..image.body.len() / 2];
    let r = call(n.port, "PUT", "/_admin/tenants/cut/file", cut, Some(ADMIN));
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(!n.dir.join("cut.fenec").exists());
    assert!(!n.dir.join("cut.fenec.importing").exists());
}

#[test]
fn each_tenant_has_its_own_change_sequence() {
    let n = start("changes", admin_cfg());
    create(n.port, "acme");
    create(n.port, "beta");
    for t in ["acme", "beta"] {
        query(n.port, t, "create collection notes (title text)");
    }
    for i in 0..5 {
        let body = format!(r#"{{"title":"{i}"}}"#);
        call(n.port, "POST", "/t/acme/notes", body.as_bytes(), None);
    }
    call(n.port, "POST", "/t/beta/notes", br#"{"title":"b"}"#, None);

    let seed = |t: &str| -> String {
        let mut s = TcpStream::connect(("127.0.0.1", n.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        s.write_all(format!("GET /t/{t}/notes/changes HTTP/1.1\r\nHost: t\r\n\r\n").as_bytes())
            .unwrap();
        let mut buf = String::new();
        let mut chunk = [0u8; 4096];
        while !buf.contains("event: seed") || !buf.ends_with("\n\n") {
            let k = s.read(&mut chunk).unwrap();
            buf.push_str(&String::from_utf8_lossy(&chunk[..k]));
        }
        buf
    };
    let acme = seed("acme");
    let beta = seed("beta");
    // The schema mark plus five writes on one side, plus one on the other:
    // the counters are the tenants' own, not a node-wide clock (which
    // would read 8 on both).
    assert!(acme.contains("\"seq\":6"), "{acme}");
    assert!(beta.contains("\"seq\":2"), "{beta}");
}

#[test]
fn deleting_a_tenant_ends_its_streams_and_removes_the_file() {
    let n = start("delete", admin_cfg());
    create(n.port, "acme");
    query(n.port, "acme", "create collection notes (title text)");

    let mut s = TcpStream::connect(("127.0.0.1", n.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(b"GET /t/acme/notes/changes HTTP/1.1\r\nHost: t\r\n\r\n")
        .unwrap();
    let mut buf = String::new();
    let mut chunk = [0u8; 4096];
    while !buf.contains("event: seed") {
        let k = s.read(&mut chunk).unwrap();
        buf.push_str(&String::from_utf8_lossy(&chunk[..k]));
    }

    let r = call(n.port, "DELETE", "/_admin/tenants/acme", b"", Some(ADMIN));
    assert_eq!(r.status, 204, "{}", r.text());
    assert!(!n.dir.join("acme.fenec").exists());

    // The stream says why it ends, then closes.
    let mut rest = String::new();
    let _ = s.read_to_string(&mut rest);
    assert!(rest.contains("event: error"), "{rest}");
    assert_eq!(call(n.port, "GET", "/t/acme/notes", b"", None).status, 404);
}

#[test]
fn an_idle_tenant_closes_and_reopens_with_its_data() {
    let n = start("idle", admin_cfg());
    create(n.port, "acme");
    query(n.port, "acme", "create collection notes (title text)");
    call(
        n.port,
        "POST",
        "/t/acme/notes",
        br#"{"title":"kept"}"#,
        None,
    );

    assert_eq!(n.tenants.close_idle(Duration::ZERO), 1);
    assert!(n.tenants.stats().open.is_empty());

    let r = call(n.port, "GET", "/t/acme/notes", b"", None);
    assert_eq!(r.text(), r#"[{"id":1,"title":"kept"}]"#);
}

#[test]
fn a_tenant_in_use_is_not_closed() {
    let n = start("busy", admin_cfg());
    create(n.port, "acme");
    let held = n.tenants.get("acme").unwrap();
    assert_eq!(n.tenants.close_idle(Duration::ZERO), 0);
    drop(held);
    assert_eq!(n.tenants.close_idle(Duration::ZERO), 1);
}

#[test]
fn over_the_memory_ceiling_idle_tenants_make_room() {
    let n = start_with("memory", admin_cfg(), |t| t.with_max_memory(1));
    create(n.port, "a");
    query(n.port, "a", "create collection notes (title text)");
    call(n.port, "POST", "/t/a/notes", br#"{"title":"x"}"#, None);
    create(n.port, "b");
    // Opening `b` over a one-byte ceiling closed `a`, which nothing held.
    let open: Vec<String> = n.tenants.stats().open.into_iter().map(|o| o.0).collect();
    assert_eq!(open, ["b"]);
    // And `a` comes back intact on the next request.
    let r = call(n.port, "GET", "/t/a/notes", b"", None);
    assert_eq!(r.text(), r#"[{"id":1,"title":"x"}]"#);
}
