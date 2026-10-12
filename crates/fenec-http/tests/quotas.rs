//! A tenant's quotas on a tenant node (`fenec_http::quota`): each at its
//! boundary, concurrent writers against the file's and the rows' quota, a
//! tenant at its streams or requests quota beside a tenant that is not,
//! deletes and a compact at a full file, a scoped token, and the counts.
//!
//! A binary of its own: it reads the process's refusal counts.

use fenec_http::access::Access;
use fenec_http::quota::{Kind, Quota};
use fenec_http::tenants::Tenants;
use fenec_http::{Config, Server};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const ADMIN: &str = "Bearer adm";
const SECRET: &[u8] = b"thirty-two bytes and a few more, for HS256";

struct Node {
    port: u16,
    tenants: Arc<Tenants>,
    dir: PathBuf,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start(tag: &str, quota: Quota) -> Node {
    start_with(tag, quota, Config::default())
}

fn start_with(tag: &str, quota: Quota, cfg: Config) -> Node {
    fenec_http::audit::set_delay(0);
    let dir = std::env::temp_dir().join(format!(
        "fenec-quotas-{tag}-{}-{:?}",
        std::process::id(),
        Instant::now()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let tenants = Arc::new(Tenants::new(&dir).unwrap().with_quota(quota));
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        admin_token: Some("adm".into()),
        stream_keepalive: Duration::from_millis(80),
        ..cfg
    };
    let server = Server::with_tenants(Arc::clone(&tenants), cfg);
    let listener = server.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    Node { port, tenants, dir }
}

struct Res {
    status: u16,
    head: String,
    body: String,
}

impl Res {
    fn header(&self, name: &str) -> Option<&str> {
        self.head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then_some(v.trim())
        })
    }

    /// The quota a refusal names, in its body and its header alike.
    fn quota(&self) -> Option<&str> {
        let named = self.header("fenec-quota");
        if let Some(k) = named {
            assert!(
                self.body.contains(&format!("\"quota\":\"{k}\"")),
                "the header names {k}, the body does not: {}",
                self.body
            );
        }
        named
    }
}

fn call(port: u16, method: &str, target: &str, body: &str, headers: &[(&str, &str)]) -> Res {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let mut req = format!(
        "{method} {target} HTTP/1.1\r\nHost: t\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let text = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    Res {
        status: head[9..12].parse().unwrap(),
        head: head.to_string(),
        body: body.to_string(),
    }
}

fn admin(port: u16, method: &str, target: &str, body: &str) -> Res {
    call(port, method, target, body, &[("Authorization", ADMIN)])
}

fn line(q: &str) -> String {
    let mut body = String::from("{\"query\":");
    fenec_core::json::escape_into(&mut body, q);
    body.push('}');
    body
}

fn query(port: u16, tenant: &str, q: &str) -> Res {
    call(port, "POST", &format!("/t/{tenant}/query"), &line(q), &[])
}

fn ok(r: Res) -> Res {
    assert!(matches!(r.status, 200 | 201), "{}: {}", r.status, r.body);
    r
}

fn create(port: u16, tenant: &str, quota: Option<&str>) {
    let r = admin(port, "PUT", &format!("/_admin/tenants/{tenant}"), "");
    assert_eq!(r.status, 201, "{}", r.body);
    if let Some(q) = quota {
        ok(admin(
            port,
            "PUT",
            &format!("/_admin/tenants/{tenant}/quota"),
            q,
        ));
    }
    ok(query(
        port,
        tenant,
        "create collection notes (title text, n int)",
    ));
}

/// A number in a JSON body, by the path of keys to it.
fn number(body: &str, path: &[&str]) -> Option<u64> {
    let mut v = fenec_core::json::parse_json(body).ok()?;
    for k in path {
        let fenec_core::prelude::Value::Object(members) = v else {
            return None;
        };
        v = members.into_iter().find(|(m, _)| m == k)?.1;
    }
    match v {
        fenec_core::prelude::Value::Int(n) => Some(n as u64),
        _ => None,
    }
}

fn usage(port: u16, tenant: &str, what: &str) -> u64 {
    let r = ok(admin(
        port,
        "GET",
        &format!("/_admin/tenants/{tenant}/quota"),
        "",
    ));
    number(&r.body, &["usage", what]).unwrap_or_else(|| panic!("{}", r.body))
}

/// Waits for the tenant's file to shrink below `full`: a compact, the
/// test's or the node's own, put in place. The wait is bounded only as the
/// event's that must come.
fn shrinks(port: u16, tenant: &str, full: u64) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while usage(port, tenant, "bytes") >= full {
        assert!(Instant::now() < deadline, "the file never shrank");
        std::thread::yield_now();
    }
}

fn count(port: u16, tenant: &str) -> u64 {
    let r = ok(query(port, tenant, "get notes count"));
    let digits: String = r.body.chars().filter(char::is_ascii_digit).collect();
    digits.parse().unwrap()
}

/// The file's quota: writes land up to it, the one that would pass it is
/// refused with 507 and the quota named, the file not a byte longer; a
/// delete lands at a full file, and so does a compact, which gives the
/// bytes back so that writes land again.
#[test]
fn a_file_quota_refuses_the_write_that_would_pass_it_and_takes_deletes() {
    let n = start("bytes", Quota::default());
    create(n.port, "acme", None);
    let base = usage(n.port, "acme", "bytes");
    let max = base + 8_192;
    ok(admin(
        n.port,
        "PUT",
        "/_admin/tenants/acme/quota",
        &format!("{{\"bytes\": {max}}}"),
    ));
    let pad = "x".repeat(200);
    let mut landed = 0;
    let refused = loop {
        let r = call(
            n.port,
            "POST",
            "/t/acme/notes",
            &format!("{{\"title\":\"{pad}\",\"n\":{landed}}}"),
            &[],
        );
        if r.status != 201 {
            break r;
        }
        landed += 1;
    };
    assert_eq!(refused.status, 507, "{}", refused.body);
    assert_eq!(refused.quota(), Some("bytes"));
    assert!(
        refused.header("retry-after").is_none(),
        "coming back alone does not help"
    );
    assert!(refused.body.contains(&max.to_string()), "{}", refused.body);
    let full = usage(n.port, "acme", "bytes");
    assert!(full <= max, "{full} past {max}");
    // A `set` over every row adds a version of each: refused whole.
    let r = query(n.port, "acme", "set notes {n: n + 1000}");
    assert_eq!((r.status, r.quota()), (507, Some("bytes")), "{}", r.body);
    assert_eq!(count(n.port, "acme"), landed);
    assert_eq!(usage(n.port, "acme", "bytes"), full);
    // A batch is refused as one block, and says where it stopped.
    let r = call(
        n.port,
        "POST",
        "/t/acme/batch",
        &format!(
            "{}\n{}",
            line(&format!("put notes {{title: \"{pad}\", n: -1}}")),
            line(&format!("put notes {{title: \"{pad}\", n: -2}}"))
        ),
        &[],
    );
    assert_eq!((r.status, r.quota()), (507, Some("bytes")), "{}", r.body);
    assert!(r.body.contains("\"completed\":0"), "{}", r.body);
    // Deletes land at a full file, a keyed one with its key; a compact
    // runs, and gives the room back.
    ok(query(
        n.port,
        "acme",
        &format!("del notes where n < {}", landed / 2),
    ));
    let r = call(
        n.port,
        "POST",
        "/t/acme/query",
        &line(&format!("del notes where n = {}", landed - 1)),
        &[("Idempotency-Key", "k-1")],
    );
    assert_eq!(r.status, 200, "{}", r.body);
    // The node's compactor may have begun one as the deletes came due
    // (`link::quota_policy`): this one is then refused as a second, and
    // the room comes from that one. Either way the file shrinks.
    let r = query(n.port, "acme", "compact");
    assert!(
        r.status == 200 || r.body.contains("beside the database already"),
        "{}",
        r.body
    );
    shrinks(n.port, "acme", full);
    ok(call(
        n.port,
        "POST",
        "/t/acme/notes",
        "{\"title\":\"back\",\"n\":0}",
        &[],
    ));
}

/// The rows quota counts the tenant's own collections: a batch that would
/// pass it lands none of its rows; an update at a full quota lands, and a
/// delete makes room.
#[test]
fn a_rows_quota_counts_what_a_write_leaves() {
    let n = start("rows", Quota::default());
    create(n.port, "acme", Some("{\"rows\": 5}"));
    ok(query(
        n.port,
        "acme",
        "put notes [{n: 1}, {n: 2}, {n: 3}, {n: 4}]",
    ));
    let r = query(n.port, "acme", "put notes [{n: 5}, {n: 6}]");
    assert_eq!((r.status, r.quota()), (507, Some("rows")), "{}", r.body);
    assert!(r.body.contains("quota of 5 rows"), "{}", r.body);
    assert!(
        !r.body.contains(" 6 "),
        "it tells no count of the tenant's: {}",
        r.body
    );
    assert_eq!(count(n.port, "acme"), 4);
    ok(query(n.port, "acme", "put notes {n: 5}"));
    let r = call(n.port, "POST", "/t/acme/notes", "{\"n\": 6}", &[]);
    assert_eq!((r.status, r.quota()), (507, Some("rows")), "{}", r.body);
    ok(query(n.port, "acme", "set notes {title: \"same count\"}"));
    // The server's own rows -- an idempotency key -- are none of it.
    let r = call(
        n.port,
        "POST",
        "/t/acme/query",
        &line("set notes {n: n * 10}"),
        &[("Idempotency-Key", "k-2")],
    );
    assert_eq!(r.status, 200, "{}", r.body);
    ok(query(n.port, "acme", "del notes where n = 10"));
    ok(query(n.port, "acme", "put notes {n: 6}"));
    assert_eq!(usage(n.port, "acme", "rows"), 5);
}

/// Writers at once cannot take the file past its quota, nor the rows past
/// theirs: the quota is asked as each block lands, under the write lock,
/// so the slack a write is let through by is none. Each writer goes until
/// it is refused; the file ends at or under the quota, and the rows at it
/// exactly.
#[test]
fn concurrent_writers_cannot_pass_a_quota() {
    let n = start("racing", Quota::default());
    create(n.port, "files", None);
    create(n.port, "rows", Some("{\"rows\": 300}"));
    let max = usage(n.port, "files", "bytes") + 32_768;
    ok(admin(
        n.port,
        "PUT",
        "/_admin/tenants/files/quota",
        &format!("{{\"bytes\": {max}}}"),
    ));
    let writers: Vec<_> = (0..8)
        .flat_map(|w| ["files", "rows"].map(|t| (w, t)))
        .map(|(w, t)| {
            let port = n.port;
            std::thread::spawn(move || {
                let mut landed = 0u64;
                loop {
                    let body = format!("{{\"title\":\"{}\",\"n\":{w}}}", "y".repeat(100));
                    let r = call(port, "POST", &format!("/t/{t}/notes"), &body, &[]);
                    match r.status {
                        201 => landed += 1,
                        507 => return (t, landed, r.quota().map(str::to_string)),
                        s => panic!("{s}: {}", r.body),
                    }
                }
            })
        })
        .collect();
    let mut landed = std::collections::HashMap::new();
    for w in writers {
        let (t, l, quota) = w.join().unwrap();
        let kind = if t == "files" { "bytes" } else { "rows" };
        assert_eq!(quota.as_deref(), Some(kind));
        *landed.entry(t).or_insert(0) += l;
    }
    let file = usage(n.port, "files", "bytes");
    assert!(file <= max, "{file} past {max}");
    assert_eq!(count(n.port, "files"), landed["files"]);
    assert_eq!(count(n.port, "rows"), 300);
    assert_eq!(landed["rows"], 300);
}

/// Opens a subscription and reads it to its seed: a stream held open for
/// as long as the returned socket is.
fn subscribe(port: u16, tenant: &str) -> TcpStream {
    try_subscribe(port, tenant).unwrap_or_else(|refused| panic!("{refused}"))
}

/// A subscription, or what it was answered with in its place.
fn try_subscribe(port: u16, tenant: &str) -> Result<TcpStream, String> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    write!(
        s,
        "GET /t/{tenant}/notes/changes HTTP/1.1\r\nHost: t\r\n\r\n"
    )
    .unwrap();
    let mut seen = String::new();
    let mut buf = [0u8; 4096];
    while !seen.contains("event: seed") {
        let k = s.read(&mut buf).unwrap();
        seen.push_str(&String::from_utf8_lossy(&buf[..k]));
        if k == 0 || (seen.len() >= 12 && !seen.starts_with("HTTP/1.1 200")) {
            return Err(seen);
        }
    }
    Ok(s)
}

/// A tenant at its quota of streams: its next subscription and
/// `Fenec-After` wait are refused with 429, while another tenant on the
/// node opens as many as it likes and is answered, and a write held until
/// it can write is no stream of its own (`held`). Told apart by what each
/// is answered, not by how long anything took.
#[test]
fn a_tenant_at_its_streams_quota_leaves_the_others_answered() {
    let n = start("streams", Quota::default().with(Kind::Streams, 2));
    create(n.port, "acme", None);
    create(n.port, "beta", Some("{\"streams\": 0}"));
    let held = [subscribe(n.port, "acme"), subscribe(n.port, "acme")];
    let r = call(n.port, "GET", "/t/acme/notes/changes", "", &[]);
    assert_eq!((r.status, r.quota()), (429, Some("streams")), "{}", r.body);
    assert_eq!(r.header("retry-after"), Some("1"));
    let r = call(
        n.port,
        "GET",
        "/t/acme/notes",
        "",
        &[("Fenec-After", "1000000"), ("Fenec-Wait", "5000")],
    );
    assert_eq!((r.status, r.quota()), (429, Some("streams")), "{}", r.body);
    // Held for its wait and answered what it wrote, nothing: a held write
    // is counted by `held`, not by the streams a dashboard opens.
    let r = call(
        n.port,
        "POST",
        "/t/acme/query",
        &line("del notes where title = \"nothing\""),
        &[("Fenec-Wait", "50")],
    );
    assert_eq!((r.status, r.quota()), (200, None), "{}", r.body);
    // A request that holds no stream is answered.
    ok(query(n.port, "acme", "get notes"));
    // The other tenant, its own quota none, beside it.
    let theirs: Vec<TcpStream> = (0..4).map(|_| subscribe(n.port, "beta")).collect();
    ok(query(n.port, "beta", "put notes {title: \"b\"}"));
    // A stream let go gives its place back.
    drop(held);
    // The server sees a socket gone at its next write to it, a keep-alive
    // line every 80 ms here: until then the place is held.
    let back = loop {
        match try_subscribe(n.port, "acme") {
            Ok(s) => break s,
            Err(refused) => {
                assert!(refused.starts_with("HTTP/1.1 429"), "{refused}");
                std::thread::yield_now();
            }
        }
    };
    drop((back, theirs));
}

/// A tenant at its quota of requests in flight: one more is refused with
/// 429 while it holds them, and another tenant is answered meanwhile. The
/// requests are held in flight by the test holding the tenant's lock --
/// they are in, and waiting for it -- not by a sleep.
#[test]
fn a_tenant_at_its_requests_quota_leaves_the_others_answered() {
    let n = start("requests", Quota::default());
    create(n.port, "acme", Some("{\"requests\": 2}"));
    create(n.port, "beta", None);
    let acme = n.tenants.get("acme").unwrap();
    let lock = acme.db.write().unwrap();
    let port = n.port;
    let inflight: Vec<_> = (0..2)
        .map(|i| {
            std::thread::spawn(move || {
                call(
                    port,
                    "POST",
                    "/t/acme/notes",
                    &format!("{{\"n\":{i}}}"),
                    &[],
                )
            })
        })
        .collect();
    // Both are in: the event waited for, not a time.
    let deadline = Instant::now() + Duration::from_secs(60);
    while acme.quota.holding().1 < 2 {
        assert!(Instant::now() < deadline, "the requests never arrived");
        std::thread::yield_now();
    }
    let r = query(n.port, "acme", "get notes");
    assert_eq!((r.status, r.quota()), (429, Some("requests")), "{}", r.body);
    assert_eq!(r.header("retry-after"), Some("1"));
    ok(query(n.port, "beta", "put notes {n: 1}"));
    ok(query(n.port, "beta", "get notes"));
    drop(lock);
    for t in inflight {
        assert_eq!(t.join().unwrap().status, 201);
    }
    assert_eq!(acme.quota.holding().1, 0);
    ok(query(n.port, "acme", "get notes"));
}

/// Bytes written a second: a write lands while the bucket holds any, is
/// charged as it lands, and the next waits out the debt -- at one byte a
/// second, longer than any test runs, so the refusal is certain.
#[test]
fn a_write_bytes_rate_charges_what_landed() {
    let n = start("rate", Quota::default());
    create(n.port, "acme", None);
    ok(admin(
        n.port,
        "PUT",
        "/_admin/tenants/acme/quota",
        "{\"write_bytes\": 1}",
    ));
    ok(query(
        n.port,
        "acme",
        "put notes {title: \"the first write\"}",
    ));
    let r = query(n.port, "acme", "put notes {title: \"the second\"}");
    assert_eq!(
        (r.status, r.quota()),
        (429, Some("write_bytes")),
        "{}",
        r.body
    );
    let wait: u64 = r.header("retry-after").unwrap().parse().unwrap();
    assert!(wait >= 10, "{wait}");
    // Reads are not writes.
    ok(query(n.port, "acme", "get notes"));
    // A rate of writes: one a second lets one through; the second is
    // refused unless a second went by between the two, which the clock
    // tells from below.
    ok(admin(
        n.port,
        "PUT",
        "/_admin/tenants/acme/quota",
        "{\"writes\": 1}",
    ));
    let t0 = Instant::now();
    ok(query(n.port, "acme", "put notes {n: 1}"));
    let r = query(n.port, "acme", "put notes {n: 2}");
    if t0.elapsed() < Duration::from_secs(1) {
        assert_eq!((r.status, r.quota()), (429, Some("writes")), "{}", r.body);
    }
}

/// Waits for `n` writes held on `tenant`, as a test waits for an event.
fn until_held(n: &Node, tenant: &str, held: usize) {
    let t = Instant::now();
    while n.tenants.get(tenant).unwrap().hub.waits().stats().held != held {
        assert!(t.elapsed() < Duration::from_secs(20), "{held} never held");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// A claim held until a note titled `title` comes.
fn held_claim(port: u16, title: &str) -> std::thread::JoinHandle<Res> {
    let body = line(&format!("del notes where title = \"{title}\""));
    std::thread::spawn(move || {
        call(
            port,
            "POST",
            "/t/acme/query",
            &body,
            &[("Fenec-Wait", "20000")],
        )
    })
}

/// A tenant at its quota of held writes: the next is refused with 429
/// naming `held`, its wait in the body to the millisecond as well as in
/// whole seconds, while its subscriptions open as before -- counted
/// together, forty couriers waiting for orders left a dispatcher's map no
/// stream to open.
#[test]
fn a_tenant_at_its_held_quota_still_opens_its_streams() {
    let n = start("held", Quota::default().with(Kind::Held, 2));
    create(n.port, "acme", Some("{\"streams\": 1}"));
    let held = [held_claim(n.port, "x1"), held_claim(n.port, "x2")];
    until_held(&n, "acme", 2);
    let r = call(
        n.port,
        "POST",
        "/t/acme/query",
        &line("del notes where title = \"x3\""),
        &[("Fenec-Wait", "20000")],
    );
    assert_eq!((r.status, r.quota()), (429, Some("held")), "{}", r.body);
    assert_eq!(r.header("retry-after"), Some("1"));
    assert!(r.body.contains("\"retry_after_ms\":1000"), "{}", r.body);
    let stream = subscribe(n.port, "acme");
    ok(query(
        n.port,
        "acme",
        "put notes [{title: \"x1\"}, {title: \"x2\"}]",
    ));
    for h in held {
        let r = h.join().unwrap();
        assert_eq!((r.status, r.body.as_str()), (200, "{\"affected\":1}"));
    }
    drop(stream);
}

/// A rate of rows written: a write that writes nothing costs nothing --
/// a claim that took no job, a `require` unmet -- and one of ten rows is
/// let through on what the bucket holds and charged ten, so the next is
/// refused, told the wait to the millisecond in the body and in whole
/// seconds, rounded up, in `Retry-After`. A claim held while the tenant
/// is in debt is not answered the 429 when its row comes: it waits the
/// debt out and runs.
#[test]
fn a_rate_of_rows_charges_what_was_written() {
    let n = start("rows-rate", Quota::default());
    create(n.port, "acme", Some("{\"writes\": 5}"));
    for _ in 0..20 {
        let r = ok(query(
            n.port,
            "acme",
            "set notes {n: 1} where title = \"no such note\"",
        ));
        assert_eq!(r.body, "{\"affected\":0}");
    }
    let held = held_claim(n.port, "target");
    until_held(&n, "acme", 1);
    let filler: Vec<String> = (0..10).map(|i| format!("{{n: {i}}}")).collect();
    let charged = Instant::now();
    ok(query(
        n.port,
        "acme",
        &format!("put notes [{}]", filler.join(", ")),
    ));
    let r = query(n.port, "acme", "put notes {n: 99}");
    assert_eq!((r.status, r.quota()), (429, Some("writes")), "{}", r.body);
    let ms = number(&r.body, &["retry_after_ms"]).expect("retry_after_ms");
    let secs: u64 = r.header("retry-after").unwrap().parse().unwrap();
    assert_eq!(secs, ms.div_ceil(1000), "{}", r.body);
    // At most six rows in debt -- the bucket's five and the collection
    // made, less ten -- at five a second: a row's worth in 1.4 s at most.
    assert!((1..=1400).contains(&ms), "{}", r.body);
    // The note the claim waits for, written by the node's own hand: the
    // claim's run is refused by the rate, and waits rather than answer.
    {
        let acme = n.tenants.get("acme").unwrap();
        let mut db = acme.db.write().unwrap();
        db.execute(&fenec_ql::parse_one("put notes {title: \"target\"}").unwrap())
            .unwrap();
    }
    let r = held.join().unwrap();
    assert_eq!((r.status, r.body.as_str()), (200, "{\"affected\":1}"));
    assert!(charged.elapsed() >= Duration::from_secs(1), "ran in debt");
}

/// A tenant's quota is kept beside its file: a tenant closed and opened
/// again is held to it, the node's defaults stand where it sets nothing,
/// 0 is none, and a tenant deleted takes its quota with it.
#[test]
fn a_tenants_quota_outlives_its_close_and_goes_with_it() {
    let n = start("kept", Quota::default().with(Kind::Rows, 2));
    create(n.port, "acme", Some("{\"rows\": 3}"));
    create(n.port, "beta", Some("{\"rows\": 0}"));
    create(n.port, "gamma", None);
    assert!(n.dir.join("acme.quota").exists());
    assert!(n.tenants.close_idle(Duration::ZERO) >= 3);
    ok(query(n.port, "acme", "put notes [{n: 1}, {n: 2}, {n: 3}]"));
    assert_eq!(query(n.port, "acme", "put notes {n: 4}").status, 507);
    ok(query(
        n.port,
        "beta",
        "put notes [{n: 1}, {n: 2}, {n: 3}, {n: 4}]",
    ));
    assert_eq!(
        query(n.port, "gamma", "put notes [{n: 1}, {n: 2}, {n: 3}]").status,
        507
    );
    let r = ok(admin(n.port, "GET", "/_admin/tenants/acme/quota", ""));
    assert_eq!(number(&r.body, &["set", "rows"]), Some(3), "{}", r.body);
    assert_eq!(number(&r.body, &["quota", "rows"]), Some(3));
    assert_eq!(number(&r.body, &["usage", "rows"]), Some(3));
    // Set back to the node's: the file goes, the default holds.
    ok(admin(n.port, "PUT", "/_admin/tenants/acme/quota", "{}"));
    assert!(!n.dir.join("acme.quota").exists());
    assert_eq!(query(n.port, "acme", "put notes {n: 5}").status, 507);
    let r = admin(n.port, "PUT", "/_admin/tenants/acme/quota", "{\"rowz\": 1}");
    assert_eq!(r.status, 400, "{}", r.body);
    ok(admin(
        n.port,
        "PUT",
        "/_admin/tenants/beta/quota",
        "{\"rows\": 9}",
    ));
    assert_eq!(
        admin(n.port, "DELETE", "/_admin/tenants/beta", "").status,
        204
    );
    assert!(!n.dir.join("beta.quota").exists());
    let r = admin(n.port, "PUT", "/_admin/tenants/nope/quota", "{\"rows\": 1}");
    assert_eq!(r.status, 404, "{}", r.body);
    // The whole node's, none opened for it.
    n.tenants.close_idle(Duration::ZERO);
    let r = ok(admin(n.port, "GET", "/_admin/quotas", ""));
    assert!(r.body.starts_with('['), "{}", r.body);
    assert!(r.body.contains("\"tenant\":\"acme\""), "{}", r.body);
    assert!(r.body.contains("\"open\":false"), "{}", r.body);
    // Only the admin token sets or reads one.
    assert_eq!(call(n.port, "GET", "/_admin/quotas", "", &[]).status, 401);
}

/// A scoped token is held to its tenant's quota as every client is, and is
/// told the quota it met, never what the tenant holds -- the rows the other
/// users' make are not its to count.
#[test]
fn a_scoped_token_is_held_to_its_tenants_quota() {
    let access =
        Arc::new(Access::new(SECRET, "notes  read,write  where owner = $jwt.sub\n").unwrap());
    let cfg = Config {
        token: Some("root".into()),
        access: Some(Arc::clone(&access)),
        ..Config::default()
    };
    let n = start_with("scoped", Quota::default(), cfg);
    let r = admin(n.port, "PUT", "/_admin/tenants/acme", "");
    assert_eq!(r.status, 201, "{}", r.body);
    ok(admin(
        n.port,
        "PUT",
        "/_admin/tenants/acme/quota",
        "{\"rows\": 3}",
    ));
    let root = [("Authorization", "Bearer root")];
    let q = |sql: &str| line(sql);
    ok(call(
        n.port,
        "POST",
        "/t/acme/query",
        &q("create collection notes (owner text @hash, title text)"),
        &root,
    ));
    ok(call(
        n.port,
        "POST",
        "/t/acme/query",
        &q("put notes [{owner: \"bob\"}, {owner: \"bob\"}]"),
        &root,
    ));
    let alice = access.mint(r#"{"sub":"alice","tenant":"acme"}"#).unwrap();
    let bearer = format!("Bearer {alice}");
    let as_alice = [("Authorization", bearer.as_str())];
    ok(call(
        n.port,
        "POST",
        "/t/acme/query",
        &q("put notes {owner: \"alice\"}"),
        &as_alice,
    ));
    let r = call(
        n.port,
        "POST",
        "/t/acme/query",
        &q("put notes {owner: \"alice\"}"),
        &as_alice,
    );
    assert_eq!((r.status, r.quota()), (507, Some("rows")), "{}", r.body);
    assert!(r.body.contains("quota of 3 rows"), "{}", r.body);
    // Her own rows she still deletes.
    ok(call(
        n.port,
        "POST",
        "/t/acme/query",
        &q("del notes"),
        &as_alice,
    ));
    // A scoped token reaches no quota of the node's.
    let r = call(n.port, "GET", "/_admin/tenants/acme/quota", "", &as_alice);
    assert_eq!(r.status, 401);
}

/// The refusals are counted by quota on `/_metrics`, never by tenant, and
/// each is a line of the audit log naming the quota and the tenant.
#[test]
fn refusals_are_counted_by_quota_and_logged() {
    let log = std::env::temp_dir().join(format!("fenec-quotas-audit-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&log);
    fenec_http::audit::open(&log).unwrap();
    let n = start("counted", Quota::default());
    create(n.port, "counted", Some("{\"rows\": 1}"));
    let before = fenec_http::quota::refused(Kind::Rows);
    ok(query(n.port, "counted", "put notes {n: 1}"));
    assert_eq!(query(n.port, "counted", "put notes {n: 2}").status, 507);
    assert!(fenec_http::quota::refused(Kind::Rows) > before);
    let r = ok(admin(n.port, "GET", "/_metrics", ""));
    let line = r
        .body
        .lines()
        .find(|l| l.starts_with("fenec_quota_refused_total{quota=\"rows\"}"))
        .unwrap_or_else(|| panic!("{}", r.body));
    assert!(!line.contains("counted"), "{line}");
    assert!(!r.body.contains("tenant=\""), "no tenant in a label");
    let logged = std::fs::read_to_string(&log).unwrap();
    let event = logged
        .lines()
        .find(|l| l.contains("\"event\":\"quota\"") && l.contains("\"tenant\":\"counted\""))
        .unwrap_or_else(|| panic!("{logged}"));
    assert!(event.contains("\"quota\":\"rows\""), "{event}");
    assert!(event.contains("\"http_status\":507"), "{event}");
}

/// A file at its quota is compacted by the node once an eighth of the
/// quota is dead, where a file held to none waits for 64 MB: deletes make
/// room without the tenant running a compact of its own. Asked of the
/// policy and run as the node's compactor runs it, rather than waited for.
#[test]
fn a_file_near_its_quota_is_due_a_compact_by_the_quota() {
    let n = start("compacted", Quota::default());
    create(n.port, "acme", None);
    let max = usage(n.port, "acme", "bytes") + 16_384;
    ok(admin(
        n.port,
        "PUT",
        "/_admin/tenants/acme/quota",
        &format!("{{\"bytes\": {max}}}"),
    ));
    let pad = "z".repeat(300);
    let mut i = 0;
    while call(
        n.port,
        "POST",
        "/t/acme/notes",
        &format!("{{\"title\":\"{pad}\",\"n\":{i}}}"),
        &[],
    )
    .status
        == 201
    {
        i += 1;
    }
    let acme = n.tenants.get("acme").unwrap();
    let full = acme.db.read().unwrap().garbage().file;
    assert!(fenec_http::link::quota_policy(&acme.db, 0).is_none());
    let policy = fenec_http::link::quota_policy(&acme.db, max).expect("near its quota");
    assert!(
        fenec_core::engine::compact_when_due(&acme.db, &policy).is_none(),
        "nothing dead yet"
    );
    ok(query(
        n.port,
        "acme",
        &format!("del notes where n < {}", i / 2),
    ));
    // Run here; or, where the node's compactor took it first by the same
    // policy -- a compact under way is no compact due -- run there.
    if let Some(r) = fenec_core::engine::compact_when_due(&acme.db, &policy) {
        assert!(
            r.as_ref()
                .map_or_else(|e| e.to_string().contains("already"), |_| true),
            "{r:?}"
        );
    }
    drop(acme);
    shrinks(n.port, "acme", full);
    ok(call(
        n.port,
        "POST",
        "/t/acme/notes",
        "{\"title\":\"room\"}",
        &[],
    ));
}
