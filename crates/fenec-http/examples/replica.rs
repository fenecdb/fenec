//! What replication costs: `cargo run --release -p fenec-http --example replica -- [N] [DIM]`
//!
//! A primary and its replicas in one process, each over its own file,
//! talking over loopback:
//!   * lag: from a write's answer on the primary to the write being readable
//!     on the replica, with every write fsynced before its answer (`--sync
//!     always`) and with a 250 ms sync interval
//!   * catch-up: N documents with a DIM vector and an HNSW index, written in
//!     statements of 1 000; how long the replica takes to apply them
//!   * an image: a replica the primary no longer holds the writes for; the
//!     image's size and the time until the replica answers from it
//!   * an archive of the bulk load, and a restore from it: to the end, and
//!     to the middle

use fenec_core::prelude::*;
use fenec_http::archive::{Archive, Target};
use fenec_http::replication::{self, fresh_id, Follower, Replication, Upstream};
use fenec_http::{Config, Server};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % 20_000) as f32 / 10_000.0 - 1.0
    }
    fn vector(&mut self, dim: usize) -> String {
        let v: Vec<String> = (0..dim).map(|_| format!("{:.4}", self.next())).collect();
        format!("[{}]", v.join(","))
    }
}

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fenec-replica-bench-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

const TOKEN: &str = "t";

struct Primary {
    db: Arc<RwLock<Database>>,
    addr: String,
}

fn primary(path: &Path, buffer: usize, sync_on_write: bool) -> Primary {
    let (mut db, feed) = replication::open(path.to_str().unwrap(), buffer).unwrap();
    if db.history().lineage.is_empty() {
        db.fork(fresh_id()).unwrap();
    }
    let db = Arc::new(RwLock::new(db));
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        sync_on_write,
        max_body: 1 << 30,
        ..Config::default()
    };
    let server = Server::new(Arc::clone(&db), cfg).with_replication(Replication::new(
        TOKEN.into(),
        Some(feed),
        None,
    ));
    let listener = server.bind().unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    Primary { db, addr }
}

/// Every `every`, what `fenec-pg --sync <ms>` does: hand the writes over
/// under the lock, fsync without it.
fn syncer(db: Arc<RwLock<Database>>, every: Duration) {
    std::thread::spawn(move || loop {
        std::thread::sleep(every);
        let durable = {
            let mut g = db.write().unwrap();
            if !g.is_dirty() {
                continue;
            }
            g.flush().unwrap()
        };
        if let Some(d) = durable {
            d().unwrap();
        }
    });
}

/// When each change became readable on a replica.
#[derive(Default)]
struct Seen(Mutex<Vec<(u64, Instant)>>);
impl Watcher for Seen {
    fn notify(&self, seq: u64) {
        self.0.lock().unwrap().push((seq, Instant::now()));
    }
}

struct Replica {
    db: Arc<RwLock<Database>>,
    seen: Arc<Seen>,
}

fn replica(path: &Path, upstream: &str) -> Replica {
    let (mut db, feed) =
        replication::open(path.to_str().unwrap(), replication::DEFAULT_BUFFER).unwrap();
    let lineage = db.history().lineage.clone();
    db.follow(lineage).unwrap();
    let seen = Arc::new(Seen::default());
    db.set_watcher(Arc::clone(&seen) as Arc<dyn Watcher>);
    let db = Arc::new(RwLock::new(db));
    let follower = Follower::new(
        &format!("http://{upstream}"),
        TOKEN.into(),
        Arc::clone(&db),
        Some(feed),
        false,
    )
    .unwrap();
    std::thread::spawn(move || follower.run());
    syncer(Arc::clone(&db), Duration::from_millis(250));
    Replica { db, seen }
}

fn seq(db: &Arc<RwLock<Database>>) -> u64 {
    db.read().unwrap().change_seq()
}

/// A keep-alive client for the primary.
struct Client {
    reader: BufReader<TcpStream>,
    out: TcpStream,
}

impl Client {
    fn new(addr: &str) -> Client {
        let s = TcpStream::connect(addr).unwrap();
        s.set_nodelay(true).unwrap();
        Client {
            out: s.try_clone().unwrap(),
            reader: BufReader::new(s),
        }
    }

    fn query(&mut self, sql: &str) {
        let mut body = String::from("{\"query\":");
        fenec_core::json::escape_into(&mut body, sql);
        body.push('}');
        write!(
            self.out,
            "POST /query HTTP/1.1\r\nHost: b\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        assert!(line.contains(" 200 "), "{line}");
        let mut len = 0;
        loop {
            line.clear();
            self.reader.read_line(&mut line).unwrap();
            let l = line.trim_end();
            if l.is_empty() {
                break;
            }
            if let Some(v) = l.strip_prefix("Content-Length: ") {
                len = v.parse().unwrap();
            }
        }
        let mut sink = vec![0u8; len];
        self.reader.read_exact(&mut sink).unwrap();
    }
}

fn percentiles(mut ms: Vec<f64>) -> String {
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |p: f64| ms[((ms.len() - 1) as f64 * p).round() as usize];
    format!(
        "p50 {:.2}  p95 {:.2}  p99 {:.2}  max {:.2} ms",
        at(0.5),
        at(0.95),
        at(0.99),
        ms[ms.len() - 1]
    )
}

/// Writes `n` documents one statement each, `pace` apart (none: back to
/// back), and returns each write's lag on the replica.
fn lag(p: &Primary, r: &Replica, n: usize, pace: Option<Duration>) -> Vec<f64> {
    let mut c = Client::new(&p.addr);
    c.query("create collection items (n int, e vector<16> @hnsw(cosine))");
    let deadline = Instant::now() + Duration::from_secs(30);
    while seq(&r.db) < seq(&p.db) {
        assert!(Instant::now() < deadline, "the replica did not connect");
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut rng = Rng(7);
    let mut acked: Vec<(u64, Instant)> = Vec::with_capacity(n);
    let start = Instant::now();
    for i in 0..n {
        if let Some(pace) = pace {
            let at = start + pace * i as u32;
            if let Some(wait) = at.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
        }
        c.query(&format!("put items {{n: {i}, e: {}}}", rng.vector(16)));
        acked.push((seq(&p.db), Instant::now()));
    }
    let last = acked.last().unwrap().0;
    let deadline = Instant::now() + Duration::from_secs(30);
    while seq(&r.db) < last {
        assert!(Instant::now() < deadline, "the replica did not catch up");
        std::thread::sleep(Duration::from_millis(1));
    }
    let seen = r.seen.0.lock().unwrap().clone();
    acked
        .iter()
        .map(|(s, at)| {
            let (_, t) = seen.iter().find(|(v, _)| v >= s).unwrap();
            t.saturating_duration_since(*at).as_secs_f64() * 1e3
        })
        .collect()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: usize = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(100_000);
    let dim: usize = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(128);

    // ---- lag, `--sync always`: each write fsynced, then answered, then sent.
    let d = dir("always");
    let p = primary(&d.join("p.fenec"), replication::DEFAULT_BUFFER, true);
    let r = replica(&d.join("r.fenec"), &p.addr);
    let always = lag(&p, &r, 1_000, None);
    println!(
        "lag, --sync always, writes back to back: {}",
        percentiles(always)
    );

    // ---- lag, `--sync 250`: answered at once, sent once the next sync ran.
    let d = dir("interval");
    let p = primary(&d.join("p.fenec"), replication::DEFAULT_BUFFER, false);
    syncer(Arc::clone(&p.db), Duration::from_millis(250));
    let r = replica(&d.join("r.fenec"), &p.addr);
    let interval = lag(&p, &r, 1_000, Some(Duration::from_millis(4)));
    println!(
        "lag, --sync 250, 250 writes/s:        {}",
        percentiles(interval)
    );

    // ---- catch-up: the writes of a bulk load, applied from the stream.
    let d = dir("bulk");
    let p = primary(&d.join("p.fenec"), 1 << 30, false);
    let mut rng = Rng(11);
    // Archived as well, from before the first write.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let archiving = {
        let (stop, arch, url) = (
            Arc::clone(&stop),
            d.join("archive"),
            format!("http://{}", p.addr),
        );
        std::thread::spawn(move || {
            let upstream = Upstream::new(&url, TOKEN.into()).unwrap();
            Archive::new(&arch)
                .unwrap()
                .follow(&upstream, &stop, &|_| {})
                .unwrap();
        })
    };
    std::thread::sleep(Duration::from_millis(200));
    let t = Instant::now();
    {
        let mut g = p.db.write().unwrap();
        g.execute(
            &fenec_ql::parse_one(&format!(
                "create collection docs (n int, e vector<{dim}> @hnsw(cosine))"
            ))
            .unwrap(),
        )
        .unwrap();
    }
    for chunk in 0..n.div_ceil(1_000) {
        let docs: Vec<String> = (chunk * 1_000..((chunk + 1) * 1_000).min(n))
            .map(|i| format!("{{n: {i}, e: {}}}", rng.vector(dim)))
            .collect();
        let stmt = fenec_ql::parse_one(&format!("put docs [{}]", docs.join(","))).unwrap();
        p.db.write().unwrap().execute(&stmt).unwrap();
    }
    p.db.write().unwrap().sync().unwrap();
    let wrote = t.elapsed();
    let target = seq(&p.db);
    let t = Instant::now();
    let r = replica(&d.join("r.fenec"), &p.addr);
    while seq(&r.db) < target {
        std::thread::sleep(Duration::from_millis(10));
    }
    let applied = t.elapsed();
    println!(
        "catch-up, {n} x {dim} with HNSW: the primary wrote it in {:.2} s, the replica \
         applied it in {:.2} s ({:.0} writes/s)",
        wrote.as_secs_f64(),
        applied.as_secs_f64(),
        target as f64 / applied.as_secs_f64()
    );

    // ---- an image: the primary checkpointed and restarted, so its feed
    // starts at its last change and an empty replica cannot be continued.
    p.db.write().unwrap().checkpoint().unwrap();
    let size = std::fs::metadata(d.join("p.fenec")).unwrap().len();
    std::fs::copy(d.join("p.fenec"), d.join("p2.fenec")).unwrap();
    let p2 = primary(&d.join("p2.fenec"), replication::DEFAULT_BUFFER, false);
    let t = Instant::now();
    let r2 = replica(&d.join("r2.fenec"), &p2.addr);
    while seq(&r2.db) < target {
        std::thread::sleep(Duration::from_millis(10));
    }
    let imaged = t.elapsed();
    let probe = format!("get docs select id near e {} limit 10", rng.vector(dim));
    let t = Instant::now();
    r2.db
        .read()
        .unwrap()
        .query(&fenec_ql::parse_one(&probe).unwrap(), &[])
        .unwrap();
    println!(
        "image: {:.1} MB, the new replica answered from it after {:.2} s \
         (its first near query: {:.2} ms)",
        size as f64 / 1e6,
        imaged.as_secs_f64(),
        t.elapsed().as_secs_f64() * 1e3
    );
    // ---- restore: the archive of that load, to the end and to its middle.
    let arch = Archive::new(d.join("archive")).unwrap();
    let probe = d.join("probe.fenec");
    let deadline = Instant::now() + Duration::from_secs(120);
    while arch.restore(&probe, Target::Change(target)).is_err() {
        assert!(Instant::now() < deadline, "the archive did not catch up");
        std::thread::sleep(Duration::from_millis(200));
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = archiving.join();
    for (name, to) in [
        ("the end", Target::End),
        ("the middle", Target::Change(target / 2)),
    ] {
        let out = d.join("restored.fenec");
        let t = Instant::now();
        let r = arch.restore(&out, to).unwrap();
        let took = t.elapsed();
        let t = Instant::now();
        let db = fenec_core::fs::open(&out).unwrap();
        println!(
            "restore to {name} (change {}): {:.2} s, the graph built and checkpointed; \
             the file then opens in {:.0} ms",
            r.seq,
            took.as_secs_f64(),
            t.elapsed().as_secs_f64() * 1e3
        );
        drop(db);
    }
    drop(r);
}
