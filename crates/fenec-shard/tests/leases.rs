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
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const REPL: &str = "tenant-replication";
/// The lease a router grants nodes that take one. It bounds how late a
/// renewal may come before the node stops writing whatever the test is
/// about, and at 600 ms, renewed every 200, a loaded machine came close:
/// at utility QoS beside eight busy loops n1's last grant before the cut
/// was 499 ms old.
const TERM: Duration = Duration::from_secs(3);
/// A router's lease over nodes that take none: nothing of theirs lapses
/// with it, so it is kept short and the test waits past it.
const SHORT: Duration = Duration::from_millis(600);

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

/// A TCP proxy: cut, it drops what it carries and every connection after;
/// told to hold the next request, it keeps it from the node until let go.
struct Proxy {
    addr: String,
    cut: Arc<AtomicBool>,
    open: Arc<Mutex<Vec<TcpStream>>>,
    /// The next request's bytes, whatever connection they come on, are to
    /// be held.
    hold: Arc<AtomicBool>,
    /// How many requests are held, and the wait for one.
    held: Arc<(Mutex<usize>, Condvar)>,
}

impl Proxy {
    fn to(target: &str) -> Proxy {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let cut = Arc::new(AtomicBool::new(false));
        let open = Arc::new(Mutex::new(Vec::new()));
        let hold = Arc::new(AtomicBool::new(false));
        let held = Arc::new((Mutex::new(0), Condvar::new()));
        let (flag, streams, to) = (Arc::clone(&cut), Arc::clone(&open), target.to_string());
        let gate = (Arc::clone(&hold), Arc::clone(&held));
        std::thread::spawn(move || {
            for client in listener.incoming() {
                let Ok(client) = client else { continue };
                if flag.load(Ordering::SeqCst) {
                    let _ = client.shutdown(Shutdown::Both);
                    continue;
                }
                carry(client, &to, &streams, &gate);
            }
        });
        Proxy {
            addr,
            cut,
            open,
            hold,
            held,
        }
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

    /// Holds the next request sent through, on whatever connection. It
    /// held the next connection, the open ones ended so that a pooled
    /// client would connect again, and no connection came to be held in 1
    /// run of 20 at background QoS beside eight busy loops (and once on
    /// CI's macOS): a connection the router looked at before its end
    /// arrived carried the grant, which met the end as no answer, or one
    /// being carried as the open ones were ended was not among them and
    /// stayed pooled. A request held where it comes holds either way.
    fn hold_next(&self) {
        self.hold.store(true, Ordering::SeqCst);
    }

    /// Waits until a request is held.
    fn until_held(&self) {
        let (held, arrived) = &*self.held;
        let (_held, waited) = arrived
            .wait_timeout_while(held.lock().unwrap(), Duration::from_secs(30), |n| *n == 0)
            .unwrap();
        assert!(!waited.timed_out(), "no request came to be held");
    }

    /// Lets the held request through to the node.
    fn release(&self) {
        let (held, released) = &*self.held;
        *held.lock().unwrap() = 0;
        released.notify_all();
    }
}

type Gate = (Arc<AtomicBool>, Arc<(Mutex<usize>, Condvar)>);

/// Connects `client` to `target`, both ways, kept among `open`; what the
/// client sends held at `gate` where it is the request to hold.
fn carry(client: TcpStream, target: &str, open: &Mutex<Vec<TcpStream>>, gate: &Gate) {
    let Ok(server) = TcpStream::connect(target) else {
        return;
    };
    let mut held = open.lock().unwrap();
    held.push(client.try_clone().unwrap());
    held.push(server.try_clone().unwrap());
    drop(held);
    let gate = (Arc::clone(&gate.0), Arc::clone(&gate.1));
    pipe(
        client.try_clone().unwrap(),
        server.try_clone().unwrap(),
        Some(gate),
    );
    pipe(server, client, None);
}

/// Copies `from` into `to` until either ends; a chunk read while `gate` is
/// set starts the request held, which waits there until let go.
fn pipe(mut from: TcpStream, mut to: TcpStream, gate: Option<Gate>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = match from.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                // As `io::copy` reads.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            if let Some((hold, held)) = &gate {
                if hold.swap(false, Ordering::SeqCst) {
                    let (count, cv) = &**held;
                    let mut waiting = count.lock().unwrap();
                    *waiting += 1;
                    cv.notify_all();
                    while *waiting > 0 {
                        waiting = cv.wait(waiting).unwrap();
                    }
                }
            }
            if to.write_all(&buf[..n]).is_err() {
                break;
            }
        }
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

/// Three nodes behind a router leasing them for `term`, each registered at
/// its proxy.
fn three(tag: &str, lease: bool, term: Duration) -> ([Node; 3], u16) {
    let nodes = [1, 2, 3].map(|i| node(&format!("{tag}{i}"), lease));
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        upstream_timeout: Duration::from_secs(10),
        replicas: true,
        auto_failover: Some(term),
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

/// The number written right after `prefix` in `text`.
fn number_after(text: &str, prefix: &str) -> f64 {
    let rest = text
        .split_once(prefix)
        .unwrap_or_else(|| panic!("no {prefix} in {text}"))
        .1;
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == 'e' || c == '-'))
        .unwrap_or(rest.len());
    rest[..end]
        .parse()
        .unwrap_or_else(|e| panic!("{prefix}{rest}: {e}"))
}

fn by_name<'a>(nodes: &'a [Node; 3], name: &str) -> &'a Node {
    &nodes[name[1..].parse::<usize>().unwrap() - 1]
}

#[test]
fn a_node_cut_off_stops_writing_before_its_tenants_are_promoted_elsewhere() {
    let (nodes, port) = three("cut", true, TERM);
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
    // What each side holds, read after the cut: the lease n1 has left by its
    // own clock, and how long ago the router last had n1's answer.
    let (status, lease) = call(nodes[0].port, "GET", "/_admin/lease", "", Some("adm-cut1"));
    assert_eq!(status, 200, "{lease}");
    let left = Duration::from_millis(number_after(&lease, "\"left_ms\":") as u64);
    let (status, scrape) = call(port, "GET", "/_metrics", "", None);
    assert_eq!(status, 200, "{scrape}");
    let age = Duration::from_secs_f64(number_after(
        &scrape,
        "fenec_router_lease_age_seconds{node=\"n1\"} ",
    ));
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
    // n1 stops as its lease runs out by its own clock, `left` after the cut
    // at the least; the router waits a lease and a tenth from the last
    // answer it had, `age` before the cut at the most. Counted from the
    // renewals' schedule instead -- n1 stopping no sooner than two thirds of
    // a lease after the cut -- the bound held only while every renewal came
    // on time: at utility QoS beside eight busy loops one came late, and n1
    // stopped, rightly, 101 ms after the cut against a bound of 350.
    eprintln!(
        "fenced {fenced:?} after the cut ({left:?} left), promoted {promoted:?} \
         (the last answer {age:?} before)"
    );
    assert!(
        fenced >= left,
        "fenced at {fenced:?}, {left:?} left at the cut"
    );
    let lapse = TERM + TERM / 10;
    assert!(
        promoted >= lapse.saturating_sub(age),
        "promoted at {promoted:?}, the last answer {age:?} before the cut"
    );
    assert_eq!(placed(port, "acme").0, replica);

    // n1 answers again: its lease names nothing of what moved, and a repair
    // has its copy follow the tenant's new primary.
    nodes[0].proxy.heal();
    until(nodes[0].port, path, "get notes select title", "after");
    let (direct, body) = query(nodes[0].port, path, r#"put notes {title: "stale"}"#);
    assert_ne!(direct, 200, "the old primary's copy took a write: {body}");
    // The router records the replica once n1 has answered that it follows
    // (`attach`), and n1 may hold the row before that answer is back: the
    // record is waited for as the row was. Read at once, it failed on a
    // loaded CI runner.
    let deadline = Instant::now() + Duration::from_secs(20);
    while placed(port, "acme").1.as_deref() != Some("n1") {
        assert!(Instant::now() < deadline, "{:?}", placed(port, "acme"));
        std::thread::sleep(Duration::from_millis(20));
    }
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
    let (nodes, port) = three("free", false, SHORT);
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
    std::thread::sleep(SHORT * 3);
    assert_eq!(
        placed(port, "acme").0,
        "n1",
        "a node that fences nothing was failed over"
    );
    // Its writes go on where its clients reach it.
    assert_eq!(query(nodes[0].port, "/t/acme/query", "put c {x: 1}").0, 200);
}

/// A tenant created with its schema under leases: the node takes a write
/// only once its lease names the tenant, so the schema is applied after the
/// grant, and its replica follows it there.
#[test]
fn a_tenant_created_with_its_schema_takes_it_under_a_lease_and_its_replica_follows() {
    let (nodes, port) = three("schema", true, TERM);
    let (status, body) = call(
        port,
        "PUT",
        "/_shard/tenants/acme",
        r#"{"schema":"create collection notes (t text)"}"#,
        None,
    );
    assert_eq!(status, 201, "{body}");
    let (status, body) = query(port, "/t/acme/query", "put notes {t: \"x\"}");
    assert_eq!(status, 200, "{body}");
    let (_, replica) = placed(port, "acme");
    let replica = by_name(&nodes, &replica.expect("a replica"));
    until(replica.port, "/t/acme/query", "get notes", "\"x\"");
}

/// A round reads every node's list as it starts, and its grant, landing
/// after the one a create sent at once, put the list from before the
/// create back: the new tenant's writes were refused on its node (503)
/// until the next round -- the write after a create, in 1 run of 30 at
/// utility QoS beside eight busy loops and 1 of 50 in a container of three
/// cores. A node is granted one grant at a time, and never a list older
/// than one it was sent: here the round's grant is held on its way, the
/// create's queues behind it, and the tenant takes its writes once both
/// have landed.
#[test]
fn a_round_that_read_the_lists_before_a_create_does_not_take_the_tenant_back() {
    let n1 = node("stale1", true);
    // A lease no round renews on its own: the test runs the one round.
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        upstream_timeout: Duration::from_secs(10),
        auto_failover: Some(Duration::from_secs(60)),
        ..Config::default()
    };
    let router = Router::new(Directory::in_memory(), cfg);
    let listener = router.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    let serving = Arc::clone(&router);
    std::thread::spawn(move || {
        let _ = serving.serve_on(listener);
    });
    let body = format!(r#"{{"addr":"{}","token":"adm-stale1"}}"#, n1.proxy.addr);
    let (status, body) = call(port, "PUT", "/_shard/nodes/n1", &body, None);
    assert!(status == 200 || status == 201, "{body}");
    let (status, body) = call(port, "PUT", "/_shard/tenants/a", r#"{"node":"n1"}"#, None);
    assert_eq!(status, 201, "{body}");
    let (status, body) = query(port, "/t/a/query", "create collection c (x int)");
    assert_eq!(status, 200, "{body}");

    // The round reads n1's list -- `a` alone -- and its grant is held.
    n1.proxy.hold_next();
    let round = {
        let r = Arc::clone(&router);
        std::thread::spawn(move || r.lease_round())
    };
    n1.proxy.until_held();
    // `b` is placed on n1 meanwhile, and granted at once: answered while
    // the round's grant was held, or queued behind it.
    let create = std::thread::spawn(move || {
        call(port, "PUT", "/_shard/tenants/b", r#"{"node":"n1"}"#, None)
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    while !create.is_finished() && router.grants_waiting() == 0 {
        assert!(
            Instant::now() < deadline,
            "the create neither answered nor queued"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    n1.proxy.release();
    round.join().unwrap();
    let (status, body) = create.join().unwrap();
    assert_eq!(status, 201, "{body}");
    let (status, body) = query(port, "/t/b/query", "create collection c (x int)");
    assert_eq!(status, 200, "the round's list took `b` back: {body}");
}
