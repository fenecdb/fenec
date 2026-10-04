//! What the server owes every client whatever it asks: writes on disk when
//! it says so, a disk that fails reported and no write taken after it, an
//! fsync that holds up neither the readers nor the other writers, a deep
//! query that does not take the process down, and its ceilings on
//! connections and silence. These were held over the pg wire until it went;
//! HTTP is every client's way in now.

use fenec_core::prelude::*;
use fenec_http::{Config, Server};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};

struct Harness {
    port: u16,
    db: Arc<RwLock<Database>>,
}

fn start(mut cfg: Config, db: Database) -> Harness {
    cfg.addr = "127.0.0.1:0".into();
    let db = Arc::new(RwLock::new(db));
    let server = Server::new(Arc::clone(&db), cfg);
    let listener = server.bind().expect("bind");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    Harness { port, db }
}

/// A keep-alive connection: requests one after another, as a client's pool
/// keeps one.
struct Conn {
    r: BufReader<TcpStream>,
    w: TcpStream,
}

impl Conn {
    fn open(port: u16) -> Conn {
        let s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        Conn {
            r: BufReader::new(s.try_clone().unwrap()),
            w: s,
        }
    }

    /// `(status, body)`, or `None` when the server closed the connection.
    fn ask(&mut self, method: &str, target: &str, body: &str) -> Option<(u16, String)> {
        let req = format!(
            "{method} {target} HTTP/1.1\r\nHost: t\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        self.w.write_all(req.as_bytes()).ok()?;
        let mut line = String::new();
        if self.r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let status = line.split_whitespace().nth(1)?.parse().ok()?;
        let mut len = 0;
        loop {
            let mut h = String::new();
            self.r.read_line(&mut h).ok()?;
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                if k.eq_ignore_ascii_case("content-length") {
                    len = v.trim().parse().ok()?;
                }
            }
        }
        let mut body = vec![0; len];
        self.r.read_exact(&mut body).ok()?;
        Some((status, String::from_utf8_lossy(&body).into_owned()))
    }

    /// A statement through `POST /query`.
    fn query(&mut self, q: &str) -> (u16, String) {
        let body = format!("{{\"query\": {}}}", json_string(q));
        self.ask("POST", "/query", &body)
            .expect("the server closed the connection")
    }
}

fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn ok(r: (u16, String)) -> String {
    assert_eq!(r.0, 200, "{}", r.1);
    r.1
}

fn tmp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("fenec-http-server-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let _ = std::fs::remove_file(&path);
    path
}

/// Under `--sync always` a write is on disk when it is answered: the file
/// opened on its own afterwards, as after a crash, holds it.
#[test]
fn sync_always_persists_every_write() {
    let path = tmp("always.fenec");
    let db = fenec_core::fs::open(&path).unwrap();
    let h = start(
        Config {
            sync_on_write: true,
            ..Config::default()
        },
        db,
    );
    let mut c = Conn::open(h.port);
    ok(c.query("create collection t (name text)"));
    ok(c.query(r#"put t {name: "durable"}"#));

    let reopened = fenec_core::fs::open(&path).unwrap();
    let rows = reopened
        .query(&fenec_ql::parse_one("get t select name").unwrap(), &[])
        .unwrap();
    let rs = rows.rows().unwrap();
    assert_eq!(rs.rows.len(), 1);
    assert_eq!(rs.rows[0].values[0], Value::Text("durable".into()));
}

/// A sink whose sync always fails, as a disk that went away does.
struct SyncFails;

impl fenec_core::engine::Sink for SyncFails {
    fn append(&mut self, _bytes: &[u8]) -> fenec_core::error::Result<()> {
        Ok(())
    }
    fn rewrite(&mut self, _bytes: &[u8]) -> fenec_core::error::Result<()> {
        Ok(())
    }
    fn sync(&mut self) -> fenec_core::error::Result<()> {
        Err(fenec_core::error::Error::Io("input/output error".into()))
    }
}

/// A sync the disk refuses is the answer, not the success the write had
/// not yet been given; every write after it is refused until the file is
/// opened again, and reads go on from memory. A failed fsync is never
/// retried: the kernel may already have dropped the pages.
#[test]
fn sync_failure_is_reported_and_stops_writes() {
    let h = start(
        Config {
            sync_on_write: true,
            ..Config::default()
        },
        Database::with_sink(Box::new(SyncFails)),
    );
    let mut c = Conn::open(h.port);
    let (status, body) = c.query("create collection t (name text)");
    assert_eq!(status, 500, "the failed sync must reach the client: {body}");

    let (status, body) = c.query(r#"put t {name: "a"}"#);
    assert_eq!(status, 500, "{body}");
    assert!(body.contains("refused"), "{body}");

    ok(c.query("collections"));
    assert!(h.db.read().unwrap().failure().is_some());
}

/// A disk whose fsync fails when `fail` is set -- the half of a sync that
/// runs outside the lock -- and waits at a [`Gate`] the test holds. It
/// keeps `FileSink`'s protocol: an fsync covers every byte appended before
/// it starts, and one that finds its bytes covered does not run. It counts
/// the fsyncs it ran.
#[derive(Clone)]
struct SlowDisk {
    fail: bool,
    fsyncs: Arc<AtomicUsize>,
    appended: Arc<AtomicU64>,
    /// Bytes on disk; held through an fsync, as the file is.
    synced: Arc<Mutex<u64>>,
    gate: Arc<(Mutex<Gate>, Condvar)>,
}

/// What an fsync waits for: the test letting it go, or the appends it
/// is told to wait for -- writes that could land only if the fsync held no
/// lock. Ordered by events, not by the clock: timed with a 200 ms fsync
/// and a 50 ms head start, the test failed on a loaded machine with no
/// lock held anywhere. An fsync that waits out `GATE_LIMIT` goes on and
/// says so in `timed_out`, which is the failure: what it waited for could
/// not happen beside it.
#[derive(Default)]
struct Gate {
    /// Appends so far: a write's record each.
    appends: u64,
    /// While set, an fsync waits until `appends` reaches it.
    hold_until: Option<u64>,
    /// Fsyncs that reached the gate.
    reached: usize,
    timed_out: bool,
}

const GATE_LIMIT: Duration = Duration::from_secs(20);

impl SlowDisk {
    fn wait_at_gate(&self) {
        let (lock, cv) = &*self.gate;
        let mut g = lock.lock().unwrap();
        g.reached += 1;
        cv.notify_all();
        let (mut g, waited) = cv
            .wait_timeout_while(g, GATE_LIMIT, |g| {
                g.hold_until.is_some_and(|n| g.appends < n)
            })
            .unwrap();
        if waited.timed_out() {
            g.timed_out = true;
        }
    }
}

impl fenec_core::engine::Sink for SlowDisk {
    fn append(&mut self, bytes: &[u8]) -> fenec_core::error::Result<()> {
        self.appended
            .fetch_add(bytes.len() as u64, Ordering::SeqCst);
        let (lock, cv) = &*self.gate;
        lock.lock().unwrap().appends += 1;
        cv.notify_all();
        Ok(())
    }
    fn rewrite(&mut self, _bytes: &[u8]) -> fenec_core::error::Result<()> {
        Ok(())
    }
    fn flush(&mut self) -> fenec_core::error::Result<Option<fenec_core::engine::Durability>> {
        let disk = self.clone();
        let upto = self.appended.load(Ordering::SeqCst);
        Ok(Some(Box::new(move || {
            let mut synced = disk.synced.lock().unwrap();
            if *synced >= upto {
                return Ok(());
            }
            let covers = disk.appended.load(Ordering::SeqCst);
            disk.wait_at_gate();
            disk.fsyncs.fetch_add(1, Ordering::SeqCst);
            if disk.fail {
                return Err(fenec_core::error::Error::Io("input/output error".into()));
            }
            *synced = covers;
            Ok(())
        })))
    }
}

struct SlowDiskServer {
    h: Harness,
    fsyncs: Arc<AtomicUsize>,
    gate: Arc<(Mutex<Gate>, Condvar)>,
}

impl SlowDiskServer {
    /// Holds every fsync from now on until `appends` more writes landed;
    /// `u64::MAX` until [`Self::open`].
    fn hold_for(&self, appends: u64) {
        let mut g = self.gate.0.lock().unwrap();
        g.hold_until = Some(g.appends.saturating_add(appends));
    }

    fn open(&self) {
        self.gate.0.lock().unwrap().hold_until = None;
        self.gate.1.notify_all();
    }

    /// Waits until `n` fsyncs in all reached the gate.
    fn reached(&self, n: usize) {
        let (lock, cv) = &*self.gate;
        let (g, waited) = cv
            .wait_timeout_while(lock.lock().unwrap(), GATE_LIMIT, |g| g.reached < n)
            .unwrap();
        assert!(
            !waited.timed_out(),
            "{} fsyncs reached the disk, not {n}",
            g.reached
        );
    }

    fn timed_out(&self) -> bool {
        self.gate.0.lock().unwrap().timed_out
    }
}

fn slow_disk_server(fail: bool) -> SlowDiskServer {
    let fsyncs = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new((Mutex::new(Gate::default()), Condvar::new()));
    let mut db = Database::with_sink(Box::new(SlowDisk {
        fail,
        fsyncs: Arc::clone(&fsyncs),
        appended: Arc::new(AtomicU64::new(0)),
        synced: Arc::new(Mutex::new(0)),
        gate: Arc::clone(&gate),
    }));
    db.execute(&fenec_ql::parse_one("create collection t (n int)").unwrap())
        .unwrap();
    let h = start(
        Config {
            sync_on_write: true,
            ..Config::default()
        },
        db,
    );
    SlowDiskServer { h, fsyncs, gate }
}

/// The fsync of a durable write runs outside the write lock: a read
/// arriving meanwhile is answered at once, and writers arriving together
/// share one fsync -- the group commit -- rather than take one each in
/// turn under the lock.
///
/// Held by order, not by the clock: the fsync is held until the read is
/// answered, or until the other writers' records have landed, which under
/// the lock neither could be -- the fsync then waits out `GATE_LIMIT` and
/// the test fails on it. Timed (a 200 ms fsync, the read under 100 ms, the
/// eight writes under a second) it failed once on a loaded machine.
#[test]
fn a_durable_write_does_not_hold_the_readers_or_the_other_writers() {
    let s = slow_disk_server(false);
    let port = s.h.port;
    s.hold_for(u64::MAX);
    let writer = std::thread::spawn(move || ok(Conn::open(port).query("put t {n: 1}")));
    s.reached(1);
    // The write's fsync is under way, and stays so until the read is in.
    ok(Conn::open(port).query("get t count"));
    s.open();
    writer.join().unwrap();
    assert!(!s.timed_out(), "the read waited for the fsync");

    // Eight writers at once, the first fsync held until all eight records
    // are in: with an fsync each under the lock, the second writer could
    // not append. Sharing, the one fsync after it covers the other seven.
    let before = s.fsyncs.load(Ordering::SeqCst);
    s.hold_for(8);
    let writers: Vec<_> = (0..8)
        .map(|i| {
            std::thread::spawn(move || ok(Conn::open(port).query(&format!("put t {{n: {i}}}"))))
        })
        .collect();
    for w in writers {
        w.join().unwrap();
    }
    assert!(!s.timed_out(), "the writers waited for each other's fsync");
    let ran = s.fsyncs.load(Ordering::SeqCst) - before;
    assert!(ran <= 2, "{ran} fsyncs for 8 writes");
}

/// An fsync that fails after the lock was let go takes the answer back:
/// the write is reported failed, and the engine refuses every write after
/// it, as after a failure of its own.
#[test]
fn a_failed_fsync_outside_the_lock_is_reported_and_stops_writes() {
    let SlowDiskServer { h, .. } = slow_disk_server(true);
    let mut c = Conn::open(h.port);
    let (status, body) = c.query("put t {n: 1}");
    assert_eq!(
        status, 500,
        "the failed fsync must reach the client: {body}"
    );

    let (status, body) = c.query("put t {n: 2}");
    assert_eq!(status, 500, "{body}");
    assert!(body.contains("refused"), "{body}");

    ok(c.query("get t count"));
    assert!(h.db.read().unwrap().failure().is_some());
}

/// The deepest expression the parser takes runs on a connection's stack:
/// one past the limit is a 400, one just under it an answer, and the
/// server goes on. A stack overflow is no panic but the process's abort.
#[test]
fn deep_expression_does_not_kill_the_server() {
    let h = start(Config::default(), Database::new());
    let mut c = Conn::open(h.port);
    ok(c.query("create collection t (n int)"));
    let nested = |d: usize| format!("get t where {}n = 1{}", "(".repeat(d), ")".repeat(d));

    let (status, body) = c.query(&nested(fenec_ql::MAX_EXPR_DEPTH + 64));
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("too deep"), "{body}");

    ok(c.query(&nested(fenec_ql::MAX_EXPR_DEPTH - 1)));
    ok(c.query("collections"));
}

/// Reads take the shared lock: one held elsewhere does not keep a
/// client's read waiting.
#[test]
fn reads_share_the_lock() {
    let h = start(Config::default(), Database::new());
    let mut setup = Conn::open(h.port);
    ok(setup.query("create collection t (name text)"));
    ok(setup.query(r#"put t {name: "a"}"#));

    let held = h.db.read().unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let d2 = Arc::clone(&done);
    let port = h.port;
    let t = std::thread::spawn(move || {
        let r = ok(Conn::open(port).query("get t select name"));
        d2.store(true, Ordering::SeqCst);
        r
    });
    let start = Instant::now();
    while !done.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        done.load(Ordering::SeqCst),
        "the client's read blocked while a read lock was held"
    );
    assert!(t.join().unwrap().contains(r#""name":"a""#));
    drop(held);
}

/// Searches running at once on many connections answer as they do one
/// after another.
#[test]
fn concurrent_vector_search_matches_serial() {
    let h = start(Config::default(), Database::new());
    let mut c = Conn::open(h.port);
    ok(c.query(
        "create collection v (name text, e vector<8> @hnsw(cosine, m=16, ef_construction=100))",
    ));
    let mut seed = 1u64;
    let mut rnd = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        ((seed >> 33) as f32) / (u32::MAX as f32 / 2.0)
    };
    let mut docs = Vec::new();
    for i in 0..2000 {
        let v: Vec<String> = (0..8).map(|_| format!("{:.4}", rnd())).collect();
        docs.push(format!("{{name: \"v{i}\", e: [{}]}}", v.join(",")));
    }
    ok(c.query(&format!("put v [{}]", docs.join(", "))));

    let queries: Vec<String> = (0..12)
        .map(|_| {
            let v: Vec<String> = (0..8).map(|_| format!("{:.4}", rnd())).collect();
            format!("get v select name near e [{}] limit 5", v.join(","))
        })
        .collect();
    let serial: Vec<String> = queries.iter().map(|q| ok(c.query(q))).collect();
    let port = h.port;
    let parallel: Vec<String> = queries
        .iter()
        .cloned()
        .map(|q| std::thread::spawn(move || ok(Conn::open(port).query(&q))))
        .collect::<Vec<_>>()
        .into_iter()
        .map(|t| t.join().unwrap())
        .collect();
    for (i, (s, p)) in serial.iter().zip(&parallel).enumerate() {
        assert!(s.contains("\"name\""), "query {i} returned nothing: {s}");
        assert_eq!(
            s, p,
            "query {i}: the parallel search differs from the serial one"
        );
    }
}

/// Every connection is a thread: past the ceiling one is refused with 503,
/// the live one goes on, and its slot is free again once it closes.
#[test]
fn connection_limit_refuses_extra_connections() {
    let h = start(
        Config {
            max_connections: 1,
            ..Config::default()
        },
        Database::new(),
    );
    let mut first = Conn::open(h.port);
    ok(first.query("collections"));
    let (status, body) = Conn::open(h.port)
        .ask("GET", "/collections", "")
        .expect("the refusal is an answer");
    assert_eq!(status, 503, "{body}");
    ok(first.query("collections"));

    drop(first);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match Conn::open(h.port).ask("GET", "/collections", "") {
            Some((200, _)) => break,
            other => {
                assert!(Instant::now() < deadline, "no slot freed up: {other:?}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// A keep-alive connection silent past `--idle-timeout` is closed, and
/// closed with nothing said: a `400 read error` written as it went was read
/// by Python's `http.client` as the answer to the next request it sent on
/// the connection, a `put` that never ran.
#[test]
fn idle_timeout_closes_the_connection_without_a_word() {
    let h = start(
        Config {
            idle_timeout: Some(Duration::from_millis(200)),
            ..Config::default()
        },
        Database::new(),
    );
    let mut c = Conn::open(h.port);
    ok(c.query("collections"));
    // The server's close is the event waited for: the read ends at it, and
    // the client's own read timeout (10 s) bounds only a server that never
    // closes.
    let mut sent = Vec::new();
    let read = c.r.read_to_end(&mut sent);
    assert!(read.is_ok(), "the connection was kept: {read:?}");
    assert_eq!(
        String::from_utf8_lossy(&sent),
        "",
        "bytes no request asked for"
    );

    // A connection opened and never written to is closed the same way.
    let mut s = TcpStream::connect(("127.0.0.1", h.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut sent = Vec::new();
    assert!(s.read_to_end(&mut sent).is_ok());
    assert!(sent.is_empty(), "{}", String::from_utf8_lossy(&sent));

    // A request cut off part way is answered, since one was asked: 408.
    let mut s = TcpStream::connect(("127.0.0.1", h.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(b"GET /collections HTTP/1.1\r\nHost: t\r\n")
        .unwrap();
    let mut sent = String::new();
    let _ = s.read_to_string(&mut sent);
    assert!(sent.starts_with("HTTP/1.1 408"), "{sent}");
}

/// `GET /_health` answers with no token and takes no lock: a probe that
/// queried would wait out a long `compact`, and a healthy server would look
/// dead.
#[test]
fn health_answers_with_no_token_and_no_lock() {
    let h = start(
        Config {
            token: Some("secret".into()),
            ..Config::default()
        },
        Database::new(),
    );
    let held = h.db.write().unwrap();
    let (status, body) = Conn::open(h.port)
        .ask("GET", "/_health", "")
        .expect("an answer");
    assert_eq!((status, body.as_str()), (200, r#"{"ok":true}"#));
    drop(held);
    let (status, _) = Conn::open(h.port).ask("GET", "/collections", "").unwrap();
    assert_eq!(status, 401, "the rest still wants the token");
}
