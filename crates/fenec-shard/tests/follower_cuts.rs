//! What a router following another's directory had out to a node the
//! other failed over is answered from the failover on, as the failing
//! router's own is (`Router::catch_up_directories`): a standby router,
//! and a region's router forwarding by its copy of the global directory,
//! fail nothing over, and what they had out to a node gone with no reset
//! waited for `--upstream-timeout` though the tenant answered on its new
//! primary. The directory says which placement was a failover
//! (`failed_from`), so a move ends nothing.
//!
//! The wires are `cuts.rs`'s: a node dies as a machine lost does, nothing
//! carried and nothing closed. The routers' timeout is a minute and each
//! client waits half of that, so an answer that comes at all is the
//! failover's doing; each request is held unanswered as the failover
//! begins, by an event.
//!
//! A binary of its own, as `cuts.rs` is: each test holds a thread a
//! request beside three nodes, their replication and two routers. Its
//! tests run one at a time ([`serial`]): the cuts counted are the
//! process's, every router's in it, and each test holds what its own
//! routers counted exactly.

#[path = "lease_nodes.rs"]
mod nodes;
#[path = "cut_wire.rs"]
mod wire;

use fenec_core::prelude::Database;
use fenec_http::replication::{fresh_id, Follower, Replication};
use fenec_shard::directory::Directory;
use fenec_shard::{Config, Router};
use nodes::{call, node, placed, until, until_copied, Node};
use std::io::Write;
use std::net::SocketAddr;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::TryRecvError;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};
use wire::*;

/// The binary's tests one at a time: `fenec_router_cut_requests_total` is
/// the process's, and counted beside another test's failover a test's
/// deltas held what that one cut too.
fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// Waits until `f` holds, bounded as a client's wait is: asked again
/// each millisecond, where a spin held a core for the wait.
fn until_so(what: &str, f: impl Fn() -> bool) {
    let deadline = Instant::now() + WITHIN;
    while !f() {
        assert!(Instant::now() < deadline, "{what} never came");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The directories' replication token: a standby's and a region's router's
/// streams of the directory they follow.
const TOKEN: &str = "follower-cuts-directory";
/// A lease whose rounds come twenty minutes apart: no round's grant lands
/// while a test runs, and a node is granted only where the test has the
/// router grant it.
const NO_ROUND: Duration = Duration::from_secs(3600);

struct Files(PathBuf);

impl Drop for Files {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn files(tag: &str) -> Files {
    let d = std::env::temp_dir().join(format!("fenec-follower-cuts-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    Files(d)
}

/// A directory database at `path`, leading -- its history forked -- or
/// following the router at `follows`, and what replicates it.
fn directory(path: &Path, follows: Option<u16>) -> (Arc<RwLock<Database>>, Arc<Replication>) {
    let (mut db, feed) = fenec_http::replication::open(path.to_str().unwrap(), 8 << 20).unwrap();
    match follows {
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
    let follower = follows.map(|port| {
        let f = Follower::new(
            &format!("http://127.0.0.1:{port}"),
            TOKEN.into(),
            Arc::clone(&db),
            Some(Arc::clone(&feed)),
            true,
        )
        .unwrap();
        f.start("fenec-test-follower".into()).unwrap();
        f
    });
    let repl = Replication::new(Some(TOKEN.into()), Some(feed), follower);
    (db, repl)
}

fn config(lease: Option<Duration>) -> Config {
    Config {
        addr: "127.0.0.1:0".into(),
        upstream_timeout: UPSTREAM,
        replicas: true,
        auto_failover: lease,
        ..Config::default()
    }
}

/// Serves `router`, leasing as `--auto-failover` has it -- a standby's
/// rounds lease nothing while it follows: its port.
fn serve(router: &Arc<Router>) -> u16 {
    let listener = router.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    router.start_leasing().unwrap();
    let serving = Arc::clone(router);
    std::thread::spawn(move || {
        let _ = serving.serve_on(listener);
    });
    port
}

/// Three nodes behind their wires, a router leading a directory over them
/// and leasing them for `lease` where there is one, and a standby router
/// following it, started with the same flags, as the deploy kit's units
/// start one.
struct Pair {
    nodes: Vec<Node>,
    wires: Vec<Wire>,
    primary: u16,
    standby: u16,
    standby_router: Arc<Router>,
    _files: Files,
}

impl Pair {
    fn port_of(&self, name: &str) -> u16 {
        self.nodes[name[1..].parse::<usize>().unwrap() - 1].port
    }
}

/// A [`Pair`], `addr` naming each node in the directory: its wire's
/// address, or a name a test looks up for it.
fn pair(tag: &str, lease: Option<Duration>, addr: impl Fn(usize, &Wire) -> String) -> Pair {
    fenec_http::audit::set_delay(0);
    let f = files(tag);
    let nodes: Vec<Node> = (1..=3)
        .map(|i| node(&format!("fcut-{tag}{i}"), lease.is_some()))
        .collect();
    let wires: Vec<Wire> = nodes.iter().map(|n| Wire::to(n.port)).collect();
    let (db, repl) = directory(&f.0.join("primary.fenec"), None);
    let lead = Router::replicated(Directory::load(db).unwrap(), config(lease), repl);
    let primary = serve(&lead);
    for (i, w) in wires.iter().enumerate() {
        let body = format!(
            r#"{{"addr":"{}","token":"adm-fcut-{tag}{}"}}"#,
            addr(i, w),
            i + 1
        );
        let target = format!("/_shard/nodes/n{}", i + 1);
        let (status, body) = call(primary, "PUT", &target, &body, None);
        assert!(status == 200 || status == 201, "{body}");
    }
    let (db, repl) = directory(&f.0.join("standby.fenec"), Some(primary));
    let standby_router = Router::replicated(Directory::load(db).unwrap(), config(lease), repl);
    let standby = serve(&standby_router);
    Pair {
        nodes,
        wires,
        primary,
        standby,
        standby_router,
        _files: f,
    }
}

/// Where the router at `port` places `tenant`, as its maps have it:
/// `None` where they hold no such tenant yet.
fn placed_by(port: u16, tenant: &str) -> Option<String> {
    let (status, list) = call(port, "GET", "/_shard/tenants", "", None);
    if status != 200 {
        return None;
    }
    let entry = list
        .split("},{")
        .find(|e| e.contains(&format!("\"name\":\"{tenant}\"")))?;
    let rest = entry.split("\"node\":\"").nth(1)?;
    Some(rest.split('"').next()?.to_string())
}

/// Waits until the router at `port` places `tenant` on `node`: a
/// standby's maps caught up. A catch-up ends what it found failed over
/// before its maps are read, so whatever it cut is cut by then.
fn until_placed(port: u16, tenant: &str, node: &str) {
    let deadline = Instant::now() + WITHIN;
    while placed_by(port, tenant).as_deref() != Some(node) {
        assert!(
            Instant::now() < deadline,
            "the router at {port} never placed {tenant} on {node}: {:?}",
            placed_by(port, tenant)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Tenant `acme` on n1 with a replica, made through the router at `at`,
/// its collections made and on the replica: the replica's node.
fn acme(at: u16, port_of: impl Fn(&str) -> u16) -> String {
    let (status, b) = call(at, "PUT", "/_shard/tenants/acme", r#"{"node":"n1"}"#, None);
    assert_eq!(status, 201, "{b}");
    for made in [
        "create collection notes (title text)",
        "create collection jobs (run_at timestamp @sorted, owner text)",
    ] {
        let (status, b) = nodes::query(at, PATH, made);
        assert_eq!(status, 200, "{made}: {b}");
    }
    assert_eq!(
        nodes::query(at, PATH, r#"put notes {title: "before"}"#).0,
        200
    );
    let replica = placed(at, "acme").1.expect("acme has a replica");
    until(port_of(&replica), PATH, "get notes select title", "before");
    replica
}

/// One request of each kind out to n1 through the router at `front`, which
/// follows the directory the router at `leader` keeps, n1's wire then
/// failed over by hand at `leader`: a read answered by the replica
/// promoted, a keyed write n1 ran answered once from its key, an unkeyed
/// one 502 and not run again, a keyed held claim handed the job put after,
/// an unkeyed one 502, and the subscription ended -- each by `front`'s own
/// cut, read off the directory it follows, and counted once. `rport` is the
/// replica's node and `copy` its wire: the job is put once the keyed claim
/// has been sent there, so the claim's own wait bounds nothing.
fn what_was_out_is_answered(front: u16, leader: u16, n1: &Wire, rport: u16, copy: &Wire) {
    let before = cuts(front);
    let stream = subscribed(front);

    n1.swallow_answers();
    let unkeyed = asking(
        front,
        "POST",
        PATH,
        "",
        &body(r#"put notes {title: "plain"}"#),
    );
    let keyed = asking(
        front,
        "POST",
        PATH,
        "Idempotency-Key: note-1\r\n",
        &body(r#"put notes {title: "keyed"}"#),
    );
    until(rport, PATH, "get notes select title", "\"plain\"");
    until(rport, PATH, "get notes select title", "\"keyed\"");
    let held_keyed = asking(
        front,
        "POST",
        PATH,
        "Fenec-Wait: 30000\r\nIdempotency-Key: claim-1\r\n",
        &claim("with-key"),
    );
    let held = asking(
        front,
        "POST",
        PATH,
        "Fenec-Wait: 30000\r\n",
        &claim("no-key"),
    );
    n1.until_heard("with-key");
    n1.until_heard("no-key");
    n1.fall_silent();
    let read = asking(front, "GET", "/t/acme/notes?select=title", "", "");
    n1.until_heard("GET /t/acme/notes?select=title");
    for (rx, what) in [
        (&unkeyed, "the unkeyed write"),
        (&keyed, "the keyed write"),
        (&held_keyed, "the keyed claim"),
        (&held, "the unkeyed claim"),
        (&read, "the read"),
    ] {
        unanswered(rx, what);
    }
    assert_eq!(
        stream.try_recv(),
        Err(TryRecvError::Empty),
        "the stream ended"
    );

    let (status, failover) = call(leader, "POST", "/_shard/nodes/n1/failover", "", None);
    assert_eq!(status, 200, "{failover}");

    let (status, _, rows) = answer(&read, "the read");
    assert_eq!(status, 200, "{rows}");
    assert!(
        rows.contains("\"plain\"") && rows.contains("\"keyed\""),
        "{rows}"
    );
    let (status, head, b) = answer(&keyed, "the keyed write");
    assert_eq!(status, 200, "{b}");
    assert!(
        head.to_ascii_lowercase()
            .contains("idempotent-replayed: true"),
        "{head}"
    );
    let (status, _, b) = answer(&unkeyed, "the unkeyed write");
    assert_eq!(status, 502, "{b}");
    assert!(b.contains("may have run"), "{b}");
    let (status, _, b) = answer(&held, "the unkeyed claim");
    assert_eq!(status, 502, "{b}");
    assert_eq!(stream.recv_timeout(WITHIN), Ok(true), "the stream");

    // Sent on to the copy promoted: held there, or about to be, and the
    // job put now its to take.
    copy.until_sent("claim-1", 1);
    let (status, b) = nodes::query(front, PATH, "put jobs {run_at: 1}");
    assert_eq!(status, 200, "{b}");
    let (status, _, b) = answer(&held_keyed, "the keyed claim");
    assert_eq!(status, 200, "{b}");
    assert!(b.contains(r#"[{"id":"#), "the job: {b}");

    assert_eq!(titled(rport, "plain"), 1);
    assert_eq!(titled(rport, "keyed"), 1);
    // Three sent again, two that may have run, one stream: each once.
    let after = cuts(front);
    let counted: Vec<f64> = after.iter().zip(before).map(|(a, b)| a - b).collect();
    assert_eq!(counted, [3.0, 2.0, 1.0], "{before:?} -> {after:?}");
}

/// A standby router's clients, n1 failed over by hand at the primary: what
/// the standby had out to n1 is answered as the primary's own would be,
/// from the moment its copy of the directory shows the failover, nothing
/// more asked of the standby: the writes landing in its directory have it
/// read them. It waited for the timeout before; no answer came within the
/// client's half of it.
#[test]
fn what_a_standby_had_out_to_a_node_failed_over_is_answered() {
    let _serial = serial();
    let p = pair("sb", None, |_, w| w.addr.clone());
    let replica = acme(p.primary, |n| p.port_of(n));
    until_placed(p.standby, "acme", "n1");
    let rport = p.port_of(&replica);
    let copy = &p.wires[replica[1..].parse::<usize>().unwrap() - 1];
    what_was_out_is_answered(p.standby, p.primary, &p.wires[0], rport, copy);
}

/// The same through a region's router whose copy of the global directory
/// places the tenant (`Target::Node`): the global router fails n1 over,
/// and the region's router ends what it had out to n1 as its copy shows
/// it.
#[test]
fn what_a_region_s_router_had_out_by_its_copy_of_the_global_directory_is_answered() {
    let _serial = serial();
    fenec_http::audit::set_delay(0);
    let f = files("region");
    let nodes: Vec<Node> = (1..=3)
        .map(|i| node(&format!("fcut-region{i}"), false))
        .collect();
    let wires: Vec<Wire> = nodes.iter().map(|n| Wire::to(n.port)).collect();
    let (db, repl) = directory(&f.0.join("global.fenec"), None);
    let global = Router::replicated(Directory::load(db).unwrap(), config(None), repl);
    let gport = serve(&global);
    for (i, w) in wires.iter().enumerate() {
        let body = format!(
            r#"{{"addr":"{}","token":"adm-fcut-region{}"}}"#,
            w.addr,
            i + 1
        );
        let target = format!("/_shard/nodes/n{}", i + 1);
        let (status, body) = call(gport, "PUT", &target, &body, None);
        assert!(status == 200 || status == 201, "{body}");
    }
    let (gdb, _following) = directory(&f.0.join("us.global.fenec"), Some(gport));
    let cfg = Config {
        region: Some("us".into()),
        global: vec![format!("http://127.0.0.1:{gport}")],
        ..config(None)
    };
    let us = Router::regional(
        Directory::in_memory(),
        cfg,
        None,
        Directory::load(gdb).unwrap(),
    )
    .unwrap();
    let uport = serve(&us);
    let port_of = |n: &str| nodes[n[1..].parse::<usize>().unwrap() - 1].port;
    let replica = acme(gport, port_of);
    // The region's router places the tenant by its copy: its own
    // directory lists none of the global one's.
    until(uport, PATH, "get notes select title", "before");
    let copy = &wires[replica[1..].parse::<usize>().unwrap() - 1];
    what_was_out_is_answered(uport, gport, &wires[0], port_of(&replica), copy);
}

/// A move a standby reads ends nothing it had out: a write out to n1 as
/// its tenant moves to n3 -- held at n1's wire, unanswered, all through
/// the move -- is n1's to answer once the standby's maps place the tenant
/// on n3. n1, the move's source, still answers, and refuses on its own
/// what it no longer holds: ended as a failover's is, the write was a 502
/// saying it may have run.
#[test]
fn a_move_a_standby_reads_ends_nothing_it_had_out() {
    let _serial = serial();
    let p = pair("move", None, |_, w| w.addr.clone());
    let replica = acme(p.primary, |n| p.port_of(n));
    until_placed(p.standby, "acme", "n1");
    let to = ["n2", "n3"].into_iter().find(|n| *n != replica).unwrap();
    p.wires[0].park("parked-write");
    let parked = asking(
        p.standby,
        "POST",
        PATH,
        "",
        &body(r#"put notes {title: "parked-write"}"#),
    );
    p.wires[0].until_parked("parked-write", 1);
    let move_to = format!(r#"{{"to":"{to}"}}"#);
    let (status, b) = call(
        p.primary,
        "POST",
        "/_shard/tenants/acme/move",
        &move_to,
        None,
    );
    assert_eq!(status, 200, "{b}");
    // Its catch-up's cuts made, if it made any: they are made before the
    // maps it read are let go of.
    until_placed(p.standby, "acme", to);
    unanswered(&parked, "the write out to the move's source");
    p.wires[0].let_through("parked-write", 1);
    let (status, _, b) = answer(&parked, "the write out to the move's source");
    assert_ne!(status, 502, "{b}");
    assert!(!b.contains("may have run"), "{b}");
}

/// A name the system's resolver is stood in for, its lookups let through
/// or held until let go -- a forward dialing n1 holds there, its maps read
/// and its connection not yet made, where no cut can see it.
struct Lookups {
    state: Arc<(Mutex<(bool, usize)>, Condvar)>,
}

impl Lookups {
    fn of(name: &str, to: SocketAddr) -> Lookups {
        let state = Arc::new((Mutex::new((false, 0)), Condvar::new()));
        let s = Arc::clone(&state);
        fenec_http::dial::stand_in(name, move || {
            let (m, cv) = &*s;
            let mut g = m.lock().unwrap();
            if g.0 {
                g.1 += 1;
                cv.notify_all();
                while g.0 {
                    g = cv.wait(g).unwrap();
                }
            }
            Ok(vec![to])
        });
        Lookups { state }
    }

    fn hold(&self) {
        self.state.0.lock().unwrap().0 = true;
    }

    /// Waits until a lookup is held: a dial is under way.
    fn until_held(&self) {
        let (m, cv) = &*self.state;
        let (g, waited) = cv
            .wait_timeout_while(m.lock().unwrap(), WITHIN, |g| g.1 == 0)
            .unwrap();
        assert!(!waited.timed_out(), "no lookup came: {:?}", *g);
    }

    fn release(&self) {
        self.state.0.lock().unwrap().0 = false;
        self.state.1.notify_all();
    }
}

impl Drop for Lookups {
    fn drop(&mut self) {
        self.release();
    }
}

/// A standby's forward that read the maps before a failover and goes out
/// after the cut looked writes nothing to the node failed over: n1, named
/// by a name looked up as the forward dials, is alive -- failed over by
/// hand, it still takes writes, as the operator who failed it over says
/// it would not -- and the forward's lookup is held across the failover
/// and the standby's catch-up. Let go, it finds its tenant's count moved,
/// reads the maps again and goes to the replica promoted. Sent on, the
/// write landed on n1, on the history the failover left.
#[test]
fn a_standby_s_forward_that_read_the_maps_before_the_failover_goes_where_they_say_after() {
    let _serial = serial();
    let n1 = format!("n1.fcut-dial-{}.test", std::process::id());
    let lookups = std::cell::OnceCell::new();
    let p = pair("dial", None, |i, w| match i {
        0 => {
            let at: SocketAddr = w.addr.parse().unwrap();
            let name = format!("{n1}:8081");
            assert!(lookups.set(Lookups::of(&name, at)).is_ok());
            name
        }
        _ => w.addr.clone(),
    });
    let lookups = lookups.into_inner().unwrap();
    let replica = acme(p.primary, |n| p.port_of(n));
    until_placed(p.standby, "acme", "n1");
    let rport = p.port_of(&replica);

    // The standby has dialed n1 for nothing yet: its forward dials now.
    lookups.hold();
    let write = asking(
        p.standby,
        "POST",
        PATH,
        "",
        &body(r#"put notes {title: "late"}"#),
    );
    lookups.until_held();
    let (status, failover) = call(p.primary, "POST", "/_shard/nodes/n1/failover", "", None);
    assert_eq!(status, 200, "{failover}");
    until_placed(p.standby, "acme", &replica);
    unanswered(&write, "the write");
    lookups.release();

    let (status, _, b) = answer(&write, "the write");
    assert_eq!(status, 200, "{b}");
    assert_eq!(titled(rport, "late"), 1, "on the replica promoted");
    assert_eq!(titled(p.nodes[0].port, "late"), 0, "on n1, failed over");
}

/// Under leases a write a standby's cut sends to the copy promoted may
/// come before the primary's grant names the tenant there, and the copy
/// refuses it, unrun, 503. The standby grants nothing -- its lists may lag
/// the primary's -- and asks the copy again until the primary's grant has
/// come: here held at the copy's wire until the standby has been refused
/// and asked again. Passed on, the client had the 503; granted, the
/// standby's grant was a second router's lease on the node. Sent again
/// however many times, the write is counted sent again once: counted at
/// each ask, one request was seven.
#[test]
fn a_standby_s_write_refused_by_a_copy_not_yet_granted_waits_for_the_grant() {
    let _serial = serial();
    let p = pair("grant", Some(NO_ROUND), |_, w| w.addr.clone());
    leased(p.primary);
    let replica = acme(p.primary, |n| p.port_of(n));
    until_placed(p.standby, "acme", "n1");
    let rport = p.port_of(&replica);
    let copy = &p.wires[replica[1..].parse::<usize>().unwrap() - 1];

    p.wires[0].fall_silent();
    let keyed = asking(
        p.standby,
        "POST",
        PATH,
        "Idempotency-Key: note-1\r\n",
        &body(r#"put notes {title: "keyed"}"#),
    );
    p.wires[0].until_heard("note-1");
    unanswered(&keyed, "the keyed write");
    copy.park("/_admin/lease");
    let before = cuts(p.standby);
    let failover = fail_over_n1(p.primary);
    // Refused by the copy, and asked again: neither passed on nor granted.
    copy.until_sent("note-1", 3);
    unanswered(&keyed, "the keyed write");
    assert_eq!(p.standby_router.cut_grants_waiting(), 0);
    assert_eq!(p.standby_router.copy_asks_waiting(), 1);
    copy.let_through("/_admin/lease", 1 << 20);
    let (status, _, b) = answer(&keyed, "the keyed write");
    assert_eq!(status, 200, "{b}");
    assert_eq!(titled(rport, "keyed"), 1);
    let after = cuts(p.standby);
    let counted: Vec<f64> = after.iter().zip(before).map(|(a, b)| a - b).collect();
    assert_eq!(counted, [1.0, 0.0, 0.0], "{before:?} -> {after:?}");
    let (status, b) = failover.join().unwrap();
    assert_eq!(status, 200, "{b}");
}

/// A write a standby's cut sends to the copy promoted, its bucket moved
/// before its first byte by another tenant's failover the standby reads
/// after -- here `beta`'s, of n1 too and in `acme`'s bucket, its
/// promotion held at its copy's wire while the write's dial of `acme`'s
/// copy is held at its lookup -- wrote nothing, and is still what it was:
/// cut, waiting for the leading router's grant to the copy, and counted
/// sent again once. Taken for a request never cut, the copy's 503 went to
/// its client.
#[test]
fn a_standby_s_cut_write_whose_bucket_another_failover_moves_still_waits_for_the_grant() {
    let _serial = serial();
    let lookups: [std::cell::OnceCell<Lookups>; 3] = Default::default();
    let p = pair("moved", Some(NO_ROUND), |i, w| {
        let at: SocketAddr = w.addr.parse().unwrap();
        let name = format!("n{}.fcut-moved-{}.test:8081", i + 1, std::process::id());
        assert!(lookups[i].set(Lookups::of(&name, at)).is_ok());
        name
    });
    let lookups = lookups.map(|l| l.into_inner().unwrap());
    leased(p.primary);
    let replica = acme(p.primary, |n| p.port_of(n));
    let beta = (0..100_000)
        .map(|i| format!("tenant-{i}"))
        .find(|t| fenec_shard::upstream::same_bucket(t, "acme"))
        .expect("one in 64 names is in a bucket");
    let at = format!("/_shard/tenants/{beta}");
    let (status, b) = call(p.primary, "PUT", &at, r#"{"node":"n1"}"#, None);
    assert_eq!(status, 201, "{b}");
    let beta_replica = placed(p.primary, &beta).1.expect("beta has a replica");
    assert_ne!(
        beta_replica, replica,
        "beta's copy is on the node acme's is not"
    );
    // Its copy has taken its primary's image and heard from it: one that
    // has not is of a lineage of its own, which its node refuses to promote
    // (409), and `beta` stayed on n1 where its image was held two seconds
    // at n1's wire.
    let index = |n: &str| n[1..].parse::<usize>().unwrap() - 1;
    until_copied(&beta, &p.nodes[0], &p.nodes[index(&beta_replica)]);
    until_placed(p.standby, "acme", "n1");
    until_placed(p.standby, &beta, "n1");
    let (x, y) = (index(&replica), index(&beta_replica));
    // The primary's pool holds a connection to each node, so the failover's
    // calls dial nothing: the lookup held is the standby's.
    let (status, b) = call(p.primary, "GET", "/_shard/tenants?bytes", "", None);
    assert_eq!(status, 200, "{b}");

    p.wires[0].fall_silent();
    let keyed = asking(
        p.standby,
        "POST",
        PATH,
        "Idempotency-Key: moved-1\r\n",
        &body(r#"put notes {title: "keyed"}"#),
    );
    p.wires[0].until_heard("moved-1");
    unanswered(&keyed, "the keyed write");
    let beta_promoted = format!("{beta}/promote");
    p.wires[x].park("/_admin/lease");
    p.wires[y].park(&beta_promoted);
    lookups[x].hold();
    let before = cuts(p.standby);
    let failover = fail_over_n1(p.primary);
    // acme promoted and its write cut: sent on, it dials acme's copy.
    until_placed(p.standby, "acme", &replica);
    lookups[x].until_held();
    // beta promoted: the standby's catch-up moves acme's bucket.
    p.wires[y].let_through(&beta_promoted, 1);
    until_placed(p.standby, &beta, &beta_replica);
    lookups[x].release();
    // Refused by the copy, and asked again.
    p.wires[x].until_sent("moved-1", 2);
    unanswered(&keyed, "the keyed write");
    p.wires[x].let_through("/_admin/lease", 1 << 20);
    let (status, _, b) = answer(&keyed, "the keyed write");
    assert_eq!(status, 200, "{b}");
    assert_eq!(titled(p.port_of(&replica), "keyed"), 1);
    let after = cuts(p.standby);
    let counted: Vec<f64> = after.iter().zip(before).map(|(a, b)| a - b).collect();
    assert_eq!(counted, [1.0, 0.0, 0.0], "{before:?} -> {after:?}");
    let (status, b) = failover.join().unwrap();
    assert_eq!(status, 200, "{b}");
}

/// n2 and n3 leased by the router at `port`: a create of a tenant of its
/// own on each grants it.
fn leased(port: u16) {
    for n in ["n2", "n3"] {
        let at = format!(r#"{{"node":"{n}"}}"#);
        let target = format!("/_shard/tenants/own-{n}");
        let (status, b) = call(port, "PUT", &target, &at, None);
        assert_eq!(status, 201, "{b}");
    }
}

/// n1 failed over by hand at the router at `port`, on a thread of its own:
/// its grant to the copy may be held at the copy's wire.
fn fail_over_n1(port: u16) -> std::thread::JoinHandle<(u16, String)> {
    std::thread::spawn(move || call(port, "POST", "/_shard/nodes/n1/failover", "", None))
}

/// A standby's write a copy not yet granted refuses, its client gone while
/// the standby waits to ask again: the standby stops asking -- at once,
/// its pause watching the client -- and the write never lands, the copy
/// granted after. Asked on past its client, it was sent again until the
/// grant came or two thirds of the lease ran out: 40 minutes at this one.
#[test]
fn a_standby_stops_asking_a_copy_for_a_client_that_left() {
    let _serial = serial();
    let p = pair("left", Some(NO_ROUND), |_, w| w.addr.clone());
    leased(p.primary);
    let replica = acme(p.primary, |n| p.port_of(n));
    until_placed(p.standby, "acme", "n1");
    let rport = p.port_of(&replica);
    let copy = &p.wires[replica[1..].parse::<usize>().unwrap() - 1];

    p.wires[0].fall_silent();
    let sql = body(r#"put notes {title: "left"}"#);
    let mut client = TcpStream::connect(("127.0.0.1", p.standby)).unwrap();
    write!(
        client,
        "POST {PATH} HTTP/1.1\r\nHost: t\r\nIdempotency-Key: left-1\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{sql}",
        sql.len()
    )
    .unwrap();
    p.wires[0].until_heard("left-1");
    copy.park("/_admin/lease");
    let failover = fail_over_n1(p.primary);
    copy.until_sent("left-1", 2);
    until_so("the standby waiting to ask again", || {
        p.standby_router.copy_asks_waiting() == 1
    });
    drop(client);
    until_so("the standby stopping", || {
        p.standby_router.copy_asks_waiting() == 0
    });
    let sent = copy.times_sent("left-1");
    copy.let_through("/_admin/lease", 1 << 20);
    // The failover answered once it granted the copy, which takes writes
    // now: one through the standby lands.
    let (status, b) = failover.join().unwrap();
    assert_eq!(status, 200, "{b}");
    let (status, b) = nodes::query(p.standby, PATH, r#"put notes {title: "after"}"#);
    assert_eq!(status, 200, "{b}");
    assert_eq!(titled(rport, "after"), 1);
    assert_eq!(
        copy.times_sent("left-1"),
        sent,
        "asked again after its client left"
    );
    assert_eq!(titled(rport, "left"), 0);
}

/// A region's router that leads its own directory under leases, and a
/// tenant its copy of the global directory places, failed over by the
/// global router: the region's router cuts its write and waits for the
/// global router's grant to reach the copy -- held at the copy's wire --
/// asking the copy again, and grants nothing, the tenant being no tenant
/// of the directory it leads. Taken for its own, the router granted the
/// copy its own list, which names nothing there, twice, and passed the
/// copy's 503 on.
#[test]
fn a_region_s_leading_router_waits_for_the_global_grant_to_a_copy_it_follows() {
    let _serial = serial();
    fenec_http::audit::set_delay(0);
    let f = files("lead");
    let nodes: Vec<Node> = (1..=3)
        .map(|i| node(&format!("fcut-lead{i}"), true))
        .collect();
    let wires: Vec<Wire> = nodes.iter().map(|n| Wire::to(n.port)).collect();
    let (db, repl) = directory(&f.0.join("global.fenec"), None);
    let global = Router::replicated(Directory::load(db).unwrap(), config(Some(NO_ROUND)), repl);
    let gport = serve(&global);
    for (i, w) in wires.iter().enumerate() {
        let body = format!(
            r#"{{"addr":"{}","token":"adm-fcut-lead{}"}}"#,
            w.addr,
            i + 1
        );
        let target = format!("/_shard/nodes/n{}", i + 1);
        let (status, body) = call(gport, "PUT", &target, &body, None);
        assert!(status == 200 || status == 201, "{body}");
    }
    leased(gport);
    let (gdb, _following) = directory(&f.0.join("us.global.fenec"), Some(gport));
    let cfg = Config {
        region: Some("us".into()),
        global: vec![format!("http://127.0.0.1:{gport}")],
        ..config(Some(NO_ROUND))
    };
    let us = Router::regional(
        Directory::in_memory(),
        cfg,
        None,
        Directory::load(gdb).unwrap(),
    )
    .unwrap();
    let uport = serve(&us);
    let port_of = |n: &str| nodes[n[1..].parse::<usize>().unwrap() - 1].port;
    let replica = acme(gport, port_of);
    until(uport, PATH, "get notes select title", "before");
    let copy = &wires[replica[1..].parse::<usize>().unwrap() - 1];

    wires[0].fall_silent();
    let keyed = asking(
        uport,
        "POST",
        PATH,
        "Idempotency-Key: note-g\r\n",
        &body(r#"put notes {title: "keyed"}"#),
    );
    wires[0].until_heard("note-g");
    unanswered(&keyed, "the keyed write");
    copy.park("/_admin/lease");
    let before = cuts(uport);
    let failover = fail_over_n1(gport);
    // Refused and asked again past the two asks a grant of its own came
    // with: the router waits, its own leases none of the copy's.
    copy.until_sent("note-g", 4);
    unanswered(&keyed, "the keyed write");
    assert_eq!(us.cut_grants_waiting(), 0);
    assert_eq!(us.copy_asks_waiting(), 1);
    copy.let_through("/_admin/lease", 1 << 20);
    let (status, _, b) = answer(&keyed, "the keyed write");
    assert_eq!(status, 200, "{b}");
    assert_eq!(titled(port_of(&replica), "keyed"), 1);
    let after = cuts(uport);
    let counted: Vec<f64> = after.iter().zip(before).map(|(a, b)| a - b).collect();
    assert_eq!(counted, [1.0, 0.0, 0.0], "{before:?} -> {after:?}");
    let (status, b) = failover.join().unwrap();
    assert_eq!(status, 200, "{b}");
}
