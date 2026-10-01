//! A tenant node: `fenec-server --dir`, the real binary.
//!
//! The tenant is the path's (`/t/<tenant>/`), looked up again for every
//! request -- so a tenant created after a connection opened is there for
//! it, one frozen for a move refuses writes while its reads go on, and one
//! deleted meanwhile is gone.

#[path = "support.rs"]
mod support;

use std::io::Write;
use std::net::TcpStream;
use std::time::{Duration, Instant};
use support::{column, count, start, tmp, Http, Server};

const ADMIN: &str = "admin-token";

struct Node {
    server: Server,
    dir: std::path::PathBuf,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.server.child.kill();
        let _ = self.server.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn node(name: &str) -> Node {
    let dir = tmp("tenants", name);
    std::fs::create_dir_all(&dir).unwrap();
    let server = start(&["--admin-token", ADMIN, "--dir", dir.to_str().unwrap()]);
    Node { server, dir }
}

impl Node {
    fn http(&self) -> Http {
        self.server.http()
    }

    /// An `/_admin/` request: create, freeze, thaw or delete a tenant.
    fn admin(&self, method: &str, path: &str) -> u16 {
        self.http().with_token(ADMIN).ask(method, path, "").status
    }
}

/// `prefix`'s statement, which must be answered.
fn run(c: &mut Http, prefix: &str, q: &str) -> String {
    c.query_at(prefix, q)
        .unwrap_or_else(|e| panic!("{prefix} {q}: {e:?}"))
}

#[test]
fn the_path_names_the_tenant() {
    let n = node("two");
    assert_eq!(n.admin("PUT", "/_admin/tenants/acme"), 201);
    assert_eq!(n.admin("PUT", "/_admin/tenants/beta"), 201);

    let mut c = n.http();
    run(&mut c, "/t/acme", "create collection notes (title text)");
    run(&mut c, "/t/acme", r#"put notes {title: "acme's"}"#);
    run(&mut c, "/t/beta", "create collection notes (title text)");
    run(&mut c, "/t/beta", r#"put notes {title: "beta's"}"#);

    // One file each: the same collection name, the same ids, other rows.
    let title = |c: &mut Http, t: &str| column(&run(c, t, "get notes select title"), "title");
    assert_eq!(title(&mut c, "/t/acme"), ["acme's"]);
    assert_eq!(title(&mut c, "/t/beta"), ["beta's"]);

    // A tenant this node does not have is a 404, and a name that is no
    // tenant's is refused before it reaches a path.
    let (status, body) = c.query_at("/t/nobody", "collections").unwrap_err();
    assert_eq!(status, 404, "{body}");
    assert!(body.contains("nobody"), "{body}");
    let (status, _) = c.query_at("/t/..", "collections").unwrap_err();
    assert!(status >= 400);
}

#[test]
fn a_tenant_created_after_the_connection_is_there_for_it() {
    // The registry is asked again for every request, so a keep-alive
    // connection outlives a tenant being created, closed as idle, or
    // deleted.
    let n = node("later");
    assert_eq!(n.admin("PUT", "/_admin/tenants/acme"), 201);
    let mut c = n.http();
    run(&mut c, "/t/acme", "create collection notes (title text)");

    assert_eq!(n.admin("DELETE", "/_admin/tenants/acme"), 204);
    let (status, body) = c.query_at("/t/acme", "get notes count").unwrap_err();
    assert_eq!(status, 404, "{body}");

    assert_eq!(n.admin("PUT", "/_admin/tenants/acme"), 201);
    run(&mut c, "/t/acme", "create collection notes (title text)");
    assert_eq!(count(&run(&mut c, "/t/acme", "get notes count")), 0);
}

#[test]
fn a_frozen_tenant_refuses_writes_and_answers_reads() {
    let n = node("frozen");
    assert_eq!(n.admin("PUT", "/_admin/tenants/acme"), 201);
    let mut c = n.http();
    run(&mut c, "/t/acme", "create collection notes (title text)");
    run(&mut c, "/t/acme", r#"put notes {title: "before"}"#);

    assert_eq!(n.admin("POST", "/_admin/tenants/acme/freeze"), 200);
    for q in [
        r#"put notes {title: "during"}"#,
        "create index on notes (title) @sorted",
    ] {
        let (status, body) = c.query_at("/t/acme", q).unwrap_err();
        assert!(status >= 400, "{q}: {status}");
        assert!(body.contains("moved"), "{q}: {body}");
    }
    assert_eq!(
        column(&run(&mut c, "/t/acme", "get notes select title"), "title"),
        ["before"]
    );

    assert_eq!(n.admin("POST", "/_admin/tenants/acme/thaw"), 200);
    run(&mut c, "/t/acme", r#"put notes {title: "after"}"#);
    assert_eq!(count(&run(&mut c, "/t/acme", "get notes count")), 2);
}

/// A client that stops reading a large answer does not hold its tenant:
/// the request lets go of it before writing. The socket has no write
/// timeout, so a tenant held through the write would keep a freeze waiting
/// for as long as the client did not read, and every request for the
/// tenant queued behind the freeze.
#[test]
fn a_client_that_stops_reading_does_not_hold_the_tenant() {
    let n = node("stalled");
    assert_eq!(n.admin("PUT", "/_admin/tenants/acme"), 201);
    let mut c = n.http();
    run(&mut c, "/t/acme", "create collection notes (body text)");
    // Some 4 MB of rows: more than the socket buffers between us hold.
    let body = "x".repeat(1_000);
    let batch: Vec<String> = (0..200).map(|_| format!("{{body: \"{body}\"}}")).collect();
    for _ in 0..20 {
        run(
            &mut c,
            "/t/acme",
            &format!("put notes [{}]", batch.join(", ")),
        );
    }
    let mut stalled = TcpStream::connect(("127.0.0.1", n.server.port)).unwrap();
    stalled
        .write_all(b"GET /t/acme/notes HTTP/1.1\r\nHost: t\r\n\r\n")
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));

    let t = Instant::now();
    assert_eq!(n.admin("POST", "/_admin/tenants/acme/freeze"), 200);
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    assert_eq!(n.admin("POST", "/_admin/tenants/acme/thaw"), 200);
    assert_eq!(count(&run(&mut c, "/t/acme", "get notes count")), 4000);
    drop(stalled);
}

/// A tenant's statements are its own: `/t/<tenant>/_stats/statements`
/// holds them alone -- their text names the tenant's collections -- and the
/// node's `/_stats/statements`, every tenant's with its name, is the
/// admin's alone.
#[test]
fn a_tenant_sees_its_own_statements() {
    let n = node("stats");
    assert_eq!(n.admin("PUT", "/_admin/tenants/acme"), 201);
    assert_eq!(n.admin("PUT", "/_admin/tenants/beta"), 201);
    let mut c = n.http();
    run(
        &mut c,
        "/t/acme",
        "create collection acme_notes (title text)",
    );
    run(
        &mut c,
        "/t/beta",
        "create collection beta_notes (title text)",
    );
    run(&mut c, "/t/beta", r#"put beta_notes {title: "x"}"#);

    let a = n.http().ask("GET", "/t/beta/_stats/statements", "");
    assert_eq!(a.status, 200, "{}", a.body);
    assert!(
        a.body.contains("beta_notes") && !a.body.contains("acme_notes"),
        "{}",
        a.body
    );

    // The node's own lists every tenant's, named, to its admin alone.
    assert_eq!(n.http().ask("GET", "/_stats/statements", "").status, 401);
    let a = n
        .http()
        .with_token(ADMIN)
        .ask("GET", "/_stats/statements", "");
    assert_eq!(a.status, 200, "{}", a.body);
    assert!(
        a.body.contains("\"tenant\":\"acme\"") && a.body.contains("\"tenant\":\"beta\""),
        "{}",
        a.body
    );
}
