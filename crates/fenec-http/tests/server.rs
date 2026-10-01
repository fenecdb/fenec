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
use std::sync::{Arc, Mutex, RwLock};
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

/// A disk that takes `delay` for an fsync, and fails it when `fail` is set
/// -- the half of a sync that runs outside the lock. It keeps `FileSink`'s
/// protocol: an fsync covers every byte appended before it starts, and one
/// that finds its bytes covered does not run. It counts the fsyncs it ran.
#[derive(Clone)]
struct SlowDisk {
    delay: Duration,
    fail: bool,
    fsyncs: Arc<AtomicUsize>,
    appended: Arc<AtomicU64>,
    /// Bytes on disk; held through an fsync, as the file is.
    synced: Arc<Mutex<u64>>,
}

impl fenec_core::engine::Sink for SlowDisk {
    fn append(&mut self, bytes: &[u8]) -> fenec_core::error::Result<()> {
        self.appended
            .fetch_add(bytes.len() as u64, Ordering::SeqCst);
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
            std::thread::sleep(disk.delay);
            disk.fsyncs.fetch_add(1, Ordering::SeqCst);
            if disk.fail {
                return Err(fenec_core::error::Error::Io("input/output error".into()));
            }
            *synced = covers;
            Ok(())
        })))
    }
}

fn slow_disk_server(delay: Duration, fail: bool) -> (Harness, Arc<AtomicUsize>) {
    let fsyncs = Arc::new(AtomicUsize::new(0));
    let mut db = Database::with_sink(Box::new(SlowDisk {
        delay,
        fail,
        fsyncs: Arc::clone(&fsyncs),
        appended: Arc::new(AtomicU64::new(0)),
        synced: Arc::new(Mutex::new(0)),
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
    (h, fsyncs)
}

/// The fsync of a durable write runs outside the write lock: a read
/// arriving meanwhile is answered at once, and writers arriving together
/// share one fsync -- the group commit -- rather than take one each in
/// turn under the lock.
#[test]
fn a_durable_write_does_not_hold_the_readers_or_the_other_writers() {
    let (h, fsyncs) = slow_disk_server(Duration::from_millis(200), false);
    let port = h.port;
    let writer = std::thread::spawn(move || ok(Conn::open(port).query("put t {n: 1}")));
    std::thread::sleep(Duration::from_millis(50));
    let mut reader = Conn::open(h.port);
    let t = Instant::now();
    ok(reader.query("get t count"));
    let read = t.elapsed();
    writer.join().unwrap();
    // The write under way was still inside its 200 ms fsync.
    assert!(
        read < Duration::from_millis(100),
        "the read waited {read:?}"
    );

    // Eight writers at once: with an fsync each under the lock they took
    // 8 x 200 ms. Sharing, they take two or three fsyncs.
    let before = fsyncs.load(Ordering::SeqCst);
    let t = Instant::now();
    let writers: Vec<_> = (0..8)
        .map(|i| {
            std::thread::spawn(move || ok(Conn::open(port).query(&format!("put t {{n: {i}}}"))))
        })
        .collect();
    for w in writers {
        w.join().unwrap();
    }
    let took = t.elapsed();
    let ran = fsyncs.load(Ordering::SeqCst) - before;
    assert!(ran <= 4, "{ran} fsyncs for 8 writes");
    assert!(
        took < Duration::from_millis(1000),
        "8 durable writes took {took:?}"
    );
}

/// An fsync that fails after the lock was let go takes the answer back:
/// the write is reported failed, and the engine refuses every write after
/// it, as after a failure of its own.
#[test]
fn a_failed_fsync_outside_the_lock_is_reported_and_stops_writes() {
    let (h, _) = slow_disk_server(Duration::from_millis(1), true);
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

/// A keep-alive connection silent past `--idle-timeout` is closed: its
/// thread is let go of.
#[test]
fn idle_timeout_closes_the_connection() {
    let h = start(
        Config {
            idle_timeout: Some(Duration::from_millis(200)),
            ..Config::default()
        },
        Database::new(),
    );
    let mut c = Conn::open(h.port);
    ok(c.query("collections"));
    std::thread::sleep(Duration::from_millis(600));
    // The close is said with the timed-out read's 400 before it, which
    // the next request finds waiting; then the connection is gone.
    let mut asked = 0;
    while let Some((status, body)) = c.ask("GET", "/collections", "") {
        assert_eq!(status, 400, "the silent connection was kept: {body}");
        asked += 1;
        assert!(asked < 2, "the silent connection was kept");
    }
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
