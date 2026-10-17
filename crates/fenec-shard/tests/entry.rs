//! The client the global entry names (`Fenec-Client`, read with
//! `--entry-token`): behind the entry every request reaches a router from
//! Cloudflare's addresses, so the router takes the address the entry's
//! mark names in the connection's place, for that request alone -- the
//! refusal's wait and count, a verified token's clearing, the audit line,
//! the trace and the client it names to a node or another region's
//! router -- and a header that does not check, or a router without the
//! flag, leaves the connection's. Every router drops the header from what
//! it forwards.
//!
//! Apart from the other tests: the counts of refusals, the audit log and
//! tracing are the process's, and the routers and nodes here share them.
//! Every connection the test makes comes from 127.0.0.1, and every address
//! a client is named by is one of the documentation's (RFC 5737), which
//! nothing connects from: a count of one of them moved is the entry's
//! name believed, a count of 127.0.0.1 moved the connection's address
//! taken. Which count a refusal went to is read off the count
//! (`would_wait`), and a request's time is bounded from below only, as
//! refusals.rs says why.

#[path = "../../fenec-server/tests/otlp.rs"]
mod otlp;

use fenec_http::audit::{self, would_wait};
use fenec_http::crypto::{b64url_encode, hmac_sha256};
use fenec_http::replication::{fresh_id, Follower, Replication};
use fenec_http::tenants::Tenants;
use fenec_shard::directory::{Directory, Node, State};
use fenec_shard::{Config, Router};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The entry's `ENTRY_TOKEN`, the routers' `--entry-token`.
const SECRET: &str = "entry-secret";
const DATA: &str = "Authorization: Bearer data\r\n";
const FORGED: &str = "Authorization: Bearer guessed\r\n";
const LOOPBACK: Option<IpAddr> = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));
const ROOT: &str = "routers-token";
const DIRS: &str = "directory-replication";

fn ip(s: &str) -> Option<IpAddr> {
    Some(s.parse().unwrap())
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// What every test here shares: the audit log, which the process writes
/// to one file, and the receiver of its spans.
struct Shared {
    audit: PathBuf,
    spans: otlp::Receiver,
}

fn shared() -> &'static Shared {
    static SHARED: OnceLock<Shared> = OnceLock::new();
    SHARED.get_or_init(|| {
        let audit =
            std::env::temp_dir().join(format!("fenec-shard-entry-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&audit);
        audit::open(&audit).unwrap();
        let spans = otlp::Receiver::start();
        let settings = fenec_http::trace::Options {
            endpoint: Some(spans.url()),
            ..Default::default()
        }
        .settings("fenec-shard")
        .unwrap()
        .unwrap();
        fenec_http::trace::install(settings).unwrap();
        Shared { audit, spans }
    })
}

static TURN: Mutex<()> = Mutex::new(());

/// This test's turn at the counts, which start from nothing for 127.0.0.1
/// and the addresses it names.
fn turn(named: &[&str]) -> MutexGuard<'static, ()> {
    let turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
    shared();
    audit::set_delay(50);
    audit::succeeded(LOOPBACK);
    for a in named {
        audit::succeeded(ip(a));
    }
    turn
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// The entry's name for `address`, made at `at` under `token`: its mark
/// worked out here as entry.js's `clientMark` works it out -- HMAC-SHA256
/// of `fenec-client <address> <seconds>`, 16 bytes, base64url.
fn named_at(address: &str, at: u64, token: &str) -> String {
    let mac = hmac_sha256(
        token.as_bytes(),
        format!("fenec-client {address} {at}").as_bytes(),
    );
    format!(
        "Fenec-Client: {address} {at} {}\r\n",
        b64url_encode(&mac[..16])
    )
}

/// The entry's name for `address`, made now.
fn named(address: &str) -> String {
    named_at(address, now(), SECRET)
}

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch(tag: &str) -> Dir {
    let d = std::env::temp_dir().join(format!("fenec-shard-entry-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    Dir(d)
}

/// A node at `dir` with tenants `held`, each with a collection `notes`,
/// its admin token `admin` and the data token `data`: its address.
fn node(dir: &Path, admin: &str, held: &[&str]) -> String {
    let tenants = Arc::new(Tenants::new(dir).unwrap());
    for t in held {
        tenants.create(t).unwrap();
    }
    let server = fenec_http::Server::with_tenants(
        Arc::clone(&tenants),
        fenec_http::Config {
            addr: "127.0.0.1:0".into(),
            admin_token: Some(admin.into()),
            token: Some("data".into()),
            ..fenec_http::Config::default()
        },
    );
    let listener = server.bind().unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    for t in held {
        let create = r#"{"query":"create collection notes (title text)"}"#;
        let (status, out, _) = ask(&addr, "POST", &format!("/t/{t}/query"), DATA, create);
        assert_eq!(status, 200, "{out}");
    }
    addr
}

/// A router over the one node at `node` (admin token `adm`), placing acme
/// there, with `entry` its `--entry-token` and `rt` its own `--token`.
fn router(node: &str, entry: Option<&str>) -> String {
    let mut dir = Directory::in_memory();
    dir.set_node(
        "n1",
        Node {
            addr: node.into(),
            token: "adm".into(),
            ..Node::default()
        },
    )
    .unwrap();
    dir.place("acme", "n1", State::Active).unwrap();
    let router = Router::new(
        dir,
        Config {
            addr: "127.0.0.1:0".into(),
            token: Some("rt".into()),
            entry_token: entry.map(str::to_string),
            upstream_timeout: Duration::from_secs(20),
            ..Config::default()
        },
    );
    serve(router)
}

fn serve(router: Arc<Router>) -> String {
    let listener = router.bind().unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = router.serve_on(listener);
    });
    addr
}

/// A request to `addr` with `headers`: its status, the whole answer, and
/// how long it took.
fn ask(
    addr: &str,
    method: &str,
    target: &str,
    headers: &str,
    body: &str,
) -> (u16, String, Duration) {
    let started = Instant::now();
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    write!(
        s,
        "{method} {target} HTTP/1.1\r\nHost: x\r\n{headers}Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    (out[9..12].parse().unwrap(), out, started.elapsed())
}

/// A count of acme's notes through `router` with `headers`.
fn count(router: &str, headers: &str) -> (u16, Duration) {
    let body = r#"{"query":"get notes count"}"#;
    let (status, _, took) = ask(router, "POST", "/t/acme/query", headers, body);
    (status, took)
}

/// The audit log's lines of the request `id`.
fn lines_of(id: &str) -> Vec<String> {
    let text = std::fs::read_to_string(&shared().audit).unwrap_or_default();
    let needle = format!("\"request_id\":\"{id}\"");
    text.lines()
        .filter(|l| l.contains(&needle))
        .map(str::to_string)
        .collect()
}

/// The value of `key`, a text, in a JSON line.
fn text_of<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = line.split(&format!("\"{key}\":\"")).nth(1)?;
    rest.split('"').next()
}

/// A scrape's `fenec_router_entry_clients_total` of `outcome`.
fn entry_clients(router: &str, outcome: &str) -> u64 {
    let (status, out, _) = ask(
        router,
        "GET",
        "/_metrics",
        "Authorization: Bearer rt\r\n",
        "",
    );
    assert_eq!(status, 200, "{out}");
    let line = format!("fenec_router_entry_clients_total{{outcome=\"{outcome}\"}} ");
    out.lines()
        .find_map(|l| l.strip_prefix(&line))
        .map_or(0, |n| n.trim().parse().unwrap())
}

#[test]
fn refusals_behind_the_entry_count_by_the_client_it_names() {
    let (a, b) = ("203.0.113.1", "198.51.100.2");
    let _turn = turn(&[a, b]);
    let files = scratch("counts");
    let node = node(&files.0, "adm", &["acme"]);
    let entry = router(&node, Some(SECRET));
    let (name_a, name_b) = (named(a), named(b));

    // a's refusals wait 50, 100, 200 ms: doubled, by its address -- and
    // 127.0.0.1, which every one of them came from, counts none.
    for least in [50, 100, 200] {
        let (status, took) = count(&entry, &format!("{FORGED}{name_a}"));
        assert_eq!(status, 401);
        assert!(took >= ms(least), "{took:?}");
    }
    assert_eq!(would_wait(ip(a)), ms(400));
    assert_eq!(would_wait(ip(b)), ms(50));
    assert_eq!(would_wait(LOOPBACK), ms(50));

    // b's first refusal is a first, not a's fourth: counted by the
    // entry's address, it was 400 ms.
    let (status, took) = count(&entry, &format!("{FORGED}{name_b}"));
    assert_eq!(status, 401);
    assert!(took >= ms(50), "{took:?}");
    assert_eq!(would_wait(ip(b)), ms(100));
    // The router's own refusal, of its `--token`, is b's too.
    let (status, _, _) = ask(&entry, "GET", "/_metrics", &format!("{FORGED}{name_b}"), "");
    assert_eq!(status, 401);
    assert_eq!(would_wait(ip(b)), ms(200));
    assert_eq!(would_wait(ip(a)), ms(400));
    assert_eq!(would_wait(LOOPBACK), ms(50));

    // b's good token -- the node's, verified there, and the router's own
    // -- starts b's count again, and a's not at all: by the entry's
    // address, any client's good token through it cleared every other's.
    let (status, _) = count(&entry, &format!("{DATA}{name_b}"));
    assert_eq!(status, 200);
    assert_eq!(would_wait(ip(b)), ms(50));
    assert_eq!(would_wait(ip(a)), ms(400));
    audit::failed(ip(b));
    let (status, _, _) = ask(
        &entry,
        "GET",
        "/_metrics",
        &format!("Authorization: Bearer rt\r\n{name_b}"),
        "",
    );
    assert_eq!(status, 200);
    assert_eq!(would_wait(ip(b)), ms(50));
    assert_eq!(would_wait(ip(a)), ms(400));
    let (status, took) = count(&entry, &format!("{FORGED}{name_a}"));
    assert_eq!(status, 401);
    assert!(took >= ms(400), "a's count was started again: {took:?}");
    assert_eq!(would_wait(LOOPBACK), ms(50));

    // The audit lines of a refusal name a: the router's, the connection's
    // address beside it as `via`, and the node's, the router's as `via`.
    let (status, _, _) = ask(
        &entry,
        "POST",
        "/t/acme/query",
        &format!("{FORGED}{name_a}X-Request-Id: entry-audit-1\r\n"),
        r#"{"query":"get notes count"}"#,
    );
    assert_eq!(status, 401);
    let lines = lines_of("entry-audit-1");
    assert_eq!(lines.len(), 2, "{lines:#?}");
    for l in &lines {
        assert_eq!(text_of(l, "event"), Some("refused"), "{l}");
        assert_eq!(text_of(l, "peer"), Some(a), "{l}");
        assert!(
            text_of(l, "via").is_some_and(|v| v.starts_with("127.0.0.1:")),
            "{l}"
        );
    }
}

#[test]
fn a_name_that_does_not_check_leaves_the_connections_address() {
    let c = "192.0.2.3";
    let _turn = turn(&[c]);
    let files = scratch("unchecked");
    let node = node(&files.0, "adm", &["acme"]);
    let entry = router(&node, Some(SECRET));
    let plain = router(&node, None);
    let before = |o| entry_clients(&entry, o);
    let (named0, bad0, stale0) = (before("named"), before("bad_mark"), before("stale"));

    // Another token's mark, a mark moved to another address, a time two
    // minutes gone, and a router without the flag: each is 127.0.0.1's
    // refusal, and c counts none of them.
    let ago = now() - 120;
    let ahead = now() + 120;
    let moved = named(c).replace(c, "192.0.2.4");
    let cases = [
        (&entry, named_at(c, now(), "another-secret")),
        (&entry, moved),
        (&entry, named_at(c, ago, SECRET)),
        (&entry, named_at(c, ahead, SECRET)),
        (&plain, named(c)),
    ];
    for (i, (router, header)) in cases.iter().enumerate() {
        let (status, took) = count(router, &format!("{FORGED}{header}"));
        assert_eq!(status, 401, "{header}");
        let least = 50 << i;
        assert!(took >= ms(least), "{header}: {took:?}");
        assert_eq!(would_wait(LOOPBACK), ms(least * 2), "{header}");
        assert_eq!(would_wait(ip(c)), ms(50), "{header}");
        assert_eq!(would_wait(ip("192.0.2.4")), ms(50), "{header}");
    }
    // And a good token beside a name that does not check clears 127.0.0.1's.
    let (status, _) = count(&entry, &format!("{DATA}{}", named_at(c, ago, SECRET)));
    assert_eq!(status, 200);
    assert_eq!(would_wait(LOOPBACK), ms(50));

    // The same name, made now, through the router with the flag: c's.
    let (status, _) = count(&entry, &format!("{FORGED}{}", named(c)));
    assert_eq!(status, 401);
    assert_eq!(would_wait(ip(c)), ms(100));
    assert_eq!(would_wait(LOOPBACK), ms(50));

    // Counted by what each came to: two marks not the entry's, two times
    // past the minute; the router without the flag read none.
    assert_eq!(entry_clients(&entry, "bad_mark") - bad0, 2);
    assert_eq!(entry_clients(&entry, "stale") - stale0, 3);
    assert!(entry_clients(&entry, "named") > named0);
}

/// What a node or another router is sent, recorded: a stand-in node that
/// answers a count, a held request and a stream.
struct StandIn {
    addr: String,
    heads: Arc<Mutex<Vec<String>>>,
}

impl StandIn {
    fn start() -> StandIn {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let heads = Arc::new(Mutex::new(Vec::new()));
        let into = Arc::clone(&heads);
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let into = Arc::clone(&into);
                std::thread::spawn(move || StandIn::serve(s, &into));
            }
        });
        StandIn { addr, heads }
    }

    fn serve(s: TcpStream, heads: &Mutex<Vec<String>>) {
        let mut out = s.try_clone().unwrap();
        let mut reader = BufReader::new(s);
        loop {
            let mut head = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                if line == "\r\n" {
                    break;
                }
                head.push_str(&line);
            }
            let len: usize = head
                .lines()
                .filter_map(|l| l.split_once(':'))
                .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                .map_or(0, |(_, v)| v.trim().parse().unwrap());
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body).unwrap();
            let stream = head.starts_with("GET /t/acme/_changes");
            heads.lock().unwrap().push(head);
            if stream {
                let _ = out.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                      Connection: close\r\n\r\nevent: seed\ndata: {}\n\n",
                );
                return;
            }
            let _ = out.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                  Content-Length: 2\r\n\r\n[]",
            );
        }
    }

    fn last(&self) -> String {
        self.heads.lock().unwrap().last().cloned().unwrap()
    }
}

#[test]
fn the_entrys_name_never_reaches_a_node() {
    let d = "198.51.100.7";
    let _turn = turn(&[d]);
    let stand_in = StandIn::start();
    let entry = router(&stand_in.addr, Some(SECRET));
    let plain = router(&stand_in.addr, None);
    let name = named(d);
    let header = |head: &str, name: &str| -> Vec<String> {
        head.lines()
            .filter_map(|l| l.split_once(':'))
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim().to_string())
            .collect()
    };
    // A request, a held one and a stream, through a router that believes
    // the entry and one that reads none of it: the node is sent the
    // router's own name for the client -- d where the router believed the
    // entry, 127.0.0.1 where it did not -- and never the entry's.
    let requests: [(&str, &str, &str); 3] = [
        ("POST", "/t/acme/query", ""),
        ("POST", "/t/acme/query", "Fenec-Wait: 50\r\n"),
        ("GET", "/t/acme/_changes", ""),
    ];
    for (router, client) in [(&entry, d), (&plain, "127.0.0.1")] {
        for (method, target, extra) in requests {
            let body = if method == "POST" { "{}" } else { "" };
            let (status, out, _) = ask(
                router,
                method,
                target,
                &format!("{DATA}{name}{extra}"),
                body,
            );
            assert_eq!(status, 200, "{out}");
            let head = stand_in.last();
            assert!(
                header(&head, "fenec-client").is_empty(),
                "the entry's name reached the node: {head}"
            );
            let routed = header(&head, "fenec-router");
            assert_eq!(routed.len(), 1, "{head}");
            assert_eq!(routed[0].split(' ').next(), Some(client), "{head}");
        }
    }
}

/// The directory a router keeps: its own primary's, or following `follows`.
fn directory(path: &Path, follows: Option<&str>) -> (Directory, Arc<Replication>) {
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
    let follower = follows.map(|url| {
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
    let repl = Replication::new(Some(DIRS.into()), Some(feed), follower);
    (Directory::load(db).unwrap(), repl)
}

fn regional_config(region: Option<&str>, global: Option<String>, entry: bool) -> Config {
    Config {
        addr: "127.0.0.1:0".into(),
        token: Some(ROOT.into()),
        upstream_timeout: Duration::from_secs(30),
        region: region.map(str::to_string),
        global: global.into_iter().collect(),
        entry_token: entry.then(|| SECRET.to_string()),
        ..Config::default()
    }
}

fn shard(addr: &str, method: &str, target: &str, body: &str) -> (u16, String) {
    let auth = format!("Authorization: Bearer {ROOT}\r\nContent-Type: application/json\r\n");
    let (status, out, _) = ask(addr, method, target, &auth, body);
    (status, out)
}

/// Waits until `check` holds, 60 s at most: a bound on what must happen.
fn until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !check() {
        assert!(Instant::now() < deadline, "never: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The entry sends a tenant it has not heard to the region of the client's
/// continent, whose router relays it home: the global router here, us's
/// tenant relayed to us's router, which forwards it to the node. The
/// client the entry named is the relayed request's at every hop -- the
/// home router's audit line and span, the node's -- and the refusal waits
/// at the first router alone, by that client.
#[test]
fn a_relay_carries_the_named_client_to_the_home_router_and_the_node() {
    let e = "203.0.113.50";
    let _turn = turn(&[e]);
    let files = scratch("relay");
    let us_node = node(&files.0.join("us1"), "adm-us1", &[]);
    let (gdir, grepl) = directory(&files.0.join("global.fenec"), None);
    let global = serve(Router::replicated(
        gdir,
        regional_config(None, None, true),
        grepl,
    ));
    let global_url = format!("http://{global}");
    let (udir, urepl) = directory(&files.0.join("us.fenec"), None);
    // us's copy of the global directory, followed from the global router.
    let (gcopy, _) = directory(&files.0.join("us.global.fenec"), Some(&global_url));
    let us = serve(
        Router::regional(
            udir,
            regional_config(Some("us"), Some(global_url.clone()), false),
            Some(urepl),
            gcopy,
        )
        .unwrap(),
    );
    let (status, out) = shard(
        &global,
        "PUT",
        "/_shard/regions/us",
        &format!(r#"{{"routers":["http://{us}"]}}"#),
    );
    assert_eq!(status, 200, "{out}");
    until("us's copy of the global directory", || {
        shard(&us, "GET", "/_shard/regions", "")
            .1
            .contains(r#""name":"us""#)
    });
    let body = format!(r#"{{"addr":"{us_node}","token":"adm-us1","region":"us"}}"#);
    let (status, out) = shard(&global, "PUT", "/_shard/nodes/us1", &body);
    assert_eq!(status, 201, "{out}");
    let body = r#"{"region":"us","schema":"create collection notes (title text)"}"#;
    let (status, out) = shard(&global, "PUT", "/_shard/tenants/acme", body);
    assert_eq!(status, 201, "{out}");

    // A refusal relayed home: every hop's line names e, and only the
    // global router, which the entry sent it to, waits it out.
    let trace = "4bf92f3577b34da6a3ce929d0e0e4736";
    let headers = format!(
        "{FORGED}{}X-Request-Id: entry-relay-1\r\ntraceparent: 00-{trace}-00f067aa0ba902b7-01\r\n",
        named(e)
    );
    let (status, took) = count(&global, &headers);
    assert_eq!(status, 401);
    assert!(took >= ms(50), "{took:?}");
    assert_eq!(would_wait(ip(e)), ms(100));
    assert_eq!(would_wait(LOOPBACK), ms(50));
    let lines = lines_of("entry-relay-1");
    // The global router's, us's router's and the node's.
    assert_eq!(lines.len(), 3, "{lines:#?}");
    for l in &lines {
        assert_eq!(text_of(l, "event"), Some("refused"), "{l}");
        assert_eq!(text_of(l, "peer"), Some(e), "{l}");
    }

    // Each server span names e as `client.address`, the connection's
    // address beside it: the global router's, us's router's, the node's.
    let spans = shared()
        .spans
        .until("three server spans of the relayed request", |s| {
            s.iter().filter(|g| g.trace == trace && g.kind == 2).count() == 3
        });
    for g in spans.iter().filter(|g| g.trace == trace && g.kind == 2) {
        assert_eq!(g.attr("client.address"), Some(e), "{g:#?}");
        assert_eq!(g.attr("network.peer.address"), Some("127.0.0.1"), "{g:#?}");
    }

    // A good token, verified at the node: the word travels back through
    // us's router to the global router, which starts e's count again.
    let (status, _) = count(&global, &format!("{DATA}{}", named(e)));
    assert_eq!(status, 200);
    assert_eq!(would_wait(ip(e)), ms(50));
}
