//! A grant a node reads late runs no lease past its failover, and one whose
//! answer the router never had runs none at all
//! (`fenec_http::lease::Numbered`).
//!
//! A node ran its lease from when it read a grant, so one the router gave
//! up on -- the node stopped, its machine paused, the request held on its
//! way -- and read after the router had failed the node over took a whole
//! lease, the old primary writing beside the new one (the review of #340,
//! `late_grant.rs`). Each grant now names the newest grant whose answer
//! came back in time, and the node runs its lease from when it answered
//! that one. Here the router's requests to n1 are kept from it -- taken
//! whole at a proxy and left unanswered, for the test to deliver late --
//! or its answers dropped on their way back.
//!
//! A binary of its own, as `lease_stalls` is: three nodes and a router a
//! test, connections kept open unanswered, beside `all` near macOS's 2 048
//! threads a process.

#[path = "lease_nodes.rs"]
mod nodes;

use fenec_core::prelude::Database;
use fenec_http::replication::{fresh_id, Follower, Replication};
use fenec_shard::directory::Directory;
use fenec_shard::{Config, Router};
use nodes::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// A proxy in front of a node, which forwards until told otherwise: to
/// take requests -- the next naming a pattern, or every one -- whole,
/// never forwarded and never answered, the connection left open as a node
/// stalled with the bytes in its socket leaves it; to drop the node's
/// answers, every request still reaching it; or to carry nothing.
struct Stall {
    addr: String,
    state: Arc<Mutex<Taking>>,
    cut: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    open: Arc<Mutex<Vec<TcpStream>>>,
}

#[derive(Default)]
struct Taking {
    /// The next request whose bytes hold this is taken.
    next: Option<&'static [u8]>,
    /// Every request is taken.
    all: bool,
    /// What was taken, in the order it came.
    taken: Vec<Vec<u8>>,
}

impl Stall {
    fn to(target: String) -> Stall {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let state = Arc::new(Mutex::new(Taking::default()));
        let cut = Arc::new(AtomicBool::new(false));
        let muted = Arc::new(AtomicBool::new(false));
        let open = Arc::new(Mutex::new(Vec::new()));
        let (s, c, m, o) = (
            Arc::clone(&state),
            Arc::clone(&cut),
            Arc::clone(&muted),
            Arc::clone(&open),
        );
        std::thread::spawn(move || {
            for client in l.incoming() {
                let Ok(client) = client else { continue };
                if c.load(Ordering::SeqCst) {
                    let _ = client.shutdown(Shutdown::Both);
                    continue;
                }
                let Ok(server) = TcpStream::connect(&target) else {
                    continue;
                };
                o.lock().unwrap().push(client.try_clone().unwrap());
                o.lock().unwrap().push(server.try_clone().unwrap());
                let (from, to, s) = (
                    client.try_clone().unwrap(),
                    server.try_clone().unwrap(),
                    Arc::clone(&s),
                );
                std::thread::spawn(move || requests(from, to, &s));
                let m = Arc::clone(&m);
                std::thread::spawn(move || answers(server, client, &m));
            }
        });
        Stall {
            addr,
            state,
            cut,
            muted,
            open,
        }
    }

    /// Takes the next request whose bytes hold `pattern`.
    fn take_next(&self, pattern: &'static [u8]) {
        self.state.lock().unwrap().next = Some(pattern);
    }

    /// Takes every request from now on; what it carries is ended, so a
    /// pooled client connects again and is taken too.
    fn take_all(&self) {
        self.state.lock().unwrap().all = true;
        self.end_open();
    }

    /// Drops every answer from now on: the requests reach the node, and
    /// the router hears nothing back.
    fn mute(&self) {
        self.muted.store(true, Ordering::SeqCst);
    }

    fn cut(&self) {
        self.cut.store(true, Ordering::SeqCst);
        self.end_open();
    }

    fn heal(&self) {
        self.cut.store(false, Ordering::SeqCst);
    }

    fn end_open(&self) {
        for s in self.open.lock().unwrap().drain(..) {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    /// The lease grants taken so far, oldest first.
    fn grants(&self) -> Vec<Vec<u8>> {
        let s = self.state.lock().unwrap();
        let grants = s
            .taken
            .iter()
            .filter(|r| r.starts_with(b"POST /_admin/lease"));
        grants.cloned().collect()
    }
}

/// Carries `from`'s requests to `to` one whole request at a time, each
/// taken instead where the proxy says so.
fn requests(mut from: TcpStream, mut to: TcpStream, state: &Mutex<Taking>) {
    let (mut buf, mut chunk) = (Vec::new(), [0u8; 16 * 1024]);
    loop {
        while let Some(end) = request_end(&buf) {
            let req: Vec<u8> = buf.drain(..end).collect();
            let taken = {
                let mut s = state.lock().unwrap();
                let named = s.next.is_some_and(|p| req.windows(p.len()).any(|w| w == p));
                if named {
                    s.next = None;
                }
                if named || s.all {
                    s.taken.push(req.clone());
                }
                named || s.all
            };
            if !taken && to.write_all(&req).is_err() {
                return;
            }
        }
        match from.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let _ = to.shutdown(Shutdown::Write);
}

/// Carries the node's answers back, or drops them while `muted`.
fn answers(mut from: TcpStream, mut to: TcpStream, muted: &AtomicBool) {
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let n = match from.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if !muted.load(Ordering::SeqCst) && to.write_all(&chunk[..n]).is_err() {
            break;
        }
    }
    let _ = to.shutdown(Shutdown::Write);
}

/// Where the first whole request in `buf` ends: its head and the body its
/// `Content-Length` names, every request of the router's carrying one.
fn request_end(buf: &[u8]) -> Option<usize> {
    let head = buf.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let text = String::from_utf8_lossy(&buf[..head]).to_ascii_lowercase();
    let length: usize = text
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .map_or(0, |v| v.trim().parse().unwrap_or(0));
    (buf.len() >= head + length).then_some(head + length)
}

/// `request` sent to `port` as it was taken: the status and body the node
/// answers.
fn deliver(port: u16, request: &[u8]) -> (u16, String) {
    let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    (&s).write_all(request).unwrap();
    let mut r = BufReader::new(&s);
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    let status = line.get(9..12).and_then(|c| c.parse().ok()).unwrap_or(0);
    let mut length = 0;
    loop {
        let mut h = String::new();
        r.read_line(&mut h).unwrap();
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                length = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; length];
    r.read_exact(&mut body).unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// The value of `"<key>":"` in a grant's body, as a grant's numbers are.
fn hex_field(request: &[u8], key: &str) -> Option<String> {
    let text = String::from_utf8_lossy(request);
    let rest = text.split(&format!("\"{key}\":\"")).nth(1)?;
    Some(rest.split('"').next()?.to_string())
}

/// A router over `nodes`' addresses leasing for `term`, its rounds run by
/// the leasing thread where `leasing`, by the test otherwise.
fn router(addrs: &[String; 3], tags: &str, leasing: bool, term: Duration) -> (Arc<Router>, u16) {
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
    if leasing {
        router.start_leasing().unwrap();
    }
    let serving = Arc::clone(&router);
    std::thread::spawn(move || {
        let _ = serving.serve_on(listener);
    });
    for (i, a) in addrs.iter().enumerate() {
        let body = format!(r#"{{"addr":"{a}","token":"adm-{tags}{}"}}"#, i + 1);
        let (status, body) = call(
            port,
            "PUT",
            &format!("/_shard/nodes/n{}", i + 1),
            &body,
            None,
        );
        assert!(status == 200 || status == 201, "{body}");
    }
    (router, port)
}

/// `acme` on n1 holding a row, its replica caught up: the replica's node.
fn acme_on_n1(port: u16, nodes: &[Node; 3]) -> String {
    let (status, body) = call(
        port,
        "PUT",
        "/_shard/tenants/acme",
        r#"{"node":"n1"}"#,
        None,
    );
    assert_eq!(status, 201, "{body}");
    let path = "/t/acme/query";
    let (status, body) = query(port, path, "create collection notes (title text)");
    assert_eq!(status, 200, "{body}");
    let (status, body) = query(port, path, r#"put notes {title: "one"}"#);
    assert_eq!(status, 200, "{body}");
    let replica = placed(port, "acme").1.expect("acme has a replica");
    until(
        by_name(nodes, &replica).port,
        path,
        "get notes select title",
        "one",
    );
    replica
}

/// Waits until `acme` is placed off n1: its failover has come.
fn failed_over(port: u16, mut round: impl FnMut()) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while placed(port, "acme").0 == "n1" {
        assert!(Instant::now() < deadline, "acme was never failed over");
        round();
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A write to acme through the router once its failover is done, as a
/// client sends it: the directory flips before the grant naming acme
/// reaches its new node, which answers a write in between 503 with
/// `retry_after` -- the one answer waited out. Asked once, it failed at
/// background QoS, the grant still on its way. `round` runs between tries.
fn new_primary_writes(port: u16, mut round: impl FnMut()) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (status, body) = query(port, "/t/acme/query", r#"put notes {title: "new primary"}"#);
        if status == 200 {
            return;
        }
        assert!(
            status == 503 && body.contains("retry_after") && Instant::now() < deadline,
            "acme's new primary: {status} {body}"
        );
        round();
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// n1 writing acme straight to it, as a client that still reaches it would.
fn n1_writes_acme(nodes: &[Node; 3]) -> (u16, String) {
    query(
        nodes[0].port,
        "/t/acme/query",
        r#"put notes {title: "old primary"}"#,
    )
}

/// n1's lease, as it says.
fn lease_of(nodes: &[Node; 3], tag: &str) -> String {
    let (status, lease) = call(
        nodes[0].port,
        "GET",
        "/_admin/lease",
        "",
        Some(&format!("adm-{tag}1")),
    );
    assert_eq!(status, 200, "{lease}");
    lease
}

/// The reviewer's repro (`late_grant.rs`): n1 stalls with the router's
/// grants in its socket, unread; the router gives up on each, waits out
/// the lease and a tenth since n1's last answer and promotes acme's
/// replica. n1 then reads the grants that waited -- each read, oldest
/// first, as the node would, and none runs it a lease: each acknowledges
/// at most the last answer the router had, whose lease the failover
/// outlasted. Run from when n1 read it, the first gave n1 a whole lease
/// over acme, and n1 took acme's write beside its new primary.
#[test]
fn a_grant_read_late_runs_no_lease_past_its_node_s_failover() {
    let nodes = [1, 2, 3].map(|i| node(&format!("late{i}"), true));
    let stall = Stall::to(format!("127.0.0.1:{}", nodes[0].port));
    let addrs = [
        stall.addr.clone(),
        nodes[1].proxy.addr.clone(),
        nodes[2].proxy.addr.clone(),
    ];
    let (_router, port) = router(&addrs, "late", true, TERM);
    acme_on_n1(port, &nodes);

    stall.take_all();
    let t0 = Instant::now();
    while stall.grants().is_empty() {
        assert!(t0.elapsed() < Duration::from_secs(20), "no grant came");
        std::thread::sleep(Duration::from_millis(10));
    }
    failed_over(port, || {});
    new_primary_writes(port, || {});

    let grants = stall.grants();
    for (i, grant) in grants.iter().enumerate() {
        let (status, answer) = deliver(nodes[0].port, grant);
        assert!(
            status == 200 || status == 412,
            "grant {i}: {status} {answer}"
        );
        let (direct, body) = n1_writes_acme(&nodes);
        assert_ne!(
            direct,
            200,
            "two primaries: n1 took acme's write after the failover, the late grant {i} of {} \
             read ({body})",
            grants.len()
        );
    }
    assert!(
        lease_of(&nodes, "late").contains("\"left_ms\":0"),
        "{}",
        lease_of(&nodes, "late")
    );
}

/// n1 reads every grant at once, and its answers are lost on the way back:
/// the router hears nothing, and fails n1 over a lease and a tenth after
/// the last answer it had. n1 answered each grant, and each acknowledged
/// that last answer: its lease ran out before the failover, though n1
/// heard from the router up to it. Run from each grant as read, it ran on
/// past the failover, and n1 wrote acme beside its new primary -- until the
/// grant naming the list the failover left reached it, which is held here
/// on its way: lost, or merely slower than the client.
#[test]
fn a_grant_whose_answer_is_lost_runs_no_lease() {
    let nodes = [1, 2, 3].map(|i| node(&format!("mute{i}"), true));
    let stall = Stall::to(format!("127.0.0.1:{}", nodes[0].port));
    let addrs = [
        stall.addr.clone(),
        nodes[1].proxy.addr.clone(),
        nodes[2].proxy.addr.clone(),
    ];
    let (_router, port) = router(&addrs, "mute", true, TERM);
    acme_on_n1(port, &nodes);
    let answered = hex_field(lease_of(&nodes, "mute").as_bytes(), "grant");

    // Until the failover every grant names n1's list by its epoch alone;
    // the failover's names the list it left.
    stall.mute();
    stall.take_next(br#""primaries":"#);
    failed_over(port, || {});
    new_primary_writes(port, || {});
    let (direct, body) = n1_writes_acme(&nodes);
    assert_ne!(
        direct, 200,
        "two primaries: n1 took acme's write after the failover ({body})"
    );
    // n1 went on answering grants: none ran its lease.
    let lease = lease_of(&nodes, "mute");
    assert_ne!(hex_field(lease.as_bytes(), "grant"), answered, "{lease}");
    assert!(lease.contains("\"left_ms\":0"), "{lease}");
}

/// A grant held on its way and read once n1 is back from its failover --
/// its lease running again, over a list that no longer names acme -- is
/// taken not at all: its list named acme, and taken beside the newer
/// lease it would have n1 write acme again, the lease running from the
/// newer grant's acknowledgement and the list from the late one. The
/// grant is the one acme's create sent n1, the only kind of grant to carry
/// a list n1 does not hold; the rounds are the test's, so no grant reaches
/// n1 between the late one and the look at its lease.
#[test]
fn a_grant_read_late_takes_no_list_beside_a_newer_lease() {
    let nodes = [1, 2, 3].map(|i| node(&format!("stale{i}"), true));
    let stall = Stall::to(format!("127.0.0.1:{}", nodes[0].port));
    let addrs = [
        stall.addr.clone(),
        nodes[1].proxy.addr.clone(),
        nodes[2].proxy.addr.clone(),
    ];
    let (router, port) = router(&addrs, "stale", false, LONG);
    // Every node leased, n1 over nothing; then acme's create, whose grant
    // to n1 -- the list naming acme -- is held on its way.
    round(&router);
    stall.take_next(br#""primaries":["acme"]"#);
    let (status, body) = call(
        port,
        "PUT",
        "/_shard/tenants/acme",
        r#"{"node":"n1"}"#,
        None,
    );
    assert_eq!(status, 201, "{body}");
    let late = stall.grants();
    assert_eq!(late.len(), 1, "the create's grant was not held");
    // The next round names acme too, and n1 takes it.
    round(&router);
    let path = "/t/acme/query";
    let (status, body) = query(port, path, "create collection notes (title text)");
    assert_eq!(status, 200, "{body}");
    assert_eq!(query(port, path, r#"put notes {title: "one"}"#).0, 200);
    let replica = placed(port, "acme").1.expect("acme has a replica");
    until(
        by_name(&nodes, &replica).port,
        path,
        "get notes select title",
        "one",
    );

    // n1 cut off, failed over, and back: leased again over nothing.
    stall.cut();
    failed_over(port, || round(&router));
    stall.heal();
    let deadline = Instant::now() + Duration::from_secs(30);
    let held = loop {
        round(&router);
        let lease = lease_of(&nodes, "stale");
        if lease.contains("\"primaries\":0") && !lease.contains("\"left_ms\":0") {
            break lease;
        }
        assert!(
            Instant::now() < deadline,
            "n1 was never leased again: {lease}"
        );
        std::thread::sleep(Duration::from_millis(20));
    };

    let (status, answer) = deliver(nodes[0].port, &late[0]);
    assert_eq!(status, 200, "{answer}");
    let after = lease_of(&nodes, "stale");
    // Read: the late grant is the newest n1 answered.
    assert_eq!(
        hex_field(after.as_bytes(), "grant"),
        hex_field(&late[0], "grant"),
        "{after}"
    );
    let epoch = |l: &str| hex_field(l.as_bytes(), "epoch");
    assert!(
        after.contains("\"primaries\":0") && epoch(&after) == epoch(&held),
        "the late grant's list named acme again beside the lease n1 holds: {after}, was {held}"
    );
    assert!(!after.contains("\"left_ms\":0"), "{after}");
    assert_ne!(n1_writes_acme(&nodes).0, 200);
}

/// How a scripted node answers its `n`th grant, given its body: the
/// status and body, or `None` for no answer.
type Script = dyn Fn(usize, &str) -> Option<(u16, String)> + Send + Sync;

/// A node that answers grants as a script says: its stats for the router
/// to record it, every grant's body kept, and each answered with what the
/// script returns for it -- `None` for no answer, the connection held
/// until the router gives up.
fn scripted_node(
    token: &str,
    script: impl Fn(usize, &str) -> Option<(u16, String)> + Send + Sync + 'static,
) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let grants = Arc::new(Mutex::new(Vec::new()));
    let (kept, token, script) = (
        Arc::clone(&grants),
        format!("Bearer {token}"),
        Arc::new(script),
    );
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let (kept, token, script) = (Arc::clone(&kept), token.clone(), Arc::clone(&script));
            std::thread::spawn(move || answer_scripted(conn, &kept, &token, &*script));
        }
    });
    (addr, grants)
}

fn answer_scripted(conn: TcpStream, kept: &Mutex<Vec<String>>, token: &str, script: &Script) {
    let mut out = conn.try_clone().unwrap();
    let mut r = BufReader::new(conn);
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let (mut length, mut authorized) = (0usize, false);
        loop {
            let mut h = String::new();
            if r.read_line(&mut h).unwrap_or(0) == 0 {
                return;
            }
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                if k.eq_ignore_ascii_case("content-length") {
                    length = v.trim().parse().unwrap_or(0);
                }
                authorized |= k.eq_ignore_ascii_case("authorization") && v.trim() == token;
            }
        }
        let mut body = vec![0u8; length];
        if r.read_exact(&mut body).is_err() {
            return;
        }
        let (status, answer) = if line.contains("/_admin/lease") && authorized {
            let body = String::from_utf8_lossy(&body).into_owned();
            let n = {
                let mut k = kept.lock().unwrap();
                k.push(body.clone());
                k.len() - 1
            };
            match script(n, &body) {
                Some(a) => a,
                // Held until the router gives up and ends the connection.
                None => {
                    let mut rest = Vec::new();
                    let _ = r.read_to_end(&mut rest);
                    return;
                }
            }
        } else if line.contains("/_admin/stats") && authorized {
            (200, "{}".to_string())
        } else {
            (404, "{}".to_string())
        };
        let head = format!(
            "HTTP/1.1 {status} X\r\nContent-Length: {}\r\n\r\n",
            answer.len()
        );
        if out
            .write_all(head.as_bytes())
            .and_then(|_| out.write_all(answer.as_bytes()))
            .is_err()
        {
            return;
        }
    }
}

/// What a node says of its lease, as a grant's answer: `left` its
/// milliseconds left (`None`, none), whether it took the grant, and the
/// grant's number answered -- a node from before numbers says neither.
fn lease_answer(left: Option<u64>, took: bool, numbered: bool) -> Option<(u16, String)> {
    let left = left.map_or("null".to_string(), |l| l.to_string());
    let (took, grant) = match numbered {
        true => (
            format!(r#""took":{took},"#),
            r#","grant":"0000000000000001""#,
        ),
        false => (String::new(), ""),
    };
    Some((
        200,
        format!(
            r#"{{{took}"epoch":"e","primaries":0,"left_ms":{left},"authority":"","term":0{grant}}}"#
        ),
    ))
}

/// Each grant to a node has a number of its own and acknowledges the
/// newest grant whose answer came back in time: none before one did; the
/// grant sent again with the list after a 412 acknowledges what the first
/// did, under a number of its own; one given up on is acknowledged by no
/// grant after it. A node whose answer says its lease is short -- none, or
/// less than lasts to the next round -- or that it did not take the grant
/// is sent at once the grant that acknowledges it, with the list where it
/// did not take it; and a node from before numbered grants, whose lease
/// runs from each grant, is not.
#[test]
fn each_grant_acknowledges_the_newest_answer_that_came_back() {
    let (addr, grants) = scripted_node("adm-script", |n, _| match n {
        // The first grant, then the one sent at once acknowledging it.
        0 => lease_answer(None, true, true),
        1 => lease_answer(Some(3_000), true, true),
        // A round: asked for the list, then the grant sent with it.
        2 => Some((412, r#"{"error":"send the primaries"}"#.into())),
        3 => lease_answer(Some(2_500), true, true),
        // A round given up on.
        4 => None,
        // A round whose answer is short: acknowledged at once.
        5 => lease_answer(Some(100), true, true),
        6 => lease_answer(Some(3_000), true, true),
        // A round not taken, a lease from before running long: acknowledged
        // at once, with the list.
        7 => lease_answer(Some(3_000), false, true),
        8 => lease_answer(Some(3_000), true, true),
        // A node from before numbers, holding no lease by what it says.
        _ => lease_answer(None, true, false),
    });
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        upstream_timeout: Duration::from_secs(10),
        auto_failover: Some(TERM),
        ..Config::default()
    };
    let router = Router::new(Directory::in_memory(), cfg);
    let listener = router.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    let serving = Arc::clone(&router);
    std::thread::spawn(move || {
        let _ = serving.serve_on(listener);
    });
    let body = format!(r#"{{"addr":"{addr}","token":"adm-script"}}"#);
    let (status, body) = call(port, "PUT", "/_shard/nodes/n1", &body, None);
    assert!(status == 200 || status == 201, "{body}");
    for _ in 0..6 {
        round(&router);
    }
    let sent = grants.lock().unwrap().clone();
    assert_eq!(sent.len(), 10, "{sent:#?}");
    let grant = |i: usize| hex_field(sent[i].as_bytes(), "grant").unwrap();
    let acked = |i: usize| hex_field(sent[i].as_bytes(), "acked");
    for i in 1..sent.len() {
        let (a, b) = (
            u64::from_str_radix(&grant(i - 1), 16),
            u64::from_str_radix(&grant(i), 16),
        );
        assert_eq!(b.unwrap(), a.unwrap().wrapping_add(1), "{sent:#?}");
    }
    assert_eq!(
        acked(0),
        None,
        "the first grant acknowledged {:?}",
        acked(0)
    );
    assert_eq!(
        acked(1),
        Some(grant(0)),
        "sent at once, acknowledging the first"
    );
    assert_eq!(acked(2), Some(grant(1)));
    assert!(!sent[2].contains("primaries") && sent[3].contains("\"primaries\":[]"));
    assert_eq!(
        acked(3),
        Some(grant(1)),
        "the grant with the list acknowledges as the first"
    );
    assert_eq!(acked(4), Some(grant(3)));
    assert_eq!(
        acked(5),
        Some(grant(3)),
        "the grant given up on was acknowledged"
    );
    assert_eq!(
        acked(6),
        Some(grant(5)),
        "the short lease was not acknowledged at once"
    );
    assert_eq!(acked(7), Some(grant(6)));
    assert!(!sent[7].contains("primaries"));
    assert_eq!(
        acked(8),
        Some(grant(7)),
        "the grant not taken was not acknowledged at once"
    );
    assert!(sent[8].contains("\"primaries\":[]"), "{}", sent[8]);
    assert_eq!(acked(9), Some(grant(8)));
}

/// A router over the directory `db` holds, leasing for `term`, its rounds
/// the test's: two of them over one directory are a router and the same
/// router restarted, the same holder.
fn router_on(db: &Arc<RwLock<Database>>, term: Duration) -> (Arc<Router>, u16) {
    let router = router_over(db, term);
    let listener = router.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    let serving = Arc::clone(&router);
    std::thread::spawn(move || {
        let _ = serving.serve_on(listener);
    });
    (router, port)
}

/// A router over the directory `db` holds, leasing for `term`, not yet
/// listening. Read under the directory's lock, which the routers over it
/// take too, and so waited for as [`bounded`] waits.
fn router_over(db: &Arc<RwLock<Database>>, term: Duration) -> Arc<Router> {
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        upstream_timeout: Duration::from_secs(10),
        replicas: true,
        auto_failover: Some(term),
        ..Config::default()
    };
    let db = Arc::clone(db);
    bounded("a router's directory read", move || {
        Router::new(Directory::load(db).unwrap(), cfg)
    })
}

/// The longest lease the directory `db` holds records, read under its lock
/// as [`router_over`] reads it.
fn record_of(db: &Arc<RwLock<Database>>) -> Option<Duration> {
    let db = Arc::clone(db);
    bounded("the lease record's read", move || {
        Directory::load(db).unwrap().lease_record()
    })
}

/// The directory `db` holds failing every write from now, as a disk gone:
/// its lock taken as [`router_over`] takes it.
fn fail(db: &Arc<RwLock<Database>>) {
    let db = Arc::clone(db);
    bounded("the directory's failure", move || {
        let gone = fenec_core::error::Error::Io("the disk is gone".into());
        db.write().unwrap().fail(&gone);
    })
}

/// Three nodes registered with a router over `db`, leasing for `term`.
fn registered(
    tag: &str,
    db: &Arc<RwLock<Database>>,
    term: Duration,
) -> ([Node; 3], Arc<Router>, u16) {
    let nodes = [1, 2, 3].map(|i| node(&format!("{tag}{i}"), true));
    let (router, port) = router_on(db, term);
    for (i, n) in nodes.iter().enumerate() {
        let body = format!(
            r#"{{"addr":"{}","token":"adm-{tag}{}"}}"#,
            n.proxy.addr,
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
    (nodes, router, port)
}

/// A router restarted over its directory -- the same holder -- whose first
/// grant to a node is a create's, with a schema: the node's lease from the
/// router before still runs, so the grant, acknowledging nothing of the new
/// router's, is not taken, its list naming the new tenant with it. Counted
/// taken, the router believed the node held the list, the schema was
/// refused 503 and the create taken back (the review of #343). The node
/// says it did not take it, and the grant acknowledging it goes at once,
/// with the list.
#[test]
fn a_create_with_a_schema_right_after_a_router_restart() {
    let db = Arc::new(RwLock::new(Database::new()));
    let (nodes, before, _) = registered("rrs", &db, LONG);
    round(&before);
    round(&before);
    let (status, lease) = call(nodes[0].port, "GET", "/_admin/lease", "", Some("adm-rrs1"));
    assert_eq!(status, 200, "{lease}");
    assert!(
        !lease.contains("\"left_ms\":0"),
        "n1 holds no lease before the restart: {lease}"
    );
    // Restarted: no round of its own yet.
    let (_after, port) = router_on(&db, LONG);
    let (status, body) = call(
        port,
        "PUT",
        "/_shard/tenants/acme",
        r#"{"node":"n1","schema":"create collection notes (title text)"}"#,
        None,
    );
    assert_eq!(status, 201, "{body}");
    let (status, body) = query(port, "/t/acme/query", r#"put notes {title: "one"}"#);
    assert_eq!(status, 200, "{body}");
}

/// A router started as `fenec-shard` starts one -- its leasing thread
/// begun, its first round at once, then its listener served -- over a
/// directory naming nodes: it answers, and leases them. Its start counted
/// the directory's nodes and tenants for its first line reading the
/// directory twice in the line's arguments, both guards held to the line's
/// end, while the first round's grants took it to write, recording the
/// lease: a writer queued between the two reads held the second behind it
/// and itself behind the first, and the router neither answered nor
/// leased again. A merge queue's CI run after #343 hung on it for its
/// hour, in the restart of the test below, whose round races the start as
/// the leasing thread's does. A debug build panics where a thread takes
/// the directory it holds, so this fails on that start every time rather
/// than once in hundreds of runs.
#[test]
fn a_router_started_beside_its_first_lease_round_answers_and_leases() {
    let db = Arc::new(RwLock::new(Database::new()));
    let (nodes, _before, _) = registered("sta", &db, Duration::from_secs(3));
    assert!(
        lease_of(&nodes, "sta").contains("\"left_ms\":null"),
        "n1 holds a lease before the router leases: {}",
        lease_of(&nodes, "sta")
    );
    // Started as main.rs starts it.
    let router = router_over(&db, Duration::from_secs(3));
    let listener = router.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    router.start_leasing().unwrap();
    let serving = Arc::clone(&router);
    let served = std::thread::spawn(move || serving.serve_on(listener));
    let (status, body) = match TcpStream::connect(("127.0.0.1", port)) {
        Ok(_) => call(port, "GET", "/_shard/tenants", "", None),
        Err(e) => (0, format!("no listener: {e}")),
    };
    assert_eq!(
        status,
        200,
        "the router started beside its first lease round did not answer ({body}): its \
         listener {}",
        match served.is_finished() {
            true => "ended, its start having panicked (above)",
            false => "is held up",
        }
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while !n1_lease(&nodes, "sta").0.is_some_and(|left| left > 0.0) {
        assert!(
            Instant::now() < deadline,
            "the router's leasing thread never leased n1: {}",
            lease_of(&nodes, "sta")
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The lease of a router restarted or promoted over a longer one, which
/// the tests below ask only to be shorter than the record by seconds. A
/// node that ran one of its grants is counted by it -- a lease and a tenth
/// -- and so is every healthy node beside the one a test is about, its
/// grants bounded by a third of it: at 1 s, 333 ms a grant, a healthy node
/// went unrenewed past 1.1 s at background QoS beside busy loops and was
/// failed over (2 runs of 9), once `"ran"` was read at all. 3 s is the
/// lease the other lease tests run ([`nodes::TERM`]).
const SHORTER: Duration = Duration::from_secs(3);

/// A router restarted over its directory -- the same holder -- with a
/// shorter lease, 20 s then 3 s, and the node then cut off. A newer grant
/// was judged by the lease it would run, so none of the new router's cut
/// the 20 s one short: it failed n1 over a lease and a tenth after n1's
/// last answer, and n1 took acme's writes beside the new primary, which,
/// its own grants not taken either, refused them (the review of #343). The
/// new router's first grant is answered as not taken, the one
/// acknowledging it goes at once and runs the node's lease from it, 3 s.
#[test]
fn a_router_restarted_with_a_shorter_lease_fails_over_a_node_on_the_shorter_one() {
    let db = Arc::new(RwLock::new(Database::new()));
    let (nodes, before, port) = registered("shl", &db, Duration::from_secs(20));
    round(&before);
    let (status, body) = call(
        port,
        "PUT",
        "/_shard/tenants/acme",
        r#"{"node":"n1"}"#,
        None,
    );
    assert_eq!(status, 201, "{body}");
    let path = "/t/acme/query";
    let (status, body) = query(port, path, "create collection notes (title text)");
    assert_eq!(status, 200, "{body}");
    let (status, body) = query(port, path, r#"put notes {title: "one"}"#);
    assert_eq!(status, 200, "{body}");
    let replica = placed(port, "acme").1.expect("acme has a replica");
    until(
        by_name(&nodes, &replica).port,
        path,
        "get notes select title",
        "one",
    );
    round(&before);
    let lease = || call(nodes[0].port, "GET", "/_admin/lease", "", Some("adm-shl1")).1;
    let left = number_after(&lease(), "\"left_ms\":");
    // Longer than the 3 s router can give, with 17 s to spare.
    let shorter = SHORTER.as_millis() as f64;
    assert!(left > shorter, "n1 holds no 20 s lease: {}", lease());

    // Restarted with the shorter lease: one round, and n1 is cut off.
    let (after, port) = router_on(&db, SHORTER);
    round(&after);
    let left = number_after(&lease(), "\"left_ms\":");
    assert!(
        left <= shorter,
        "the 20 s lease runs on under the {SHORTER:?} router: {}",
        lease()
    );
    nodes[0].proxy.cut();
    let deadline = Instant::now() + Duration::from_secs(30);
    while placed(port, "acme").0 == "n1" {
        assert!(Instant::now() < deadline, "acme was never failed over");
        round(&after);
        std::thread::sleep(Duration::from_millis(50));
    }
    let (direct, body) = query(nodes[0].port, path, r#"put notes {title: "old primary"}"#);
    assert_ne!(
        direct,
        200,
        "two primaries: n1 took acme's write after the {SHORTER:?} router failed it over \
         ({body}); n1's lease: {}",
        lease()
    );
    // The new primary takes them: the grants naming acme there are taken.
    new_primary_writes(port, || round(&after));
}

/// The lease the router before granted with, and how long it may run past
/// its last grant: what a router started with a shorter one waits out for a
/// node it never reaches.
const LONG: Duration = Duration::from_secs(10);

/// Runs `router`'s rounds until `acme` leaves n1, asserting that it did no
/// sooner than [`LONG`] and a tenth past `granted` -- when the answer to the
/// router before's last grant to n1 was in -- and that n1 then takes none
/// of acme's writes. Seen moved only after the round that moved it, so a
/// failover seen early was early; one made early and seen late is missed,
/// never one made in time taken for early.
fn waits_out_the_long_lease(router: &Arc<Router>, port: u16, nodes: &[Node], granted: Instant) {
    let deadline = granted + LONG * 4;
    loop {
        round(router);
        if placed(port, "acme").0 != "n1" {
            let after = granted.elapsed();
            assert!(
                after > LONG + LONG / 10,
                "acme was failed over {after:?} after the last grant of a lease of {LONG:?} to n1, \
                 which may still hold it"
            );
            break;
        }
        assert!(Instant::now() < deadline, "acme was never failed over");
        std::thread::sleep(Duration::from_millis(50));
    }
    let (direct, body) = query(
        nodes[0].port,
        "/t/acme/query",
        r#"put notes {title: "old"}"#,
    );
    assert_ne!(
        direct, 200,
        "n1 took acme's write after its failover: {body}"
    );
}

/// A router restarted over its directory with a shorter lease -- 10 s, then
/// 3 s -- and n1 cut off across the restart: the new router never reaches
/// it, and n1 holds the 10 s lease the router before granted, and writes.
/// Counted by its own lease, the new router failed it over 3.3 s after it
/// started, two primaries for the rest of the 10 s; it counts a node that
/// has taken none of its grants by the longest lease the directory
/// recorded, and once it has led that long lowers the record to its own.
#[test]
fn a_router_restarted_with_a_shorter_lease_waits_out_a_node_it_never_reaches() {
    let db = Arc::new(RwLock::new(Database::new()));
    let (nodes, before, port) = registered("unr", &db, LONG);
    round(&before);
    acme_on_n1(port, &nodes);
    round(&before);
    let granted = Instant::now();
    nodes[0].proxy.cut();
    assert_eq!(record_of(&db), Some(LONG));

    let (after, port) = router_on(&db, SHORTER);
    waits_out_the_long_lease(&after, port, &nodes, granted);
    // Led a lease and a tenth of the one recorded: no node holds it now.
    round(&after);
    assert_eq!(record_of(&db), Some(SHORTER));
}

/// A router restarted with a shorter lease -- 10 s, then 3 s -- whose
/// grant n1 runs: its first answered as not taken, the 10 s lease running,
/// the one acknowledging it sent at once and run (`"ran": true`). n1 is
/// the router's own from then on, counted by its 3 s: cut off, its tenant
/// is failed over a lease and a tenth past its last answer, where the
/// record would hold it until 11 s past the router's first round at the
/// soonest -- and n1, whose own lease ran from an answer before that one,
/// has stopped writing by then. The router looked for `"ran":true`, ten
/// bytes, in windows of eleven, and never found it: every node was counted
/// by the record until it came down, and n1 was failed over 11 s after the
/// router led at the soonest. #343's tests held only the other side, a
/// node that ran none counted by the record.
#[test]
fn a_node_that_ran_the_router_s_lease_is_counted_by_it_not_the_record() {
    let db = Arc::new(RwLock::new(Database::new()));
    let (nodes, before, port) = registered("own", &db, LONG);
    round(&before);
    acme_on_n1(port, &nodes);
    round(&before);
    assert_eq!(record_of(&db), Some(LONG));

    let (after, port) = router_on(&db, SHORTER);
    // Before its first round, which every node is counted from: by the
    // record, none lapses before this and a record and a tenth.
    let led = Instant::now();
    round(&after);
    let (left, _) = n1_lease(&nodes, "own");
    assert!(
        left.is_some_and(|left| left <= SHORTER.as_millis() as f64),
        "n1 runs no lease of the router's: {}",
        lease_of(&nodes, "own")
    );
    nodes[0].proxy.cut();
    // When the round that failed acme over had ended: a failover made
    // before then, and so no later.
    let moved = loop {
        round(&after);
        let ended = Instant::now();
        if placed(port, "acme").0 != "n1" {
            break ended;
        }
        assert!(
            led.elapsed() < LONG * 4,
            "acme was never failed over: {}",
            lease_of(&nodes, "own")
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let record = led + fenec_shard::lease::lapse(LONG);
    assert!(
        moved < record,
        "n1 ran the router's lease of {SHORTER:?} and was counted by the record of {LONG:?}: \
         failed over {:?} after the router led",
        moved - led
    );
    let (direct, body) = n1_writes_acme(&nodes);
    assert_ne!(
        direct, 200,
        "two primaries: n1 took acme's write after its failover on the router's own lease: {body}"
    );
}

/// A router of a directory file of its own leasing for `term`, its rounds
/// the test's, following `upstream` -- its primary router -- where given,
/// as a standby: the database it keeps beside it.
fn standing(
    path: &std::path::Path,
    upstream: Option<&str>,
    term: Duration,
) -> (Arc<Router>, u16, Arc<RwLock<Database>>) {
    let (mut db, feed) = fenec_http::replication::open(path.to_str().unwrap(), 8 << 20).unwrap();
    match upstream {
        Some(_) => {
            let lineage = db.history().lineage.clone();
            db.follow(lineage).unwrap();
        }
        None => {
            db.fork(fresh_id()).unwrap();
            db.sync().unwrap();
        }
    }
    let db = Arc::new(RwLock::new(db));
    let dir = Directory::load(Arc::clone(&db)).unwrap();
    let follower = upstream.map(|url| {
        let f = Follower::new(
            url,
            DIRS.into(),
            Arc::clone(&db),
            Some(Arc::clone(&feed)),
            true,
        )
        .unwrap();
        let run = Arc::clone(&f);
        std::thread::spawn(move || run.run());
        f
    });
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        upstream_timeout: Duration::from_secs(10),
        replicas: true,
        auto_failover: Some(term),
        ..Config::default()
    };
    let replication = Replication::new(Some(DIRS.into()), Some(feed), follower);
    let router = Router::replicated(dir, cfg, replication);
    let listener = router.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    let serving = Arc::clone(&router);
    std::thread::spawn(move || {
        let _ = serving.serve_on(listener);
    });
    (router, port, db)
}

const DIRS: &str = "directory-secret";

/// A standby router with a shorter lease than its primary's -- 3 s against
/// 10 s -- promoted with n1 cut off from both: the record its primary wrote
/// came with the directory, and the promoted router waits out the 10 s
/// lease for n1 as a restarted one does.
#[test]
fn a_promoted_standby_with_a_shorter_lease_waits_out_a_node_it_never_reaches() {
    let files = std::env::temp_dir().join(format!("fenec-late-standby-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&files);
    std::fs::create_dir_all(&files).unwrap();
    let nodes = [1, 2].map(|i| node(&format!("stb{i}"), true));
    let (a, a_port, _) = standing(&files.join("a.fenec"), None, LONG);
    let upstream = format!("http://127.0.0.1:{a_port}");
    let (b, b_port, b_db) = standing(&files.join("b.fenec"), Some(&upstream), SHORTER);
    for (i, n) in nodes.iter().enumerate() {
        let body = format!(
            r#"{{"addr":"{}","token":"adm-stb{}"}}"#,
            n.proxy.addr,
            i + 1
        );
        let (status, body) = call(
            a_port,
            "PUT",
            &format!("/_shard/nodes/n{}", i + 1),
            &body,
            None,
        );
        assert!(status == 200 || status == 201, "{body}");
    }
    let create = r#"{"node":"n1","schema":"create collection notes (title text)"}"#;
    let (status, body) = call(a_port, "PUT", "/_shard/tenants/acme", create, None);
    assert_eq!(status, 201, "{body}");
    assert_eq!(
        query(a_port, "/t/acme/query", r#"put notes {title: "one"}"#).0,
        200
    );
    until(
        nodes[1].port,
        "/t/acme/query",
        "get notes select title",
        "one",
    );
    // The record and the placement have reached the standby's directory.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let db = Arc::clone(&b_db);
        let (record, placed) = bounded("the standby's directory read", move || {
            let dir = Directory::load(db).unwrap();
            (dir.lease_record(), dir.placement("acme").is_some())
        });
        if record == Some(LONG) && placed {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the standby's directory never caught up"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    round(&a);
    let granted = Instant::now();
    nodes[0].proxy.cut();
    let (status, body) = call(b_port, "POST", "/_replication/promote", "", Some(DIRS));
    assert_eq!(status, 200, "{body}");
    waits_out_the_long_lease(&b, b_port, &nodes, granted);
    drop(a);
    let _ = std::fs::remove_dir_all(&files);
}

/// A router restarted with a shorter lease, 13 s then 3 s, whose first
/// grant to n1 comes after n1's lease from the router before has lapsed:
/// taken for its list alone, running no lease, which n1 says (`"ran":
/// false`). A grant of the router before, held on its way and read after
/// it, acknowledges an answer n1 kept, later than the one its lapsed lease
/// came from: newer, and it runs the 13 s lease again, 5.5 s past the
/// lapse. Counted by the new router's 3 s from that take -- `own` set on
/// any `Taken` -- n1, cut off, was failed over 3.3 s after its last answer
/// and took acme's writes beside the new primary (the review of #343's
/// `r2_a_late_grant_of_the_router_before_outlives_a_list_only_take`, 9 s
/// then 1 s, B 4 s after A). At 1 s the new router failed its healthy
/// nodes over under load once a node that ran its lease was counted by
/// it ([`SHORTER`]); at 3 s the late lease must run past the take by more
/// than 3.3 s for n1 to be found writing where it was counted so, and B
/// must come under half the lease after A. A node is the router's own
/// once it ran a lease from it.
#[test]
fn a_take_of_the_list_alone_leaves_a_node_counted_by_the_longest_lease() {
    const OLD: Duration = Duration::from_secs(13);
    let db = Arc::new(RwLock::new(Database::new()));
    let nodes = [1, 2, 3].map(|i| node(&format!("rtk{i}"), true));
    let stall = Stall::to(format!("127.0.0.1:{}", nodes[0].port));
    let addrs = [
        stall.addr.clone(),
        nodes[1].proxy.addr.clone(),
        nodes[2].proxy.addr.clone(),
    ];
    let (r1, p1) = router_on(&db, OLD);
    for (i, a) in addrs.iter().enumerate() {
        let body = format!(r#"{{"addr":"{a}","token":"adm-rtk{}"}}"#, i + 1);
        let (status, body) = call(p1, "PUT", &format!("/_shard/nodes/n{}", i + 1), &body, None);
        assert!(status == 200 || status == 201, "{body}");
    }
    round(&r1);
    acme_on_n1(p1, &nodes);
    // Round A, then round B 5.5 s later -- under half the lease by a
    // second, so B's answer is not short and no grant follows it: n1's
    // lease runs from A's answer. Round C acknowledges B, and is held on
    // its way.
    round(&r1);
    std::thread::sleep(Duration::from_millis(5_500));
    round(&r1);
    stall.take_next(b"POST /_admin/lease");
    round(&r1);
    let late = stall.grants();
    assert_eq!(late.len(), 1, "round C's grant to n1 was not held");
    // n1's lease from A's answer runs out.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !lease_of(&nodes, "rtk").contains("\"left_ms\":0") {
        assert!(Instant::now() < deadline, "{}", lease_of(&nodes, "rtk"));
        std::thread::sleep(Duration::from_millis(20));
    }
    // Restarted with the shorter lease; its grant acknowledging the first
    // is lost.
    let (r2, p2) = router_on(&db, SHORTER);
    stall.take_next(br#""acked":""#);
    round(&r2);
    assert_eq!(
        stall.grants().len(),
        2,
        "the new router's second grant was not held"
    );
    // Round C's grant is read now, and runs the 13 s lease again.
    let (status, answer) = deliver(nodes[0].port, &late[0]);
    assert_eq!(status, 200, "{answer}");
    assert!(answer.contains("\"ran\":true"), "{answer}");
    stall.cut();
    let deadline = Instant::now() + Duration::from_secs(60);
    while placed(p2, "acme").0 == "n1" {
        assert!(Instant::now() < deadline, "acme was never failed over");
        round(&r2);
        std::thread::sleep(Duration::from_millis(50));
    }
    let (direct, body) = n1_writes_acme(&nodes);
    assert_ne!(
        direct,
        200,
        "two primaries: n1 took acme's write after the {SHORTER:?} router failed it over \
         ({body}); n1's lease: {}",
        lease_of(&nodes, "rtk")
    );
}

/// The lease n1 says it holds, `null` where none ran, and the newest grant
/// it answered.
fn n1_lease(nodes: &[Node; 3], tag: &str) -> (Option<f64>, Option<String>) {
    let lease = lease_of(nodes, tag);
    let left = lease
        .split("\"left_ms\":")
        .nth(1)
        .and_then(|r| r.split([',', '}']).next())
        .and_then(|l| l.parse().ok());
    (left, hex_field(lease.as_bytes(), "grant"))
}

/// A router whose directory cannot record its longer lease still grants
/// one, no longer than the record: refused, every node's lease lapsed and
/// every node was failed over, by a router whose directory then refused
/// the failovers too. With a record of 2 s and a router of 20 s, n1 runs a
/// lease of 2 s at most; with no record yet -- a directory from before it
/// -- the router's own, as a router before the record granted.
#[test]
fn a_router_that_cannot_record_its_lease_grants_no_longer_than_the_record() {
    // A record of 2 s, written by the router before; n1's lease from it
    // runs out.
    let db = Arc::new(RwLock::new(Database::new()));
    let (nodes, before, _) = registered("unw", &db, Duration::from_secs(2));
    round(&before);
    let deadline = Instant::now() + Duration::from_secs(30);
    while !lease_of(&nodes, "unw").contains("\"left_ms\":0") {
        assert!(Instant::now() < deadline, "{}", lease_of(&nodes, "unw"));
        std::thread::sleep(Duration::from_millis(20));
    }
    let (_, answered) = n1_lease(&nodes, "unw");
    // Restarted, and its disk fails before its first grant.
    let (after, _) = router_on(&db, Duration::from_secs(20));
    fail(&db);
    round(&after);
    let (left, now) = n1_lease(&nodes, "unw");
    assert_ne!(
        now,
        answered,
        "n1 was granted nothing: {}",
        lease_of(&nodes, "unw")
    );
    assert!(
        left.is_some_and(|left| left <= 2_000.0),
        "n1 runs a lease longer than the record, or none: {}",
        lease_of(&nodes, "unw")
    );

    // No record yet: nodes registered, and the directory fails before the
    // router's first grant.
    let db = Arc::new(RwLock::new(Database::new()));
    let (nodes, router, _) = registered("unn", &db, Duration::from_secs(20));
    fail(&db);
    round(&router);
    let (left, _) = n1_lease(&nodes, "unn");
    // A lease longer than the record above, with 18 s to spare.
    assert!(
        left.is_some_and(|left| left > 2_000.0),
        "n1 was granted no lease: {}",
        lease_of(&nodes, "unn")
    );
}

/// A router of 60 s restarted over a record of 3 s that it cannot raise --
/// its directory's disk gone -- grants 3 s leases, and its leasing thread
/// renews them: its first round at once, and the rounds after a third of
/// the lease it grants apart. A third of its own apart, it granted 3 s
/// every 20, the first 20 s after its start, and n1 held no lease for 17 s
/// of every 20 -- every tenant refused its writes most of the time (the
/// review of #343's
/// `r3_a_grant_of_the_record_s_length_between_rounds_of_the_router_s_own`).
#[test]
fn a_router_that_cannot_record_its_lease_renews_at_the_lease_it_grants() {
    let db = Arc::new(RwLock::new(Database::new()));
    let (nodes, before, _) = registered("rnd", &db, Duration::from_secs(3));
    round(&before);
    let (after, _) = router_on(&db, Duration::from_secs(60));
    fail(&db);
    after.start_leasing().unwrap();
    let (mut lapsed, mut all, mut longest) = (0, 0, 0.0_f64);
    let end = Instant::now() + Duration::from_secs(12);
    while Instant::now() < end {
        let lease = lease_of(&nodes, "rnd");
        all += 1;
        if lease.contains("\"left_ms\":0") || lease.contains("\"left_ms\":null") {
            lapsed += 1;
        }
        longest = longest.max(n1_lease(&nodes, "rnd").0.unwrap_or(0.0));
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        longest <= 3_000.0,
        "n1 ran a lease past the record: {longest} ms"
    );
    // A round every 20 s, the first 20 s in, leaves n1 without a lease
    // from 3 s on: three samples in four. Rounds 1 s apart leave it none
    // only past a stall of 2 s, and fail this only past 6 s of such
    // stalls in the 12.
    assert!(
        lapsed * 2 < all,
        "n1 held no lease in {lapsed} of {all} samples: {}",
        lease_of(&nodes, "rnd")
    );
}
