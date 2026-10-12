//! The router's pooled connections to a node, through a relay standing
//! where the network does: it passes bytes both ways and, as told, passes
//! the node's close of a connection on as a reset -- what macOS does to
//! the router's end about a minute after the node's FIN -- or drops an
//! answer the node sent and closes the router's end, as a node that ran a
//! request and went down before answering would.

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
    /// Connections open now.
    open: Mutex<usize>,
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
            open: Mutex::new(0),
            changed: Condvar::new(),
        });
        let s = Arc::clone(&shared);
        std::thread::spawn(move || {
            for router in l.incoming() {
                let Ok(router) = router else { continue };
                *s.open.lock().unwrap() += 1;
                let s = Arc::clone(&s);
                std::thread::spawn(move || {
                    pass(&s, router);
                    *s.open.lock().unwrap() -= 1;
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
            .wait_timeout_while(open, Duration::from_secs(30), |n| *n > 0)
            .unwrap();
        assert!(!waited.timed_out(), "{} connections still open", *open);
    }
}

/// One connection from the router, passed on to the node until either end
/// closes it.
fn pass(s: &Shared, router: TcpStream) {
    let node = TcpStream::connect(&s.node).unwrap();
    let mut from_router = router.try_clone().unwrap();
    let mut to_node = node.try_clone().unwrap();
    let up = std::thread::spawn(move || {
        let mut buf = [0u8; 16 << 10];
        while let Ok(n @ 1..) = from_router.read(&mut buf) {
            if to_node.write_all(&buf[..n]).is_err() {
                break;
            }
        }
        // The router closed its end, or the relay is letting go: the node
        // is told as the router would tell it.
        let _ = to_node.shutdown(Shutdown::Write);
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
    dir: PathBuf,
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A node holding tenant `acme` with a collection `notes`, the relay in
/// front of it and a router in front of that. The directory is written
/// before the router starts, so nothing the test does not send goes from
/// the router to the node: no request but the test's meets the node's
/// idle timeout on its way.
fn setup(tag: &str, idle_timeout: Option<Duration>, reset: bool) -> Setup {
    fenec_http::audit::set_delay(0);
    let dir = std::env::temp_dir().join(format!(
        "fenec-shard-upstream-{tag}-{}-{:?}",
        std::process::id(),
        Instant::now()
    ));
    let tenants = Arc::new(Tenants::new(&dir).unwrap());
    let acme = tenants.create("acme").unwrap();
    let create = fenec_ql::parse_one("create collection notes (title text)").unwrap();
    acme.db.write().unwrap().execute(&create).unwrap();
    drop(acme);
    let server = fenec_http::Server::with_tenants(
        tenants,
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
    let mut d = Directory::in_memory();
    let (addr, token) = (relay.addr.clone(), "adm".to_string());
    d.set_node("n1", Node { addr, token }).unwrap();
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
    Setup {
        router: front,
        node,
        relay,
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
