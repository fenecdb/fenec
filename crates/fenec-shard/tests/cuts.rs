//! What a router had out to a node as it failed the node over is answered
//! from the failover on, not left to wait for `--upstream-timeout`
//! (`upstream::Pool::cut`, `Router::after_cut`).
//!
//! A node dies here as a machine lost or cut off does: the wire between it
//! and the router stops carrying anything, either way, and closes nothing
//! -- no end and no reset reaches the router, which waited on such a node
//! for its timeout even once its tenants answered on their replicas. The
//! router's timeout is a minute and each client waits half of that, so an
//! answer that comes at all is the failover's doing; each test holds each
//! request unanswered as the failover begins, by an event, so the answer
//! came after it.
//!
//! A binary of its own: each test holds a thread a request beside three
//! nodes, their replication and a router, and `all` stands near macOS's
//! 2 048 threads a process.

#[path = "lease_nodes.rs"]
mod nodes;
#[path = "cut_wire.rs"]
mod wire;

use fenec_shard::directory::Directory;
use fenec_shard::{Config, Router};
use nodes::{call, node, placed, query, until, until_copied, written, Node};
use wire::*;

/// The lease of the test that waits for one to lapse: long enough that its
/// requests are out, and seen so, well before.
const LONG_TERM: Duration = Duration::from_secs(10);
/// A lease whose rounds come twenty minutes apart: no round's grant lands
/// while a test runs, and a node is granted only where the test has the
/// router grant it.
const NO_ROUND: Duration = Duration::from_secs(3600);
use std::sync::mpsc::TryRecvError;
use std::sync::Arc;
use std::time::Duration;

/// Three nodes, each behind its wire, and a router over them giving each
/// tenant a replica; leasing them for `lease`, and failing a node over on
/// its own once its lease lapses, where there is one.
struct Cluster {
    nodes: Vec<Node>,
    wires: Vec<Wire>,
    port: u16,
    router: Arc<Router>,
}

impl Cluster {
    fn port_of(&self, name: &str) -> u16 {
        self.node_of(name).port
    }

    fn node_of(&self, name: &str) -> &Node {
        &self.nodes[name[1..].parse::<usize>().unwrap() - 1]
    }
}

fn cluster(tag: &str, lease: Option<Duration>) -> Cluster {
    fenec_http::audit::set_delay(0);
    let nodes: Vec<Node> = (1..=3)
        .map(|i| node(&format!("cut-{tag}{i}"), lease.is_some()))
        .collect();
    let wires: Vec<Wire> = nodes.iter().map(|n| Wire::to(n.port)).collect();
    let router = Router::new(
        Directory::in_memory(),
        Config {
            addr: "127.0.0.1:0".into(),
            upstream_timeout: UPSTREAM,
            replicas: true,
            auto_failover: lease,
            ..Config::default()
        },
    );
    let listener = router.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    router.start_leasing().unwrap();
    let serving = Arc::clone(&router);
    std::thread::spawn(move || {
        let _ = serving.serve_on(listener);
    });
    for (i, w) in wires.iter().enumerate() {
        let body = format!(
            r#"{{"addr":"{}","token":"adm-cut-{tag}{}"}}"#,
            w.addr,
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
    Cluster {
        nodes,
        wires,
        port,
        router,
    }
}

/// Tenant `acme` on n1 with a replica, its collections made and on the
/// replica: the replica's node.
fn acme(c: &Cluster) -> String {
    let (status, body) = call(
        c.port,
        "PUT",
        "/_shard/tenants/acme",
        r#"{"node":"n1"}"#,
        None,
    );
    assert_eq!(status, 201, "{body}");
    // Each once `acme` is leased on n1 ([`nodes::fenced`]): at background
    // QoS beside busy loops and other suites, the ten-second lease of the
    // test that waits for one to lapse lapsed as these were sent.
    for made in [
        "create collection if not exists notes (title text)",
        "create collection if not exists jobs (run_at timestamp @sorted, owner text)",
        r#"put notes {id: 1, title: "before"}"#,
    ] {
        written(c.port, "acme", made);
    }
    let replica = placed(c.port, "acme").1.expect("acme has a replica");
    until(
        c.port_of(&replica),
        PATH,
        "get notes select title",
        "before",
    );
    replica
}

/// Each of `nodes` leased by the router, as every node is once a round has
/// come: a tenant of its own made there, which a create grants. A round
/// comes twenty minutes into a test leasing for [`NO_ROUND`].
fn leased(c: &Cluster, nodes: &[&str]) {
    for n in nodes {
        let at = format!(r#"{{"node":"{n}"}}"#);
        let target = format!("/_shard/tenants/own-{n}");
        let (status, b) = call(c.port, "PUT", &target, &at, None);
        assert_eq!(status, 201, "{b}");
    }
}

/// n1 failed over by hand with one request of each kind out to it, its wire
/// silent: a read is answered by the replica promoted, a keyed write n1 ran
/// is answered once -- from its key, which reached the replica -- an
/// unkeyed one n1 ran is answered 502 and not run again, a held claim under
/// a key waits on the new primary and takes the job put there, one with no
/// key is answered 502, and the subscription ends. Each waited for the
/// router's minute before; without the cut no answer comes within the
/// client's half of it.
#[test]
fn what_was_out_to_a_node_failed_over_by_hand_is_answered() {
    let c = cluster("hand", None);
    let replica = acme(&c);
    let rport = c.port_of(&replica);
    let before = cuts(c.port);
    let stream = subscribed(c.port);

    // n1's answers go nowhere, its replica's stream on: writes it runs and
    // never answers, claims it holds.
    c.wires[0].swallow_answers();
    let unkeyed = asking(
        c.port,
        "POST",
        PATH,
        "",
        &body(r#"put notes {title: "plain"}"#),
    );
    let keyed = asking(
        c.port,
        "POST",
        PATH,
        "Idempotency-Key: note-1\r\n",
        &body(r#"put notes {title: "keyed"}"#),
    );
    until(rport, PATH, "get notes select title", "\"plain\"");
    until(rport, PATH, "get notes select title", "\"keyed\"");
    let held_keyed = asking(
        c.port,
        "POST",
        PATH,
        "Fenec-Wait: 20000\r\nIdempotency-Key: claim-1\r\n",
        &claim("with-key"),
    );
    let held = asking(
        c.port,
        "POST",
        PATH,
        "Fenec-Wait: 20000\r\n",
        &claim("no-key"),
    );
    c.wires[0].until_heard("with-key");
    c.wires[0].until_heard("no-key");
    // Then nothing at all: a read goes out into it.
    c.wires[0].fall_silent();
    let read = asking(c.port, "GET", "/t/acme/notes?select=title", "", "");
    c.wires[0].until_heard("GET /t/acme/notes?select=title");

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
    let (status, failover) = call(c.port, "POST", "/_shard/nodes/n1/failover", "", None);
    assert_eq!(status, 200, "{failover}");
    assert_eq!(placed(c.port, "acme").0, replica);

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

    // The keyed claim waits on the new primary, and takes the job put
    // there now.
    let (status, b) = query(c.port, PATH, "put jobs {run_at: 1}");
    assert_eq!(status, 200, "{b}");
    let (status, _, b) = answer(&held_keyed, "the keyed claim");
    assert_eq!(status, 200, "{b}");
    assert!(b.contains(r#"[{"id":"#), "the job: {b}");

    // Nothing ran twice.
    assert_eq!(titled(rport, "plain"), 1);
    assert_eq!(titled(rport, "keyed"), 1);
    let after = cuts(c.port);
    assert!(after[0] - before[0] >= 3.0, "{before:?} -> {after:?}");
    assert!(after[1] - before[1] >= 2.0, "{before:?} -> {after:?}");
    assert!(after[2] - before[2] >= 1.0, "{before:?} -> {after:?}");
}

/// n1 cut off with one request of each kind out to it, failed over on its
/// lease: each is answered once the lease lapses, as by hand. n1 never got
/// any of them -- the router cannot tell -- so the keyed write runs once on
/// the new primary and the unkeyed one nowhere. The lease is ten seconds,
/// so the five requests are out and seen unanswered well before it lapses:
/// at three, a loaded machine could have the failover come first. The
/// claims wait thirty seconds, the most a request may: the router judges
/// the lapse a lease and a tenth after n1's last renewal, at a round, a
/// third of a lease apart, once the grant to n1 out then has given up, a
/// third of a lease at most -- 17.7 s in all before it promotes -- and at
/// twenty the keyed claim's wait ran out before the job was put, beside
/// three other suites and eight busy loops at background QoS (2 runs of
/// 3). The job is put once the keyed write is answered, which the new
/// primary does once it holds the lease naming the tenant there.
#[test]
fn what_was_out_to_a_node_failed_over_on_its_lease_is_answered() {
    let c = cluster("lease", Some(LONG_TERM));
    let replica = acme(&c);
    let rport = c.port_of(&replica);
    let stream = subscribed(c.port);

    c.wires[0].fall_silent();
    let read = asking(c.port, "GET", "/t/acme/notes?select=title", "", "");
    let unkeyed = asking(
        c.port,
        "POST",
        PATH,
        "",
        &body(r#"put notes {title: "plain"}"#),
    );
    let keyed = asking(
        c.port,
        "POST",
        PATH,
        "Idempotency-Key: note-1\r\n",
        &body(r#"put notes {title: "keyed"}"#),
    );
    let held_keyed = asking(
        c.port,
        "POST",
        PATH,
        "Fenec-Wait: 30000\r\nIdempotency-Key: claim-1\r\n",
        &claim("with-key"),
    );
    let held = asking(
        c.port,
        "POST",
        PATH,
        "Fenec-Wait: 30000\r\n",
        &claim("no-key"),
    );
    for marker in [
        "GET /t/acme/notes?select=title",
        "plain\\\"",
        "note-1",
        "with-key",
        "no-key",
    ] {
        c.wires[0].until_heard(marker);
    }
    // Out, and the tenant still on n1: its lease has not lapsed.
    assert_eq!(placed(c.port, "acme").0, "n1");
    for (rx, what) in [
        (&unkeyed, "the unkeyed write"),
        (&keyed, "the keyed write"),
        (&held_keyed, "the keyed claim"),
        (&held, "the unkeyed claim"),
        (&read, "the read"),
    ] {
        unanswered(rx, what);
    }

    let (status, _, rows) = answer(&read, "the read");
    assert_eq!(status, 200, "{rows}");
    assert!(rows.contains("before"), "{rows}");
    assert_eq!(placed(c.port, "acme").0, replica);
    let (status, head, b) = answer(&keyed, "the keyed write");
    assert_eq!(status, 200, "{b}");
    assert!(
        !head.to_ascii_lowercase().contains("idempotent-replayed"),
        "{head}"
    );
    // The new primary took the keyed write: it holds the lease naming
    // `acme`, and the job goes in while the keyed claim waits there.
    let (status, b) = query(c.port, PATH, "put jobs {run_at: 1}");
    let tok = format!("adm-cut-lease{}", &replica[1..]);
    let lease = call(rport, "GET", "/_admin/lease", "", Some(&tok));
    assert_eq!(status, 200, "{b} lease {lease:?}");
    let (status, _, b) = answer(&held_keyed, "the keyed claim");
    assert_eq!(status, 200, "{b}");
    assert!(b.contains(r#"[{"id":"#), "the job: {b}");
    let (status, _, b) = answer(&unkeyed, "the unkeyed write");
    assert_eq!(status, 502, "{b}");
    let (status, _, b) = answer(&held, "the unkeyed claim");
    assert_eq!(status, 502, "{b}");
    assert_eq!(stream.recv_timeout(WITHIN), Ok(true), "the stream");
    assert_eq!(titled(rport, "plain"), 0);
    assert_eq!(titled(rport, "keyed"), 1);
}

/// Under leases a write cut at its tenant's promotion reaches the copy
/// promoted before the router's grant names the tenant there -- the
/// failover grants the node once every tenant is promoted -- and the copy
/// refuses it, unrun, 503: the router grants it its list then and asks once
/// more, so a keyed write and a keyed claim are answered by the new
/// primary, not refused. The lease is long, so no round lands between the
/// failover and the answers.
#[test]
fn a_write_cut_under_leases_is_answered_by_the_copy_promoted_not_refused() {
    let c = cluster("granted", Some(Duration::from_secs(60)));
    let replica = acme(&c);
    let rport = c.port_of(&replica);
    c.wires[0].fall_silent();
    let keyed = asking(
        c.port,
        "POST",
        PATH,
        "Idempotency-Key: note-1\r\n",
        &body(r#"put notes {title: "keyed"}"#),
    );
    let held_keyed = asking(
        c.port,
        "POST",
        PATH,
        "Fenec-Wait: 20000\r\nIdempotency-Key: claim-1\r\n",
        &claim("with-key"),
    );
    c.wires[0].until_heard("note-1");
    c.wires[0].until_heard("with-key");
    unanswered(&keyed, "the keyed write");
    unanswered(&held_keyed, "the keyed claim");
    let (status, failover) = call(c.port, "POST", "/_shard/nodes/n1/failover", "", None);
    assert_eq!(status, 200, "{failover}");
    let (status, _, b) = answer(&keyed, "the keyed write");
    assert_eq!(status, 200, "{b}");
    assert_eq!(titled(rport, "keyed"), 1);
    let (status, b) = query(c.port, PATH, "put jobs {run_at: 1}");
    assert_eq!(status, 200, "{b}");
    let (status, _, b) = answer(&held_keyed, "the keyed claim");
    assert_eq!(status, 200, "{b}");
    assert!(b.contains(r#"[{"id":"#), "the job: {b}");
}

/// A tenant failed over by hand under leases takes writes on its new
/// primary as soon as the failover is answered: the failover grants the
/// node its tenants went to before it answers, as a lapse's does. Left to
/// the next round -- twenty minutes away here, a third of a lease in all --
/// the copy promoted refused every write, unrun, 503, until then: its node
/// held a lease, and the lease named no `acme` there.
#[test]
fn a_tenant_failed_over_by_hand_under_leases_takes_writes_at_once() {
    let c = cluster("hand-lease", Some(NO_ROUND));
    leased(&c, &["n2", "n3"]);
    let replica = acme(&c);
    c.wires[0].fall_silent();
    let (status, failover) = call(c.port, "POST", "/_shard/nodes/n1/failover", "", None);
    assert_eq!(status, 200, "{failover}");
    let (status, b) = query(c.port, PATH, r#"put notes {title: "after"}"#);
    assert_eq!(status, 200, "{b}");
    assert_eq!(titled(c.port_of(&replica), "after"), 1);
}

/// Two tenants of n1, each with its replica on n2 -- n3 taken out of the
/// directory, so both go there -- failed over by hand under leases, a
/// keyed write of each out to n1 as it falls silent. `acme` is
/// promoted first, its write cut, sent to n2 and refused there -- no grant
/// names it -- and the grant it asks for reads n2's list then, `acme`
/// alone, and is held at n2's wire. `beta` is promoted, its write cut,
/// refused in turn, and joins that grant. The grant goes on, naming `acme`
/// alone, and the failover's own, naming both, is held at the wire: `beta`'s
/// write, sent again, is refused once more, and asks for a grant of its own,
/// which reads n2's list after its promotion. Asking once, it was answered
/// the copy's 503.
#[test]
fn a_write_cut_onto_a_copy_whose_grant_read_its_list_too_soon_asks_again() {
    let c = cluster("again", Some(NO_ROUND));
    let (status, b) = call(c.port, "DELETE", "/_shard/nodes/n3", "", None);
    assert!(status == 200 || status == 204, "{b}");
    // A grant to a node holding no lease is followed at once by the one
    // acknowledging it: leased first, n2 is sent each grant below as one
    // request.
    leased(&c, &["n2"]);
    for t in ["acme", "beta"] {
        let (status, b) = call(
            c.port,
            "PUT",
            &format!("/_shard/tenants/{t}"),
            r#"{"node":"n1"}"#,
            None,
        );
        assert_eq!(status, 201, "{b}");
        let path = format!("/t/{t}/query");
        let made = "create collection notes (title text)";
        assert_eq!(query(c.port, &path, made).0, 200);
        assert_eq!(
            query(c.port, &path, r#"put notes {title: "before"}"#).0,
            200
        );
        assert_eq!(placed(c.port, t).1.as_deref(), Some("n2"));
        until(c.port_of("n2"), &path, "get notes select title", "before");
    }
    let n2 = &c.wires[1];
    n2.park("/_admin/lease");
    n2.park("/_admin/tenants/beta/promote");
    c.wires[0].fall_silent();
    let write = |t: &str| {
        let path = format!("/t/{t}/query");
        let key = format!("Idempotency-Key: {t}-key\r\n");
        asking(
            c.port,
            "POST",
            &path,
            &key,
            &body(r#"put notes {title: "keyed"}"#),
        )
    };
    let (a, b) = (write("acme"), write("beta"));
    c.wires[0].until_heard("acme-key");
    c.wires[0].until_heard("beta-key");
    unanswered(&a, "acme's write");
    unanswered(&b, "beta's write");
    let port = c.port;
    let failover =
        std::thread::spawn(move || call(port, "POST", "/_shard/nodes/n1/failover", "", None));

    // acme's write refused on n2 and its grant held there, read before
    // beta's promotion, which is held too.
    n2.until_parked("/_admin/lease", 1);
    n2.until_parked("/_admin/tenants/beta/promote", 1);
    n2.let_through("/_admin/tenants/beta/promote", 1);
    let joined = std::time::Instant::now() + WITHIN;
    while c.router.cut_grants_waiting() < 2 {
        assert!(
            std::time::Instant::now() < joined,
            "beta's write joined no grant"
        );
        std::thread::yield_now();
    }
    n2.let_through("/_admin/lease", 1);
    let (status, _, body_a) = answer(&a, "acme's write");
    assert_eq!(status, 200, "{body_a}");
    // beta's sent again after the grant naming acme alone, and refused.
    n2.until_sent("beta-key", 2);
    n2.let_through("/_admin/lease", 1 << 20);
    let (status, _, body_b) = answer(&b, "beta's write");
    assert_eq!(status, 200, "{body_b}");
    let (status, b) = failover.join().unwrap();
    assert_eq!(status, 200, "{b}");
}

/// A failover cuts its own tenants' requests and no other's: two tenants
/// on n1, `tenant-1` and `tenant-q` -- of 8 bytes, the last differing only
/// in its top bits, which the mark kept low dropped, so they shared one --
/// `tenant-1`'s copy deleted so that a failover by hand leaves it on n1,
/// `tenant-q` promoted. An unkeyed write of `tenant-1`, which n1 ran and
/// whose answer had begun on the wire short of a whole status line as the
/// failover ran, is answered as n1 answered it, 200. Cut with `tenant-q`'s,
/// it was a 502 saying it may have run, though n1 still served `tenant-1`
/// and its answer was on its way.
#[test]
fn a_failover_leaves_another_tenant_s_request_on_the_node_be() {
    let c = cluster("mark", None);
    for t in ["tenant-1", "tenant-q"] {
        let (status, b) = call(
            c.port,
            "PUT",
            &format!("/_shard/tenants/{t}"),
            r#"{"node":"n1"}"#,
            None,
        );
        assert_eq!(status, 201, "{b}");
        let path = format!("/t/{t}/query");
        let (status, b) = query(c.port, &path, "create collection notes (title text)");
        assert_eq!(status, 200, "{b}");
    }
    // `tenant-q`'s copy has taken its primary's image and heard from it: it
    // is the tenant the failover promotes. A copy that has not is of a
    // lineage of its own, a new file's, and its node refuses to promote it
    // (409, `tenants::Expected`): failed over first, `tenant-q` stayed on n1
    // -- on CI's ubuntu runner, in 9 runs of 400 on three cores beside three
    // busy loops, and in every run with its image held a second at n1's
    // wire.
    let q = placed(c.port, "tenant-q")
        .1
        .expect("tenant-q has a replica");
    until_copied("tenant-q", &c.nodes[0], c.node_of(&q));
    let copy = placed(c.port, "tenant-1")
        .1
        .expect("tenant-1 has a replica");
    let tok = format!("adm-cut-mark{}", &copy[1..]);
    let target = "/_admin/tenants/tenant-1";
    let (status, b) = call(c.port_of(&copy), "DELETE", target, "", Some(&tok));
    assert!(status == 200 || status == 204, "{status} {b}");

    c.wires[0].hold_answer_after(5);
    let write = asking(
        c.port,
        "POST",
        "/t/tenant-1/query",
        "",
        &body(r#"put notes {title: "plain"}"#),
    );
    c.wires[0].until_passed();
    unanswered(&write, "the write");
    let (_, failover) = call(c.port, "POST", "/_shard/nodes/n1/failover", "", None);
    assert_eq!(placed(c.port, "tenant-1").0, "n1", "{failover}");
    assert_ne!(placed(c.port, "tenant-q").0, "n1", "{failover}");
    c.wires[0].release();
    let (status, _, b) = answer(&write, "the write");
    assert_eq!(status, 200, "{b}");
    let (status, got) = query(c.nodes[0].port, "/t/tenant-1/query", "get notes count");
    assert_eq!(status, 200, "{got}");
    assert!(got.contains("\"count\":1"), "{got}");
}

/// A read whose answer is under way as its node is failed over -- its head
/// read, its body coming -- is the node's: the failover leaves it, and it
/// comes whole, one answer. The wire passes the first 16 MB of it and holds
/// the rest, more than the sockets between hold, so the router has read
/// its head before the failover begins. Cut, its body would have been cut
/// short: a 502 that the node broke off.
#[test]
fn an_answer_under_way_as_its_node_is_failed_over_comes_whole() {
    let c = cluster("whole", None);
    let replica = acme(&c);
    let title = "x".repeat(1 << 20);
    for _ in 0..20 {
        let sql = format!(r#"put notes {{title: "{title}"}}"#);
        let (status, b) = query(c.port, PATH, &sql);
        assert_eq!(status, 200, "{b}");
    }
    let count = || {
        let (_, b) = query(c.port_of(&replica), PATH, "get notes count");
        b
    };
    let deadline = std::time::Instant::now() + WITHIN;
    while !count().contains("\"count\":21") {
        assert!(std::time::Instant::now() < deadline, "{}", count());
        std::thread::sleep(Duration::from_millis(20));
    }

    c.wires[0].hold_answer_after(16 << 20);
    let read = asking(c.port, "POST", PATH, "", &body("get notes select title"));
    c.wires[0].until_passed();
    unanswered(&read, "the read");
    let (status, failover) = call(c.port, "POST", "/_shard/nodes/n1/failover", "", None);
    assert_eq!(status, 200, "{failover}");
    c.wires[0].release();
    let (status, _, rows) = answer(&read, "the read");
    assert_eq!(status, 200, "{}", &rows[..rows.len().min(200)]);
    assert_eq!(rows.matches("\"title\"").count(), 21);
    assert!(
        rows.ends_with(']'),
        "{}",
        &rows[rows.len().saturating_sub(200)..]
    );
}

/// The same through two routers: the global router relays a tenant's
/// requests to the router of region `us`, which keeps its own directory and
/// fails its node over. The cut is the region's router's, and its answers
/// go back as any: the read answered by the replica promoted, the write
/// that may have run a 502, and the stream ended at the region's router,
/// whose end the relaying router passes on.
#[test]
fn a_cut_at_a_region_s_router_is_passed_on_by_the_router_relaying_to_it() {
    fenec_http::audit::set_delay(0);
    let nodes: Vec<Node> = (1..=2)
        .map(|i| node(&format!("cut-relay{i}"), false))
        .collect();
    let wires: Vec<Wire> = nodes.iter().map(|n| Wire::to(n.port)).collect();
    let config = |region: Option<&str>| Config {
        addr: "127.0.0.1:0".into(),
        upstream_timeout: UPSTREAM,
        replicas: true,
        region: region.map(String::from),
        ..Config::default()
    };
    let serve = |router: Arc<Router>| {
        let listener = router.bind().unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let _ = router.serve_on(listener);
        });
        port
    };
    let regional = Router::regional(
        Directory::in_memory(),
        config(Some("us")),
        None,
        Directory::in_memory(),
    )
    .unwrap();
    let home = serve(regional);
    for (i, w) in wires.iter().enumerate() {
        let body = format!(
            r#"{{"addr":"{}","token":"adm-cut-relay{}","region":"us"}}"#,
            w.addr,
            i + 1
        );
        let target = format!("/_shard/nodes/n{}", i + 1);
        let (status, body) = call(home, "PUT", &target, &body, None);
        assert!(status == 200 || status == 201, "{body}");
    }
    // Made as the global router has a region's router make it.
    let (status, _, b) = ask(
        home,
        "PUT",
        "/_shard/tenants/acme",
        "Fenec-Global: 1\r\n",
        r#"{"node":"n1"}"#,
    );
    assert_eq!(status, 201, "{b}");
    let mut d = Directory::in_memory();
    let routers = [format!("http://127.0.0.1:{home}")];
    d.set_region("us", Some(&routers)).unwrap();
    d.place_home("acme", "us").unwrap();
    let front = serve(Router::new(d, config(None)));

    let (status, b) = query(front, PATH, "create collection notes (title text)");
    assert_eq!(status, 200, "{b}");
    assert_eq!(query(front, PATH, r#"put notes {title: "before"}"#).0, 200);
    let replica = placed(home, "acme").1.expect("acme has a replica");
    let rport = nodes[replica[1..].parse::<usize>().unwrap() - 1].port;
    until(rport, PATH, "get notes select title", "before");
    let stream = subscribed(front);

    wires[0].fall_silent();
    let read = asking(front, "GET", "/t/acme/notes?select=title", "", "");
    let plain = asking(
        front,
        "POST",
        PATH,
        "",
        &body(r#"put notes {title: "plain"}"#),
    );
    wires[0].until_heard("GET /t/acme/notes?select=title");
    wires[0].until_heard("plain\\\"");
    unanswered(&read, "the read");
    unanswered(&plain, "the write");
    assert_eq!(
        stream.try_recv(),
        Err(TryRecvError::Empty),
        "the stream ended"
    );
    let (status, failover) = call(home, "POST", "/_shard/nodes/n1/failover", "", None);
    assert_eq!(status, 200, "{failover}");

    let (status, _, rows) = answer(&read, "the read");
    assert_eq!(status, 200, "{rows}");
    assert!(rows.contains("before"), "{rows}");
    let (status, _, b) = answer(&plain, "the write");
    assert_eq!(status, 502, "{b}");
    assert!(b.contains("may have run"), "{b}");
    assert_eq!(stream.recv_timeout(WITHIN), Ok(true), "the stream");
    assert_eq!(titled(rport, "plain"), 0);
}
