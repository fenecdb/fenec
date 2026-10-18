//! Automatic failover: the router leases each node the tenants it places
//! there, and fails a node over once its lease has certainly lapsed.
//!
//! The router reaches each node here through a proxy the test can cut, which
//! stands for a node gone or cut off: its clients still reach it, the router
//! and its replicas do not. What must hold is that it stops taking writes
//! before its tenants take them elsewhere -- no tenant with two primaries.

#[path = "lease_nodes.rs"]
mod nodes;

use fenec_shard::directory::Directory;
use fenec_shard::{Config, Router};
use nodes::*;
use std::sync::Arc;
use std::time::{Duration, Instant};

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

/// Under leases a node lost is failed over within its region: its tenants
/// go to their replicas, which were placed in their region and in another
/// zone, past a node of another region holding less.
#[test]
fn a_lapsed_lease_fails_a_node_over_within_its_region() {
    let nodes = [1, 2, 3, 4].map(|i| node(&format!("region{i}"), true));
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
    // eu1 and eu2 in two zones of eu, us1 and us2 in two of us.
    for (i, (name, region, zone)) in [
        ("eu1", "eu", "a"),
        ("eu2", "eu", "b"),
        ("us1", "us", "a"),
        ("us2", "us", "b"),
    ]
    .iter()
    .enumerate()
    {
        let body = format!(
            r#"{{"addr":"{}","token":"adm-region{}","region":"{region}","zone":"{zone}"}}"#,
            nodes[i].proxy.addr,
            i + 1
        );
        let (status, body) = call(port, "PUT", &format!("/_shard/nodes/{name}"), &body, None);
        assert_eq!(status, 201, "{body}");
    }
    // eu2 is the busiest node, so a replica placed by load alone would go
    // to us.
    for t in ["e1", "e2"] {
        let (status, body) = call(
            port,
            "PUT",
            &format!("/_shard/tenants/{t}"),
            r#"{"node":"eu2"}"#,
            None,
        );
        assert_eq!(status, 201, "{body}");
    }
    let (status, body) = call(
        port,
        "PUT",
        "/_shard/tenants/acme",
        r#"{"node":"eu1","schema":"create collection notes (title text)"}"#,
        None,
    );
    assert_eq!(status, 201, "{body}");
    assert!(body.contains(r#""replica_on":"eu2""#), "{body}");
    let path = "/t/acme/query";
    assert_eq!(query(port, path, r#"put notes {title: "one"}"#).0, 200);
    until(nodes[1].port, path, "get notes select title", "one");

    // eu1 is cut off: once its lease has lapsed, acme takes writes again on
    // eu2, in eu.
    nodes[0].proxy.cut();
    let deadline = Instant::now() + Duration::from_secs(30);
    while query(port, path, r#"put notes {title: "after"}"#).0 != 200 {
        assert!(Instant::now() < deadline, "acme was never failed over");
        std::thread::sleep(Duration::from_millis(20));
    }
    let (status, body) = call(port, "GET", "/_shard/tenants/acme", "", None);
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains(r#""node":"eu2","state":"active","region":"eu""#),
        "{body}"
    );
    for (i, us) in nodes.iter().enumerate().skip(2) {
        let token = format!("adm-region{}", i + 1);
        let (_, held) = call(us.port, "GET", "/_admin/tenants", "", Some(&token));
        assert!(!held.contains("acme"), "{held}");
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
