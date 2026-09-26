//! Automatic failover: the router leases each node the tenants it places
//! there, and fails a node over once its lease has certainly lapsed.
//!
//! The router reaches each node here through a proxy the test can cut, which
//! stands for a node gone or cut off: its clients still reach it, the router
//! and its replicas do not. What must hold is that it stops taking writes
//! before its tenants take them elsewhere -- no tenant with two primaries.

use fenec_http::tenants::{Replicated, Tenants};
use fenec_shard::directory::Directory;
use fenec_shard::{Config, Router};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REPL: &str = "tenant-replication";
const TERM: Duration = Duration::from_millis(600);

struct Node {
    port: u16,
    dir: PathBuf,
    /// What the router and the replicas reach the node through.
    proxy: Proxy,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn node(tag: &str, lease: bool) -> Node {
    let dir = std::env::temp_dir().join(format!("fenec-leases-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut tenants = Tenants::new(&dir).unwrap().with_replication(Replicated {
        token: REPL.into(),
        buffer: 8 << 20,
        upstream: None,
        sync_on_write: true,
    });
    if lease {
        tenants = tenants.with_lease();
    }
    let cfg = fenec_http::Config {
        addr: "127.0.0.1:0".into(),
        admin_token: Some(format!("adm-{tag}")),
        sync_on_write: true,
        ..fenec_http::Config::default()
    };
    let server = fenec_http::Server::with_tenants(Arc::new(tenants), cfg);
    let listener = server.bind().unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    Node {
        port: addr.port(),
        dir,
        proxy: Proxy::to(&addr.to_string()),
    }
}

/// A TCP proxy: cut, it drops what it carries and every connection after.
struct Proxy {
    addr: String,
    cut: Arc<AtomicBool>,
    open: Arc<Mutex<Vec<TcpStream>>>,
}

impl Proxy {
    fn to(target: &str) -> Proxy {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let cut = Arc::new(AtomicBool::new(false));
        let open = Arc::new(Mutex::new(Vec::new()));
        let (flag, streams, target) = (Arc::clone(&cut), Arc::clone(&open), target.to_string());
        std::thread::spawn(move || {
            for client in listener.incoming() {
                let Ok(client) = client else { continue };
                if flag.load(Ordering::SeqCst) {
                    let _ = client.shutdown(Shutdown::Both);
                    continue;
                }
                let Ok(server) = TcpStream::connect(&target) else {
                    continue;
                };
                let mut held = streams.lock().unwrap();
                held.push(client.try_clone().unwrap());
                held.push(server.try_clone().unwrap());
                drop(held);
                pipe(client.try_clone().unwrap(), server.try_clone().unwrap());
                pipe(server, client);
            }
        });
        Proxy { addr, cut, open }
    }

    fn cut(&self) {
        self.cut.store(true, Ordering::SeqCst);
        for s in self.open.lock().unwrap().drain(..) {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    fn heal(&self) {
        self.cut.store(false, Ordering::SeqCst);
    }
}

fn pipe(mut from: TcpStream, mut to: TcpStream) {
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut from, &mut to);
        let _ = to.shutdown(Shutdown::Write);
    });
}

fn call(port: u16, method: &str, target: &str, body: &str, auth: Option<&str>) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let auth = auth.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    write!(
        s,
        "{method} {target} HTTP/1.1\r\nHost: x\r\n{auth}Content-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status = out[9..12].parse().unwrap();
    let body = out.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (status, body.to_string())
}

fn query(port: u16, path: &str, sql: &str) -> (u16, String) {
    let mut body = String::from("{\"query\":");
    fenec_core::json::escape_into(&mut body, sql);
    body.push('}');
    call(port, "POST", path, &body, None)
}

/// Waits until `sql` on `port` answers with `want` in it.
fn until(port: u16, path: &str, sql: &str, want: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (_, body) = query(port, path, sql);
        if body.contains(want) {
            return;
        }
        assert!(Instant::now() < deadline, "{path} {sql}: {body}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Three nodes behind a router leasing them for `TERM`, each registered at
/// its proxy.
fn three(tag: &str, lease: bool) -> ([Node; 3], u16) {
    let nodes = [1, 2, 3].map(|i| node(&format!("{tag}{i}"), lease));
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        upstream_timeout: Duration::from_secs(10),
        replicas: true,
        auto_failover: Some(TERM),
        ..Config::default()
    };
    let router = Router::new(Directory::in_memory(), cfg);
    let listener = router.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    router.start_leasing().unwrap();
    std::thread::spawn(move || {
        let _ = router.serve_on(listener);
    });
    for (i, nd) in nodes.iter().enumerate() {
        let body = format!(
            r#"{{"addr":"{}","token":"adm-{tag}{}"}}"#,
            nd.proxy.addr,
            i + 1
        );
        let (status, body) = call(
            port,
            "PUT",
            &format!("/_shard/nodes/n{}", i + 1),
            &body,
            None,
        );
        assert!(status == 200 || status == 201, "{body}");
    }
    (nodes, port)
}

/// Where the directory keeps `tenant`: its node, and its replica's.
fn placed(port: u16, tenant: &str) -> (String, Option<String>) {
    let (_, list) = call(port, "GET", "/_shard/tenants", "", None);
    let entry = list
        .split("},{")
        .find(|e| e.contains(&format!("\"name\":\"{tenant}\"")))
        .unwrap_or_else(|| panic!("no {tenant} in {list}"));
    let value = |key: &str| {
        let rest = entry.split(&format!("\"{key}\":\"")).nth(1)?;
        Some(rest.split('"').next()?.to_string())
    };
    (value("node").unwrap(), value("replica"))
}

fn by_name<'a>(nodes: &'a [Node; 3], name: &str) -> &'a Node {
    &nodes[name[1..].parse::<usize>().unwrap() - 1]
}

#[test]
fn a_node_cut_off_stops_writing_before_its_tenants_are_promoted_elsewhere() {
    let (nodes, port) = three("cut", true);
    let (status, body) = call(
        port,
        "PUT",
        "/_shard/tenants/acme",
        r#"{"node":"n1"}"#,
        None,
    );
    assert_eq!(status, 201, "{body}");
    // Writable through the router at once: the create granted n1 its lease.
    let path = "/t/acme/query";
    assert_eq!(
        query(port, path, "create collection notes (title text)").0,
        200
    );
    assert_eq!(query(port, path, r#"put notes {title: "one"}"#).0, 200);
    let replica = placed(port, "acme").1.expect("acme has a replica");
    until(
        by_name(&nodes, &replica).port,
        path,
        "get notes select title",
        "one",
    );

    // n1 is cut off from the router and from its replica; its clients still
    // reach it. It takes their writes while its lease runs, then none; only
    // after that does the router promote acme's replica.
    nodes[0].proxy.cut();
    let cut = Instant::now();
    let (mut fenced, mut promoted) = (None, None);
    while fenced.is_none() || promoted.is_none() {
        assert!(
            cut.elapsed() < Duration::from_secs(20),
            "fenced {fenced:?}, promoted {promoted:?}"
        );
        let (direct, body) = query(nodes[0].port, path, r#"put notes {title: "direct"}"#);
        match direct {
            200 => assert!(
                promoted.is_none(),
                "n1 wrote acme after it was promoted elsewhere"
            ),
            503 => {
                assert!(body.contains("lease"), "{body}");
                fenced.get_or_insert_with(|| cut.elapsed());
            }
            other => panic!("n1 answered {other}: {body}"),
        }
        if promoted.is_none() {
            let (through, _) = query(port, path, r#"put notes {title: "after"}"#);
            if through == 200 {
                promoted = Some(cut.elapsed());
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let (fenced, promoted) = (fenced.unwrap(), promoted.unwrap());
    assert!(
        fenced < promoted,
        "fenced at {fenced:?}, promoted at {promoted:?}"
    );
    // Renewed every third of a lease: n1 stops between two thirds and one
    // lease after the cut, and the router waits a lease and a tenth.
    assert!(
        fenced >= TERM * 2 / 3 - Duration::from_millis(50),
        "{fenced:?}"
    );
    assert!(promoted >= TERM + TERM / 10, "{promoted:?}");
    assert_eq!(placed(port, "acme").0, replica);

    // n1 answers again: its lease names nothing of what moved, and a repair
    // has its copy follow the tenant's new primary.
    nodes[0].proxy.heal();
    until(nodes[0].port, path, "get notes select title", "after");
    let (direct, body) = query(nodes[0].port, path, r#"put notes {title: "stale"}"#);
    assert_ne!(direct, 200, "the old primary's copy took a write: {body}");
    assert_eq!(placed(port, "acme").1.as_deref(), Some("n1"));
}

#[test]
fn a_node_taking_leases_writes_nothing_before_one_and_only_what_it_names() {
    let n = node("names", true);
    let admin = |method: &str, target: &str, body: &str| {
        call(n.port, method, target, body, Some("adm-names"))
    };
    for t in ["a", "b"] {
        assert_eq!(admin("PUT", &format!("/_admin/tenants/{t}"), "").0, 201);
    }
    let (status, body) = query(n.port, "/t/a/query", "create collection c (x int)");
    assert_eq!(status, 503, "{body}");
    assert!(body.contains("no lease"), "{body}");

    let (status, body) = admin(
        "POST",
        "/_admin/lease",
        r#"{"ms":60000,"epoch":"e1","primaries":["a"]}"#,
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        query(n.port, "/t/a/query", "create collection c (x int)").0,
        200
    );
    let (status, body) = query(n.port, "/t/b/query", "create collection c (x int)");
    assert_eq!(status, 503, "{body}");
    assert!(body.contains("another node"), "{body}");
    // Reads go on: a copy the lease does not name is still read.
    assert_eq!(query(n.port, "/t/b/query", "collections").0, 200);

    // A renewal names the list; one the node does not hold is asked for.
    assert_eq!(
        admin("POST", "/_admin/lease", r#"{"ms":60000,"epoch":"e1"}"#).0,
        200
    );
    assert_eq!(
        admin("POST", "/_admin/lease", r#"{"ms":60000,"epoch":"e2"}"#).0,
        412
    );
    // Lapsed, it takes no write, the one it named included.
    assert_eq!(
        admin("POST", "/_admin/lease", r#"{"ms":1,"epoch":"e1"}"#).0,
        200
    );
    std::thread::sleep(Duration::from_millis(20));
    let (status, body) = query(n.port, "/t/a/query", "put c {x: 1}");
    assert_eq!(status, 503, "{body}");
    assert!(body.contains("lapsed"), "{body}");
}

#[test]
fn a_node_that_takes_no_lease_is_never_failed_over_on_its_own() {
    let (nodes, port) = three("free", false);
    let (status, body) = call(
        port,
        "PUT",
        "/_shard/tenants/acme",
        r#"{"node":"n1"}"#,
        None,
    );
    assert_eq!(status, 201, "{body}");
    assert_eq!(
        query(port, "/t/acme/query", "create collection c (x int)").0,
        200
    );
    nodes[0].proxy.cut();
    std::thread::sleep(TERM * 3);
    assert_eq!(
        placed(port, "acme").0,
        "n1",
        "a node that fences nothing was failed over"
    );
    // Its writes go on where its clients reach it.
    assert_eq!(query(nodes[0].port, "/t/acme/query", "put c {x: 1}").0, 200);
}
