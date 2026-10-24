//! What the lease tests stand up: nodes taking leases, each behind a
//! proxy the test can cut or hang, a router over them, and the calls the
//! tests make of both. Shared by `leases.rs` and `lease_stalls.rs`
//! (`#[path]`), so not every helper is used by each.

#![allow(dead_code)]

use fenec_http::tenants::{Replicated, Tenants};
use fenec_shard::directory::Directory;
use fenec_shard::{Config, Router};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const REPL: &str = "tenant-replication";
/// The lease a router grants nodes that take one. It bounds how late a
/// renewal may come before the node stops writing whatever the test is
/// about, and at 600 ms, renewed every 200, a loaded machine came close:
/// at utility QoS beside eight busy loops n1's last grant before the cut
/// was 499 ms old.
pub const TERM: Duration = Duration::from_secs(3);
/// A router's lease over nodes that take none: nothing of theirs lapses
/// with it, so it is kept short and the test waits past it.
pub const SHORT: Duration = Duration::from_millis(600);

pub struct Node {
    pub port: u16,
    pub dir: PathBuf,
    /// What the router and the replicas reach the node through.
    pub proxy: Proxy,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub fn node(tag: &str, lease: bool) -> Node {
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
/// hung, it drops what it carries and takes every connection after without
/// a byte carried or answered, as a machine that swallows packets would
/// leave a client waiting -- what it was sent kept, for a test to read;
/// told to hold the next request, it keeps it from the node until let go.
pub struct Proxy {
    pub addr: String,
    cut: Arc<AtomicBool>,
    hung: Arc<AtomicBool>,
    /// What connections taken while hung sent, in the order it came.
    heard: Arc<Mutex<Vec<u8>>>,
    open: Arc<Mutex<Vec<TcpStream>>>,
    /// The next request's bytes, whatever connection they come on, are to
    /// be held.
    hold: Arc<AtomicBool>,
    /// How many requests are held, and the wait for one.
    held: Arc<(Mutex<usize>, Condvar)>,
}

impl Proxy {
    pub fn to(target: &str) -> Proxy {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let cut = Arc::new(AtomicBool::new(false));
        let hung = Arc::new(AtomicBool::new(false));
        let open = Arc::new(Mutex::new(Vec::new()));
        let hold = Arc::new(AtomicBool::new(false));
        let held = Arc::new((Mutex::new(0), Condvar::new()));
        let (flag, streams, to) = (Arc::clone(&cut), Arc::clone(&open), target.to_string());
        let swallow = Arc::clone(&hung);
        let heard = Arc::new(Mutex::new(Vec::new()));
        let kept = Arc::clone(&heard);
        let gate = (Arc::clone(&hold), Arc::clone(&held));
        std::thread::spawn(move || {
            for client in listener.incoming() {
                let Ok(client) = client else { continue };
                let state = State {
                    cut: &flag,
                    hung: &swallow,
                    open: &streams,
                    heard: &kept,
                };
                carry(client, &to, &state, &gate);
            }
        });
        Proxy {
            addr,
            cut,
            hung,
            heard,
            open,
            hold,
            held,
        }
    }

    /// Hangs: what it carries is ended, so a pooled client connects again,
    /// and every connection after is taken and left unanswered. Set under
    /// the lock a connection is kept among the open ones with, which asks
    /// it again there ([`carry`]): set apart from it, a connection taken
    /// before the flag and kept after the ends was carried on, and a
    /// promotion meant to hang on it was answered (a run in twelve beside
    /// eleven other suites and sixteen busy loops).
    pub fn hang(&self) {
        let mut open = self.open.lock().unwrap();
        self.hung.store(true, Ordering::SeqCst);
        for s in open.drain(..) {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    /// What connections taken while hung have sent so far.
    pub fn heard(&self) -> String {
        String::from_utf8_lossy(&self.heard.lock().unwrap()).into_owned()
    }

    /// Cuts it off: what it carries is ended -- what it took while hung
    /// too, so what waits on it meets the end -- and every connection
    /// after, set as [`Proxy::hang`] is.
    pub fn cut(&self) {
        let mut open = self.open.lock().unwrap();
        self.cut.store(true, Ordering::SeqCst);
        for s in open.drain(..) {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    pub fn heal(&self) {
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
    pub fn hold_next(&self) {
        self.hold.store(true, Ordering::SeqCst);
    }

    /// Waits until a request is held.
    pub fn until_held(&self) {
        let (held, arrived) = &*self.held;
        let (_held, waited) = arrived
            .wait_timeout_while(held.lock().unwrap(), Duration::from_secs(30), |n| *n == 0)
            .unwrap();
        assert!(!waited.timed_out(), "no request came to be held");
    }

    /// Lets the held request through to the node.
    pub fn release(&self) {
        let (held, released) = &*self.held;
        *held.lock().unwrap() = 0;
        released.notify_all();
    }
}

type Gate = (Arc<AtomicBool>, Arc<(Mutex<usize>, Condvar)>);

/// What a proxy's connections are carried by: cut, hung, the connections
/// it carries, what it heard while hung.
struct State<'a> {
    cut: &'a AtomicBool,
    hung: &'a AtomicBool,
    open: &'a Mutex<Vec<TcpStream>>,
    heard: &'a Arc<Mutex<Vec<u8>>>,
}

/// Connects `client` to `target`, both ways, kept among the open ones;
/// what the client sends held at `gate` where it is the request to hold.
/// Cut, it is ended; hung, it is taken and left unanswered, what it sends
/// kept -- asked as it is kept among the open ones, under their lock, which
/// [`Proxy::hang`] and [`Proxy::cut`] set their flags under.
fn carry(mut client: TcpStream, target: &str, state: &State<'_>, gate: &Gate) {
    let server = match state.cut.load(Ordering::SeqCst) || state.hung.load(Ordering::SeqCst) {
        true => None,
        false => TcpStream::connect(target).ok(),
    };
    let mut held = state.open.lock().unwrap();
    if state.cut.load(Ordering::SeqCst) {
        let _ = client.shutdown(Shutdown::Both);
        return;
    }
    if state.hung.load(Ordering::SeqCst) {
        // Among the open ones, so a cut ends it: what waits on it then
        // meets the end.
        held.push(client.try_clone().unwrap());
        drop(held);
        let kept = Arc::clone(state.heard);
        std::thread::spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            while let Ok(n @ 1..) = client.read(&mut buf) {
                kept.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        return;
    }
    let Some(server) = server else {
        return;
    };
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

pub fn call(
    port: u16,
    method: &str,
    target: &str,
    body: &str,
    auth: Option<&str>,
) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    // As long as the router waits on any one node it calls ([`upstream`]):
    // a client giving up sooner measured the machine. At twenty seconds a
    // tenant's create through `cuts.rs`' router -- a minute a node call --
    // went unanswered beside three other suites and eight busy loops at
    // background QoS, in 2 runs of 3.
    s.set_read_timeout(Some(WITHIN)).unwrap();
    let auth = auth.map_or(String::new(), |t| format!("Authorization: Bearer {t}\r\n"));
    write!(
        s,
        "{method} {target} HTTP/1.1\r\nHost: x\r\n{auth}Content-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    // No answer in time is status 0 and the error, for the test to say what
    // it means: a panic here named only the read that timed out.
    if let Err(e) = s.read_to_string(&mut out) {
        return (0, format!("no answer from the router in {WITHIN:?}: {e}"));
    }
    let Some(status) = out.get(9..12).and_then(|s| s.parse().ok()) else {
        return (0, format!("not an answer: {out:?}"));
    };
    let body = out.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    (status, body.to_string())
}

pub fn query(port: u16, path: &str, sql: &str) -> (u16, String) {
    let mut body = String::from("{\"query\":");
    fenec_core::json::escape_into(&mut body, sql);
    body.push('}');
    call(port, "POST", path, &body, None)
}

/// Waits until `sql` on `port` answers with `want` in it.
pub fn until(port: u16, path: &str, sql: &str, want: &str) {
    let deadline = Instant::now() + WITHIN;
    loop {
        let (_, body) = query(port, path, sql);
        if body.contains(want) {
            return;
        }
        assert!(Instant::now() < deadline, "{path} {sql}: {body}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The upstream timeout of the lease tests' routers: the router's own
/// default, a minute. Each call it bounds does what a node does durably --
/// a tenant made, a replica told to follow, a schema applied -- and ten
/// seconds was a guess at the disk: beside eleven other suites and sixteen
/// busy loops a node's rename of a copy's `.follows` took 13.3 s, the
/// router gave its follow up, and the tenant it made had no replica (in 2
/// runs of 3, and `leases.rs`' in 1 of 2). A test whose subject is a call
/// that waits out the timeout gives its own.
pub fn upstream() -> Duration {
    Config::default().upstream_timeout
}

/// Three nodes behind a router leasing them for `term`, each registered at
/// its proxy.
pub fn three(tag: &str, lease: bool, term: Duration) -> ([Node; 3], u16) {
    let (nodes, port, _) = three_leased(tag, lease, term, upstream());
    (nodes, port)
}

/// [`three`] with the router's upstream timeout `upstream`, and the router.
pub fn three_leased(
    tag: &str,
    lease: bool,
    term: Duration,
    upstream: Duration,
) -> ([Node; 3], u16, Arc<Router>) {
    let nodes = [1, 2, 3].map(|i| node(&format!("{tag}{i}"), lease));
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        upstream_timeout: upstream,
        replicas: true,
        auto_failover: Some(term),
        ..Config::default()
    };
    let router = Router::new(Directory::in_memory(), cfg);
    let listener = router.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    router.start_leasing().unwrap();
    let serving = Arc::clone(&router);
    std::thread::spawn(move || {
        let _ = serving.serve_on(listener);
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
    (nodes, port, router)
}

/// Where the directory keeps `tenant`: its node, and its replica's.
pub fn placed(port: u16, tenant: &str) -> (String, Option<String>) {
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
pub fn number_after(text: &str, prefix: &str) -> f64 {
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

/// How long a call into a router a test waits for: a lease round, a
/// router over a directory another one keeps. Each of its calls to a node
/// is bounded -- a grant by a third of the lease, a promotion by the
/// upstream timeout -- and a round takes milliseconds; one that has not
/// ended in a minute waits for good. And so every wait the lease tests make
/// for what must come -- a failover, a node answering again, a copy taking
/// its image, a request's answer: each comes once calls the router bounds
/// by its upstream timeout, a minute ([`upstream`]), have been answered,
/// and a wait shorter than one of them measured the machine. At 20 and 30
/// s, at background QoS beside busy loops and other suites' work, `t was
/// never failed over` and `n1 never answered again` each came once in 20
/// runs.
pub const WITHIN: Duration = Duration::from_secs(60);

/// `f` run on a thread of its own and waited for [`WITHIN`] at most: a
/// router that waits for good -- a lock a thread holds taken again, a
/// round joining a grant that waits on it -- fails the test, naming
/// `what`, where it hung CI's job for its hour. A panic in `f` fails it
/// too, its message above.
pub fn bounded<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(WITHIN) {
        Ok(v) => v,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("{what} did not end in {WITHIN:?}: it waits for good")
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => panic!("{what} panicked"),
    }
}

/// One lease round of `router`'s, waited for as [`bounded`] waits.
pub fn round(router: &Arc<Router>) {
    let router = Arc::clone(router);
    bounded("a lease round", move || router.lease_round());
}

pub fn by_name<'a>(nodes: &'a [Node; 3], name: &str) -> &'a Node {
    &nodes[name[1..].parse::<usize>().unwrap() - 1]
}
