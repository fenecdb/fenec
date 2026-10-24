//! What the cut tests stand up between a router and its nodes: a wire
//! that falls silent, swallows answers, holds one part way or parks the
//! requests naming a marker, and the requests a test sends through a
//! router and waits on. Shared by `cuts.rs` and `follower_cuts.rs`
//! (`#[path]`), so not every helper is used by each.

#![allow(dead_code)]

use crate::nodes::{call, number_after, query};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// The router's wait for a node: an answer within [`WITHIN`] is not its.
pub const UPSTREAM: Duration = Duration::from_secs(60);
/// How long a client here waits for its answer: half the router's timeout.
pub const WITHIN: Duration = Duration::from_secs(30);

/// The network between the router and one node, as a test breaks it.
/// Silent, nothing it carries goes on either way and nothing is closed,
/// and connections made after are taken and never answered -- a machine
/// gone, as its peers see it; swallowing, the node's answers to anything
/// but a replica's stream go nowhere, so the node runs a request, its copy
/// gets the write, and the router never hears; holding, the next such
/// answer goes on as far as it says and waits there. What the router sent
/// while silent or swallowing is kept, for a test to wait on.
pub struct Wire {
    pub addr: String,
    s: Arc<WireState>,
}

#[derive(Default)]
pub struct WireState {
    silent: AtomicBool,
    swallowing: AtomicBool,
    heard: Mutex<Vec<u8>>,
    came: Condvar,
    /// The next answer's bytes to pass before it waits, `0` for none.
    hold: AtomicUsize,
    /// `(passed, released)` of the answer held.
    held: Mutex<(bool, bool)>,
    turned: Condvar,
    traffic: Mutex<Traffic>,
    moved: Condvar,
}

/// What the router sent through a wire, whatever the wire did with it, and
/// the requests parked there.
#[derive(Default)]
pub struct Traffic {
    sent: Vec<u8>,
    /// `(marker, parked, let through)`: a request naming `marker` waits at
    /// the wire until one is let through.
    parks: Vec<(String, usize, usize)>,
}

impl WireState {
    fn hear(&self, bytes: &[u8]) {
        self.heard.lock().unwrap().extend_from_slice(bytes);
        self.came.notify_all();
    }

    /// `bytes` from the router, kept; where they name a parked marker,
    /// waited on until one such request is let through.
    fn pass(&self, bytes: &[u8]) {
        let mut traffic = self.traffic.lock().unwrap();
        traffic.sent.extend_from_slice(bytes);
        self.moved.notify_all();
        let named = |m: &str| bytes.windows(m.len()).any(|w| w == m.as_bytes());
        let Some(i) = traffic.parks.iter().position(|(m, ..)| named(m)) else {
            return;
        };
        traffic.parks[i].1 += 1;
        self.moved.notify_all();
        while traffic.parks[i].2 == 0 {
            traffic = self.moved.wait(traffic).unwrap();
        }
        traffic.parks[i].2 -= 1;
    }
}

impl Wire {
    pub fn to(port: u16) -> Wire {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let s = Arc::new(WireState::default());
        let shared = Arc::clone(&s);
        let target = format!("127.0.0.1:{port}");
        std::thread::spawn(move || {
            for router in listener.incoming() {
                let Ok(router) = router else { continue };
                let s = Arc::clone(&shared);
                if s.silent.load(Ordering::SeqCst) {
                    std::thread::spawn(move || swallow(router, &s));
                    continue;
                }
                let Ok(node) = TcpStream::connect(&target) else {
                    continue;
                };
                let replica = Arc::new(AtomicBool::new(false));
                let (r2, n2) = (router.try_clone().unwrap(), node.try_clone().unwrap());
                let (rep2, s2) = (Arc::clone(&replica), Arc::clone(&s));
                std::thread::spawn(move || up(router, node, &replica, &s));
                std::thread::spawn(move || down(n2, r2, &rep2, &s2));
            }
        });
        Wire { addr, s }
    }

    pub fn fall_silent(&self) {
        self.s.silent.store(true, Ordering::SeqCst);
    }

    pub fn swallow_answers(&self) {
        self.s.swallowing.store(true, Ordering::SeqCst);
    }

    /// Waits until the router has sent `marker` through while silent or
    /// swallowing: its request is out.
    pub fn until_heard(&self, marker: &str) {
        let heard = self.s.heard.lock().unwrap();
        let (heard, waited) = self
            .s
            .came
            .wait_timeout_while(heard, WITHIN, |h| {
                !h.windows(marker.len()).any(|w| w == marker.as_bytes())
            })
            .unwrap();
        assert!(
            !waited.timed_out(),
            "{marker} never came: {}",
            String::from_utf8_lossy(&heard)
        );
    }

    /// The next answer passes its first `n` bytes, then waits.
    pub fn hold_answer_after(&self, n: usize) {
        self.s.hold.store(n, Ordering::SeqCst);
    }

    /// Waits until the answer held has passed its bytes: written whole into
    /// the router's socket.
    pub fn until_passed(&self) {
        let held = self.s.held.lock().unwrap();
        let (_held, waited) = self
            .s
            .turned
            .wait_timeout_while(held, WITHIN, |h| !h.0)
            .unwrap();
        assert!(!waited.timed_out(), "no answer came to be held");
    }

    pub fn release(&self) {
        self.s.held.lock().unwrap().1 = true;
        self.s.turned.notify_all();
    }

    /// Requests from the router naming `marker` wait at the wire from now,
    /// each until [`Wire::let_through`] lets one on.
    pub fn park(&self, marker: &str) {
        let mut traffic = self.s.traffic.lock().unwrap();
        traffic.parks.push((marker.to_string(), 0, 0));
    }

    /// Lets `n` more requests naming `marker` on.
    pub fn let_through(&self, marker: &str, n: usize) {
        let mut traffic = self.s.traffic.lock().unwrap();
        let park = traffic.parks.iter_mut().find(|(m, ..)| m == marker);
        park.expect("a marker parked").2 += n;
        self.s.moved.notify_all();
    }

    /// Waits until `n` requests naming `marker` have come to its park.
    pub fn until_parked(&self, marker: &str, n: usize) {
        let traffic = self.s.traffic.lock().unwrap();
        let parked = |t: &Traffic| t.parks.iter().find(|(m, ..)| m == marker).map(|p| p.1);
        let (traffic, waited) = self
            .s
            .moved
            .wait_timeout_while(traffic, WITHIN, |t| parked(t).unwrap_or(0) < n)
            .unwrap();
        assert!(
            !waited.timed_out(),
            "{n} requests naming {marker} never came: {:?}",
            parked(&traffic)
        );
    }

    /// How many times the router has sent `marker` through the wire.
    pub fn times_sent(&self, marker: &str) -> usize {
        let traffic = self.s.traffic.lock().unwrap();
        traffic
            .sent
            .windows(marker.len())
            .filter(|w| *w == marker.as_bytes())
            .count()
    }

    /// Waits until the router has sent `marker` through the wire `n` times.
    pub fn until_sent(&self, marker: &str, n: usize) {
        let traffic = self.s.traffic.lock().unwrap();
        let times = |t: &Traffic| {
            t.sent
                .windows(marker.len())
                .filter(|w| *w == marker.as_bytes())
                .count()
        };
        let (traffic, waited) = self
            .s
            .moved
            .wait_timeout_while(traffic, WITHIN, |t| times(t) < n)
            .unwrap();
        assert!(
            !waited.timed_out(),
            "{marker} came {} times of {n}",
            times(&traffic)
        );
    }
}

/// A connection taken while silent: what it sends is heard, and nothing
/// answered or closed.
pub fn swallow(mut router: TcpStream, s: &WireState) {
    let mut buf = [0u8; 16 << 10];
    while let Ok(n @ 1..) = router.read(&mut buf) {
        s.hear(&buf[..n]);
    }
}

/// The router's side to the node's. A connection whose first request is a
/// replica's stream (`/_replication`) is `replica`'s, whose answers flow
/// while the node's others are swallowed.
pub fn up(mut from: TcpStream, mut to: TcpStream, replica: &AtomicBool, s: &WireState) {
    let mut buf = [0u8; 16 << 10];
    let mut first = true;
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if std::mem::take(&mut first) && buf[..n].windows(12).any(|w| w == b"_replication") {
            replica.store(true, Ordering::SeqCst);
        }
        let silent = s.silent.load(Ordering::SeqCst);
        if (silent || s.swallowing.load(Ordering::SeqCst)) && !replica.load(Ordering::SeqCst) {
            s.hear(&buf[..n]);
        }
        if !replica.load(Ordering::SeqCst) {
            s.pass(&buf[..n]);
        }
        if silent {
            continue;
        }
        if to.write_all(&buf[..n]).is_err() {
            break;
        }
    }
    if !s.silent.load(Ordering::SeqCst) {
        let _ = to.shutdown(Shutdown::Write);
    }
}

/// The node's side to the router's.
pub fn down(mut from: TcpStream, mut to: TcpStream, replica: &AtomicBool, s: &WireState) {
    let mut buf = [0u8; 16 << 10];
    let mut holding: Option<usize> = None;
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let replica = replica.load(Ordering::SeqCst);
        if s.silent.load(Ordering::SeqCst) || (!replica && s.swallowing.load(Ordering::SeqCst)) {
            continue;
        }
        let mut chunk = &buf[..n];
        if !replica && holding.is_none() {
            let n = s.hold.swap(0, Ordering::SeqCst);
            holding = (n > 0).then_some(n);
        }
        if let Some(left) = holding {
            let pass = left.min(chunk.len());
            if to.write_all(&chunk[..pass]).is_err() {
                break;
            }
            chunk = &chunk[pass..];
            holding = Some(left - pass);
            if left == pass {
                let mut held = s.held.lock().unwrap();
                held.0 = true;
                s.turned.notify_all();
                while !held.1 {
                    held = s.turned.wait(held).unwrap();
                }
                drop(held);
                // Held once: `Some(0)` arms no other.
            }
        }
        if to.write_all(chunk).is_err() {
            break;
        }
    }
    if !s.silent.load(Ordering::SeqCst) {
        let _ = to.shutdown(Shutdown::Write);
    }
}

pub const PATH: &str = "/t/acme/query";

/// `sql` as a `/query` body.
pub fn body(sql: &str) -> String {
    let mut b = String::from("{\"query\":");
    fenec_core::json::escape_into(&mut b, sql);
    b.push('}');
    b
}

/// A claim of the oldest ready job for `owner`, held on an empty queue.
pub fn claim(owner: &str) -> String {
    format!(
        r#"{{"query":"set jobs {{owner: $1, run_at: now() + 60000}} where run_at <= now() order run_at limit 1 returning id","params":["{owner}"]}}"#
    )
}

/// The status, head and body of one request through the router, on a
/// connection of its own, waited for [`WITHIN`]: status 0 where none came.
pub type Asked = (u16, String, String);

pub fn ask(port: u16, method: &str, target: &str, extra: &str, body: &str) -> Asked {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(WITHIN)).unwrap();
    let req = format!(
        "{method} {target} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n{extra}\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut out = Vec::new();
    if let Err(e) = s.read_to_end(&mut out) {
        return (0, String::new(), format!("no answer in {WITHIN:?}: {e}"));
    }
    let out = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = out.split_once("\r\n\r\n").unwrap_or((&out, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, head.to_string(), body.to_string())
}

/// [`ask`] on a thread of its own: its answer, once it comes.
pub fn asking(port: u16, method: &str, target: &str, extra: &str, body: &str) -> Receiver<Asked> {
    let (tx, rx) = channel();
    let (method, target, extra, body) = (
        method.to_string(),
        target.to_string(),
        extra.to_string(),
        body.to_string(),
    );
    std::thread::spawn(move || {
        let _ = tx.send(ask(port, &method, &target, &extra, &body));
    });
    rx
}

pub fn answer(rx: &Receiver<Asked>, what: &str) -> Asked {
    rx.recv_timeout(WITHIN + Duration::from_secs(5))
        .unwrap_or_else(|e| panic!("{what}: no answer: {e}"))
}

pub fn unanswered(rx: &Receiver<Asked>, what: &str) {
    match rx.try_recv() {
        Err(TryRecvError::Empty) => {}
        other => panic!("{what} was answered before the failover: {other:?}"),
    }
}

/// A subscription through the router, its seed in, read on a thread of its
/// own: told as it ends, `true` for its end and `false` for a client's wait
/// run out.
pub fn subscribed(port: u16) -> Receiver<bool> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(WITHIN)).unwrap();
    s.write_all(b"GET /t/acme/notes/changes HTTP/1.1\r\nHost: t\r\n\r\n")
        .unwrap();
    let mut seen = Vec::new();
    let mut chunk = [0u8; 4096];
    while !seen.windows(11).any(|w| w == b"event: seed") {
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => panic!("no seed: {}", String::from_utf8_lossy(&seen)),
            Ok(n) => seen.extend_from_slice(&chunk[..n]),
        }
    }
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let ended = loop {
            match s.read(&mut chunk) {
                Ok(0) => break true,
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break false,
            }
        };
        let _ = tx.send(ended);
    });
    rx
}

/// How many of `acme`'s notes on the node at `port` are titled `title`.
pub fn titled(port: u16, title: &str) -> usize {
    let sql = format!(r#"get notes where title = "{title}" count"#);
    let (status, b) = query(port, PATH, &sql);
    assert_eq!(status, 200, "{b}");
    number_after(&b, "\"count\":") as usize
}

/// `fenec_router_cut_requests_total` of each outcome.
pub fn cuts(port: u16) -> [f64; 3] {
    let (status, scrape) = call(port, "GET", "/_metrics", "", None);
    assert_eq!(status, 200, "{scrape}");
    ["sent_again", "may_have_run", "stream_ended"].map(|o| {
        number_after(
            &scrape,
            &format!("fenec_router_cut_requests_total{{outcome=\"{o}\"}} "),
        )
    })
}
