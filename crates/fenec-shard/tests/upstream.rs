//! The router's pooled connections to a node, through a relay standing
//! where the network does: it passes bytes both ways and, as told, passes
//! the node's close of a connection on as a reset -- what macOS does to
//! the router's end about a minute after the node's FIN -- or drops an
//! answer the node sent and closes the router's end, as a node that ran a
//! request and went down before answering would. It counts the
//! connections the router let go of, which a held claim's client gone
//! makes the router do.

use fenec_http::tenants::Tenants;
use fenec_shard::directory::{Directory, Node, State};
use fenec_shard::{Config, Router};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

struct Relay {
    addr: String,
    shared: Arc<Shared>,
}

struct Shared {
    node: String,
    /// The node's close of a connection goes on to the router as a reset.
    reset: bool,
    /// The node's next answer is dropped and the router's end closed.
    swallow: AtomicBool,
    /// Connections open now, and those the router closed its end of
    /// while the node's was open.
    open: Mutex<(usize, usize)>,
    changed: Condvar,
}

impl Relay {
    fn start(node: &str, reset: bool) -> Relay {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let shared = Arc::new(Shared {
            node: node.to_string(),
            reset,
            swallow: AtomicBool::new(false),
            open: Mutex::new((0, 0)),
            changed: Condvar::new(),
        });
        let s = Arc::clone(&shared);
        std::thread::spawn(move || {
            for router in l.incoming() {
                let Ok(router) = router else { continue };
                s.open.lock().unwrap().0 += 1;
                let s = Arc::clone(&s);
                std::thread::spawn(move || {
                    pass(&s, router);
                    s.open.lock().unwrap().0 -= 1;
                    s.changed.notify_all();
                });
            }
        });
        Relay { addr, shared }
    }

    fn swallow_next_answer(&self) {
        self.shared.swallow.store(true, Ordering::SeqCst);
    }

    /// Waits for every connection to be closed, each by the node's idle
    /// timeout and passed on to the router.
    fn until_all_closed(&self) {
        let open = self.shared.open.lock().unwrap();
        let (open, waited) = self
            .shared
            .changed
            .wait_timeout_while(open, Duration::from_secs(30), |n| n.0 > 0)
            .unwrap();
        assert!(!waited.timed_out(), "{} connections still open", open.0);
    }

    /// Waits for the router to have closed its end of `n` connections the
    /// node had not closed.
    fn until_router_closed(&self, n: usize) {
        let open = self.shared.open.lock().unwrap();
        let (open, waited) = self
            .shared
            .changed
            .wait_timeout_while(open, Duration::from_secs(20), |c| c.1 < n)
            .unwrap();
        assert!(
            !waited.timed_out(),
            "the router let go of {} connections of {n}",
            open.1
        );
    }
}

/// One connection from the router, passed on to the node until either end
/// closes it.
fn pass(s: &Arc<Shared>, router: TcpStream) {
    let node = TcpStream::connect(&s.node).unwrap();
    let mut from_router = router.try_clone().unwrap();
    let mut to_node = node.try_clone().unwrap();
    // Set once the node's end is done, so that the router's end read to its
    // close is told from the relay letting go after the node.
    let node_done = Arc::new(AtomicBool::new(false));
    let (done, shared) = (Arc::clone(&node_done), Arc::clone(s));
    let up = std::thread::spawn(move || {
        let mut buf = [0u8; 16 << 10];
        while let Ok(n @ 1..) = from_router.read(&mut buf) {
            if to_node.write_all(&buf[..n]).is_err() {
                break;
            }
        }
        // The router closed its end, or the relay is letting go: the node
        // is told as the router would tell it -- and only then is the
        // router's close counted, so that a test waiting for it acts on a
        // node that has been told.
        let _ = to_node.shutdown(Shutdown::Write);
        if !done.load(Ordering::SeqCst) {
            shared.open.lock().unwrap().1 += 1;
            shared.changed.notify_all();
        }
    });
    let (mut from_node, mut to_router) = (node, router);
    let mut buf = [0u8; 16 << 10];
    let mut swallowed = false;
    while let Ok(n @ 1..) = from_node.read(&mut buf) {
        if s.swallow.swap(false, Ordering::SeqCst) {
            swallowed = true;
            break;
        }
        if to_router.write_all(&buf[..n]).is_err() {
            break;
        }
    }
    node_done.store(true, Ordering::SeqCst);
    if s.reset && !swallowed {
        reset_on_close(&to_router);
    }
    // Wakes the copy the other way, which then lets go of its handle, so
    // this one's close is the socket's: a FIN, or the reset.
    let _ = to_router.shutdown(Shutdown::Read);
    let _ = up.join();
}

/// `SO_LINGER` of zero: the socket's close resets the connection. The
/// crate takes no `libc`, so the option's numbers are each system's.
fn reset_on_close(s: &TcpStream) {
    use std::os::fd::AsRawFd;
    #[repr(C)]
    struct Linger {
        on: i32,
        seconds: i32,
    }
    extern "C" {
        fn setsockopt(fd: i32, level: i32, name: i32, value: *const Linger, len: u32) -> i32;
    }
    #[cfg(target_vendor = "apple")]
    let (level, name) = (0xffff, 0x0080);
    #[cfg(not(target_vendor = "apple"))]
    let (level, name) = (1, 13);
    let linger = Linger { on: 1, seconds: 0 };
    let len = std::mem::size_of::<Linger>() as u32;
    // SAFETY: the option's value lives through the call.
    let rc = unsafe { setsockopt(s.as_raw_fd(), level, name, &linger, len) };
    assert_eq!(rc, 0, "SO_LINGER: {}", std::io::Error::last_os_error());
}

struct Setup {
    router: String,
    node: String,
    relay: Relay,
    tenants: Arc<Tenants>,
    dir: PathBuf,
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A node holding tenant `acme` with collections `notes` and `jobs`, the
/// relay in front of it and a router in front of that. The directory is
/// written before the router starts, so nothing the test does not send
/// goes from the router to the node: no request but the test's meets the
/// node's idle timeout on its way.
fn setup(tag: &str, idle_timeout: Option<Duration>, reset: bool) -> Setup {
    setup_in(tag, idle_timeout, reset, Front::Global)
}

/// Which router stands in front of the relay, and where it finds the node.
#[derive(Clone, Copy)]
enum Front {
    /// The global directory's router, placing the tenant itself.
    Global,
    /// Region `us`'s router, its own directory placing the tenant.
    Region,
    /// Region `us`'s router whose own directory places nothing, the tenant
    /// on a node its copy of the global directory places it on
    /// (`Target::Node`).
    GlobalCopy,
}

/// [`setup`], the router in front as `front` says.
fn setup_in(tag: &str, idle_timeout: Option<Duration>, reset: bool, front: Front) -> Setup {
    fenec_http::audit::set_delay(0);
    let dir = std::env::temp_dir().join(format!(
        "fenec-shard-upstream-{tag}-{}-{:?}",
        std::process::id(),
        Instant::now()
    ));
    let tenants = Arc::new(Tenants::new(&dir).unwrap());
    let acme = tenants.create("acme").unwrap();
    for made in [
        "create collection notes (title text)",
        "create collection jobs (run_at timestamp @sorted, owner text)",
    ] {
        let create = fenec_ql::parse_one(made).unwrap();
        acme.db.write().unwrap().execute(&create).unwrap();
    }
    drop(acme);
    let server = fenec_http::Server::with_tenants(
        Arc::clone(&tenants),
        fenec_http::Config {
            addr: "127.0.0.1:0".into(),
            admin_token: Some("adm".into()),
            idle_timeout,
            ..fenec_http::Config::default()
        },
    );
    let listener = server.bind().unwrap();
    let node = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    let relay = Relay::start(&node, reset);
    let region = match front {
        Front::Global => None,
        Front::Region | Front::GlobalCopy => Some("us"),
    };
    let placing = |region: Option<&str>| {
        let mut d = Directory::in_memory();
        d.settle_scope(region).unwrap();
        let (addr, token) = (relay.addr.clone(), "adm".to_string());
        let placed = Node {
            addr,
            token,
            region: region.unwrap_or_default().to_string(),
            ..Node::default()
        };
        d.set_node("n1", placed).unwrap();
        d.place("acme", "n1", State::Active).unwrap();
        d
    };
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        upstream_timeout: Duration::from_secs(10),
        region: region.map(String::from),
        ..Config::default()
    };
    let router = match front {
        Front::Global => Router::new(placing(None), cfg),
        Front::Region => {
            Router::regional(placing(region), cfg, None, Directory::in_memory()).unwrap()
        }
        Front::GlobalCopy => {
            let mut own = Directory::in_memory();
            own.settle_scope(region).unwrap();
            Router::regional(own, cfg, None, placing(None)).unwrap()
        }
    };
    let listener = router.bind().unwrap();
    let front = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = router.serve_on(listener);
    });
    Setup {
        router: front,
        node,
        relay,
        tenants,
        dir,
    }
}

/// The status, the head and the body of one request on a connection of
/// its own.
fn call(addr: &str, method: &str, target: &str, body: &str, extra: &str) -> (u16, String, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let req = format!(
        "{method} {target} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n{extra}\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    let (head, body) = out.split_once("\r\n\r\n").unwrap_or((&out, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, head.to_string(), body.to_string())
}

fn put(addr: &str, title: &str, extra: &str) -> (u16, String, String) {
    let body = format!(r#"{{"query":"put notes {{title: \"{title}\"}}"}}"#);
    call(addr, "POST", "/t/acme/query", &body, extra)
}

/// The titles the node holds, asked of the node itself.
fn titles(s: &Setup) -> Vec<String> {
    let (status, _, body) = call(
        &s.node,
        "POST",
        "/t/acme/query",
        r#"{"query":"get notes select title order id"}"#,
        "",
    );
    assert_eq!(status, 200, "{body}");
    body.split("\"title\":\"")
        .skip(1)
        .map(|t| t.split('"').next().unwrap().to_string())
        .collect()
}

/// A write through a connection the node closed while idle: the router
/// finds it closed before it writes, and the write lands once. Passed on
/// as a reset -- macOS's state a minute after the node's FIN -- the router
/// answered 502 "Invalid argument (os error 22)": `setsockopt` refuses
/// such a socket there, and the error was not one the retry knew. Passed
/// on as the FIN alone, a router that sent a write without looking would
/// read the end of the connection after it, and could not tell that the
/// node never ran it.
fn idle_close(tag: &str, reset: bool) {
    let s = setup(tag, Some(Duration::from_millis(200)), reset);
    let r = put(&s.router, "first", "");
    assert_eq!(r.0, 200, "{}", r.2);
    s.relay.until_all_closed();
    let r = put(&s.router, "second", "");
    assert_eq!(r.0, 200, "{}", r.2);
    assert_eq!(titles(&s), ["first", "second"]);
}

#[test]
fn a_write_through_a_connection_the_node_closed_and_reset_lands_once() {
    idle_close("reset", true);
}

#[test]
fn a_write_through_a_connection_the_node_closed_lands_once() {
    idle_close("fin", false);
}

/// A node that ran a write and went down before its answer looks, from
/// the router, like a connection closed as the write came in. Sent again,
/// a `put` with no id was made twice: the router answers 502 and the node
/// holds it once.
#[test]
fn a_write_the_node_ran_and_never_answered_is_not_sent_again() {
    let s = setup("unanswered", None, false);
    let r = put(&s.router, "first", "");
    assert_eq!(r.0, 200, "{}", r.2);
    // Over the connection the first left in the pool.
    s.relay.swallow_next_answer();
    let r = put(&s.router, "second", "");
    assert_eq!(r.0, 502, "{}", r.2);
    assert_eq!(titles(&s), ["first", "second"]);
}

/// What may run twice is sent again: a read, and a write under an
/// `Idempotency-Key`, which the node answers from the first one's answer.
#[test]
fn a_keyed_write_or_a_read_the_node_never_answered_is_sent_again() {
    let s = setup("replayed", None, false);
    let r = put(&s.router, "first", "");
    assert_eq!(r.0, 200, "{}", r.2);

    s.relay.swallow_next_answer();
    let r = put(&s.router, "second", "Idempotency-Key: k-1\r\n");
    assert_eq!(r.0, 200, "{}", r.2);
    let replayed = r.1.to_ascii_lowercase();
    assert!(replayed.contains("idempotent-replayed: true"), "{}", r.1);
    assert_eq!(titles(&s), ["first", "second"]);

    s.relay.swallow_next_answer();
    let r = call(&s.router, "GET", "/t/acme/notes", "", "");
    assert_eq!(r.0, 200, "{}", r.2);
    assert!(r.2.contains("second"), "{}", r.2);
}

/// A claim a worker sends, held up to 30 s for a job.
const CLAIM: &str = r#"{"query":"set jobs {owner: $1, run_at: now() + 60000} where run_at <= now() order run_at limit 1 returning id","params":["OWNER"]}"#;

/// How many claims the node holds for `acme`.
fn held(s: &Setup) -> usize {
    let acme = s.tenants.get("acme").unwrap();
    let n = acme.hub.waits().stats().held;
    n
}

/// Waits until the node holds `n` claims.
fn until_held(s: &Setup, n: usize) {
    let t = Instant::now();
    while held(s) < n {
        assert!(t.elapsed() < Duration::from_secs(20), "{n} never held");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// `owner`'s claim, sent to `front` to be held, on a connection of its own.
fn claim(front: &str, owner: &str) -> TcpStream {
    let body = CLAIM.replace("OWNER", owner);
    let mut c = TcpStream::connect(front).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    write!(
        c,
        "POST /t/acme/query HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\
         Fenec-Wait: 30000\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    c
}

/// Two claims held through `front` on an empty queue, and the first one's
/// client gone -- closed, or `reset` -- the job enqueued after goes to the
/// second at once. The router waiting on the node's held answer watched
/// nothing of its client, so the first claim, still open at the node, took
/// the job -- leased to nobody for a minute -- and the second waited on.
/// Asked of the node itself, the node sees its client's end before it runs
/// the claim and hands the job on; through the router, the router passes
/// the end on, or lets go of the connection on a reset. The job is put once
/// the relay has seen the router do so: put before, it would race the
/// router's look.
fn a_held_claim_s_client_gone_hands_the_job_on(s: &Setup, front: &str, reset: bool) {
    let gone = claim(front, "gone");
    until_held(s, 1);
    let mut next = claim(front, "next");
    until_held(s, 2);
    if reset {
        reset_on_close(&gone);
    }
    drop(gone);
    s.relay.until_router_closed(1);
    let r = call(
        front,
        "POST",
        "/t/acme/query",
        r#"{"query":"put jobs {run_at: 1}"}"#,
        "",
    );
    assert_eq!(r.0, 200, "{}", r.2);
    let mut answer = String::new();
    next.read_to_string(&mut answer).unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(answer.contains(r#"[{"id":"#), "the job: {answer}");
    let (status, _, owners) = call(
        &s.node,
        "POST",
        "/t/acme/query",
        r#"{"query":"get jobs select owner"}"#,
        "",
    );
    assert_eq!(status, 200, "{owners}");
    assert_eq!(owners, r#"[{"owner":"next"}]"#);
}

#[test]
fn a_held_claim_whose_client_went_away_hands_its_job_on_through_the_router() {
    let s = setup("gone", None, false);
    a_held_claim_s_client_gone_hands_the_job_on(&s, &s.router, false);
}

/// The same with the client's connection reset, which can read nothing
/// more: the router lets go of the node's connection and writes nothing.
/// Nothing else of it differs from an end where the test can see: the node
/// reads the router's end of its connection either way, and the client
/// reads nothing either way; what differs is the router reading the node's
/// answer and counting the request, `gone` or by its status, which
/// `tests/metrics.rs` holds.
#[test]
fn a_held_claim_whose_client_reset_hands_its_job_on_through_the_router() {
    let s = setup("reset-client", None, false);
    a_held_claim_s_client_gone_hands_the_job_on(&s, &s.router, true);
}

/// The same through region `us`'s router, which finds the node in its copy
/// of the global directory (`Target::Node`), its own placing nothing.
#[test]
fn a_held_claim_whose_client_went_away_hands_its_job_on_through_a_global_copy() {
    let s = setup_in("gone-copy", None, false, Front::GlobalCopy);
    a_held_claim_s_client_gone_hands_the_job_on(&s, &s.router, false);
}

/// A client that half-closes its connection once its claim is sent, and
/// reads on: on a busy queue the claim takes a job and is answered it, as
/// it is asked of the node itself, whose first run looks at no client.
/// Taken for a client gone, its end let go of the node's connection: the
/// claim took the job all the same, leased to a client answered nothing.
#[test]
fn a_half_closed_held_claim_is_answered_its_job_through_the_router() {
    let s = setup("half", None, false);
    for _ in 0..2 {
        let r = call(
            &s.router,
            "POST",
            "/t/acme/query",
            r#"{"query":"put jobs {run_at: 1}"}"#,
            "",
        );
        assert_eq!(r.0, 200, "{}", r.2);
    }
    for (front, owner) in [(&s.node, "direct"), (&s.router, "half")] {
        let mut c = claim(front, owner);
        c.shutdown(Shutdown::Write).unwrap();
        let mut answer = String::new();
        c.read_to_string(&mut answer).unwrap();
        assert!(answer.starts_with("HTTP/1.1 200"), "{owner}: {answer}");
        assert!(answer.contains(r#"[{"id":"#), "{owner}'s job: {answer}");
    }
    let (status, _, owners) = call(
        &s.node,
        "POST",
        "/t/acme/query",
        r#"{"query":"get jobs select owner order owner"}"#,
        "",
    );
    assert_eq!(status, 200, "{owners}");
    assert_eq!(owners, r#"[{"owner":"direct"},{"owner":"half"}]"#);
}

/// The same through two routers: the global router relays the claim to the
/// router of region `us`, which keeps its own directory and forwards it to
/// the node. The relaying router lets go of the home router's connection,
/// and the home router, its client gone, of the node's.
#[test]
fn a_held_claim_whose_client_went_away_hands_its_job_on_through_a_relay() {
    let s = setup_in("gone-relay", None, false, Front::Region);
    let mut d = Directory::in_memory();
    let home = [format!("http://{}", s.router)];
    d.set_region("us", Some(&home)).unwrap();
    d.place_home("acme", "us").unwrap();
    let global = Router::new(
        d,
        Config {
            addr: "127.0.0.1:0".into(),
            upstream_timeout: Duration::from_secs(10),
            ..Config::default()
        },
    );
    let listener = global.bind().unwrap();
    let front = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = global.serve_on(listener);
    });
    a_held_claim_s_client_gone_hands_the_job_on(&s, &front, false);
}

/// A node that says its answer is a petabyte long and sends three bytes of
/// it: the router answers 502 and goes on answering -- the allocation
/// refused (macOS), or lent by an overcommitting system (Linux) and the
/// answer broken off at its three bytes. Its body's buffer was made whole
/// from the length, and the allocation's failure aborted the router, every
/// tenant's client with it.
#[test]
fn a_node_declaring_an_answer_no_memory_holds_is_a_502_and_the_router_goes_on() {
    fenec_http::audit::set_delay(0);
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let node = l.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for c in l.incoming() {
            let Ok(mut c) = c else { continue };
            std::thread::spawn(move || {
                let mut asked = Vec::new();
                let mut chunk = [0u8; 4096];
                while !asked.windows(4).any(|w| w == b"\r\n\r\n") {
                    match c.read(&mut chunk) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => asked.extend_from_slice(&chunk[..n]),
                    }
                }
                let _ =
                    c.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1125899906842624\r\n\r\nabc");
            });
        }
    });
    let mut d = Directory::in_memory();
    let placed = Node {
        addr: node,
        token: "adm".into(),
        ..Node::default()
    };
    d.set_node("n1", placed).unwrap();
    d.place("acme", "n1", State::Active).unwrap();
    let router = Router::new(
        d,
        Config {
            addr: "127.0.0.1:0".into(),
            upstream_timeout: Duration::from_secs(10),
            ..Config::default()
        },
    );
    let listener = router.bind().unwrap();
    let front = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = router.serve_on(listener);
    });
    let r = call(&front, "GET", "/t/acme/notes", "", "");
    assert_eq!(r.0, 502, "{}", r.2);
    assert!(r.2.contains("broke off"), "{}", r.2);
    let r = call(&front, "GET", "/_shard/nodes", "", "");
    assert_eq!(r.0, 200, "{}", r.2);
    assert!(r.2.contains(r#""name":"n1""#), "{}", r.2);
}
