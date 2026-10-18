//! A region keeping its own directory (`fenec-shard --region`): its router
//! leases its nodes and fails them over with no word from any other region,
//! so a region cut off from the global router goes on taking writes and
//! fails a node over in itself -- never with two nodes taking one tenant's
//! writes -- while what needs every region, a create or a move across, is
//! refused until the regions reach each other again.
//!
//! The global router stands in eu, us keeps its own directory. Every link
//! between the two goes through a proxy the test cuts: us's router reaching
//! the global one (its copy of the global directory, the changes it sends
//! there), the eu routers reaching us's (the relays, the changes they ask
//! of it). A node of us is reached through a proxy too, which cuts it off
//! from its region's router while its own clients still reach it.

use fenec_http::replication::{fresh_id, Follower, Replication};
use fenec_http::tenants::{Replicated, Tenants};
use fenec_shard::directory::Directory;
use fenec_shard::{Config, Router};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

const ROOT: &str = "routers-token";
const DIRS: &str = "directory-replication";
const REPL: &str = "tenant-replication";
/// The lease the routers grant. A renewal comes every third of it, and a
/// loaded machine has to be late by two of them for a node to stop: at 600
/// ms one came 499 ms late beside eight busy loops (`leases.rs`).
const TERM: Duration = Duration::from_secs(3);

struct Node {
    port: u16,
    dir: PathBuf,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn node(tag: &str) -> Node {
    let dir = std::env::temp_dir().join(format!("fenec-rl-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let tenants = Tenants::new(&dir)
        .unwrap()
        .with_replication(Replicated {
            token: REPL.into(),
            buffer: 8 << 20,
            upstream: None,
            sync_on_write: true,
        })
        .with_lease();
    let cfg = fenec_http::Config {
        addr: "127.0.0.1:0".into(),
        admin_token: Some(format!("adm-{tag}")),
        sync_on_write: true,
        ..fenec_http::Config::default()
    };
    let server = fenec_http::Server::with_tenants(Arc::new(tenants), cfg);
    let listener = server.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    Node { port, dir }
}

/// A TCP proxy: cut, it drops what it carries and refuses every connection
/// after, until healed.
struct Proxy {
    addr: String,
    cut: Arc<AtomicBool>,
    open: Arc<Mutex<Vec<TcpStream>>>,
}

impl Proxy {
    fn to(port: u16) -> Proxy {
        let target = format!("127.0.0.1:{port}");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let cut = Arc::new(AtomicBool::new(false));
        let open = Arc::new(Mutex::new(Vec::new()));
        let (flag, streams) = (Arc::clone(&cut), Arc::clone(&open));
        std::thread::spawn(move || {
            for client in listener.incoming() {
                let Ok(client) = client else { continue };
                if flag.load(Ordering::SeqCst) {
                    let _ = client.shutdown(Shutdown::Both);
                    continue;
                }
                let Ok(server) = TcpStream::connect(&target) else {
                    continue;
                };
                let mut held = streams.lock().unwrap();
                held.push(client.try_clone().unwrap());
                held.push(server.try_clone().unwrap());
                drop(held);
                let (a, b) = (client.try_clone().unwrap(), server.try_clone().unwrap());
                std::thread::spawn(move || copy(a, b));
                std::thread::spawn(move || copy(server, client));
            }
        });
        Proxy { addr, cut, open }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
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
}

fn copy(mut from: TcpStream, mut to: TcpStream) {
    let _ = std::io::copy(&mut from, &mut to);
    let _ = to.shutdown(Shutdown::Write);
}

struct Files(PathBuf);

impl Drop for Files {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn files(tag: &str) -> Files {
    let d = std::env::temp_dir().join(format!("fenec-rl-dirs-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    Files(d)
}

/// A directory file a router serves to its standbys: its own primary's, or
/// following `upstream`. The database and the follower, not yet started.
fn directory(
    path: &Path,
    upstream: Option<&str>,
) -> (Arc<RwLock<fenec_core::prelude::Database>>, Replication2) {
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
    let follower = upstream.map(|url| {
        let f = Follower::new(
            url,
            DIRS.into(),
            Arc::clone(&db),
            Some(Arc::clone(&feed)),
            true,
        )
        .unwrap();
        f.start("fenec-test-follower".into()).unwrap();
        f
    });
    (db, Replication2 { feed, follower })
}

struct Replication2 {
    feed: Arc<fenec_http::replication::Feed>,
    follower: Option<Arc<Follower>>,
}

impl Replication2 {
    fn into_replication(self) -> Arc<Replication> {
        Replication::new(Some(DIRS.into()), Some(self.feed), self.follower)
    }
}

fn config(region: Option<&str>, global: Option<String>) -> Config {
    Config {
        addr: "127.0.0.1:0".into(),
        token: Some(ROOT.into()),
        // Bounds every call the routers make, the relays among them: short,
        // a create relayed through a loaded machine came back 503.
        upstream_timeout: Duration::from_secs(60),
        replicas: true,
        auto_failover: Some(TERM),
        region: region.map(str::to_string),
        global: global.into_iter().collect(),
        ..Config::default()
    }
}

/// Serves `router` and starts its leasing: its port.
fn serve(router: Arc<Router>, leasing: bool) -> u16 {
    let listener = router.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    if leasing {
        router.start_leasing().unwrap();
    }
    std::thread::spawn(move || {
        let _ = router.serve_on(listener);
    });
    port
}

/// The global router over a directory of its own at `path`, or, with
/// `follows`, a standby of the one at that URL.
fn global_router(path: &Path, follows: Option<&str>) -> (Arc<Router>, u16) {
    let (db, repl) = directory(path, follows);
    let dir = Directory::load(db).unwrap();
    let router = Router::replicated(dir, config(None, None), repl.into_replication());
    let port = serve(Arc::clone(&router), follows.is_none());
    (router, port)
}

/// Region `region`'s router over its own directory at `path` -- following
/// `follows` where it is a standby of the one keeping it -- with a copy of
/// the global directory followed from `global`. Not `leasing`, its rounds
/// are the test's to run, and its lease long: no grant lapses between two
/// steps of a test on a loaded machine.
fn region_router(
    region: &str,
    path: &Path,
    follows: Option<&str>,
    global: &str,
    leasing: bool,
) -> (Arc<Router>, u16) {
    let (db, repl) = directory(path, follows);
    let dir = Directory::load(db).unwrap();
    let (gdb, grepl) = directory(&path.with_extension("global.fenec"), Some(global));
    let gdir = Directory::load(gdb).unwrap();
    // The global copy's follower runs on a thread of its own, which holds
    // it; only the region's own directory is served to standbys.
    drop(grepl);
    let mut cfg = config(Some(region), Some(global.to_string()));
    if !leasing {
        cfg.auto_failover = Some(Duration::from_secs(600));
    }
    let router = Router::regional(dir, cfg, Some(repl.into_replication()), gdir).unwrap();
    let port = serve(Arc::clone(&router), leasing);
    (router, port)
}

fn call(port: u16, method: &str, target: &str, body: &str, auth: Option<&str>) -> (u16, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
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

fn shard(port: u16, method: &str, target: &str, body: &str) -> (u16, String) {
    call(port, method, target, body, Some(ROOT))
}

fn query(port: u16, tenant: &str, sql: &str) -> (u16, String) {
    let mut body = String::from("{\"query\":");
    fenec_core::json::escape_into(&mut body, sql);
    body.push('}');
    call(port, "POST", &format!("/t/{tenant}/query"), &body, None)
}

/// A count of `tenant`'s notes through the router at `port`, asking for the
/// tenant's home region (`Fenec-Home`) or not: the status and the region
/// the answer names.
fn home(port: u16, tenant: &str, ask: bool) -> (u16, Option<String>) {
    let body = r#"{"query":"get notes count"}"#;
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    let asked = if ask { "Fenec-Home: 1\r\n" } else { "" };
    write!(
        s,
        "POST /t/{tenant}/query HTTP/1.1\r\nHost: x\r\n{asked}Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let head = out.split_once("\r\n\r\n").map_or(out.as_str(), |(h, _)| h);
    let values = |name: &str| -> Vec<&str> {
        head.lines()
            .filter_map(|l| l.split_once(':'))
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim())
            .collect()
    };
    let named = values("fenec-home");
    assert!(named.len() <= 1, "one home named at most: {head}");
    // Every answer a router writes carries its mark, once: a relayed one
    // the relaying router's.
    assert_eq!(values("fenec-shard"), ["1"], "{head}");
    (
        out[9..12].parse().unwrap(),
        named.first().map(|v| v.to_string()),
    )
}

/// Waits until `check` holds, 60 s at most: a bound on something that must
/// happen, never on how soon.
fn until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !check() {
        assert!(Instant::now() < deadline, "never: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The global router and its standby in eu, with eu1; us's router keeping
/// us's directory, with us1 and us2, each reached through a proxy; the
/// links between eu and us through proxies. Returned in a struct so nothing
/// is dropped while the test runs.
struct World {
    _files: Files,
    eu1: Node,
    /// us1 and us2.
    us_nodes: [Node; 2],
    /// Each, as us's router and the other reach it.
    us_links: [Proxy; 2],
    /// The global router, as us reaches it.
    to_global: Proxy,
    /// us's router, as eu reaches it.
    to_us: Proxy,
    global: u16,
    standby: u16,
    us: u16,
}

impl World {
    fn new(tag: &str) -> World {
        let files = files(tag);
        let eu1 = node(&format!("{tag}-eu1"));
        let us_nodes = [1, 2].map(|i| node(&format!("{tag}-us{i}")));
        let us_links = [0, 1].map(|i| Proxy::to(us_nodes[i].port));
        let (_, global) = global_router(&files.0.join("global.fenec"), None);
        let to_global = Proxy::to(global);
        let (_, standby) = global_router(
            &files.0.join("standby.fenec"),
            Some(&format!("http://127.0.0.1:{global}")),
        );
        let (_, us) = region_router(
            "us",
            &files.0.join("us.fenec"),
            None,
            &to_global.url(),
            true,
        );
        let to_us = Proxy::to(us);

        // eu's node, the global directory's; us keeps its own directory,
        // whose nodes the global router hands to us's router.
        let body = format!(
            r#"{{"addr":"127.0.0.1:{}","token":"adm-{tag}-eu1","region":"eu","zone":"eu-a"}}"#,
            eu1.port
        );
        let (status, answer) = shard(global, "PUT", "/_shard/nodes/eu1", &body);
        assert_eq!(status, 201, "{answer}");
        let body = format!(r#"{{"routers":["{}"]}}"#, to_us.url());
        let (status, answer) = shard(global, "PUT", "/_shard/regions/us", &body);
        assert_eq!(status, 200, "{answer}");
        for (i, zone) in ["us-a", "us-b"].iter().enumerate() {
            let (name, addr) = (format!("us{}", i + 1), &us_links[i].addr);
            let body = format!(
                r#"{{"addr":"{addr}","token":"adm-{tag}-{name}","region":"us","zone":"{zone}"}}"#
            );
            let (status, answer) = shard(global, "PUT", &format!("/_shard/nodes/{name}"), &body);
            assert_eq!(status, 201, "{name}: {answer}");
        }
        World {
            _files: files,
            eu1,
            us_nodes,
            us_links,
            to_global,
            to_us,
            global,
            standby,
            us,
        }
    }

    /// eu and us cut off from each other.
    fn partition(&self) {
        self.to_global.cut();
        self.to_us.cut();
    }

    fn heal(&self) {
        self.to_global.heal();
        self.to_us.heal();
    }
}

#[test]
fn a_region_cut_off_keeps_writing_and_fails_over_inside_itself() {
    let w = World::new("cut");
    // us's nodes went to its router's directory, not the global one.
    let (_, nodes) = shard(w.global, "GET", "/_shard/nodes", "");
    assert!(nodes.contains("eu1") && !nodes.contains("us1"), "{nodes}");
    let (_, nodes) = shard(w.us, "GET", "/_shard/nodes", "");
    assert!(nodes.contains("us1") && nodes.contains("us2"), "{nodes}");
    // A node name the global directory holds, asked of us's router, goes to
    // the global router: there is one eu1, eu's.
    until("us's copy of the global directory arrives", || {
        shard(w.us, "GET", "/_shard/regions", "")
            .1
            .contains(r#""name":"us""#)
    });
    let body = format!(
        r#"{{"addr":"127.0.0.1:{}","token":"adm-cut-eu1"}}"#,
        w.eu1.port
    );
    let (status, answer) = shard(w.us, "PUT", "/_shard/nodes/eu1", &body);
    assert_eq!(status, 201, "{answer}");
    let (_, nodes) = shard(w.us, "GET", "/_shard/nodes", "");
    assert!(!nodes.contains("eu1"), "{nodes}");

    // A tenant made in us through the global router: its name taken there,
    // its placement us's.
    let (status, body) = shard(
        w.global,
        "PUT",
        "/_shard/tenants/acme",
        r#"{"region":"us","node":"us1","schema":"create collection notes (title text)"}"#,
    );
    assert_eq!(status, 201, "{body}");
    assert!(body.contains(r#""replica_on":"us2""#), "{body}");
    let (status, body) = shard(
        w.global,
        "PUT",
        "/_shard/tenants/beta",
        r#"{"region":"eu","schema":"create collection notes (title text)"}"#,
    );
    assert_eq!(status, 201, "{body}");
    // A name taken in one region is taken in every other.
    let (status, body) = shard(
        w.global,
        "PUT",
        "/_shard/tenants/acme",
        r#"{"region":"eu"}"#,
    );
    assert_eq!(status, 409, "{body}");

    // acme is written through any router: us's, the global one, its standby.
    assert_eq!(query(w.us, "acme", r#"put notes {title: "us"}"#).0, 200);
    let (status, body) = query(w.global, "acme", r#"put notes {title: "eu"}"#);
    assert_eq!(status, 200, "{body}");
    until("the standby's directory arrives", || {
        query(w.standby, "acme", r#"put notes {title: "standby"}"#).0 == 200
    });
    // The global directory names its region and no node; us's router has
    // the placement.
    let (_, list) = shard(w.global, "GET", "/_shard/tenants", "");
    assert!(
        list.contains(r#"{"name":"acme","state":"active","region":"us"}"#),
        "{list}"
    );
    let (_, acme) = shard(w.global, "GET", "/_shard/tenants/acme", "");
    assert!(acme.contains(r#""node":"us"#), "{acme}");

    // eu and us cut off from each other.
    w.partition();
    let cut = Instant::now();
    // From eu, us's tenant is out of reach, and the answer says to ask again.
    let (status, body) = query(w.global, "acme", r#"put notes {title: "lost"}"#);
    assert!(status == 502 || status == 503, "{status} {body}");
    // Past every lease granted before the cut, us goes on writing: its
    // leases come from its own router. A sleep never ends early.
    std::thread::sleep(TERM + TERM / 10);
    until("us writes acme with eu cut off", || {
        query(w.us, "acme", r#"put notes {title: "cut off"}"#).0 == 200
    });
    assert!(cut.elapsed() > TERM);
    // eu goes on writing its own.
    assert_eq!(query(w.global, "beta", r#"put notes {title: "eu"}"#).0, 200);

    // What needs both regions is refused, nothing done by half: a move
    // across, a create in us -- through either router.
    let (status, body) = shard(
        w.global,
        "POST",
        "/_shard/tenants/beta/move",
        r#"{"region":"us"}"#,
    );
    assert_eq!(status, 503, "{body}");
    assert_eq!(
        query(w.global, "beta", r#"put notes {title: "thawed"}"#).0,
        200,
        "the move was undone"
    );
    let (status, body) = shard(
        w.global,
        "PUT",
        "/_shard/tenants/gamma",
        r#"{"region":"us"}"#,
    );
    assert_eq!(status, 503, "{body}");
    let (status, body) = shard(w.us, "PUT", "/_shard/tenants/delta", "");
    assert_eq!(status, 503, "{body}");
    let (_, list) = shard(w.global, "GET", "/_shard/tenants", "");
    assert!(!list.contains("gamma") && !list.contains("delta"), "{list}");

    // acme's node cut off from its region's router; its own clients still
    // reach it. It stops taking writes before us's router promotes acme's
    // replica -- inside us, with eu out of reach. Its node is read here: a
    // machine loaded enough to leave a renewal two thirds of a lease late
    // has us's router fail acme over on its own before, as it must.
    let (_, acme) = shard(w.us, "GET", "/_shard/tenants/acme", "");
    let p = usize::from(acme.contains(r#""node":"us2""#));
    let (old, new) = (&w.us_nodes[p], format!(r#""node":"us{}""#, 2 - p));
    w.us_links[p].cut();
    let (mut fenced, mut promoted) = (None, None);
    until("us1 fenced and acme promoted on us2", || {
        let (direct, body) = query(old.port, "acme", r#"put notes {title: "direct"}"#);
        match direct {
            200 => assert!(
                promoted.is_none(),
                "the old node wrote acme after the new took it"
            ),
            // Its lease lapsed, or -- a grant landing as the link was cut
            // -- names acme no more: the fence either way.
            503 => {
                assert!(
                    body.contains("lease") || body.contains("another node"),
                    "{body}"
                );
                fenced.get_or_insert_with(Instant::now);
            }
            // Landed while the lease ran and answered once it had lapsed --
            // its wait for the replica's cut stream a second -- so not a
            // success, which the replica promoted may never have had.
            500 => assert!(body.contains(r#""outcome":"unknown""#), "{body}"),
            other => panic!("us1 answered {other}: {body}"),
        }
        if promoted.is_none() && query(w.us, "acme", r#"put notes {title: "failed over"}"#).0 == 200
        {
            let (_, acme) = shard(w.us, "GET", "/_shard/tenants/acme", "");
            if acme.contains(&new) {
                promoted = Some(Instant::now());
            }
        }
        fenced.is_some() && promoted.is_some()
    });
    assert!(
        fenced.unwrap() < promoted.unwrap(),
        "the old node stopped after the new took acme"
    );
    // Both written: one refuses.
    let (status, body) = query(old.port, "acme", r#"put notes {title: "stale"}"#);
    assert_eq!(status, 503, "{body}");
    assert_eq!(query(w.us, "acme", r#"put notes {title: "us2"}"#).0, 200);

    // The regions reach each other again: every router sends acme where us
    // failed it over, having been told nothing.
    w.heal();
    for (router, name) in [(w.global, "global"), (w.standby, "standby"), (w.us, "us")] {
        until(&format!("{name} writes acme on us2"), || {
            let (status, _) = query(router, "acme", &format!(r#"put notes {{title: "{name}"}}"#));
            status == 200
        });
        let (_, rows) = query(
            router,
            "acme",
            r#"get notes where title = "failed over" count"#,
        );
        assert!(rows.contains('1'), "{name}: {rows}");
    }
    let (_, acme) = shard(w.global, "GET", "/_shard/tenants/acme", "");
    assert!(acme.contains(&new), "{acme}");
    let (status, body) = query(old.port, "acme", r#"put notes {title: "stale"}"#);
    assert_ne!(status, 200, "the old primary took a write: {body}");

    // And the move refused under the partition goes through.
    let (status, body) = shard(
        w.global,
        "POST",
        "/_shard/tenants/beta/move",
        r#"{"region":"us"}"#,
    );
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains(r#""region":"us""#) && body.contains(r#""source_removed":true"#),
        "{body}"
    );
    for router in [w.global, w.us] {
        until("beta written in us", || {
            query(router, "beta", r#"put notes {title: "moved"}"#).0 == 200
        });
    }
    let (_, rows) = query(w.us, "beta", r#"get notes where title = "thawed" count"#);
    assert!(rows.contains('1'), "beta's rows came with it: {rows}");
    let (_, held) = call(
        w.eu1.port,
        "GET",
        "/_admin/tenants",
        "",
        Some("adm-cut-eu1"),
    );
    assert!(!held.contains("beta"), "{held}");
    let (_, list) = shard(w.global, "GET", "/_shard/tenants?region=us", "");
    assert!(list.contains("acme") && list.contains("beta"), "{list}");

    // Back to eu: out of us's directory, into the global one.
    let (status, body) = shard(
        w.global,
        "POST",
        "/_shard/tenants/beta/move",
        r#"{"region":"eu"}"#,
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""to":"eu1""#), "{body}");
    until("beta written in eu through us's router", || {
        query(w.us, "beta", r#"put notes {title: "home"}"#).0 == 200
    });
    let (_, beta) = shard(w.us, "GET", "/_shard/tenants/beta", "");
    assert!(beta.contains(r#""node":"eu1""#), "{beta}");

    // A delete through us's router goes by the global one.
    let (status, body) = shard(w.us, "DELETE", "/_shard/tenants/acme", "");
    assert_eq!(status, 204, "{body}");
    let (status, _) = shard(w.global, "GET", "/_shard/tenants/acme", "");
    assert_eq!(status, 404);
}

/// A request asking for its tenant's home (`Fenec-Home`) is told the
/// region of the node it reached, by whichever router it was sent to --
/// the one placing the tenant, or one relaying it there, which names
/// nothing of its own -- and after a move across, the region it went to;
/// and so is it where the placing router cannot reach the node. One that
/// does not ask is told nothing: the global entry
/// (integrations/cloudflare/entry) learns where to send a tenant from it.
/// Every answer carries the router's mark (`Fenec-Shard`).
#[test]
fn a_tenants_home_is_named_to_a_request_asking_for_it() {
    let w = World::new("home");
    until("us's copy of the global directory arrives", || {
        shard(w.us, "GET", "/_shard/regions", "")
            .1
            .contains(r#""name":"us""#)
    });
    for (tenant, region) in [("acme", "us"), ("beta", "eu")] {
        let body =
            format!(r#"{{"region":"{region}","schema":"create collection notes (title text)"}}"#);
        let (status, answer) = shard(w.global, "PUT", &format!("/_shard/tenants/{tenant}"), &body);
        assert_eq!(status, 201, "{tenant}: {answer}");
    }
    let us = Some("us".to_string());
    let eu = Some("eu".to_string());
    // us's tenant: placed by us's router, relayed to it by the global one.
    assert_eq!(home(w.us, "acme", true), (200, us.clone()));
    assert_eq!(home(w.global, "acme", true), (200, us.clone()));
    assert_eq!(home(w.global, "acme", false), (200, None));
    assert_eq!(home(w.us, "acme", false), (200, None));
    until("the standby's directory arrives", || {
        home(w.standby, "acme", true) == (200, us.clone())
    });
    // eu's, placed by the global directory: by the global router, and by
    // us's router from its copy of it, which forwards to the node itself.
    assert_eq!(home(w.global, "beta", true), (200, eu.clone()));
    until("us's copy places beta", || {
        home(w.us, "beta", true) == (200, eu.clone())
    });
    // A tenant no directory holds: the router's own 404, naming nothing.
    assert_eq!(home(w.global, "nobody", true), (404, None));
    // us's nodes out of its router's reach: its 502 names the home it
    // could not reach, through the global router too -- a home's answer,
    // which the entry tries no other region after.
    for link in &w.us_links {
        link.cut();
    }
    assert_eq!(home(w.us, "acme", true), (502, us.clone()));
    assert_eq!(home(w.global, "acme", true), (502, us.clone()));
    for link in &w.us_links {
        link.heal();
    }
    until("us reaches its nodes again", || {
        home(w.us, "acme", true) == (200, us.clone())
    });

    // acme moved across to eu: every router names eu from then on.
    let (status, body) = shard(
        w.global,
        "POST",
        "/_shard/tenants/acme/move",
        r#"{"region":"eu"}"#,
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(home(w.global, "acme", true), (200, eu.clone()));
    until("us's router sends acme to eu", || {
        home(w.us, "acme", true) == (200, eu.clone())
    });
}

/// us's router lost -- cut off, say, and its standby promoted over it --
/// and still running: the nodes the promoted one leases refuse the old
/// one's grants and its promotions, so the two never lease a node at once.
#[test]
fn a_promoted_region_router_fences_the_one_before_it_at_its_nodes() {
    let files = files("fence");
    let us1 = node("fence-us1");
    let (_, global) = global_router(&files.0.join("global.fenec"), None);
    let global_url = format!("http://127.0.0.1:{global}");
    let (a, a_port) = region_router("us", &files.0.join("a.fenec"), None, &global_url, false);
    let (b, b_port) = region_router(
        "us",
        &files.0.join("b.fenec"),
        Some(&format!("http://127.0.0.1:{a_port}")),
        &global_url,
        false,
    );
    let routers =
        format!(r#"{{"routers":["http://127.0.0.1:{a_port}","http://127.0.0.1:{b_port}"]}}"#);
    let (status, body) = shard(global, "PUT", "/_shard/regions/us", &routers);
    assert_eq!(status, 200, "{body}");
    let body = format!(
        r#"{{"addr":"127.0.0.1:{}","token":"adm-fence-us1"}}"#,
        us1.port
    );
    let (status, answer) = shard(a_port, "PUT", "/_shard/nodes/us1", &body);
    assert_eq!(status, 201, "{answer}");
    let (status, answer) = shard(
        global,
        "PUT",
        "/_shard/tenants/acme",
        r#"{"region":"us","schema":"create collection notes (title text)"}"#,
    );
    assert_eq!(status, 201, "{answer}");
    assert_eq!(query(global, "acme", r#"put notes {title: "one"}"#).0, 200);
    let lease = || call(us1.port, "GET", "/_admin/lease", "", Some("adm-fence-us1")).1;
    assert!(
        lease().contains(r#""authority":"us","term":1"#),
        "{}",
        lease()
    );
    // b follows a's directory, and refuses its changes until promoted.
    until("b's directory arrives", || {
        shard(b_port, "GET", "/_shard/tenants", "")
            .1
            .contains("acme")
    });
    let (status, body) = shard(b_port, "PUT", "/_shard/nodes/us1", &body);
    assert_eq!(status, 409, "{body}");

    // b promoted: its term is 2, and its first grant fences a at us1.
    let (status, body) = call(b_port, "POST", "/_replication/promote", "", Some(DIRS));
    assert_eq!(status, 200, "{body}");
    let (_, region) = shard(b_port, "GET", "/_shard/region", "");
    assert!(
        region.contains(r#""region":"us","following":false,"term":2"#),
        "{region}"
    );
    b.lease_round();
    assert!(lease().contains(r#""term":2"#), "{}", lease());
    a.lease_round();
    assert!(
        lease().contains(r#""term":2"#),
        "a took us1 back: {}",
        lease()
    );
    // a's grant and a's promotion, as a sends them, refused by name.
    let (_, a_region) = shard(a_port, "GET", "/_shard/region", "");
    let history = a_region
        .split("\"history\":\"")
        .nth(1)
        .and_then(|r| r.split('"').next())
        .unwrap()
        .to_string();
    let grant = format!(
        r#"{{"ms":60000,"epoch":"x","primaries":["acme"],"authority":"us","term":1,"history":"{history}"}}"#
    );
    let (status, body) = call(
        us1.port,
        "POST",
        "/_admin/lease",
        &grant,
        Some("adm-fence-us1"),
    );
    assert_eq!(status, 409, "{body}");
    assert!(
        body.contains(r#""held_by":{"authority":"us","term":2"#),
        "{body}"
    );
    let mut s = TcpStream::connect(("127.0.0.1", us1.port)).unwrap();
    write!(
        s,
        "POST /_admin/tenants/acme/promote HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer \
         adm-fence-us1\r\nFenec-Lease-Holder: us 1 {history}\r\nContent-Length: 0\r\n\
         Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    assert!(out.starts_with("HTTP/1.1 409"), "{out}");
    // b leases it, and acme takes writes through it.
    let (status, body) = query(b_port, "acme", r#"put notes {title: "two"}"#);
    assert_eq!(status, 200, "{body}");
}
