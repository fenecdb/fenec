//! A claim held until it can claim (`Fenec-Wait`): woken by a job enqueued,
//! by a delayed job's time and by a lease lapsing, with no write at all for
//! those two; one job to many held workers goes to one, the one held
//! longest, and the rest stay held; the wait ends on time with what the
//! claim answered; a held claim under a key is replayed with its rows; a
//! worker's token is woken for its own queue's jobs only; a held client
//! that went away hands its job on; and many workers and producers at once
//! claim every job exactly once, a delayed one never before its time.

use fenec_core::prelude::*;
use fenec_http::access::Access;
use fenec_http::{Config, Server};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

const SECRET: &[u8] = b"thirty-two bytes and a few more, for HS256";

const CLAIM: &str = "set jobs {owner: $1, run_at: now() + 60000, attempts: attempts + 1} \
                     where run_at <= now() order run_at limit 1 returning id, kind, run_at";
const ACK: &str = "del jobs where id = $1 and owner = $2 require 1";

struct Node {
    port: u16,
    server: Arc<Server>,
    access: Option<Arc<Access>>,
}

fn start() -> Node {
    start_with(None)
}

fn start_with(policy: Option<&str>) -> Node {
    fenec_http::audit::set_delay(0);
    let mut db = Database::new();
    db.execute(
        &fenec_ql::parse_one(
            "create collection jobs (kind text, run_at timestamp @sorted, owner text, \
             attempts int, claimed_at timestamp)",
        )
        .unwrap(),
    )
    .unwrap();
    let access = policy.map(|p| Arc::new(Access::new(SECRET, p).unwrap()));
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        max_connections: 0,
        access: access.clone(),
        token: access.as_ref().map(|_| "root".to_string()),
        ..Config::default()
    };
    let db = Arc::new(RwLock::new(db));
    let server = Arc::new(Server::new(db, cfg));
    let listener = server.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    let s = Arc::clone(&server);
    std::thread::spawn(move || {
        let _ = s.serve_on(listener);
    });
    Node {
        port,
        server,
        access,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// A keep-alive connection, a worker's.
struct Conn {
    r: BufReader<TcpStream>,
    w: TcpStream,
}

#[derive(Debug)]
struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

impl Conn {
    fn open(port: u16) -> Conn {
        let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
        s.set_nodelay(true).unwrap();
        Conn {
            r: BufReader::new(s.try_clone().unwrap()),
            w: s,
        }
    }

    fn send(&mut self, path: &str, body: &str, headers: &[(&str, &str)]) {
        let mut head = format!("POST {path} HTTP/1.1\r\nHost: t\r\n");
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
        self.w.write_all(head.as_bytes()).unwrap();
    }

    fn read(&mut self) -> Answer {
        let mut line = String::new();
        self.r.read_line(&mut line).unwrap();
        let status = line
            .split_whitespace()
            .nth(1)
            .unwrap_or_else(|| panic!("no status line: {line:?}"))
            .parse()
            .unwrap();
        let mut headers = Vec::new();
        let mut len = 0;
        loop {
            let mut h = String::new();
            self.r.read_line(&mut h).unwrap();
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                if k.eq_ignore_ascii_case("content-length") {
                    len = v.trim().parse().unwrap();
                }
                headers.push((k.to_string(), v.trim().to_string()));
            }
        }
        let mut body = vec![0; len];
        self.r.read_exact(&mut body).unwrap();
        Answer {
            status,
            headers,
            body: String::from_utf8(body).unwrap(),
        }
    }

    fn post(&mut self, path: &str, body: &str, headers: &[(&str, &str)]) -> Answer {
        self.send(path, body, headers);
        self.read()
    }

    fn query(&mut self, q: &str, params: &str) -> Answer {
        self.post("/query", &line(q, params), &[])
    }

    /// A claim held up to `ms`.
    fn claim(&mut self, owner: &str, ms: u64) -> Answer {
        self.post(
            "/query",
            &line(CLAIM, &format!("[\"{owner}\"]")),
            &[("Fenec-Wait", &ms.to_string())],
        )
    }
}

fn line(q: &str, params: &str) -> String {
    let mut out = String::from("{\"query\":");
    fenec_core::json::escape_into(&mut out, q);
    out.push_str(",\"params\":");
    out.push_str(params);
    out.push('}');
    out
}

/// The rows of a `/query` answer: each row's members by name.
fn rows(body: &str) -> Vec<HashMap<String, Value>> {
    let v = fenec_core::json::parse_json(body).unwrap_or_else(|e| panic!("{body}: {e}"));
    let rows = match v {
        Value::List(rows) => rows,
        Value::Object(m) => match m.into_iter().find(|(k, _)| k == "results") {
            Some((_, Value::List(results))) => match results.into_iter().next() {
                Some(Value::Object(r)) => match r.into_iter().find(|(k, _)| k == "rows") {
                    Some((_, Value::List(rows))) => rows,
                    other => panic!("{other:?}"),
                },
                other => panic!("{other:?}"),
            },
            other => panic!("{body}: {other:?}"),
        },
        other => panic!("{other:?}"),
    };
    rows.into_iter()
        .map(|r| match r {
            Value::Object(m) => m.into_iter().collect(),
            other => panic!("{other:?}"),
        })
        .collect()
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int(n) => *n,
        other => panic!("{other:?}"),
    }
}

fn put(port: u16, kind: &str, run_at: i64) -> i64 {
    let a = Conn::open(port).query(
        "put jobs {kind: $1, run_at: $2, attempts: 0}",
        &format!("[\"{kind}\", {run_at}]"),
    );
    assert_eq!(a.status, 200, "{}", a.body);
    let ids = rows(
        &Conn::open(port)
            .query("get jobs select id order id desc limit 1", "[]")
            .body,
    );
    int(&ids[0]["id"])
}

/// Waits for `n` requests held on `node`, the way a test waits for an
/// event rather than for a time.
fn until_held(node: &Node, n: usize) {
    let t = Instant::now();
    while node.server.waits().held < n {
        assert!(t.elapsed() < Duration::from_secs(20), "{n} never held");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn a_held_claim_takes_the_job_enqueued() {
    let node = start();
    let port = node.port;
    let held = std::thread::spawn(move || {
        let a = Conn::open(port).claim("w1", 10_000);
        (a, now_ms())
    });
    until_held(&node, 1);
    let at = now_ms();
    let id = put(port, "mail", at - 1);
    let (a, answered) = held.join().unwrap();
    assert_eq!(a.status, 200, "{}", a.body);
    let got = rows(&a.body);
    assert_eq!(got.len(), 1, "{}", a.body);
    assert_eq!(int(&got[0]["id"]), id);
    assert!(a.header("Fenec-Seq").is_some(), "a write's seq: {a:?}");
    eprintln!("enqueued to answered: {} ms", answered - at);
    assert!(answered - at < 2_000, "{} ms", answered - at);
}

/// Time passing with no write wakes it: a delayed job is claimed at its
/// time and not before, and a lease that lapses hands its job on.
#[test]
fn a_delayed_job_and_a_lapsed_lease_are_claimed_at_their_time() {
    let node = start();
    let port = node.port;
    let ready = now_ms() + 300;
    let id = put(port, "mail", ready);
    let mut c = Conn::open(port);
    let a = c.claim("w1", 10_000);
    let answered = now_ms();
    let got = rows(&a.body);
    assert_eq!(int(&got[0]["id"]), id, "{}", a.body);
    assert!(answered >= ready, "claimed {} ms early", ready - answered);
    eprintln!("delayed job claimed {} ms after its time", answered - ready);
    assert!(answered - ready < 1_000);

    // w1's lease ends in 250 ms and it never acks: w2 takes the job then.
    let lease = "set jobs {owner: \"w1\", run_at: now() + 250} where id = $1 returning run_at";
    let a = c.query(lease, &format!("[{id}]"));
    assert_eq!(a.status, 200, "{}", a.body);
    let lapses = match &rows(&a.body)[0]["run_at"] {
        Value::Text(t) => fenec_core::time::parse(t).unwrap(),
        Value::Int(n) => *n,
        other => panic!("{other:?}"),
    };
    let a = c.claim("w2", 10_000);
    let answered = now_ms();
    assert_eq!(int(&rows(&a.body)[0]["id"]), id, "{}", a.body);
    assert!(answered >= lapses);
    eprintln!(
        "lapsed lease claimed {} ms after it lapsed",
        answered - lapses
    );
    assert!(answered - lapses < 1_000);
    let a = c.query(ACK, &format!("[{id}, \"w2\"]"));
    assert_eq!(a.status, 200, "{}", a.body);
}

/// The wait ends on time, answered as the claim was: no row. Nothing ran
/// meanwhile -- no write, no row's time -- so it slept throughout.
#[test]
fn the_wait_ends_on_time_with_no_row_having_run_nothing() {
    let node = start();
    let mut c = Conn::open(node.port);
    let t = Instant::now();
    let a = c.claim("w1", 300);
    let took = t.elapsed();
    assert_eq!(a.status, 200, "{}", a.body);
    assert_eq!(a.body, "[]");
    assert!(took >= Duration::from_millis(300), "{took:?}");
    assert!(took < Duration::from_millis(1_300), "{took:?}");
    let s = node.server.waits();
    assert_eq!((s.held, s.runs), (0, 0), "{s:?}");
    assert!(s.looks <= 1, "{s:?}");
    // A `require` unmet is answered as it was: 412.
    let a = c.post(
        "/query",
        &line(&format!("{CLAIM} require 1"), "[\"w1\"]"),
        &[("Fenec-Wait", "200")],
    );
    assert_eq!(a.status, 412, "{}", a.body);
}

/// Twenty held workers and one job: one gets it, the one held longest, and
/// the job costs one look and one run, not twenty. The rest stay held and
/// take the next jobs, one each, the longest held first.
#[test]
fn one_job_to_many_held_workers_goes_to_one_and_wakes_no_others() {
    let node = start();
    let port = node.port;
    let (tx, rx) = std::sync::mpsc::channel();
    let mut workers = Vec::new();
    for w in 0..20 {
        let tx = tx.clone();
        workers.push(std::thread::spawn(move || {
            let mut c = Conn::open(port);
            let a = c.claim(&format!("w{w}"), 20_000);
            let ids: Vec<i64> = rows(&a.body).iter().map(|r| int(&r["id"])).collect();
            tx.send((w, ids)).unwrap();
        }));
        // In order, so the longest held is known.
        until_held(&node, w + 1);
    }
    let before = node.server.waits();
    let id = put(port, "mail", now_ms() - 1);
    let (w, ids) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!((w, ids), (0, vec![id]), "the longest held takes it");
    // The others stay held: none answers.
    assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
    let after = node.server.waits();
    assert_eq!(after.held, 19);
    assert_eq!(after.runs - before.runs, 1, "{before:?} -> {after:?}");
    // The put woke the group's head, which looked once; its claim woke the
    // next, whose look found nothing.
    assert!(after.looks - before.looks <= 3, "{before:?} -> {after:?}");
    for n in 1..20 {
        let id = put(port, "mail", now_ms() - 1);
        let (w, ids) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!((w, ids), (n, vec![id]));
    }
    for w in workers {
        w.join().unwrap();
    }
    let s = node.server.waits();
    assert_eq!(s.held, 0);
    assert_eq!(s.runs - before.runs, 20, "{s:?}");
}

/// A held claim under an `Idempotency-Key` keeps no key while it writes
/// nothing, and the key with the rows it took once it does: sent again, it
/// is answered those rows and takes none.
#[test]
fn a_held_claim_under_a_key_is_replayed_with_its_rows() {
    let node = start();
    let port = node.port;
    for path in ["/query", "/batch"] {
        let key = format!("claim-{path}");
        let k = key.clone();
        let held = std::thread::spawn(move || {
            Conn::open(port).post(
                path,
                &line(CLAIM, "[\"w1\"]"),
                &[("Fenec-Wait", "10000"), ("Idempotency-Key", &k)],
            )
        });
        until_held(&node, 1);
        let id = put(port, "mail", now_ms() - 1);
        put(port, "mail", now_ms() - 1);
        let a = held.join().unwrap();
        assert_eq!(a.status, 200, "{}", a.body);
        assert_eq!(int(&rows(&a.body)[0]["id"]), id);
        let again = Conn::open(port).post(
            path,
            &line(CLAIM, "[\"w1\"]"),
            &[("Fenec-Wait", "10000"), ("Idempotency-Key", &key)],
        );
        assert_eq!(
            again.header("Idempotent-Replayed"),
            Some("true"),
            "{again:?}"
        );
        assert_eq!(again.body, a.body);
        // The other job is still there to take.
        let a = Conn::open(port).claim("w2", 1000);
        assert_eq!(rows(&a.body).len(), 1, "{}", a.body);
        Conn::open(port).query("del jobs", "[]");
    }
}

/// A held batch: its `set`s and `del`s wrote nothing, so it waits, and
/// runs whole once a look finds a row; a `put` in a held request is
/// refused, as is a time the server cannot place, and a bad header.
#[test]
fn what_may_be_held() {
    let node = start();
    let port = node.port;
    let mut c = Conn::open(port);
    let wait = [("Fenec-Wait", "5000")];
    for (q, why) in [
        ("put jobs {kind: \"x\"}", "writes otherwise"),
        (
            "set jobs {owner: \"w\"} where bucket(run_at, 60000) <= now()",
            "time only",
        ),
        ("set jobs {owner: \"w\"} where run_at = now()", "time only"),
        ("create collection other (x int)", "writes otherwise"),
    ] {
        let a = c.post("/query", &line(q, "[]"), &wait);
        assert_eq!(a.status, 400, "{q}: {}", a.body);
        assert!(a.body.contains(why), "{q}: {}", a.body);
    }
    let a = c.post("/query", &line(CLAIM, "[\"w\"]"), &[("Fenec-Wait", "soon")]);
    assert_eq!(a.status, 400, "{}", a.body);
    // A read is answered at once, as is a claim with `Fenec-After`, whose
    // wait the header is.
    let t = Instant::now();
    let a = c.post("/query", &line("get jobs", "[]"), &wait);
    assert_eq!((a.status, a.body.as_str()), (200, "[]"));
    let a = c.post(
        "/query",
        &line(CLAIM, "[\"w\"]"),
        &[("Fenec-Wait", "5000"), ("Fenec-After", "0")],
    );
    assert_eq!((a.status, a.body.as_str()), (200, "[]"));
    assert!(t.elapsed() < Duration::from_secs(2));

    let batch = format!(
        "{}\n{}\n",
        line("get jobs count", "[]"),
        line(CLAIM, "[\"w\"]")
    );
    let held = std::thread::spawn(move || Conn::open(port).post("/batch", &batch, &wait));
    until_held(&node, 1);
    let id = put(port, "mail", now_ms() - 1);
    let a = held.join().unwrap();
    assert_eq!(a.status, 200, "{}", a.body);
    assert!(a.body.contains(&format!("\"id\":{id}")), "{}", a.body);
    assert!(
        a.body.contains("\"count\":1"),
        "run whole, after the put: {}",
        a.body
    );
}

/// A worker's token is woken for its own queue's jobs: another queue's,
/// which its rules do not let it update, leave it held, as the look is its
/// claim under its scope.
#[test]
fn a_worker_token_is_woken_for_its_own_queue_alone() {
    let node = start_with(Some(
        "jobs  read                               where kind = $jwt.queue\n\
         jobs  update(owner, run_at, attempts)    where kind = $jwt.queue   for worker\n",
    ));
    let port = node.port;
    let token = node
        .access
        .as_ref()
        .unwrap()
        .mint(r#"{"sub":"w1","role":"worker","queue":"mail"}"#)
        .unwrap();
    let bearer = format!("Bearer {token}");
    let held = std::thread::spawn(move || {
        Conn::open(port).post(
            "/query",
            &line(CLAIM, "[\"w1\"]"),
            &[("Fenec-Wait", "10000"), ("Authorization", &bearer)],
        )
    });
    until_held(&node, 1);
    let root = [("Authorization", "Bearer root")];
    let enqueue = |kind: &str| {
        let a = Conn::open(port).post(
            "/query",
            &line(
                "put jobs {kind: $1, run_at: $2, attempts: 0}",
                &format!("[\"{kind}\", {}]", now_ms() - 1),
            ),
            &root,
        );
        assert_eq!(a.status, 200, "{}", a.body);
    };
    let before = node.server.waits();
    enqueue("sms");
    enqueue("sms");
    std::thread::sleep(Duration::from_millis(150));
    let s = node.server.waits();
    assert_eq!(s.held, 1, "still held");
    assert_eq!(
        s.runs, before.runs,
        "no run for another queue's jobs: {s:?}"
    );
    enqueue("mail");
    let a = held.join().unwrap();
    assert_eq!(a.status, 200, "{}", a.body);
    let got = rows(&a.body);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0]["kind"], Value::Text("mail".into()));
}

/// A held worker whose connection closed hands the job on: the next held
/// takes it at once, rather than it leased to no one for a minute.
#[test]
fn a_held_worker_gone_hands_its_job_on() {
    let node = start();
    let port = node.port;
    let mut gone = Conn::open(port);
    gone.send(
        "/query",
        &line(CLAIM, "[\"gone\"]"),
        &[("Fenec-Wait", "20000")],
    );
    until_held(&node, 1);
    let next = std::thread::spawn(move || Conn::open(port).claim("next", 20_000));
    until_held(&node, 2);
    drop(gone);
    let id = put(port, "mail", now_ms() - 1);
    let a = next.join().unwrap();
    assert_eq!(int(&rows(&a.body)[0]["id"]), id, "{}", a.body);
    let a = Conn::open(port).query("get jobs select owner", "[]");
    assert!(a.body.contains("\"next\""), "{}", a.body);
}

/// Many held workers, producers enqueueing ready and delayed jobs, and
/// workers that die holding a job: every job is acked exactly once, a
/// delayed one claimed no earlier than its time, a dead worker's job again
/// after its lease -- in rounds, beside whatever else the machine runs.
#[test]
fn under_load_every_job_is_claimed_once_and_none_before_its_time() {
    const WORKERS: usize = 12;
    const PRODUCERS: usize = 3;
    const PER_PRODUCER: i64 = 150;
    let node = start();
    let port = node.port;
    let claim = "set jobs {owner: $1, run_at: now() + $2, attempts: attempts + 1, \
                 claimed_at: now()} where run_at <= now() order run_at limit 3 \
                 returning id, kind, claimed_at";
    let done = Arc::new(Mutex::new(Vec::<(String, i64)>::new()));
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let workers: Vec<_> = (0..WORKERS)
        .map(|w| {
            let done = Arc::clone(&done);
            let finished = Arc::clone(&finished);
            std::thread::spawn(move || {
                let mut c = Conn::open(port);
                let mut n = 0;
                while !finished.load(std::sync::atomic::Ordering::Acquire) {
                    n += 1;
                    let owner = format!("w{w}:{n}");
                    // One worker in four dies on every fifth claim: its
                    // lease is short, so its jobs come back soon.
                    let dies = w % 4 == 0 && n % 5 == 0;
                    let lease = if dies { 150 } else { 30_000 };
                    let a = c.post(
                        "/query",
                        &line(claim, &format!("[\"{owner}\", {lease}]")),
                        &[("Fenec-Wait", "500")],
                    );
                    assert_eq!(a.status, 200, "{}", a.body);
                    for r in rows(&a.body) {
                        if dies {
                            continue;
                        }
                        let id = int(&r["id"]);
                        let a = c.query(ACK, &format!("[{id}, \"{owner}\"]"));
                        assert_eq!(a.status, 200, "{}", a.body);
                        let claimed = match &r["claimed_at"] {
                            Value::Text(t) => fenec_core::time::parse(t).unwrap(),
                            other => panic!("{other:?}"),
                        };
                        let Value::Text(kind) = &r["kind"] else {
                            panic!("{r:?}")
                        };
                        done.lock().unwrap().push((kind.clone(), claimed));
                    }
                }
            })
        })
        .collect();
    let due = Arc::new(Mutex::new(HashMap::<String, i64>::new()));
    let producers: Vec<_> = (0..PRODUCERS)
        .map(|p| {
            let due = Arc::clone(&due);
            std::thread::spawn(move || {
                let mut c = Conn::open(port);
                for i in 0..PER_PRODUCER {
                    // A third delayed up to 400 ms.
                    let delay = if i % 3 == 0 {
                        (i * 37 + p as i64 * 11) % 400
                    } else {
                        0
                    };
                    let at = now_ms() + delay;
                    let kind = format!("p{p}-{i}");
                    due.lock().unwrap().insert(kind.clone(), at);
                    let a = c.query(
                        "put jobs {kind: $1, run_at: $2, attempts: 0}",
                        &format!("[\"{kind}\", {at}]"),
                    );
                    assert_eq!(a.status, 200, "{}", a.body);
                    if i % 10 == 0 {
                        std::thread::sleep(Duration::from_millis(7));
                    }
                }
            })
        })
        .collect();
    for p in producers {
        p.join().unwrap();
    }
    let total = PRODUCERS as i64 * PER_PRODUCER;
    let t = Instant::now();
    while (done.lock().unwrap().len() as i64) < total {
        assert!(
            t.elapsed() < Duration::from_secs(30),
            "{} of {total} acked",
            done.lock().unwrap().len()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    finished.store(true, std::sync::atomic::Ordering::Release);
    for w in workers {
        w.join().unwrap();
    }
    let done = done.lock().unwrap();
    let due = due.lock().unwrap();
    let mut seen = std::collections::HashSet::new();
    let mut late = Vec::new();
    for (kind, claimed) in done.iter() {
        assert!(seen.insert(kind.clone()), "job {kind} acked twice");
        let at = due[kind];
        assert!(
            *claimed >= at,
            "job {kind} claimed {} ms before its time",
            at - claimed
        );
        late.push(claimed - at);
    }
    assert_eq!(seen.len() as i64, total);
    late.sort_unstable();
    eprintln!(
        "claimed after their time: p50 {} ms, p99 {} ms, max {} ms",
        late[late.len() / 2],
        late[late.len() * 99 / 100],
        late[late.len() - 1]
    );
    let left = Conn::open(port).query("get jobs count", "[]");
    assert_eq!(left.body, "[{\"count\":0}]");
    assert_eq!(node.server.waits().held, 0);
}

/// A tenant's held claim holds no gate between its runs: a freeze -- a
/// move's first step -- goes through at once, and the claim stays held and
/// takes the job enqueued after the thaw.
#[test]
fn a_held_claim_holds_no_tenant_against_a_freeze() {
    fenec_http::audit::set_delay(0);
    let dir = std::env::temp_dir().join(format!(
        "fenec-held-tenant-{}-{:?}",
        std::process::id(),
        Instant::now()
    ));
    let tenants = Arc::new(fenec_http::tenants::Tenants::new(&dir).unwrap());
    tenants.create("acme").unwrap();
    let server = Server::with_tenants(
        Arc::clone(&tenants),
        Config {
            addr: "127.0.0.1:0".into(),
            ..Config::default()
        },
    );
    let listener = server.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    let mut c = Conn::open(port);
    let made = c.post(
        "/t/acme/query",
        &line("create collection jobs (kind text, run_at timestamp @sorted, owner text, attempts int)", "[]"),
        &[],
    );
    assert_eq!(made.status, 200, "{}", made.body);
    let held = std::thread::spawn(move || {
        Conn::open(port).post(
            "/t/acme/query",
            &line(CLAIM, "[\"w1\"]"),
            &[("Fenec-Wait", "10000")],
        )
    });
    let t = Instant::now();
    while tenants.get("acme").unwrap().hub.waits().stats().held == 0 {
        assert!(t.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(1));
    }
    let t = Instant::now();
    tenants.freeze("acme").unwrap();
    assert!(
        t.elapsed() < Duration::from_millis(500),
        "{:?}",
        t.elapsed()
    );
    tenants.thaw("acme").unwrap();
    let a = c.post(
        "/t/acme/query",
        &line(
            "put jobs {kind: \"mail\", run_at: $1, attempts: 0}",
            &format!("[{}]", now_ms() - 1),
        ),
        &[],
    );
    assert_eq!(a.status, 200, "{}", a.body);
    let a = held.join().unwrap();
    assert_eq!(a.status, 200, "{}", a.body);
    assert_eq!(rows(&a.body).len(), 1, "{}", a.body);
    let _ = std::fs::remove_dir_all(dir);
}
