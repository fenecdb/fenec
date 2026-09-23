//! A tenant node over the pg wire: `fenec-pg --dir` with `--listen`.
//!
//! The database in the startup packet is the tenant, and it is looked up
//! again for every statement -- so a tenant created after a session opened is
//! there for it, one frozen for a move refuses writes while its reads go on,
//! and one deleted meanwhile is gone. The node is the real binary; the
//! client is the one `fenec import` connects to PostgreSQL with.

use fenec_pg::client::{Client, Url};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const ADMIN: &str = "admin-token";

struct Node {
    child: Child,
    pg: u16,
    http: u16,
    dir: std::path::PathBuf,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start(name: &str) -> Node {
    let dir = std::env::temp_dir().join(format!("fenecpg-tenants-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_fenec-pg"))
        .args(["--listen", "127.0.0.1:0", "--http", "127.0.0.1:0"])
        .args(["--admin-token", ADMIN])
        .arg("--dir")
        .arg(&dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("could not start fenec-pg");
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let port = |line: &str, after: &str| -> Option<u16> {
        line.split(after)
            .nth(1)?
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .ok()
    };
    let (mut pg, mut http) = (None, None);
    let mut seen = String::new();
    while pg.is_none() || http.is_none() {
        let mut line = String::new();
        if err.read_line(&mut line).unwrap_or(0) == 0 {
            panic!("fenec-pg ended before it listened:\n{seen}");
        }
        seen.push_str(&line);
        if line.contains("postgres://localhost:") {
            pg = port(&line, "localhost:");
        } else if line.contains("listening on: http://127.0.0.1:") {
            http = port(&line, "127.0.0.1:");
        }
    }
    std::thread::spawn(move || {
        let mut line = String::new();
        while err.read_line(&mut line).unwrap_or(0) > 0 {
            line.clear();
        }
    });
    Node {
        child,
        pg: pg.unwrap(),
        http: http.unwrap(),
        dir,
    }
}

impl Node {
    fn connect(&self, tenant: &str) -> Result<Client, String> {
        Client::connect(&Url {
            user: "fenec".into(),
            password: None,
            host: "127.0.0.1".into(),
            port: self.pg,
            database: tenant.into(),
        })
        .map_err(|e| e.to_string())
    }

    /// An `/_admin/` request: create, freeze, thaw or delete a tenant.
    fn admin(&self, method: &str, path: &str) -> u16 {
        let mut s = TcpStream::connect(("127.0.0.1", self.http)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        write!(
            s,
            "{method} {path} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {ADMIN}\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out[9..12].parse().unwrap()
    }
}

fn rows(c: &mut Client, sql: &str) -> Vec<Vec<Option<String>>> {
    c.query(sql).expect(sql).rows
}

#[test]
fn the_database_in_the_startup_packet_is_the_tenant() {
    let n = start("two");
    assert_eq!(n.admin("PUT", "/_admin/tenants/acme"), 201);
    assert_eq!(n.admin("PUT", "/_admin/tenants/beta"), 201);

    let mut acme = n.connect("acme").expect("acme");
    let mut beta = n.connect("beta").expect("beta");
    rows(&mut acme, "create collection notes (title text)");
    rows(&mut acme, "put notes {title: \"acme's\"}");
    rows(&mut beta, "create collection notes (title text)");
    rows(&mut beta, "put notes {title: \"beta's\"}");

    // One file each: the same collection name, the same ids, other rows.
    assert_eq!(
        rows(&mut acme, "get notes select title"),
        vec![vec![Some("acme's".to_string())]]
    );
    assert_eq!(
        rows(&mut beta, "get notes select title"),
        vec![vec![Some("beta's".to_string())]]
    );

    // psql's \d over pg_catalog answers from the tenant's own schemas.
    let tables = rows(
        &mut acme,
        "SELECT relname FROM pg_class WHERE relkind = 'r' ORDER BY relname",
    );
    assert!(
        tables.iter().any(|r| r[0].as_deref() == Some("notes")),
        "{tables:?}"
    );

    // A tenant this node does not have is PostgreSQL's unknown database, and
    // it is refused at connect.
    let err = n.connect("nobody").unwrap_err();
    assert!(err.contains("nobody"), "{err}");
    let err = n.connect("../escape").unwrap_err();
    assert!(!err.is_empty());
}

#[test]
fn a_tenant_created_after_the_session_is_there_for_it() {
    // The registry is asked again for every statement, so a session outlives
    // a tenant being created, closed as idle, or deleted.
    let n = start("later");
    assert_eq!(n.admin("PUT", "/_admin/tenants/acme"), 201);
    let mut acme = n.connect("acme").expect("acme");
    rows(&mut acme, "create collection notes (title text)");

    assert_eq!(n.admin("DELETE", "/_admin/tenants/acme"), 204);
    let gone = acme.query("get notes count").unwrap_err().to_string();
    assert!(gone.contains("acme"), "{gone}");

    assert_eq!(n.admin("PUT", "/_admin/tenants/acme"), 201);
    rows(&mut acme, "create collection notes (title text)");
    assert_eq!(
        rows(&mut acme, "get notes count"),
        vec![vec![Some("0".to_string())]]
    );
}

#[test]
fn a_frozen_tenant_refuses_writes_and_answers_reads() {
    let n = start("frozen");
    assert_eq!(n.admin("PUT", "/_admin/tenants/acme"), 201);
    let mut acme = n.connect("acme").expect("acme");
    rows(&mut acme, "create collection notes (title text)");
    rows(&mut acme, "put notes {title: \"before\"}");

    assert_eq!(n.admin("POST", "/_admin/tenants/acme/freeze"), 200);
    let refused = acme
        .query("put notes {title: \"during\"}")
        .unwrap_err()
        .to_string();
    assert!(refused.contains("moved"), "{refused}");
    let refused = acme
        .query("create index on notes (title) @sorted")
        .unwrap_err()
        .to_string();
    assert!(refused.contains("moved"), "{refused}");
    assert_eq!(
        rows(&mut acme, "get notes select title"),
        vec![vec![Some("before".to_string())]]
    );

    assert_eq!(n.admin("POST", "/_admin/tenants/acme/thaw"), 200);
    rows(&mut acme, "put notes {title: \"after\"}");
    assert_eq!(
        rows(&mut acme, "get notes count"),
        vec![vec![Some("2".to_string())]]
    );
}

/// A pg connection written and read by hand, past the startup handshake.
fn raw(n: &Node, tenant: &str) -> TcpStream {
    let mut s = TcpStream::connect(("127.0.0.1", n.pg)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut body = 196_608i32.to_be_bytes().to_vec();
    for (k, v) in [("user", "fenec"), ("database", tenant)] {
        body.extend_from_slice(k.as_bytes());
        body.push(0);
        body.extend_from_slice(v.as_bytes());
        body.push(0);
    }
    body.push(0);
    let mut msg = ((body.len() + 4) as i32).to_be_bytes().to_vec();
    msg.extend_from_slice(&body);
    s.write_all(&msg).unwrap();
    while read_message(&mut s).unwrap().0 != b'Z' {}
    s
}

fn send(s: &mut TcpStream, tag: u8, body: &[u8]) {
    let mut msg = vec![tag];
    msg.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    msg.extend_from_slice(body);
    s.write_all(&msg).unwrap();
}

fn read_message(s: &mut TcpStream) -> std::io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    s.read_exact(&mut head)?;
    let len = i32::from_be_bytes(head[1..5].try_into().unwrap()) as usize;
    let mut body = vec![0u8; len - 4];
    s.read_exact(&mut body)?;
    Ok((head[0], body))
}

/// A client that stops reading a large answer does not hold its tenant:
/// the session lets go of it before writing, as the HTTP path does. The
/// socket has no write timeout, so the tenant was held through the write --
/// a freeze waited on it for as long as the client did not read, and every
/// request for the tenant queued behind the freeze.
#[test]
fn a_client_that_stops_reading_does_not_hold_the_tenant() {
    let n = start("stalled");
    assert_eq!(n.admin("PUT", "/_admin/tenants/acme"), 201);
    let mut acme = n.connect("acme").expect("acme");
    rows(&mut acme, "create collection notes (body text)");
    // Some 4 MB of rows: more than the socket buffers between us hold.
    let body = "x".repeat(1_000);
    let batch: Vec<String> = (0..200).map(|_| format!("{{body: \"{body}\"}}")).collect();
    for _ in 0..20 {
        rows(&mut acme, &format!("put notes [{}]", batch.join(", ")));
    }
    let mut stalled = raw(&n, "acme");
    send(&mut stalled, b'Q', b"get notes\0");
    std::thread::sleep(Duration::from_millis(300));

    let t = std::time::Instant::now();
    assert_eq!(n.admin("POST", "/_admin/tenants/acme/freeze"), 200);
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    assert_eq!(n.admin("POST", "/_admin/tenants/acme/thaw"), 200);
    assert_eq!(
        rows(&mut acme, "get notes count"),
        vec![vec![Some("4000".to_string())]]
    );
    drop(stalled);
}

/// A Describe the tenant cannot answer -- deleted since the connect -- is
/// answered with the error alone, and the client's Sync brings the one
/// ReadyForQuery. It sent one of its own as well, and libpq, reading two
/// for one Sync, stayed an answer behind for the rest of the session.
#[test]
fn a_refused_describe_answers_one_ready_for_query_per_sync() {
    let n = start("describe");
    assert_eq!(n.admin("PUT", "/_admin/tenants/acme"), 201);
    let mut s = raw(&n, "acme");
    assert_eq!(n.admin("DELETE", "/_admin/tenants/acme"), 204);
    send(&mut s, b'P', b"\0get notes\0\0\0");
    send(&mut s, b'D', b"S\0");
    send(&mut s, b'S', b"");
    let mut tags = Vec::new();
    loop {
        let (tag, _) = read_message(&mut s).unwrap();
        tags.push(tag);
        if tag == b'Z' {
            break;
        }
    }
    assert_eq!(tags, vec![b'1', b'E', b'Z']);
    s.set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    assert!(read_message(&mut s).is_err(), "a second ReadyForQuery");
}
