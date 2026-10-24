//! A node that is slow to answer, or to be found at all, holds up no other
//! node's lease, and a failover on a lease leaves no tenant behind
//! (`Router::lease_round`, `fenec_http::dial`).
//!
//! A binary of its own: each test holds threads for its whole run --
//! lookups a stand-in resolver never answers, connections a hung node
//! swallows, an answer trickled forever -- beside three nodes and a router,
//! and `all` stands near macOS's 2 048 threads a process (2 000 at its peak
//! when these came).

#[path = "lease_nodes.rs"]
mod nodes;

use fenec_http::replication::{fresh_id, Follower, Replication};
use fenec_shard::directory::Directory;
use fenec_shard::{Config, Router};
use nodes::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};

/// A node's name, looked up by a stand-in for the system's resolver: the
/// node's address until silenced, then no answer at all -- every lookup
/// hangs until this is dropped, as one sent to a resolver that never
/// answers hangs until the system gives up on it. Docker's resolver, asked
/// for a container that is gone, forwards the name upstream, where it can
/// wait as long.
struct Resolver {
    /// `(silenced, released)`, and the wait for the release.
    state: Arc<(Mutex<(bool, bool)>, Condvar)>,
}

impl Resolver {
    fn new(name: &str, to: std::net::SocketAddr) -> Resolver {
        let state = Arc::new((Mutex::new((false, false)), Condvar::new()));
        let s = Arc::clone(&state);
        fenec_http::dial::stand_in(name, move || {
            let (m, released) = &*s;
            let mut g = m.lock().unwrap();
            if !g.0 {
                return Ok(vec![to]);
            }
            while !g.1 {
                g = released.wait(g).unwrap();
            }
            Err(std::io::Error::other("the resolver gave up"))
        });
        Resolver { state }
    }

    fn silence(&self) {
        self.state.0.lock().unwrap().0 = true;
    }
}

impl Drop for Resolver {
    fn drop(&mut self) {
        self.state.0.lock().unwrap().1 = true;
        self.state.1.notify_all();
    }
}

/// How many rounds' grants more each node is sent while something hangs,
/// in the tests below that a hang holds up no other node's lease: a round
/// every third of a lease, so five are well past a lease and a tenth.
const RENEWALS: u64 = 5;

/// Waits until each of `nodes` has taken [`RENEWALS`] grants of `router`'s
/// rounds more than it had when this was called ([`Router::round_grants`],
/// which counts a grant given up on not at all: a hang that had the healthy
/// nodes' grants time out would pass a count of grants ended), while `what`
/// hangs: what the tests that a hang holds up no other node's
/// lease count, rather than writes answered throughout. A machine that
/// does not run the router's threads for a lease lapses every lease,
/// whatever hangs -- at background QoS beside six busy loops a grant
/// bounded by a second ran for 2.3 s, and the healthy nodes' leases
/// lapsed, the fence working -- which writes answered cannot tell from a
/// hang that held their renewals up. Held up -- a round waiting on the
/// grant that hangs, a repair or a failover run on the leasing thread, a
/// repair run in a node's grant -- a node is sent no round's grant more
/// for as long as the hang lasts, which in these tests is past their end,
/// whatever grants its failovers send it meanwhile: the count tells the
/// two apart on any machine. Bounded by [`WITHIN`], a round taking
/// milliseconds.
fn renewed_meanwhile(router: &Router, nodes: &[&str], what: &str) {
    let from: Vec<u64> = nodes.iter().map(|n| router.round_grants(n)).collect();
    let deadline = Instant::now() + WITHIN;
    loop {
        let short: Vec<String> = nodes
            .iter()
            .zip(&from)
            .map(|(n, f)| (n, router.round_grants(n).saturating_sub(*f)))
            .filter(|(_, more)| *more < RENEWALS)
            .map(|(n, more)| format!("{n}: {more} of {RENEWALS}"))
            .collect();
        if short.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{what} held up the other nodes' renewals: in {WITHIN:?} they were granted {short:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Waits until a write through the router to `tenant` is answered while
/// `what` hangs: the tenant is leased where it is. A write refused on the
/// way as [`written`]'s may be -- by the fence ([`fenced`]), or cut at its
/// node's failover ([`cut_at_failover`]), a lease lapsed on a machine that
/// did not run the router for one -- is sent again, the next renewal or the
/// failover ending it, and its row counts for nothing here; held up, no
/// renewal comes while the hang lasts, and no write is answered. Any other
/// answer fails the test.
fn written_meanwhile(port: u16, tenant: &str, what: &str) {
    let path = format!("/t/{tenant}/query");
    let deadline = Instant::now() + WITHIN;
    loop {
        let (status, body) = query(port, &path, r#"put notes {title: "meanwhile"}"#);
        if status == 200 {
            return;
        }
        assert!(
            (fenced(status, &body) || cut_at_failover(status, &body)) && Instant::now() < deadline,
            "{tenant} took no write while {what}: the last answered {status} {body}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Makes `tenant` on `node` with a collection to write, through the router:
/// once it is leased there ([`fenced`]).
fn tenant_on(port: u16, tenant: &str, node: &str) {
    let body = format!(r#"{{"node":"{node}","schema":"create collection notes (title text)"}}"#);
    let deadline = Instant::now() + WITHIN;
    loop {
        let (status, made) = call(
            port,
            "PUT",
            &format!("/_shard/tenants/{tenant}"),
            &body,
            None,
        );
        if status == 201 {
            return;
        }
        assert!(
            fenced(status, &made) && Instant::now() < deadline,
            "{tenant} on {node}: {status} {made}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Waits until a write through the router to `tenant` is answered: its
/// failover has come.
fn writes_again(port: u16, tenant: &str) {
    let path = format!("/t/{tenant}/query");
    let deadline = Instant::now() + WITHIN;
    loop {
        let (status, body) = query(port, &path, r#"put notes {title: "after"}"#);
        if status == 200 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{tenant} was never failed over: placed {:?}, a write answered {status} {body}",
            placed(port, tenant)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `fenec_router_node_failed_over` of each of `nodes`, from the scrape.
fn failed_over(port: u16, nodes: &[&str]) -> Vec<f64> {
    let (status, scrape) = call(port, "GET", "/_metrics", "", None);
    assert_eq!(status, 200, "{scrape}");
    nodes
        .iter()
        .map(|n| {
            number_after(
                &scrape,
                &format!("fenec_router_node_failed_over{{node=\"{n}\"}} "),
            )
        })
        .collect()
}

/// Waits until the scrape says `node` was failed over (`1`) or not (`0`).
fn until_failed_over(port: u16, node: &str, want: f64, what: &str) {
    let deadline = Instant::now() + WITHIN;
    while failed_over(port, &[node]) != [want] {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The leasing thread's rounds are each due a third of a lease after the
/// last was due, not after it woke ([`fenec_shard::lease::round_due`]):
/// counted from the wake, every wake's lateness put the rounds after it as
/// late, and at background QoS beside busy loops the next grant came too
/// late for the lease. Read by what the thread works each round's due out
/// as ([`Router::rounds_on_schedule`]), never by a clock: a round counted
/// from its wake is never due exactly a third after the last, since a wake
/// is late by a few nanoseconds at least. Three rounds of a second each
/// woken within a second of their due, in [`WITHIN`]: a machine that runs
/// the thread at all does that.
#[test]
fn the_leasing_thread_s_rounds_are_due_from_when_the_last_was_due() {
    let (router, _port) = leasing_replicas(upstream());
    let deadline = Instant::now() + WITHIN;
    while router.rounds_on_schedule() < 3 {
        assert!(
            Instant::now() < deadline,
            "in {WITHIN:?} {} rounds were due a third of a lease after the one before",
            router.rounds_on_schedule()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A node gone whose name its resolver never answers holds up no other
/// node's lease. A grant looked its node's name up with no bound, and the
/// round waited for every grant: in V5's drill the region's healthy nodes
/// went unrenewed until they stopped writing, 18 s in CI, and the router
/// failed one over. Here the lookup hangs until the test ends: the dead
/// node's tenant is failed over -- its grant's lookup ends at its bound, so
/// its failover, which waits for that grant, comes -- and the other two
/// nodes are granted again and again and take writes meanwhile
/// ([`renewed_meanwhile`]). Its tenant's replica follows it by the same
/// name, so the failover also halts a follower in the middle of a lookup
/// that never ends.
#[test]
fn a_dead_node_whose_name_never_resolves_holds_up_no_other_node_s_lease() {
    let (nodes, port, router) = three_leased("dns", true, TERM, upstream());
    let name = "n1.dns.leases.test:8081";
    let resolver = Resolver::new(name, nodes[0].proxy.addr.parse().unwrap());
    let body = format!(r#"{{"addr":"{name}","token":"adm-dns1"}}"#);
    let (status, body) = call(port, "PUT", "/_shard/nodes/n1", &body, None);
    assert!(status == 200 || status == 201, "{body}");
    tenant_on(port, "b", "n2");
    tenant_on(port, "c", "n3");
    for t in ["b", "c"] {
        written(port, t, r#"put notes {id: 1, title: "one"}"#);
    }
    // `gone` last, and n1 cut off as soon as its replica holds its write
    // ([`leased_until_cut`]).
    tenant_on(port, "gone", "n1");
    written(port, "gone", r#"put notes {id: 1, title: "one"}"#);
    let replica = placed(port, "gone").1.expect("gone has a replica");
    until(
        by_name(&nodes, &replica).port,
        "/t/gone/query",
        "get notes select title",
        "one",
    );
    leased_until_cut(port, "gone", "n1", &replica);
    nodes[0].proxy.cut();
    resolver.silence();
    // Its failover waited for, and only then a write: one sent before it
    // waits on n1's lookup for the router's upstream timeout.
    failover_done(port, "gone", &replica);
    written(port, "gone", r#"put notes {id: 4, title: "after"}"#);
    let hang = "n1's lookup that never ends";
    renewed_meanwhile(&router, &["n2", "n3"], hang);
    written_meanwhile(port, "b", hang);
    written_meanwhile(port, "c", hang);
    assert_eq!(failed_over(port, &["n1"]), [1.0]);
    drop(resolver);
}

/// A repair holds up no lease, its own node's among them. A node back from
/// a failover has its copies follow the tenants' new primaries, and the
/// repair asks every node what it holds, each call as long as its node
/// takes: run on the leasing thread, a node that does not answer held every
/// renewal up for as long -- V5's drill, 20 s -- and the healthy nodes
/// stopped writing; run in the grant that heard the node back, it held that
/// node's renewals, and the node was failed over again. Here n3 hangs,
/// taking connections and answering nothing, as n1 comes back: the repair
/// waits on n3 past the end of the test, and meanwhile n1 and n2 are
/// granted again and again and n2's tenant takes a write
/// ([`renewed_meanwhile`]). n3 takes no lease, so it is never failed over
/// itself: the grants a failover of it sends every other node would renew
/// n1 beside a repair holding n1's own. And it joins once the tenants have
/// their replicas, keeping none: kept on n3, a replica made a failover of
/// its tenant's node -- at background QoS beside busy loops, n2 run past
/// its lease -- wait on n3 too, in n2's turn, and n2 was granted no more.
#[test]
fn a_repair_waiting_on_a_node_that_hangs_holds_up_no_lease() {
    let nodes = [1, 2, 3].map(|i| node(&format!("repair{i}"), i != 3));
    // The repair's wait on n3 lasts past the end of the test, so what it
    // held up would not come while the test counts.
    let (router, port) = leasing_replicas(Duration::from_secs(600));
    put_node(port, "n1", &nodes[0].proxy.addr, "adm-repair1");
    put_node(port, "n2", &nodes[1].proxy.addr, "adm-repair2");
    // `b` first, its replica on n1, then `a`, its replica on n2.
    tenant_on(port, "b", "n2");
    tenant_on(port, "a", "n1");
    written(port, "a", r#"put notes {id: 1, title: "one"}"#);
    until(nodes[1].port, "/t/a/query", "get notes select title", "one");
    put_node(port, "n3", &nodes[2].proxy.addr, "adm-repair3");
    leased_until_cut(port, "a", "n1", "n2");
    // n1 goes, and its tenant is failed over.
    nodes[0].proxy.cut();
    writes_again(port, "a");
    assert_eq!(failed_over(port, &["n1"]), [1.0]);

    // n3 hangs, and n1 comes back: its next grant is answered, and the
    // repair that follows asks n3 what it holds and waits.
    nodes[2].proxy.hang();
    nodes[0].proxy.heal();
    until_failed_over(port, "n1", 0.0, "n1 never answered again");
    until_heard(
        &nodes[2].proxy,
        "GET /_admin/tenants ",
        "the repair never asked n3",
    );
    let hang = "the repair waiting on n3";
    renewed_meanwhile(&router, &["n1", "n2"], hang);
    written_meanwhile(port, "b", hang);
}

/// Holds that `tenant` is on `node` with its replica on `replica` as the
/// test is about to cut `node` off, where what the test is about starts. A
/// router the machine ran past a lease fails over a node that was only
/// starved -- its lease lapsed by its own clock as well, the fence working
/// -- and at background QoS beside eight busy loops that came during a
/// test's setup in 1 run of 20, the tenant promoted before the cut: each
/// test makes its other tenants first and cuts the node as soon as this
/// one's replica holds its write, which leaves that the least time to
/// come in, and says so where it came.
fn leased_until_cut(port: u16, tenant: &str, node: &str, replica: &str) {
    let (on, copy) = placed(port, tenant);
    assert!(
        on == node && copy.as_deref() == Some(replica),
        "{tenant} left {node} before the test cut it off (on {on}, its replica {copy:?}): the \
         router failed {node} over as its renewals ran past its lease"
    );
}

/// Waits until `proxy`, hung, has been sent `request` (bounded by
/// [`WITHIN`]): what hangs on it has reached it.
fn until_heard(proxy: &Proxy, request: &str, what: &str) {
    let deadline = Instant::now() + WITHIN;
    while !proxy.heard().contains(request) {
        assert!(Instant::now() < deadline, "{what} in {WITHIN:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A failover holds up no lease. Promoting a node's tenants calls the nodes
/// they go to, each call as long as its node takes, and the leasing thread
/// ran it between rounds: a promotion's node that hung held every other
/// node's renewal for the router's upstream timeout. Here n1 is cut off and
/// its tenant's promotion is held at the door of n2, which keeps the
/// tenant's replica: n1's failover waits on n2 past the end of the test,
/// and meanwhile n2 and n3 are granted again and again and n3's tenant takes
/// a write ([`renewed_meanwhile`]). Only the promotion is held: n2 hung
/// whole lapsed as n1 did, and its failover, first in 1 run of 8 at
/// background QoS beside seven busy loops, dropped the replica n1's was to
/// promote; and taking no lease, it lapsed all the same where the test hung
/// it before a round had heard it say so (2 runs of 20).
#[test]
fn a_failover_waiting_on_a_promotion_that_hangs_holds_up_no_lease() {
    let nodes = [1, 2, 3].map(|i| node(&format!("promote{i}"), true));
    let n2 = Filter::to(format!("127.0.0.1:{}", nodes[1].port), b"/promote ");
    // The promotion's wait on n2 lasts past the end of the test, so what it
    // held up would not come while the test counts.
    let (router, port) = leasing_replicas(Duration::from_secs(600));
    // `w` made while n3 is the only node, before `a` ([`leased_until_cut`]).
    put_node(port, "n3", &nodes[2].proxy.addr, "adm-promote3");
    tenant_on(port, "w", "n3");
    put_node(port, "n1", &nodes[0].proxy.addr, "adm-promote1");
    put_node(port, "n2", &n2.addr, "adm-promote2");
    // n3 holds `w`, so `a`'s replica goes to n2, which holds nothing.
    tenant_on(port, "a", "n1");
    assert_eq!(placed(port, "a").1.as_deref(), Some("n2"));
    written(port, "a", r#"put notes {id: 1, title: "one"}"#);
    until(nodes[1].port, "/t/a/query", "get notes select title", "one");
    leased_until_cut(port, "a", "n1", "n2");
    n2.armed.store(true, Ordering::SeqCst);
    nodes[0].proxy.cut();
    let deadline = Instant::now() + WITHIN;
    while n2.held() == 0 {
        assert!(
            Instant::now() < deadline,
            "n1's failover never asked n2 to promote a in {WITHIN:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let hang = "n1's failover waiting on n2";
    renewed_meanwhile(&router, &["n2", "n3"], hang);
    written_meanwhile(port, "w", hang);
}

/// A node that answers a grant a byte at a time: its node's stats at once,
/// so the router records it, and every grant's answer trickled for as long
/// as the router reads. The connect, the write and each read are all
/// within the lease pool's bound; the grant is not, and the round waited
/// for it.
fn trickling_node(token: &str) -> (String, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let granted = Arc::new(AtomicBool::new(false));
    let (told, token) = (Arc::clone(&granted), format!("Bearer {token}"));
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let (told, token) = (Arc::clone(&told), token.clone());
            std::thread::spawn(move || answer_trickling(conn, &told, &token));
        }
    });
    (addr, granted)
}

fn answer_trickling(conn: TcpStream, granted: &AtomicBool, token: &str) {
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
        if line.contains("/_admin/lease") && authorized {
            granted.store(true, Ordering::SeqCst);
            let _ = out.write_all(b"HTTP/1.1 200 OK\r\nX-Slow: ");
            while out.write_all(b"a").is_ok() {
                std::thread::sleep(Duration::from_millis(200));
            }
            return;
        }
        let answer: &[u8] = if line.contains("/_admin/stats") && authorized {
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}"
        } else {
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 2\r\n\r\n{}"
        };
        if out.write_all(answer).is_err() {
            return;
        }
    }
}

/// A round waits for no grant. Every read of a grant's answer is bounded by
/// a third of the lease, but the grant as a whole is not, and the round
/// waited for every grant before the next: a node that answered a byte at
/// a time held every other node's renewal for as long. Here n3 trickles,
/// and meanwhile n1 is granted again and again and its tenant takes a
/// write ([`renewed_meanwhile`]).
#[test]
fn a_node_trickling_its_answer_holds_up_no_other_node_s_lease() {
    let n1 = node("trickle1", true);
    let (slow, granted) = trickling_node("adm-trickle3");
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        upstream_timeout: upstream(),
        auto_failover: Some(TERM),
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
    for (name, addr, token) in [
        ("n1", n1.proxy.addr.as_str(), "adm-trickle1"),
        ("n3", slow.as_str(), "adm-trickle3"),
    ] {
        let body = format!(r#"{{"addr":"{addr}","token":"{token}"}}"#);
        let (status, body) = call(port, "PUT", &format!("/_shard/nodes/{name}"), &body, None);
        assert_eq!(status, 201, "{body}");
    }
    tenant_on(port, "w", "n1");
    written(port, "w", r#"put notes {id: 1, title: "one"}"#);
    let deadline = Instant::now() + WITHIN;
    while !granted.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "n3 was never sent a grant");
        std::thread::sleep(Duration::from_millis(20));
    }
    // n3 trickles its answer for as long as the router reads it.
    let hang = "n3 trickling its answer";
    renewed_meanwhile(&router, &["n1"], hang);
    written_meanwhile(port, "w", hang);
}

/// A tenant another operation holds as its node is failed over on a lease
/// is failed over once that operation lets it go. The failover leaves a
/// tenant whose claim another holds -- a quota being set, a move, a repair
/// giving it a replica, which runs beside the failovers -- and a failover
/// by hand says so and is asked again; on a lease nobody asked again, and
/// the tenant stayed on its node, gone. Here a quota is being set on n1's
/// tenant as n1 hangs: the quota's call waits on n1 and holds the tenant
/// past n1's failover until the test cuts n1 off, and the failover comes
/// back to it. And meanwhile no grant to n1 names the tenant -- read off
/// what n1's hung proxy heard: each grant's list is sent, without it. Left
/// in n1's list, every grant of the hold named it by its list's epoch, and
/// a late one n1 took (the lease module's hole) would run its lease past
/// the tenant's promotion for as long as the quota held it.
#[test]
fn a_tenant_held_through_its_node_s_failover_is_failed_over_once_let_go() {
    let (nodes, port) = three("held", true, TERM);
    tenant_on(port, "t", "n1");
    written(port, "t", r#"put notes {id: 1, title: "one"}"#);
    let replica = placed(port, "t").1.expect("t has a replica");
    until(
        by_name(&nodes, &replica).port,
        "/t/t/query",
        "get notes select title",
        "one",
    );
    nodes[0].proxy.hang();
    // Holds `t` from now until its call to n1 ends, which the test ends
    // once n1 has been failed over and sent a grant whole past it: n1's lapse
    // comes a lease and a tenth after its last renewal at most. Ended by
    // the router's timeout, ten seconds, the hold was a guess at how soon
    // all of that came, and the router's other calls in the test held to
    // the same ten seconds ([`upstream`]).
    let quota = std::thread::spawn(move || {
        call(
            port,
            "PUT",
            "/_shard/tenants/t/quota",
            r#"{"streams": 5}"#,
            None,
        )
    });
    until_failed_over(port, "n1", 1.0, "n1 was never judged lapsed");
    // The failover leaves a held tenant to be failed over once let go of
    // only where it has a copy to go to; one whose replica went with a
    // failover of the replica's node -- at background QoS beside busy loops,
    // that node run past its lease -- stays on n1, named by n1's grants,
    // and was read as a grant naming it by mistake (2 runs of 20).
    let (on, copy) = placed(port, "t");
    assert!(
        copy.is_some(),
        "t lost its replica before n1's failover (on {on}): the router failed the replica's \
         node over as its renewals ran past its lease"
    );
    let from = nodes[0].proxy.heard().len();
    let deadline = Instant::now() + WITHIN;
    while grants_heard(&nodes[0].proxy.heard()[from..]).is_empty() {
        assert!(
            Instant::now() < deadline,
            "n1 was sent no grant while t was held"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    nodes[0].proxy.cut();
    let (status, body) = quota.join().unwrap();
    assert_eq!(status, 502, "the quota's call to n1 was answered: {body}");
    // The grants n1 was sent while the quota held `t` past the failover.
    let heard = nodes[0].proxy.heard();
    for g in grants_heard(&heard[from..]) {
        let list = g
            .split_once("\"primaries\":[")
            .and_then(|(_, rest)| rest.split_once(']'))
            .map(|(names, _)| names);
        assert!(
            list.is_some_and(|names| !names.contains("\"t\"")),
            "a grant to n1 named t, held through its failover: {g:?}"
        );
    }
    writes_again(port, "t");
    assert_eq!(placed(port, "t").0, replica);
}

/// The bodies of the grants in what a proxy heard that came whole. One the
/// proxy was cut off from before its body came was never sent whole: cut
/// once a grant's head was heard, the body behind it was not there yet
/// beside ten other copies of the suite and sixteen busy loops (4 runs of
/// 110), and read as a grant with no list. A grant n1 never answered
/// carries its list, the router not knowing what n1 holds.
fn grants_heard(heard: &str) -> Vec<&str> {
    heard
        .split("POST /_admin/lease")
        .skip(1)
        .filter_map(|g| {
            let (head, rest) = g.split_once("\r\n\r\n")?;
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length: "))?
                .trim()
                .parse()
                .ok()?;
            rest.get(..len)
        })
        .collect()
}

/// A router over a directory file of its own, leasing for [`TERM`] with
/// its rounds run by the test, and following `upstream` -- its primary
/// router -- when it is a standby.
fn leasing_router(path: &Path, upstream: Option<&str>) -> (Arc<Router>, u16) {
    let (mut db, feed) = fenec_http::replication::open(path.to_str().unwrap(), 8 << 20).unwrap();
    match upstream {
        Some(_) => {
            let lineage = db.history().lineage.clone();
            db.follow(lineage).unwrap();
        }
        None => {
            if db.history().lineage.is_empty() {
                db.fork(fresh_id()).unwrap();
                db.sync().unwrap();
            }
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
        upstream_timeout: nodes::upstream(),
        replicas: true,
        auto_failover: Some(TERM),
        ..Config::default()
    };
    let router = Router::replicated(
        dir,
        cfg,
        Replication::new(Some(DIRS.into()), Some(feed), follower),
    );
    let listener = router.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    let serving = Arc::clone(&router);
    std::thread::spawn(move || {
        let _ = serving.serve_on(listener);
    });
    (router, port)
}

const DIRS: &str = "directory-secret";

struct Files(PathBuf);

impl Drop for Files {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A standby router promoted counts every node's lease from its promotion.
/// It forgot every lease at each round while it followed, and once
/// promoted counted from the last of those rounds -- up to a third of a
/// lease before -- so a grant its primary sent after that round could still
/// run when the new primary failed the node over: two primaries. Here the
/// standby's last round while it follows comes well before the primary's
/// last grant to n1; promoted, it fails n1 over only once n1's own lease
/// from that grant has lapsed.
#[test]
fn a_promoted_standby_router_counts_every_lease_from_its_promotion() {
    let n1 = node("promoted1", true);
    let n2 = node("promoted2", true);
    let files =
        Files(std::env::temp_dir().join(format!("fenec-lease-stalls-dirs-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&files.0);
    std::fs::create_dir_all(&files.0).unwrap();
    let (a, a_port) = leasing_router(&files.0.join("a.fenec"), None);
    let upstream = format!("http://127.0.0.1:{a_port}");
    let (b, b_port) = leasing_router(&files.0.join("b.fenec"), Some(&upstream));
    for (name, nd, token) in [("n1", &n1, "adm-promoted1"), ("n2", &n2, "adm-promoted2")] {
        let body = format!(r#"{{"addr":"{}","token":"{token}"}}"#, nd.proxy.addr);
        let (status, body) = call(a_port, "PUT", &format!("/_shard/nodes/{name}"), &body, None);
        assert_eq!(status, 201, "{body}");
    }
    tenant_on(a_port, "t", "n1");
    let (_, replica) = placed(a_port, "t");
    assert_eq!(replica.as_deref(), Some("n2"));
    let deadline = Instant::now() + WITHIN;
    while !call(b_port, "GET", "/_shard/tenants", "", None)
        .1
        .contains("\"t\"")
    {
        assert!(Instant::now() < deadline, "the standby never had t");
        std::thread::sleep(Duration::from_millis(20));
    }

    // The standby's round while it follows, then, most of a lease later,
    // the primary's grant to n1, which runs a lease from then.
    round(&b);
    std::thread::sleep(TERM * 5 / 6);
    round(&a);
    let (status, lease) = call(n1.port, "GET", "/_admin/lease", "", Some("adm-promoted1"));
    assert_eq!(status, 200, "{lease}");
    assert!(
        lease.contains("\"primaries\":1"),
        "n1 holds no lease over t: {lease}"
    );

    // The standby promoted, the primary gone quiet, n1 cut off from both.
    let (status, body) = call(b_port, "POST", "/_replication/promote", "", Some(DIRS));
    assert_eq!(status, 200, "{body}");
    n1.proxy.cut();
    let deadline = Instant::now() + WITHIN;
    while placed(b_port, "t").0 == "n1" {
        assert!(Instant::now() < deadline, "t was never failed over");
        round(&b);
        std::thread::sleep(Duration::from_millis(20));
    }
    // Failed over: n1 takes no write of t, by its own clock.
    let (status, body) = query(n1.port, "/t/t/query", r#"put notes {title: "late"}"#);
    assert_ne!(
        status, 200,
        "n1 took a write of t after the promoted router failed it over: {body}"
    );
    drop(a);
}

/// A proxy that forwards, but once armed does with a request whose bytes
/// name `pattern` what it is told -- holds it until let go (from the review
/// of #340, `held_empty.rs`), or ends its connection unanswered -- and, cut,
/// carries nothing. A request held is out at the node as far as its client
/// can tell, so the node's end of that connection is not passed on while
/// it is: the node closes a connection idle past its `idle_timeout` (60 s
/// from its last use, which may be the setup's), and passed on, that close
/// ended the router's wait on a promotion meant to hang -- in the review of
/// #369, at 56 s of the test's 60, so the test passed with its bug put
/// back.
struct Filter {
    addr: String,
    armed: Arc<AtomicBool>,
    /// Ends a matching request's connection rather than hold it.
    refuse: Arc<AtomicBool>,
    cut: Arc<AtomicBool>,
    open: Arc<Mutex<Vec<TcpStream>>>,
    gate: Arc<(Mutex<(usize, bool)>, Condvar)>,
}

impl Filter {
    fn to(target: String, pattern: &'static [u8]) -> Filter {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let armed = Arc::new(AtomicBool::new(false));
        let refuse = Arc::new(AtomicBool::new(false));
        let cut = Arc::new(AtomicBool::new(false));
        let open = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new((Mutex::new((0usize, false)), Condvar::new()));
        let (a, r, c, o, g) = (
            Arc::clone(&armed),
            Arc::clone(&refuse),
            Arc::clone(&cut),
            Arc::clone(&open),
            Arc::clone(&gate),
        );
        std::thread::spawn(move || {
            for client in l.incoming() {
                let Ok(client) = client else { continue };
                if c.load(Ordering::SeqCst) {
                    let _ = client.shutdown(std::net::Shutdown::Both);
                    continue;
                }
                let Ok(server) = TcpStream::connect(&target) else {
                    continue;
                };
                o.lock().unwrap().push(client.try_clone().unwrap());
                o.lock().unwrap().push(server.try_clone().unwrap());
                let (a, r, g) = (Arc::clone(&a), Arc::clone(&r), Arc::clone(&g));
                let (c2, s2) = (client.try_clone().unwrap(), server.try_clone().unwrap());
                // Whether this connection's request is held at the gate, set
                // and cleared under the gate's lock.
                let holding = Arc::new(AtomicBool::new(false));
                let (h, back) = (Arc::clone(&holding), Arc::clone(&g));
                std::thread::spawn(move || {
                    let (mut from, mut to) = (c2, s2);
                    let mut buf = [0u8; 16 * 1024];
                    loop {
                        let n = match from.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        let named = buf[..n].windows(pattern.len()).any(|w| w == pattern);
                        if a.load(Ordering::SeqCst) && named {
                            if r.load(Ordering::SeqCst) {
                                let _ = from.shutdown(std::net::Shutdown::Both);
                                let _ = to.shutdown(std::net::Shutdown::Both);
                                return;
                            }
                            let (m, cv) = &*g;
                            let mut s = m.lock().unwrap();
                            h.store(true, Ordering::SeqCst);
                            s.0 += 1;
                            cv.notify_all();
                            while !s.1 {
                                s = cv.wait(s).unwrap();
                            }
                            h.store(false, Ordering::SeqCst);
                            cv.notify_all();
                        }
                        if to.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                    let _ = to.shutdown(std::net::Shutdown::Write);
                });
                std::thread::spawn(move || {
                    let (mut from, mut to) = (server, client);
                    let _ = std::io::copy(&mut from, &mut to);
                    let (m, cv) = &*back;
                    let mut s = m.lock().unwrap();
                    while holding.load(Ordering::SeqCst) {
                        s = cv.wait(s).unwrap();
                    }
                    drop(s);
                    let _ = to.shutdown(std::net::Shutdown::Write);
                });
            }
        });
        Filter {
            addr,
            armed,
            refuse,
            cut,
            open,
            gate,
        }
    }

    fn held(&self) -> usize {
        self.gate.0.lock().unwrap().0
    }

    fn release(&self) {
        self.gate.0.lock().unwrap().1 = true;
        self.gate.1.notify_all();
    }

    fn cut(&self) {
        self.cut.store(true, Ordering::SeqCst);
        for s in self.open.lock().unwrap().drain(..) {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }

    fn heal(&self) {
        self.cut.store(false, Ordering::SeqCst);
    }
}

/// A router leasing for [`TERM`] over n1 alone at first, with replicas.
fn router_with_replicas() -> u16 {
    leasing_replicas(upstream()).1
}

/// A router leasing for [`TERM`] with replicas, its upstream timeout
/// `upstream`, over no node yet.
fn leasing_replicas(upstream: Duration) -> (Arc<Router>, u16) {
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        upstream_timeout: upstream,
        replicas: true,
        auto_failover: Some(TERM),
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
    (router, port)
}

fn put_node(port: u16, name: &str, addr: &str, token: &str) {
    let body = format!(r#"{{"addr":"{addr}","token":"{token}"}}"#);
    let (status, body) = call(port, "PUT", &format!("/_shard/nodes/{name}"), &body, None);
    assert!(status == 200 || status == 201, "{body}");
}

/// Waits until `tenant` is placed on `to`: its node's failover, which takes
/// the tenants in name order, has come to it and to every one before it.
fn failover_done(port: u16, tenant: &str, to: &str) {
    let deadline = Instant::now() + WITHIN;
    while placed(port, tenant).0 != to {
        assert!(Instant::now() < deadline, "{tenant} was never failed over");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Holds that `tenant` stays placed on `node` for `long`, looked at as it
/// goes: the bound is a floor on how long it was watched, not a guess at
/// how soon anything moves.
fn stays_on(port: u16, tenant: &str, node: &str, long: Duration) {
    let until = Instant::now() + long;
    while Instant::now() < until {
        let (on, replica) = placed(port, tenant);
        assert_eq!(
            on, node,
            "{tenant} was promoted onto a copy that was not its copy as its node was failed \
             over (replica {replica:?})"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Waits for `t`'s row "one" to read through the router again: its rows
/// survived, wherever it is placed.
fn rows_survive(port: u16, what: &str) {
    let deadline = Instant::now() + WITHIN;
    loop {
        let (status, rows) = query(port, "/t/t/query", "get notes select title");
        if status == 200 && rows.contains("\"one\"") {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{what}: t's rows are gone ({status} {rows}), placed {:?}",
            placed(port, "t")
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A tenant held through its node's failover goes to the copy it had then,
/// and to no copy attached meanwhile. A repair holding `t` -- which had no
/// replica -- to give it one, its follow held on the way, as n1 is failed
/// over: once the repair lets go, `t` has a replica following n1, gone, and
/// holding nothing. Read again then rather than pinned as the failover ran,
/// that copy was promoted, `t`'s rows gone from it, and wiped from n1 once
/// n1 came back and followed it (the review's `held_empty.rs`). Pinned,
/// `t` had no copy to go to and stays on n1, whole, until n1 is back; and
/// the copy would be refused besides, of a lineage of its own.
#[test]
fn a_tenant_held_through_its_failover_goes_to_no_copy_attached_meanwhile() {
    let nodes = [1, 2, 3].map(|i| node(&format!("hempty{i}"), true));
    let h2 = Filter::to(format!("127.0.0.1:{}", nodes[1].port), b"/follow");
    let h3 = Filter::to(format!("127.0.0.1:{}", nodes[2].port), b"/follow");
    let port = router_with_replicas();
    // `t` made while n1 is the only node: it has no replica.
    put_node(port, "n1", &nodes[0].proxy.addr, "adm-hempty1");
    tenant_on(port, "t", "n1");
    written(port, "t", r#"put notes {id: 1, title: "one"}"#);
    put_node(port, "n2", &h2.addr, "adm-hempty2");
    put_node(port, "n3", &h3.addr, "adm-hempty3");
    // `u` on n1 with a replica, so n1 has something to fail over.
    tenant_on(port, "u", "n1");
    assert_eq!(placed(port, "t").1, None, "t has a replica already");
    let ru = placed(port, "u").1.expect("u has a replica");
    written(port, "u", r#"put notes {id: 1, title: "u"}"#);
    until(
        by_name(&nodes, &ru).port,
        "/t/u/query",
        "get notes select title",
        "\"u\"",
    );

    // A repair gives `t` a replica while n1 answers, its follow held; then
    // n1 goes, and its failover comes while the repair holds `t`.
    h2.armed.store(true, Ordering::SeqCst);
    h3.armed.store(true, Ordering::SeqCst);
    let repair = std::thread::spawn(move || call(port, "POST", "/_shard/replicas", "", None));
    let deadline = Instant::now() + WITHIN;
    while h2.held() + h3.held() == 0 {
        assert!(Instant::now() < deadline, "no follow held");
        std::thread::sleep(Duration::from_millis(10));
    }
    nodes[0].proxy.cut();
    failover_done(port, "u", &ru);
    h2.release();
    h3.release();
    let (status, body) = repair.join().unwrap();
    assert_eq!(status, 200, "{body}");
    assert!(
        placed(port, "t").1.is_some(),
        "the repair gave t no replica"
    );
    // Its claim let go of, `t` is the failover's to come back to: as soon
    // as that, and a lease and a tenth past n1's last grant at the latest.
    stays_on(port, "t", "n1", TERM * 2);
    // n1 back: `t` reads its rows, from n1, where they stayed.
    nodes[0].proxy.heal();
    rows_survive(
        port,
        "t was moved onto a copy attached after its failover began",
    );
    assert_eq!(placed(port, "t").0, "n1");
}

/// A copy attached while its primary is gone is never promoted. A repair
/// that ran after n1 died and before its lapse gave `t`, which had no
/// replica, one following n1, gone: an empty file, which n1's failover then
/// promoted, and the repair after n1 came back had n1's copy follow it,
/// `t`'s rows wiped -- so on main, before any of this branch. Now the repair
/// gives no tenant a copy while its primary does not answer, and a node
/// refuses to promote a copy of another lineage than its tenant's, as a
/// copy that never took its primary's image is: `t` stays on n1, whole,
/// until n1 is back.
#[test]
fn a_copy_attached_after_its_primary_died_is_never_promoted() {
    let nodes = [1, 2, 3].map(|i| node(&format!("dead{i}"), true));
    let port = router_with_replicas();
    put_node(port, "n1", &nodes[0].proxy.addr, "adm-dead1");
    tenant_on(port, "t", "n1");
    written(port, "t", r#"put notes {id: 1, title: "one"}"#);
    put_node(port, "n2", &nodes[1].proxy.addr, "adm-dead2");
    put_node(port, "n3", &nodes[2].proxy.addr, "adm-dead3");
    tenant_on(port, "u", "n1");
    let ru = placed(port, "u").1.expect("u has a replica");
    written(port, "u", r#"put notes {id: 1, title: "u"}"#);
    until(
        by_name(&nodes, &ru).port,
        "/t/u/query",
        "get notes select title",
        "\"u\"",
    );
    // Still no replica for `t` as n1 is cut off ([`leased_until_cut`]): n1
    // failed over during the setup and back would have had the repair
    // after its answer give `t` one, as it should, from n1 alive.
    assert_eq!(
        placed(port, "t"),
        ("n1".to_string(), None),
        "t got a replica before the test cut n1 off: the router failed n1 over as its \
         renewals ran past its lease, and the repair after gave t one"
    );
    // n1 goes, and a repair runs at once, well before n1's lease lapses.
    nodes[0].proxy.cut();
    let (_, repaired) = call(port, "POST", "/_shard/replicas", "", None);
    // `u` moved: n1's failover is done, `t`, before it, judged.
    failover_done(port, "u", &ru);
    assert_eq!(
        placed(port, "t").0,
        "n1",
        "t was promoted onto a copy made after n1 died (repair: {repaired})"
    );
    nodes[0].proxy.heal();
    rows_survive(
        port,
        &format!("t was promoted onto a copy made after n1 died (repair: {repaired})"),
    );
    assert_eq!(placed(port, "t").0, "n1");
}

/// A node refuses to promote a copy that is not its tenant as the router
/// knows it, and the copy goes on following; an operator's force promotes
/// it. Here the copy was made to follow an address nobody answers on: told
/// nothing, the node refuses it as holding nothing -- no write taken, no
/// hello since it opened; told the tenant's lineage, as a router tells it,
/// it refuses a copy of a lineage of its own, a new file's; forced, it
/// promotes it.
#[test]
fn a_node_refuses_to_promote_a_copy_that_is_not_its_tenant_unless_forced() {
    let n = node("refuse1", true);
    let admin = |method: &str, target: &str, body: &str| {
        call(n.port, method, target, body, Some("adm-refuse1"))
    };
    assert_eq!(admin("PUT", "/_admin/tenants/t", "").0, 201);
    let gone = TcpListener::bind("127.0.0.1:0").unwrap();
    let from = format!(r#"{{"from":"http://{}"}}"#, gone.local_addr().unwrap());
    drop(gone);
    let (status, body) = admin("POST", "/_admin/tenants/t/follow", &from);
    assert_eq!(status, 200, "{body}");
    let (status, body) = admin("POST", "/_admin/tenants/t/promote", "");
    assert_eq!(status, 409, "a copy that took nothing was promoted: {body}");
    assert!(
        body.contains("has taken no write") && body.contains("force"),
        "{body}"
    );
    let (status, body) = admin(
        "POST",
        "/_admin/tenants/t/promote",
        r#"{"lineage":"0badcafe"}"#,
    );
    assert_eq!(
        status, 409,
        "a copy of another lineage was promoted: {body}"
    );
    assert!(
        body.contains("not 0badcafe, the tenant's") && body.contains("force"),
        "{body}"
    );
    let (status, body) = admin("GET", "/_admin/tenants/t/lineage", "");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"ready\":false"), "{body}");
    let (status, body) = admin(
        "POST",
        "/_admin/tenants/t/promote",
        r#"{"lineage":"0badcafe","force":true}"#,
    );
    assert_eq!(status, 200, "a forced promotion was refused: {body}");
    // A primary now, following nobody: ready, whatever its primary said.
    let (status, body) = admin("GET", "/_admin/tenants/t/lineage", "");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"ready\":true"), "{body}");
}

/// A tenant that took no write fails over onto the copy that took its
/// image. Refused for having taken no write -- change 0 -- a tenant made
/// and not yet written stayed on its dead node, every write to it a 502,
/// and neither a promotion by hand nor a failover by hand moved it, though
/// its copy was of its lineage and had heard from its primary (the review
/// of #340, `empty_refused.rs`).
#[test]
fn a_tenant_that_took_no_write_fails_over_onto_the_copy_that_took_its_image() {
    let (nodes, port) = three("empty", true, TERM);
    let (status, body) = call(port, "PUT", "/_shard/tenants/e", r#"{"node":"n1"}"#, None);
    assert_eq!(status, 201, "{body}");
    let r = placed(port, "e").1.expect("e has a replica");
    let rn = by_name(&nodes, &r);
    let (_, made) = call(
        nodes[0].port,
        "GET",
        "/_admin/tenants/e/lineage",
        "",
        Some("adm-empty1"),
    );
    let lineage = made
        .split("\"lineage\":\"")
        .nth(1)
        .and_then(|l| l.get(..8))
        .unwrap_or_else(|| panic!("e's lineage: {made}"))
        .to_string();
    let tag = format!("adm-empty{}", &r[1..]);
    let deadline = Instant::now() + WITHIN;
    loop {
        let (_, copy) = call(rn.port, "GET", "/_admin/tenants/e/lineage", "", Some(&tag));
        if copy.contains(&format!("\"lineage\":\"{lineage}\"")) && copy.contains("\"ready\":true") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "e's copy never took its image: {copy}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    nodes[0].proxy.cut();
    failover_done(port, "e", &r);
    written(
        port,
        "e",
        "create collection if not exists notes (title text)",
    );
}

/// An old primary's copy that never heard from the tenant's new primary is
/// never promoted. `t` fails over from n1 onto its replica; n1 comes back
/// and a repair makes its copy -- the file n1 held as primary -- follow the
/// new one, whose stream to it is held; then the new primary goes too.
/// Promoted, n1's copy took back every write since the first failover: a
/// copy of the tenant's lineage, it held writes, so neither a check of its
/// lineage nor of its change let it be. It lacks the history the new
/// primary forked, which the router tells the node: `t` stays where its
/// rows are, and reads them all once that node is back. `u`, on the new
/// primary with a replica that heard from it, fails over after `t` -- a
/// node's failover takes its tenants in name order -- and says when `t`
/// was judged.
#[test]
fn an_old_primary_s_copy_that_never_heard_from_the_new_one_is_never_promoted() {
    let nodes = [1, 2, 3].map(|i| node(&format!("stale{i}"), true));
    let f2 = Filter::to(format!("127.0.0.1:{}", nodes[1].port), b"/t/t/_replication");
    let f3 = Filter::to(format!("127.0.0.1:{}", nodes[2].port), b"/t/t/_replication");
    let filters = [&f2, &f3];
    let port = router_with_replicas();
    put_node(port, "n1", &nodes[0].proxy.addr, "adm-stale1");
    put_node(port, "n2", &f2.addr, "adm-stale2");
    put_node(port, "n3", &f3.addr, "adm-stale3");
    tenant_on(port, "t", "n1");
    written(port, "t", r#"put notes {id: 1, title: "one"}"#);
    let r = placed(port, "t").1.expect("t has a replica");
    until(
        by_name(&nodes, &r).port,
        "/t/t/query",
        "get notes select title",
        "\"one\"",
    );
    let filter = filters[r[1..].parse::<usize>().unwrap() - 2];

    // n1 goes; `t` is promoted on its replica and takes a write there.
    nodes[0].proxy.cut();
    failover_done(port, "t", &r);
    written(port, "t", r#"put notes {id: 2, title: "two"}"#);
    // Every stream from the new primary held from here: n1's copy, made to
    // follow it, never hears from it.
    filter.armed.store(true, Ordering::SeqCst);
    nodes[0].proxy.heal();
    let deadline = Instant::now() + WITHIN;
    while placed(port, "t").1.as_deref() != Some("n1") {
        assert!(
            Instant::now() < deadline,
            "n1's copy was never made t's replica: {:?}",
            placed(port, "t")
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    while filter.held() == 0 {
        assert!(
            Instant::now() < deadline,
            "n1's copy never asked the new primary"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // `u` on the new primary, its replica -- on the third node, which holds
    // the fewest -- holding its row.
    tenant_on(port, "u", &r);
    written(port, "u", r#"put notes {id: 1, title: "u"}"#);
    let ru = placed(port, "u").1.expect("u has a replica");
    until(
        by_name(&nodes, &ru).port,
        "/t/u/query",
        "get notes select title",
        "\"u\"",
    );
    // The new primary goes, and its failover is judged.
    filter.cut();
    failover_done(port, "u", &ru);
    assert_eq!(
        placed(port, "t").0,
        r,
        "an old primary's copy that never heard from the new primary was promoted"
    );
    filter.heal();
    filter.release();
    let deadline = Instant::now() + WITHIN;
    loop {
        let (status, rows) = query(port, "/t/t/query", "get notes select title");
        if status == 200 && rows.contains("\"one\"") && rows.contains("\"two\"") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "t lost a write: {status} {rows}, placed {:?}",
            placed(port, "t")
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A tenant a repair passed over because its primary did not answer gets
/// its replica once the primary answers, with nothing more asked. A repair
/// runs only when a node failed over answers again or an operator asks, so
/// a primary missing the one look left its tenant with no replica until the
/// next.
#[test]
fn a_tenant_a_repair_passed_over_gets_its_replica_once_its_primary_answers() {
    let nodes = [1, 2].map(|i| node(&format!("later{i}"), true));
    let f1 = Filter::to(
        format!("127.0.0.1:{}", nodes[0].port),
        b"GET /_admin/tenants ",
    );
    let port = router_with_replicas();
    put_node(port, "n1", &f1.addr, "adm-later1");
    // `t` made while n1 is the only node: it has no replica.
    tenant_on(port, "t", "n1");
    put_node(port, "n2", &nodes[1].proxy.addr, "adm-later2");
    assert_eq!(placed(port, "t").1, None, "t has a replica already");
    // n1 answers its lease and every request but the repair's look.
    f1.refuse.store(true, Ordering::SeqCst);
    f1.armed.store(true, Ordering::SeqCst);
    let (status, body) = call(port, "POST", "/_shard/replicas", "", None);
    assert_eq!(status, 502, "{body}");
    assert!(body.contains("does not answer"), "{body}");
    f1.armed.store(false, Ordering::SeqCst);
    let deadline = Instant::now() + WITHIN;
    while placed(port, "t").1.is_none() {
        assert!(
            Instant::now() < deadline,
            "t got no replica once n1 answered the repair's look"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    until_failed_over(port, "n1", 0.0, "n1 was failed over");
}

/// A request with a body of bytes, its answer's status and body as bytes: a
/// tenant's image goes and comes so.
fn raw(port: u16, method: &str, target: &str, token: &str, body: &[u8]) -> (u16, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(WITHIN)).unwrap();
    write!(
        s,
        "{method} {target} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .unwrap();
    s.write_all(body).unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).unwrap();
    let status = String::from_utf8_lossy(&out[9..12]).parse().unwrap();
    let at = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    (status, out[at..].to_vec())
}

/// A tenant restored in place on its primary's node to a moment before its
/// last failover fails over again. The restored file -- what `fenec restore`
/// gives, installed with `DELETE` and `PUT .../file` -- holds the history it
/// had then, without the one the failover forked and the router names, and
/// so does every copy that takes it whole: refused for that, `t` stayed on
/// its dead primary, every write a 502, until an operator forced it -- and
/// on a lease nobody does (the review of #340, `restore_trap.rs`). The copy
/// heard from the node it follows since it was told to follow it, and that
/// is a copy of its primary whatever history the primary holds.
#[test]
fn a_tenant_restored_in_place_before_its_last_failover_fails_over_again() {
    let (nodes, port) = three("rtrap", true, TERM);
    let tok = |n: &str| format!("adm-rtrap{}", &n[1..]);
    tenant_on(port, "t", "n1");
    written(port, "t", r#"put notes {id: 1, title: "one"}"#);
    let r = placed(port, "t").1.expect("t has a replica");
    until(
        by_name(&nodes, &r).port,
        "/t/t/query",
        "get notes select title",
        "one",
    );
    // The file as it stands: what a restore to this moment would give.
    let (status, image) = raw(
        nodes[0].port,
        "GET",
        "/_admin/tenants/t/file",
        &tok("n1"),
        b"",
    );
    assert_eq!(status, 200);

    // n1 goes: t fails over onto r, whose promotion forks the history the
    // router keeps.
    nodes[0].proxy.cut();
    failover_done(port, "t", &r);
    written(port, "t", r#"put notes {id: 2, title: "two"}"#);
    // n1 back: its copy follows r.
    nodes[0].proxy.heal();
    until_failed_over(port, "n1", 0.0, "n1 never answered again");
    let deadline = Instant::now() + WITHIN;
    while placed(port, "t").1.as_deref() != Some("n1") {
        assert!(Instant::now() < deadline, "n1 never became t's replica");
        std::thread::sleep(Duration::from_millis(20));
    }
    until(nodes[0].port, "/t/t/query", "get notes select title", "two");

    // t restored on r, its primary, to the moment before the failover.
    let rp = by_name(&nodes, &r).port;
    let (status, b) = raw(rp, "DELETE", "/_admin/tenants/t", &tok(&r), b"");
    assert_eq!(status, 204, "{}", String::from_utf8_lossy(&b));
    let (status, b) = raw(rp, "PUT", "/_admin/tenants/t/file", &tok(&r), &image);
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&b));
    written(port, "t", r#"put notes {id: 3, title: "restored"}"#);
    // n1's copy takes the restored file whole.
    until(
        nodes[0].port,
        "/t/t/query",
        "get notes select title",
        "restored",
    );

    // r goes: n1's copy, which holds every row of its primary, is promoted.
    by_name(&nodes, &r).proxy.cut();
    failover_done(port, "t", "n1");
    written(port, "t", r#"put notes {id: 4, title: "after"}"#);
    until(port, "/t/t/query", "get notes select title", "restored");
}

/// A copy's word that it heard from the node it was told to follow is kept
/// beside its file (`.follows`), and asked as it is promoted where its
/// history lacks the one the router names, as a primary restored in place
/// leaves every copy: a node restarted between a follow and its hello still
/// refuses its copy -- its primary out of reach, the copy has heard nothing
/// since -- and one restarted after the hello, its primary out of reach,
/// promotes it, its `ready` not set in this open.
#[test]
fn a_copy_s_word_of_its_primary_s_hello_outlives_a_restart() {
    use fenec_http::tenants::{Expected, Replicated, Tenants};
    let p = node("heard1", false);
    let (status, body) = call(p.port, "PUT", "/_admin/tenants/t", "", Some("adm-heard1"));
    assert_eq!(status, 201, "{body}");
    let (status, body) = query(p.port, "/t/t/query", "create collection notes (title text)");
    assert_eq!(status, 200, "{body}");
    let (_, said) = call(
        p.port,
        "GET",
        "/_admin/tenants/t/lineage",
        "",
        Some("adm-heard1"),
    );
    let lineage = said
        .split("\"lineage\":\"")
        .nth(1)
        .and_then(|l| u32::from_str_radix(l.get(..8)?, 16).ok())
        .unwrap_or_else(|| panic!("t's lineage: {said}"));
    let link = Proxy::to(&format!("127.0.0.1:{}", p.port));
    let url = format!("http://{}", link.addr);
    let dir = std::env::temp_dir().join(format!("fenec-leases-heard-copy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let _gone = Files(dir.clone());
    let open = || {
        Tenants::new(&dir).unwrap().with_replication(Replicated {
            token: REPL.into(),
            buffer: 8 << 20,
            upstream: None,
            sync_on_write: true,
        })
    };
    // A restart: the tenants written out and held, as a process exits with
    // them -- each instance is kept, since a follower it started holds its
    // database -- then a registry opened anew over the same files.
    let restart = |old: Tenants| {
        old.shutdown();
        std::mem::forget(old);
        let new = open();
        new.resume_following();
        new
    };
    let ready = |copy: &Tenants| copy.get("t").unwrap().ready();
    let until_ready = |copy: &Tenants| {
        let deadline = Instant::now() + WITHIN;
        while !ready(copy) {
            assert!(
                Instant::now() < deadline,
                "the copy never heard from its primary"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    let copy = open();
    copy.create("t").unwrap();
    copy.follow("t", Some(&url)).unwrap();
    until_ready(&copy);
    // What a router names after a failover the copy's history lacks.
    let want = Expected {
        lineage: Some(lineage),
        history: Some((u64::from(lineage) << 32) | 0x0bad_f00d),
        force: false,
    };

    // Told to follow again, its primary out of reach: heard nothing since.
    link.cut();
    copy.follow("t", Some(&url)).unwrap();
    let refused = copy.promote_as("t", &want).unwrap_err();
    assert!(
        refused.0 == 409 && refused.1.contains("has heard nothing"),
        "{refused:?}"
    );
    // Restarted between the follow and a hello: still nothing heard.
    let copy = restart(copy);
    let refused = copy.promote_as("t", &want).unwrap_err();
    assert!(
        refused.0 == 409 && refused.1.contains("has heard nothing"),
        "a copy that heard nothing since its follow was promoted after a restart: {refused:?}"
    );
    // Its primary back: heard. Then out of reach, and the node restarted.
    link.heal();
    until_ready(&copy);
    link.cut();
    let copy = restart(copy);
    assert!(!ready(&copy), "heard from a primary out of reach");
    let promoted = copy.promote_as("t", &want);
    assert!(
        promoted.is_ok(),
        "a copy that heard from its primary was refused after a restart: {promoted:?}"
    );
}
