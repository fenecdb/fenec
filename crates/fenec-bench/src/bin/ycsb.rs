//! YCSB's core workloads, fenecdb against SQLite, PostgreSQL and MongoDB:
//! `make ycsb`.
//!
//! Written here rather than run through the Java YCSB, keeping its
//! definitions: a record is a key and ten fields of 100 random characters
//! (`field0`..`field9`), a read reads the whole record, an update writes one
//! field, and the keys are drawn as YCSB draws them -- its scrambled
//! zipfian (constant 0.99, the item hashed with FNV-1a so the popular keys
//! are spread over the key space), its skewed-latest for D, and a scan's
//! length uniform from 1 to 100 for E:
//!
//!   A  50% read, 50% update          zipfian
//!   B  95% read,  5% update          zipfian
//!   C 100% read                      zipfian
//!   D  95% read,  5% insert          latest
//!   E  95% scan,  5% insert          zipfian, 1-100 records a scan
//!   F  50% read, 50% read-modify-write  zipfian
//!
//! The key is an integer, 1 to the record count, and an insert takes the
//! next: YCSB's `insertorder=ordered` without the `user` prefix, so that
//! every system keys by the integer it keys best by -- fenecdb's `id`,
//! SQLite's `INTEGER PRIMARY KEY`, a `bigint` primary key, MongoDB's `_id`.
//! A read of a key not yet acknowledged is never asked: the inserts in
//! flight are published, and the latest key is the one below the lowest.
//!
//! The systems, each at its documented best with the key indexed:
//!
//!   * `fenec`: fenec-core in process, reads under the shared lock and
//!     writes under the exclusive one, as the server takes them. Durable is
//!     a flush after each write and its fsync outside the lock; buffered a
//!     flush every 250 ms, as `--sync 250` does. A `Compactor` compacts the
//!     file beside the database once half of it is dead, as the native
//!     library and the server do unless told not to (`--no-auto-compact`
//!     for neither);
//!   * `sqlite`: SQLite through rusqlite, a connection a thread, WAL,
//!     `synchronous=FULL` with `fullfsync` or `synchronous=NORMAL`, the file
//!     mapped (`mmap_size`) as fenecdb's is;
//!   * `server`: fenec-server over HTTP, `POST /query` with parameters on a
//!     kept-alive connection a thread, `--sync always` or `--sync 250`;
//!     `server-docker` the same server in a container, to show what
//!     Docker's virtual machine adds to the two below;
//!   * `pg`: PostgreSQL 17 in a container, prepared statements,
//!     `synchronous_commit` on or off;
//!   * `mongo`: MongoDB 8 in a container, the official driver, `j: true`
//!     or `w: 1, j: false`.
//!
//! The load is timed apart, batches of 1 000 records from one client, each
//! system's documented bulk path. Each cell runs a 5 s warm-up and 30 s
//! measured; before each one the harness waits for a CPU probe to run at
//! the speed it ran at idle (`cool`), since the fanless M1 Air this was
//! measured on slows to a third under minutes of load on every core. A
//! latency is counted in a log-linear histogram of 64 steps an octave
//! (under 1.6% off), the longest kept exact.
//!
//! Each cell's line goes to a TSV file (`--out`); `ycsb report <file>`
//! prints the median of the runs of each cell.
//!
//! `--verify <file>` logs every operation and what it was answered, and
//! after each cell holds every read, scan and write to what was written
//! (`check`), the verdict a line of `<file>`. A read under concurrent
//! writes is right if it returns any value a write that could have been
//! the last one left, so judging it needs every write's span, not a
//! sample: each thread logs into its own `Vec` and the check runs once
//! the cell has ended. A client keeps its last answer either way, so the
//! timed path is the one measured without `--verify`; a verifying run
//! times a read-modify-write's digests too and its figures are not the
//! published ones.

#[path = "../http.rs"]
mod http;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use fenec_core::prelude::*;

const FIELDS: usize = 10;
const FIELD_LEN: usize = 100;
const SEED: u64 = 0x5943_5342; // "YCSB"
const LOAD_BATCH: usize = 1_000;
const PG_URL: &str = "host=127.0.0.1 port=55433 user=postgres password=fenec dbname=ycsb";
const MONGO_URL: &str = "mongodb://127.0.0.1:27018/?maxPoolSize=64";

// ------------------------------------------------------------ randomness

/// xoshiro256** seeded through splitmix64.
#[derive(Clone)]
struct Rng([u64; 4]);

impl Rng {
    fn new(seed: u64) -> Rng {
        let mut x = seed;
        let mut next = || {
            x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = x;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        Rng([next(), next(), next(), next()])
    }
    fn next(&mut self) -> u64 {
        let s = &mut self.0;
        let out = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        out
    }
    /// Uniform in [0, 1).
    fn f64(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn below(&mut self, n: u64) -> u64 {
        ((self.next() as u128 * n as u128) >> 64) as u64
    }
    /// A field's value: 100 random letters and digits, as YCSB's random
    /// bytes are printable -- and none a JSON or SQL string escapes.
    fn value(&mut self, out: &mut String) {
        const ABC: &[u8; 62] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        out.clear();
        let mut bits = 0u64;
        for i in 0..FIELD_LEN {
            if i % 10 == 0 {
                bits = self.next();
            }
            out.push(ABC[(bits & 63) as usize % 62] as char);
            bits >>= 6;
        }
    }
}

const ZIPF: f64 = 0.99;

fn zeta(from: u64, to: u64, theta: f64, start: f64) -> f64 {
    let mut sum = start;
    for i in from..to {
        sum += 1.0 / ((i + 1) as f64).powf(theta);
    }
    sum
}

/// YCSB's `ZipfianGenerator` (Gray et al., "Quickly generating
/// billion-record synthetic databases"), its item count allowed to grow as
/// YCSB's `nextLong(itemcount)` lets it: the zeta sum is carried on.
#[derive(Clone)]
struct Zipfian {
    items: u64,
    zetan: f64,
    zeta2: f64,
    alpha: f64,
    eta: f64,
}

impl Zipfian {
    fn new(items: u64) -> Zipfian {
        Zipfian::with_zeta(items, zeta(0, items, ZIPF, 0.0))
    }
    fn with_zeta(items: u64, zetan: f64) -> Zipfian {
        let zeta2 = zeta(0, 2, ZIPF, 0.0);
        let mut z = Zipfian {
            items,
            zetan,
            zeta2,
            alpha: 1.0 / (1.0 - ZIPF),
            eta: 0.0,
        };
        z.eta = z.eta_for();
        z
    }
    fn eta_for(&self) -> f64 {
        (1.0 - (2.0 / self.items as f64).powf(1.0 - ZIPF)) / (1.0 - self.zeta2 / self.zetan)
    }
    /// An item in [0, items).
    fn next(&mut self, rng: &mut Rng, items: u64) -> u64 {
        if items > self.items {
            self.zetan = zeta(self.items, items, ZIPF, self.zetan);
            self.items = items;
            self.eta = self.eta_for();
        }
        let u = rng.f64();
        let uz = u * self.zetan;
        if uz < 1.0 {
            return 0;
        }
        if uz < 1.0 + 0.5f64.powf(ZIPF) {
            return 1;
        }
        let v = (self.items as f64 * (self.eta * u - self.eta + 1.0).powf(self.alpha)) as u64;
        v.min(self.items - 1)
    }
}

fn fnv64(mut v: u64) -> u64 {
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    for _ in 0..8 {
        h ^= v & 0xff;
        v >>= 8;
        h = h.wrapping_mul(1_099_511_628_211);
    }
    (h as i64).unsigned_abs()
}

/// YCSB's `ScrambledZipfianGenerator`: a zipfian over ten billion items,
/// with the zeta YCSB precomputes for them, hashed onto the key space.
#[derive(Clone)]
struct Scrambled {
    z: Zipfian,
}

const SCRAMBLED_ITEMS: u64 = 10_000_000_000;
const SCRAMBLED_ZETAN: f64 = 26.469_028_201_783_02;

impl Scrambled {
    fn new() -> Scrambled {
        Scrambled {
            z: Zipfian::with_zeta(SCRAMBLED_ITEMS + 1, SCRAMBLED_ZETAN),
        }
    }
    fn next(&mut self, rng: &mut Rng, items: u64) -> u64 {
        let v = self.z.next(rng, SCRAMBLED_ITEMS + 1);
        fnv64(v) % items
    }
}

// --------------------------------------------------------------- workload

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Op {
    Read,
    Update,
    Insert,
    Scan,
    Rmw,
}

const OPS: [Op; 5] = [Op::Read, Op::Update, Op::Insert, Op::Scan, Op::Rmw];

impl Op {
    fn name(self) -> &'static str {
        match self {
            Op::Read => "READ",
            Op::Update => "UPDATE",
            Op::Insert => "INSERT",
            Op::Scan => "SCAN",
            Op::Rmw => "RMW",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Dist {
    Zipfian,
    Latest,
}

struct Workload {
    name: char,
    /// Each op's share, summing to 1.
    mix: &'static [(Op, f64)],
    dist: Dist,
}

const WORKLOADS: [Workload; 6] = [
    Workload {
        name: 'A',
        mix: &[(Op::Read, 0.5), (Op::Update, 0.5)],
        dist: Dist::Zipfian,
    },
    Workload {
        name: 'B',
        mix: &[(Op::Read, 0.95), (Op::Update, 0.05)],
        dist: Dist::Zipfian,
    },
    Workload {
        name: 'C',
        mix: &[(Op::Read, 1.0)],
        dist: Dist::Zipfian,
    },
    Workload {
        name: 'D',
        mix: &[(Op::Read, 0.95), (Op::Insert, 0.05)],
        dist: Dist::Latest,
    },
    Workload {
        name: 'E',
        mix: &[(Op::Scan, 0.95), (Op::Insert, 0.05)],
        dist: Dist::Zipfian,
    },
    Workload {
        name: 'F',
        mix: &[(Op::Read, 0.5), (Op::Rmw, 0.5)],
        dist: Dist::Zipfian,
    },
];

/// The keys handed out to inserts and those acknowledged. `last` is the
/// highest key every key at or below which is in the database: the next
/// key less one, or the key below the lowest insert still in flight.
struct Keys {
    next: AtomicU64,
    in_flight: Vec<AtomicU64>,
}

impl Keys {
    fn new(records: u64, threads: usize) -> Keys {
        Keys {
            next: AtomicU64::new(records + 1),
            in_flight: (0..threads).map(|_| AtomicU64::new(u64::MAX)).collect(),
        }
    }
    fn last(&self) -> u64 {
        let mut low = self.next.load(Ordering::SeqCst);
        for f in &self.in_flight {
            low = low.min(f.load(Ordering::SeqCst));
        }
        low - 1
    }
}

/// One thread's way of choosing keys.
struct Chooser {
    dist: Dist,
    scrambled: Scrambled,
    latest: Zipfian,
    /// The key space a scrambled draw covers: the records, and room for
    /// the inserts a cell expects (YCSB's `insertcount + expectednewkeys`).
    space: u64,
}

impl Chooser {
    fn next(&mut self, rng: &mut Rng, keys: &Keys) -> u64 {
        let last = keys.last();
        match self.dist {
            // YCSB's `SkewedLatestGenerator`: the latest key, less a
            // zipfian draw over the keys there are.
            Dist::Latest => last - self.latest.next(rng, last),
            // A key not acknowledged yet is drawn again, as YCSB does.
            Dist::Zipfian => loop {
                let k = self.scrambled.next(rng, self.space) + 1;
                if k <= last {
                    return k;
                }
            },
        }
    }
}

// -------------------------------------------------------------- histogram

/// Log-linear: values under 128 ns exact, then 64 steps an octave.
struct Hist {
    counts: Vec<u64>,
    n: u64,
    max: u64,
}

const SUB: u32 = 6;

impl Hist {
    fn new() -> Hist {
        Hist {
            counts: vec![0; 64 << SUB],
            n: 0,
            max: 0,
        }
    }
    fn index(v: u64) -> usize {
        if v < (2 << SUB) {
            return v as usize;
        }
        let shift = 63 - v.leading_zeros() - SUB;
        ((shift as u64) << SUB) as usize + (v >> shift) as usize
    }
    /// The middle of the bucket `i` covers.
    fn value(i: usize) -> f64 {
        if i < (2 << SUB) {
            return i as f64;
        }
        let shift = (i >> SUB) as u32 - 1;
        let m = (i - ((shift as usize) << SUB)) as u64;
        ((m << shift) as f64) + ((1u64 << shift) as f64 - 1.0) / 2.0
    }
    fn record(&mut self, ns: u64) {
        self.counts[Hist::index(ns)] += 1;
        self.n += 1;
        self.max = self.max.max(ns);
    }
    fn merge(&mut self, o: &Hist) {
        for (a, b) in self.counts.iter_mut().zip(&o.counts) {
            *a += b;
        }
        self.n += o.n;
        self.max = self.max.max(o.max);
    }
    /// The `p` quantile in microseconds.
    fn pct(&self, p: f64) -> f64 {
        if self.n == 0 {
            return 0.0;
        }
        let want = ((self.n as f64 * p).ceil() as u64).max(1);
        let mut seen = 0;
        for (i, c) in self.counts.iter().enumerate() {
            seen += c;
            if seen >= want {
                return (Hist::value(i) / 1e3).min(self.max as f64 / 1e3);
            }
        }
        self.max as f64 / 1e3
    }
}

// ---------------------------------------------------------------- clients

/// What a client thread asks of a system. Each checks its answer enough to
/// know the work was done -- a read found a record, a scan its rows -- and
/// keeps what it was answered, which `--verify` reads after the timed span
/// (`got`, `affected`) and holds to what was written (`Check`).
trait Client: Send {
    fn read(&mut self, key: u64);
    fn update(&mut self, key: u64, field: usize, value: &str);
    fn insert(&mut self, key: u64, values: &[String]);
    fn scan(&mut self, key: u64, len: u64);
    /// The rows the last read or scan was answered, in the order they came:
    /// each one's key and its fields' digests (`digest`, 0 for a field the
    /// row did not hold).
    fn got(&mut self) -> Vec<(u64, [u64; FIELDS])>;
    /// The records the last write's answer said it wrote, where the client
    /// does not hold it to 1 itself.
    fn affected(&mut self) -> u64 {
        1
    }
    /// The least the system can be asked over its wire -- an empty query,
    /// a health check -- or false in process: what the network alone
    /// costs a request, printed beside the cells.
    fn ping(&mut self) -> bool {
        false
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Durable,
    Buffered,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Durable => "durable",
            Mode::Buffered => "buffered",
        }
    }
}

/// A system under test: it loads the records, is set to a mode, and hands
/// out a client a thread.
trait System {
    fn name(&self) -> &'static str;
    fn load(&mut self, records: u64);
    fn set_mode(&mut self, mode: Mode);
    fn client(&self) -> Box<dyn Client>;
    /// The bytes the data takes on disk, where they can be read: an
    /// engine that writes a record again elsewhere grows by every update.
    fn size(&self) -> Option<u64> {
        None
    }
}

/// A record's values: the same for every system, drawn from its key.
fn record_values(key: u64) -> Vec<String> {
    let mut rng = Rng::new(SEED ^ key.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    (0..FIELDS)
        .map(|_| {
            let mut s = String::with_capacity(FIELD_LEN);
            rng.value(&mut s);
            s
        })
        .collect()
}

/// A field's value as `--verify` keeps it: FNV-1a over its bytes. The
/// values are 100 random characters, so two that differ share one with a
/// chance of 2^-64 and a wrong value never passes for the right one.
fn digest(b: &[u8]) -> u64 {
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x100_0000_01B3);
    }
    h
}

/// A record's fields in one: what a scan's row is held to.
fn combine(fields: &[u64; FIELDS]) -> u64 {
    fields.iter().fold(0x9E37_79B9_7F4A_7C15, |h, d| {
        (h ^ d).wrapping_mul(0x100_0000_01B3).rotate_left(29)
    })
}

fn base_digests(key: u64) -> [u64; FIELDS] {
    let mut out = [0; FIELDS];
    for (o, v) in out.iter_mut().zip(record_values(key)) {
        *o = digest(v.as_bytes());
    }
    out
}

// ---- fenecdb in process

struct FenecLocal {
    dir: PathBuf,
    db: Option<Arc<RwLock<Database>>>,
    durable: bool,
    syncer: Option<(Arc<AtomicBool>, std::thread::JoinHandle<()>)>,
    /// What an app's database has unless it opts out: the file compacted
    /// beside the queries once half of it is dead.
    auto_compact: bool,
    compactor: Option<fenec_core::engine::Compactor>,
}

fn stmt(src: &str) -> Statement {
    fenec_ql::parse_one(src).unwrap_or_else(|e| panic!("{src}: {e:?}"))
}

const MAX_SCAN: u64 = 100;

/// E's scan: the records from a key on, in key order -- a scan with an
/// `id` floor walks the id index from there and stops at the limit.
fn scan_text(len: u64) -> String {
    format!("get usertable where id >= $1 limit {len}")
}

fn insert_text() -> String {
    let fields: Vec<String> = (0..FIELDS)
        .map(|i| format!("field{i}: ${}", i + 2))
        .collect();
    format!("insert usertable {{id: $1, {}}}", fields.join(", "))
}

impl FenecLocal {
    fn new(dir: &Path, auto_compact: bool) -> FenecLocal {
        FenecLocal {
            dir: dir.to_path_buf(),
            db: None,
            durable: false,
            syncer: None,
            auto_compact,
            compactor: None,
        }
    }
    fn stop_syncer(&mut self) {
        if let Some((stop, h)) = self.syncer.take() {
            stop.store(true, Ordering::Relaxed);
            h.join().unwrap();
        }
    }
}

impl Drop for FenecLocal {
    fn drop(&mut self) {
        self.stop_syncer();
    }
}

impl System for FenecLocal {
    fn name(&self) -> &'static str {
        "fenec"
    }
    fn size(&self) -> Option<u64> {
        std::fs::metadata(self.dir.join("ycsb.fenec"))
            .ok()
            .map(|m| m.len())
    }
    fn load(&mut self, records: u64) {
        // A load starts afresh: the syncer holds the database it replaces.
        self.stop_syncer();
        self.compactor = None;
        self.db = None;
        let path = self.dir.join("ycsb.fenec");
        let _ = std::fs::remove_file(&path);
        let mut db = fenec_core::fs::open(&path).unwrap();
        let fields: Vec<String> = (0..FIELDS).map(|i| format!("field{i} text")).collect();
        db.execute(&stmt(&format!(
            "create collection usertable ({})",
            fields.join(", ")
        )))
        .unwrap();
        let mut key = 1;
        while key <= records {
            let end = (key + LOAD_BATCH as u64).min(records + 1);
            let docs = (key..end)
                .map(|k| {
                    let mut d = vec![("id".to_string(), Expr::Lit(Value::Int(k as i64)))];
                    for (i, v) in record_values(k).into_iter().enumerate() {
                        d.push((format!("field{i}"), Expr::Lit(Value::Text(v))));
                    }
                    d
                })
                .collect();
            db.execute(&Statement::Put {
                collection: "usertable".into(),
                docs,
                insert: true,
                if_absent: false,
                require: None,
            })
            .unwrap();
            key = end;
        }
        db.sync().unwrap();
        let db = Arc::new(RwLock::new(db));
        if self.auto_compact {
            self.compactor =
                Some(fenec_core::engine::Compactor::start(&db, Default::default()).unwrap());
        }
        self.db = Some(db);
    }
    fn set_mode(&mut self, mode: Mode) {
        self.stop_syncer();
        self.durable = mode == Mode::Durable;
        let db = Arc::clone(self.db.as_ref().unwrap());
        {
            let d = db.write().unwrap().flush().unwrap();
            if let Some(d) = d {
                d().unwrap();
            }
        }
        if mode == Mode::Buffered {
            // `--sync 250`: what was written goes to disk every 250 ms,
            // the fsync outside the lock, as the server's syncer runs it.
            let stop = Arc::new(AtomicBool::new(false));
            let s = Arc::clone(&stop);
            let h = std::thread::spawn(move || {
                while !s.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(250));
                    let d = db.write().unwrap().flush().unwrap();
                    if let Some(d) = d {
                        d().unwrap();
                    }
                }
            });
            self.syncer = Some((stop, h));
        }
    }
    fn client(&self) -> Box<dyn Client> {
        Box::new(FenecLocalClient {
            db: Arc::clone(self.db.as_ref().unwrap()),
            durable: self.durable,
            get: stmt("get usertable where id = $1"),
            // A limit is a number in the text, not a parameter: a
            // statement for each length a scan can have.
            scan: (1..=MAX_SCAN).map(|n| stmt(&scan_text(n))).collect(),
            set: (0..FIELDS)
                .map(|i| stmt(&format!("set usertable {{field{i}: $2}} where id = $1")))
                .collect(),
            insert: stmt(&insert_text()),
            args: Vec::with_capacity(FIELDS + 1),
            last: None,
        })
    }
}

struct FenecLocalClient {
    db: Arc<RwLock<Database>>,
    durable: bool,
    get: Statement,
    scan: Vec<Statement>,
    set: Vec<Statement>,
    insert: Statement,
    args: Vec<Value>,
    /// The last read's or scan's answer, kept rather than dropped.
    last: Option<Response>,
}

impl FenecLocalClient {
    fn write(&mut self, which: Option<usize>) {
        let durability = {
            let mut g = self.db.write().unwrap();
            let stmt = match which {
                Some(f) => &self.set[f],
                None => &self.insert,
            };
            let out = g.execute_with(stmt, &self.args).unwrap();
            assert!(matches!(out, Response::Affected(1)), "{out:?}");
            match self.durable {
                true => g.flush().unwrap(),
                false => None,
            }
        };
        if let Some(d) = durability {
            d().unwrap();
        }
    }
}

impl Client for FenecLocalClient {
    fn read(&mut self, key: u64) {
        let out = self
            .db
            .read()
            .unwrap()
            .query(&self.get, &[Value::Int(key as i64)])
            .unwrap();
        let rows = out.rows().unwrap();
        assert!(rows.rows.len() == 1 && rows.rows[0].values.len() > FIELDS);
        self.last = Some(out);
    }
    fn update(&mut self, key: u64, field: usize, value: &str) {
        self.args.clear();
        self.args.push(Value::Int(key as i64));
        self.args.push(Value::Text(value.to_string()));
        self.write(Some(field));
    }
    fn insert(&mut self, key: u64, values: &[String]) {
        self.args.clear();
        self.args.push(Value::Int(key as i64));
        self.args
            .extend(values.iter().map(|v| Value::Text(v.clone())));
        self.write(None);
    }
    fn scan(&mut self, key: u64, len: u64) {
        let out = self
            .db
            .read()
            .unwrap()
            .query(&self.scan[len as usize - 1], &[Value::Int(key as i64)])
            .unwrap();
        let rows = out.rows().unwrap();
        assert!(!rows.rows.is_empty() && rows.rows[0].id == key);
        assert!(rows.rows.windows(2).all(|w| w[0].id < w[1].id));
        self.last = Some(out);
    }
    fn got(&mut self) -> Vec<(u64, [u64; FIELDS])> {
        let Some(rs) = self.last.as_ref().and_then(|o| o.rows()) else {
            return Vec::new();
        };
        let at: Vec<Option<usize>> = (0..FIELDS)
            .map(|i| rs.columns.iter().position(|c| *c == format!("field{i}")))
            .collect();
        rs.rows
            .iter()
            .map(|r| {
                let mut d = [0; FIELDS];
                for (o, a) in d.iter_mut().zip(&at) {
                    if let Some(Value::Text(t)) = a.and_then(|a| r.values.get(a)) {
                        *o = digest(t.as_bytes());
                    }
                }
                (r.id, d)
            })
            .collect()
    }
}

// ---- SQLite

struct Sqlite {
    path: PathBuf,
    durable: bool,
}

fn sqlite_conn(path: &Path, durable: bool) -> rusqlite::Connection {
    let c = rusqlite::Connection::open(path).unwrap();
    c.pragma_update(None, "journal_mode", "WAL").unwrap();
    c.pragma_update(None, "synchronous", if durable { "FULL" } else { "NORMAL" })
        .unwrap();
    c.pragma_update(None, "fullfsync", durable).unwrap();
    // The file read through the page cache's mapping, as fenecdb reads
    // its own, and 64 MB of page cache a connection.
    c.pragma_update(None, "mmap_size", 8i64 << 30).unwrap();
    c.pragma_update(None, "cache_size", -65_536).unwrap();
    c.busy_timeout(Duration::from_secs(30)).unwrap();
    c
}

impl System for Sqlite {
    fn name(&self) -> &'static str {
        "sqlite"
    }
    fn size(&self) -> Option<u64> {
        let len = |ext: &str| {
            std::fs::metadata(format!("{}{ext}", self.path.display()))
                .map(|m| m.len())
                .unwrap_or(0)
        };
        Some(len("") + len("-wal"))
    }
    fn load(&mut self, records: u64) {
        for ext in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{ext}", self.path.display()));
        }
        let mut c = sqlite_conn(&self.path, false);
        let fields: Vec<String> = (0..FIELDS).map(|i| format!("field{i} text")).collect();
        c.execute(
            &format!(
                "create table usertable (ycsb_key integer primary key, {})",
                fields.join(", ")
            ),
            [],
        )
        .unwrap();
        let mut key = 1;
        while key <= records {
            let end = (key + LOAD_BATCH as u64).min(records + 1);
            let tx = c.transaction().unwrap();
            {
                let mut ins = tx.prepare_cached(&sqlite_insert()).unwrap();
                for k in key..end {
                    let v = record_values(k);
                    ins.execute(rusqlite::params![
                        k as i64, v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7], v[8], v[9]
                    ])
                    .unwrap();
                }
            }
            tx.commit().unwrap();
            key = end;
        }
        c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    }
    fn set_mode(&mut self, mode: Mode) {
        self.durable = mode == Mode::Durable;
    }
    fn client(&self) -> Box<dyn Client> {
        Box::new(SqliteClient {
            c: sqlite_conn(&self.path, self.durable),
            last: Vec::new(),
        })
    }
}

fn sqlite_insert() -> String {
    let fields: Vec<String> = (0..FIELDS).map(|i| format!("field{i}")).collect();
    let marks: Vec<String> = (0..=FIELDS).map(|i| format!("?{}", i + 1)).collect();
    format!(
        "insert into usertable (ycsb_key, {}) values ({})",
        fields.join(", "),
        marks.join(", ")
    )
}

struct SqliteClient {
    c: rusqlite::Connection,
    last: Vec<(i64, [String; FIELDS])>,
}

fn sqlite_row(r: &rusqlite::Row) -> rusqlite::Result<(i64, [String; FIELDS])> {
    let mut out: [String; FIELDS] = Default::default();
    for (i, o) in out.iter_mut().enumerate() {
        *o = r.get(i + 1)?;
    }
    Ok((r.get(0)?, out))
}

/// Rows read into strings, as every client reads them, to their digests.
fn digests_of(rows: &[(i64, [String; FIELDS])]) -> Vec<(u64, [u64; FIELDS])> {
    rows.iter()
        .map(|(k, f)| {
            let mut d = [0; FIELDS];
            for (o, v) in d.iter_mut().zip(f) {
                *o = digest(v.as_bytes());
            }
            (*k as u64, d)
        })
        .collect()
}

impl Client for SqliteClient {
    fn read(&mut self, key: u64) {
        let mut s = self
            .c
            .prepare_cached("select * from usertable where ycsb_key = ?1")
            .unwrap();
        let row = s.query_row([key as i64], sqlite_row).unwrap();
        self.last.clear();
        self.last.push(row);
    }
    fn update(&mut self, key: u64, field: usize, value: &str) {
        let mut s = self
            .c
            .prepare_cached(&format!(
                "update usertable set field{field} = ?2 where ycsb_key = ?1"
            ))
            .unwrap();
        assert_eq!(s.execute(rusqlite::params![key as i64, value]).unwrap(), 1);
    }
    fn insert(&mut self, key: u64, v: &[String]) {
        let mut s = self.c.prepare_cached(&sqlite_insert()).unwrap();
        s.execute(rusqlite::params![
            key as i64, v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7], v[8], v[9]
        ])
        .unwrap();
    }
    fn scan(&mut self, key: u64, len: u64) {
        let mut s = self
            .c
            .prepare_cached(
                "select * from usertable where ycsb_key >= ?1 order by ycsb_key limit ?2",
            )
            .unwrap();
        let rows: Vec<(i64, [String; FIELDS])> = s
            .query_map([key as i64, len as i64], sqlite_row)
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert!(!rows.is_empty());
        self.last = rows;
    }
    fn got(&mut self) -> Vec<(u64, [u64; FIELDS])> {
        digests_of(&self.last)
    }
}

// ---- fenec-server over HTTP

struct Server {
    /// Native: the file the server is started over; in Docker the volume.
    dir: PathBuf,
    docker: bool,
    port: u16,
    proc: Option<http::Server>,
    image: String,
    /// `--auto-compact off` when the run says so; the server's default
    /// otherwise.
    auto_compact: bool,
}

const DOCKER_SERVER: &str = "fenecycsb-server";
const DOCKER_VOLUME: &str = "fenecycsb-data";

impl Server {
    fn start(&mut self, sync: &str) {
        self.stop();
        if !self.docker {
            let file = self.dir.join("ycsb-server.fenec");
            let mut args = vec!["--sync", sync];
            if !self.auto_compact {
                args.extend(["--auto-compact", "off"]);
            }
            self.proc = Some(http::start_fenec(&file, self.port, "ycsb", &args));
            return;
        }
        let st = std::process::Command::new("docker")
            .args(["run", "-d", "--name", DOCKER_SERVER])
            .args(["-p", &format!("127.0.0.1:{}:8080", self.port)])
            .args(["-v", &format!("{DOCKER_VOLUME}:/data")])
            .arg(&self.image)
            .args(["--http", "0.0.0.0:8080", "--insecure", "--no-checkpoint"])
            .args(["--file", "/data/ycsb.fenec", "--sync", sync])
            .args(match self.auto_compact {
                true => &[][..],
                false => &["--auto-compact", "off"][..],
            })
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(st.success(), "docker run {}", self.image);
        let until = Instant::now() + Duration::from_secs(120);
        loop {
            if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", self.port)) {
                // Docker's proxy takes the connection before the server
                // listens: ask for something.
                let _ = s.set_read_timeout(Some(Duration::from_secs(1)));
                let _ = s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n");
                let mut b = [0u8; 16];
                if matches!(std::io::Read::read(&mut s, &mut b), Ok(n) if n > 0) {
                    break;
                }
            }
            assert!(Instant::now() < until, "the container did not start");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    /// Stopped as a supervisor stops it, so that the writes of the last
    /// `--sync 250` interval are on disk when it starts again.
    fn stop(&mut self) {
        if let Some(p) = self.proc.take() {
            p.terminate();
        }
        if self.docker {
            let _ = std::process::Command::new("docker")
                .args(["stop", "-t", "60", DOCKER_SERVER])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", DOCKER_SERVER])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
    fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
        if self.docker {
            remove_volume();
        }
    }
}

/// Removes the server's volume, waiting for the container that held it to
/// go: started with `--rm`, a container was still being removed when
/// `docker rm -f` returned, its volume stayed, and the next load found the
/// collection made (409) -- or a GB or more stayed in the VM after a run.
/// The container is started without it now, which makes the removal
/// synchronous; the wait stays for a daemon that answers late.
fn remove_volume() {
    for _ in 0..40 {
        let gone = std::process::Command::new("docker")
            .args(["volume", "rm", "-f", DOCKER_VOLUME])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if gone {
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    panic!("the volume {DOCKER_VOLUME} is still in use");
}

impl System for Server {
    fn name(&self) -> &'static str {
        if self.docker {
            "server-docker"
        } else {
            "server"
        }
    }
    fn size(&self) -> Option<u64> {
        // The container's file is in the VM's volume, out of reach here.
        match self.docker {
            true => None,
            false => std::fs::metadata(self.dir.join("ycsb-server.fenec"))
                .ok()
                .map(|m| m.len()),
        }
    }
    fn load(&mut self, records: u64) {
        if self.docker {
            self.stop();
            remove_volume();
        } else {
            let _ = std::fs::remove_file(self.dir.join("ycsb-server.fenec"));
        }
        self.start("250");
        let mut c = http::Http::connect(&self.addr());
        let fields: Vec<String> = (0..FIELDS).map(|i| format!("field{i} text")).collect();
        c.query(
            &format!("create collection usertable ({})", fields.join(", ")),
            "",
        );
        let mut key = 1;
        let mut body = String::new();
        while key <= records {
            let end = (key + LOAD_BATCH as u64).min(records + 1);
            body.clear();
            body.push('[');
            for k in key..end {
                if k > key {
                    body.push(',');
                }
                write!(body, "{{\"id\":{k}").unwrap();
                for (i, v) in record_values(k).iter().enumerate() {
                    write!(body, ",\"field{i}\":\"{v}\"").unwrap();
                }
                body.push('}');
            }
            body.push(']');
            c.post("/usertable", "application/json", body.as_bytes());
            key = end;
        }
    }
    fn set_mode(&mut self, mode: Mode) {
        // The server is started again over its file with the policy.
        self.start(match mode {
            Mode::Durable => "always",
            Mode::Buffered => "250",
        });
    }
    fn client(&self) -> Box<dyn Client> {
        Box::new(ServerClient {
            c: http::Http::connect(&self.addr()),
            scan: (1..=MAX_SCAN).map(scan_text).collect(),
            params: String::with_capacity(2048),
            insert: insert_text(),
            set: (0..FIELDS)
                .map(|i| format!("set usertable {{field{i}: $2}} where id = $1"))
                .collect(),
        })
    }
}

struct ServerClient {
    c: http::Http,
    scan: Vec<String>,
    params: String,
    insert: String,
    set: Vec<String>,
}

fn expect(body: &[u8], what: &[u8]) {
    assert!(
        body.windows(what.len()).any(|w| w == what),
        "{}",
        String::from_utf8_lossy(&body[..body.len().min(300)])
    );
}

impl Client for ServerClient {
    fn ping(&mut self) -> bool {
        self.c.get("/_health");
        true
    }
    fn read(&mut self, key: u64) {
        let out = self
            .c
            .query("get usertable where id = $1", &key.to_string());
        expect(out, b"\"field9\"");
    }
    fn update(&mut self, key: u64, field: usize, value: &str) {
        self.params.clear();
        write!(self.params, "{key},\"{value}\"").unwrap();
        let out = self.c.query(&self.set[field], &self.params);
        expect(out, b"1");
    }
    fn insert(&mut self, key: u64, values: &[String]) {
        self.params.clear();
        write!(self.params, "{key}").unwrap();
        for v in values {
            write!(self.params, ",\"{v}\"").unwrap();
        }
        self.c.query(&self.insert, &self.params);
    }
    fn scan(&mut self, key: u64, len: u64) {
        let out = self.c.query(&self.scan[len as usize - 1], &key.to_string());
        expect(out, b"\"field9\"");
    }
    fn got(&mut self) -> Vec<(u64, [u64; FIELDS])> {
        json_rows(self.c.body())
    }
    fn affected(&mut self) -> u64 {
        // `{"affected":1}`, all of it.
        let b = self.c.body();
        b.strip_prefix(b"{\"affected\":")
            .and_then(|r| r.strip_suffix(b"}"))
            .and_then(|n| std::str::from_utf8(n).ok()?.parse().ok())
            .unwrap_or(u64::MAX)
    }
}

/// The rows of an answer to `get`, `[{"id":1,"field0":"..",..},..]`: each
/// one's `id` and its fields' digests. The values are letters and digits,
/// so a string ends at the next quote; anything else in the body is no row.
fn json_rows(body: &[u8]) -> Vec<(u64, [u64; FIELDS])> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(open) = body[i..].iter().position(|&b| b == b'{') {
        let start = i + open + 1;
        let Some(close) = body[start..].iter().position(|&b| b == b'}') else {
            break;
        };
        let obj = &body[start..start + close];
        i = start + close + 1;
        let mut key = u64::MAX;
        let mut d = [0; FIELDS];
        for pair in obj.split(|&b| b == b',') {
            let Some(colon) = pair.iter().position(|&b| b == b':') else {
                continue;
            };
            let (name, value) = (&pair[..colon], &pair[colon + 1..]);
            let name = name.strip_prefix(b"\"").and_then(|n| n.strip_suffix(b"\""));
            match name {
                Some(b"id") => {
                    key = std::str::from_utf8(value)
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(u64::MAX)
                }
                Some(n) if n.starts_with(b"field") => {
                    let f: Option<usize> = std::str::from_utf8(&n[5..])
                        .ok()
                        .and_then(|f| f.parse().ok());
                    let v = value
                        .strip_prefix(b"\"")
                        .and_then(|v| v.strip_suffix(b"\""));
                    if let (Some(f), Some(v)) = (f, v) {
                        if f < FIELDS {
                            d[f] = digest(v);
                        }
                    }
                }
                _ => {}
            }
        }
        out.push((key, d));
    }
    out
}

// ---- PostgreSQL

struct Pg {
    url: String,
    durable: bool,
    _container: Option<Container>,
}

/// PostgreSQL 17 as its docs tune it for this machine's Docker VM (3.8 GB):
/// a quarter of the memory as shared buffers, the rest left to the page
/// cache it counts on, and checkpoints spaced so the load is not one. Its
/// data in a volume, the VM's own file system, not the image's layers.
const PG_RUN: &[&str] = &[
    "-e",
    "POSTGRES_PASSWORD=fenec",
    "-e",
    "POSTGRES_DB=ycsb",
    "-p",
    "127.0.0.1:55433:5432",
    "--shm-size=1g",
    "-v",
    "fenecycsb-pg:/var/lib/postgresql/data",
    "postgres:17",
    "-c",
    "shared_buffers=1GB",
    "-c",
    "effective_cache_size=2GB",
    "-c",
    "max_wal_size=4GB",
    "-c",
    "max_connections=200",
];

/// MongoDB 8 as it comes: WiredTiger's cache is half the memory less 1 GB.
const MONGO_RUN: &[&str] = &[
    "-p",
    "127.0.0.1:27018:27017",
    "-v",
    "fenecycsb-mongo:/data/db",
    "mongo:8",
];

/// A container started for a system's turn, removed with its volume after,
/// so that one system's memory is not in the VM beside another's.
struct Container(String);

impl Container {
    fn start(name: &str, args: &[&str]) -> Container {
        let c = Container(format!("fenecycsb-{name}"));
        c.remove();
        let st = std::process::Command::new("docker")
            .args(["run", "-d", "--name", &c.0])
            .args(args)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(st.success(), "docker run {args:?}");
        c
    }
    fn remove(&self) {
        let quiet = |args: &[&str]| {
            let _ = std::process::Command::new("docker")
                .args(args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        };
        quiet(&["rm", "-f", "-v", &self.0]);
        quiet(&["volume", "rm", "-f", &self.0]);
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        self.remove();
    }
}

impl System for Pg {
    fn name(&self) -> &'static str {
        "pg"
    }
    fn size(&self) -> Option<u64> {
        // The table, its TOAST and its primary key; the WAL apart.
        let mut c = postgres::Client::connect(&self.url, postgres::NoTls).ok()?;
        let n: i64 = c
            .query_one("SELECT pg_total_relation_size('usertable')", &[])
            .ok()?
            .get(0);
        Some(n as u64)
    }
    fn load(&mut self, records: u64) {
        let mut c = postgres::Client::connect(&self.url, postgres::NoTls).unwrap();
        let fields: Vec<String> = (0..FIELDS).map(|i| format!("field{i} text")).collect();
        c.batch_execute(&format!(
            "DROP TABLE IF EXISTS usertable;
             CREATE TABLE usertable (ycsb_key bigint PRIMARY KEY, {});",
            fields.join(", ")
        ))
        .unwrap();
        // COPY, PostgreSQL's bulk path, a transaction of 1 000 rows each.
        let mut key = 1;
        let mut line = String::new();
        while key <= records {
            let end = (key + LOAD_BATCH as u64).min(records + 1);
            let mut w = c.copy_in("COPY usertable FROM STDIN").unwrap();
            for k in key..end {
                line.clear();
                write!(line, "{k}").unwrap();
                for v in record_values(k) {
                    write!(line, "\t{v}").unwrap();
                }
                line.push('\n');
                w.write_all(line.as_bytes()).unwrap();
            }
            w.finish().unwrap();
            key = end;
        }
        c.batch_execute("VACUUM ANALYZE usertable").unwrap();
        c.batch_execute("CHECKPOINT").unwrap();
    }
    fn set_mode(&mut self, mode: Mode) {
        self.durable = mode == Mode::Durable;
    }
    fn client(&self) -> Box<dyn Client> {
        let mut c = postgres::Client::connect(&self.url, postgres::NoTls).unwrap();
        c.batch_execute(if self.durable {
            "SET synchronous_commit = on"
        } else {
            "SET synchronous_commit = off"
        })
        .unwrap();
        let fields: Vec<String> = (0..FIELDS).map(|i| format!("field{i}")).collect();
        let marks: Vec<String> = (0..=FIELDS).map(|i| format!("${}", i + 1)).collect();
        let read = c
            .prepare("SELECT * FROM usertable WHERE ycsb_key = $1")
            .unwrap();
        let scan = c
            .prepare("SELECT * FROM usertable WHERE ycsb_key >= $1 ORDER BY ycsb_key LIMIT $2")
            .unwrap();
        let set = (0..FIELDS)
            .map(|i| {
                c.prepare(&format!(
                    "UPDATE usertable SET field{i} = $2 WHERE ycsb_key = $1"
                ))
                .unwrap()
            })
            .collect();
        let insert = c
            .prepare(&format!(
                "INSERT INTO usertable (ycsb_key, {}) VALUES ({})",
                fields.join(", "),
                marks.join(", ")
            ))
            .unwrap();
        Box::new(PgClient {
            c,
            last: Vec::new(),
            read,
            scan,
            set,
            insert,
        })
    }
}

struct PgClient {
    c: postgres::Client,
    last: Vec<(i64, [String; FIELDS])>,
    read: postgres::Statement,
    scan: postgres::Statement,
    set: Vec<postgres::Statement>,
    insert: postgres::Statement,
}

fn pg_fields(r: &postgres::Row) -> (i64, [String; FIELDS]) {
    let mut out: [String; FIELDS] = Default::default();
    for (i, o) in out.iter_mut().enumerate() {
        *o = r.get(i + 1);
    }
    (r.get(0), out)
}

impl Client for PgClient {
    fn ping(&mut self) -> bool {
        self.c.simple_query("").unwrap();
        true
    }
    fn read(&mut self, key: u64) {
        let r = self.c.query_one(&self.read, &[&(key as i64)]).unwrap();
        self.last.clear();
        self.last.push(pg_fields(&r));
    }
    fn update(&mut self, key: u64, field: usize, value: &str) {
        let n = self
            .c
            .execute(&self.set[field], &[&(key as i64), &value])
            .unwrap();
        assert_eq!(n, 1);
    }
    fn insert(&mut self, key: u64, v: &[String]) {
        let k = key as i64;
        self.c
            .execute(
                &self.insert,
                &[
                    &k, &v[0], &v[1], &v[2], &v[3], &v[4], &v[5], &v[6], &v[7], &v[8], &v[9],
                ],
            )
            .unwrap();
    }
    fn scan(&mut self, key: u64, len: u64) {
        let rows = self
            .c
            .query(&self.scan, &[&(key as i64), &(len as i64)])
            .unwrap();
        assert!(!rows.is_empty());
        self.last = rows.iter().map(pg_fields).collect();
    }
    fn got(&mut self) -> Vec<(u64, [u64; FIELDS])> {
        digests_of(&self.last)
    }
}

// ---- MongoDB

use mongodb::bson::{doc, Document};
use mongodb::options::{Acknowledgment, CollectionOptions, WriteConcern};

struct Mongo {
    client: mongodb::sync::Client,
    durable: bool,
    _container: Option<Container>,
}

impl Mongo {
    fn coll(&self, durable: bool) -> mongodb::sync::Collection<Document> {
        let wc = WriteConcern::builder()
            .w(Acknowledgment::Nodes(1))
            .journal(durable)
            .build();
        self.client.database("ycsb").collection_with_options(
            "usertable",
            CollectionOptions::builder().write_concern(wc).build(),
        )
    }
}

fn mongo_doc(key: u64, values: &[String]) -> Document {
    let mut d = doc! { "_id": key as i64 };
    for (i, v) in values.iter().enumerate() {
        d.insert(format!("field{i}"), v.as_str());
    }
    d
}

impl System for Mongo {
    fn name(&self) -> &'static str {
        "mongo"
    }
    fn size(&self) -> Option<u64> {
        // The collection's files and its _id index, as WiredTiger holds
        // them compressed.
        let d = self
            .client
            .database("ycsb")
            .run_command(doc! {"collStats": "usertable"})
            .run()
            .ok()?;
        let num = |k: &str| match d.get(k) {
            Some(mongodb::bson::Bson::Int32(n)) => *n as u64,
            Some(mongodb::bson::Bson::Int64(n)) => *n as u64,
            Some(mongodb::bson::Bson::Double(n)) => *n as u64,
            _ => 0,
        };
        Some(num("storageSize") + num("totalIndexSize"))
    }
    fn load(&mut self, records: u64) {
        let coll = self.coll(false);
        coll.drop().run().unwrap();
        let mut key = 1;
        while key <= records {
            let end = (key + LOAD_BATCH as u64).min(records + 1);
            let docs: Vec<Document> = (key..end)
                .map(|k| mongo_doc(k, &record_values(k)))
                .collect();
            coll.insert_many(docs).run().unwrap();
            key = end;
        }
        // The load's writes on disk, as the others' are.
        self.client
            .database("admin")
            .run_command(doc! {"fsync": 1})
            .run()
            .unwrap();
    }
    fn set_mode(&mut self, mode: Mode) {
        self.durable = mode == Mode::Durable;
    }
    fn client(&self) -> Box<dyn Client> {
        Box::new(MongoClient {
            admin: self.client.database("admin"),
            coll: self.coll(self.durable),
            fields: (0..FIELDS).map(|i| format!("field{i}")).collect(),
            last: Vec::new(),
        })
    }
}

struct MongoClient {
    admin: mongodb::sync::Database,
    coll: mongodb::sync::Collection<Document>,
    fields: Vec<String>,
    last: Vec<Document>,
}

impl Client for MongoClient {
    fn ping(&mut self) -> bool {
        self.admin.run_command(doc! {"ping": 1}).run().unwrap();
        true
    }
    fn read(&mut self, key: u64) {
        let d = self
            .coll
            .find_one(doc! {"_id": key as i64})
            .run()
            .unwrap()
            .expect("the record");
        assert_eq!(d.len(), FIELDS + 1);
        self.last.clear();
        self.last.push(d);
    }
    fn update(&mut self, key: u64, field: usize, value: &str) {
        let mut set = Document::new();
        set.insert(self.fields[field].as_str(), value);
        let r = self
            .coll
            .update_one(doc! {"_id": key as i64}, doc! {"$set": set})
            .run()
            .unwrap();
        assert_eq!(r.matched_count, 1);
    }
    fn insert(&mut self, key: u64, values: &[String]) {
        self.coll.insert_one(mongo_doc(key, values)).run().unwrap();
    }
    fn scan(&mut self, key: u64, len: u64) {
        let docs: Vec<Document> = self
            .coll
            .find(doc! {"_id": {"$gte": key as i64}})
            .sort(doc! {"_id": 1})
            .limit(len as i64)
            .run()
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert!(!docs.is_empty());
        self.last = docs;
    }
    fn got(&mut self) -> Vec<(u64, [u64; FIELDS])> {
        self.last
            .iter()
            .map(|d| {
                let mut out = [0; FIELDS];
                for (o, f) in out.iter_mut().zip(&self.fields) {
                    if let Ok(v) = d.get_str(f) {
                        *o = digest(v.as_bytes());
                    }
                }
                (d.get_i64("_id").map(|k| k as u64).unwrap_or(u64::MAX), out)
            })
            .collect()
    }
}

// ------------------------------------------------------------- the runs

struct Config {
    records: u64,
    warmup: Duration,
    run: Duration,
    threads: Vec<usize>,
    workloads: Vec<char>,
    modes: Vec<Mode>,
    systems: Vec<String>,
    out: PathBuf,
    runs: usize,
    /// The probe's time at idle, in ms; measured at the start unless given.
    probe_base: Option<f64>,
    /// The least idle time between two cells, and between two systems.
    gap: Duration,
    turn_gap: Duration,
    image: String,
    run_id: String,
    /// Where `--verify` writes each cell's verdict; every operation is
    /// then logged and held to what was written (`check`).
    verify: Option<PathBuf>,
    /// fenecdb's files compacted on their own, in process and in the
    /// server, as each ships; `--no-auto-compact` for the runs before.
    auto_compact: bool,
}

/// What a cell measured, and with `--verify` what each thread logged.
struct Cell {
    ops: u64,
    hist: BTreeMap<Op, Hist>,
    logs: Vec<Vec<Entry>>,
}

fn run_cell(
    sys: &dyn System,
    w: &Workload,
    threads: usize,
    records_now: u64,
    cfg: &Config,
) -> (Cell, u64) {
    let keys = Keys::new(records_now, threads);
    let expect_new = if w.mix.iter().any(|(o, _)| *o == Op::Insert) {
        (records_now / 10).max(10_000)
    } else {
        0
    };
    let space = records_now + expect_new;
    let base_scrambled = Scrambled::new();
    let base_latest = Zipfian::new(records_now);
    let measuring = AtomicBool::new(false);
    let stop = AtomicBool::new(false);
    let verify = cfg.verify.is_some();
    let clients: Vec<Box<dyn Client>> = (0..threads).map(|_| sys.client()).collect();
    let mut cell = Cell {
        ops: 0,
        hist: BTreeMap::new(),
        logs: Vec::new(),
    };
    let t0 = Instant::now();
    std::thread::scope(|s| {
        let handles: Vec<_> = clients
            .into_iter()
            .enumerate()
            .map(|(t, mut client)| {
                let (keys, measuring, stop) = (&keys, &measuring, &stop);
                let mut chooser = Chooser {
                    dist: w.dist,
                    scrambled: base_scrambled.clone(),
                    latest: base_latest.clone(),
                    space,
                };
                // The same seed for every system: the cell and the thread.
                let mut rng =
                    Rng::new(SEED ^ ((w.name as u64) << 32) ^ ((threads as u64) << 16) ^ t as u64);
                let mix = w.mix;
                s.spawn(move || {
                    let mut hist: BTreeMap<Op, Hist> = BTreeMap::new();
                    let mut value = String::with_capacity(FIELD_LEN);
                    let mut ops = 0u64;
                    let mut log = Vec::new();
                    let at = |i: Instant| (i - t0).as_nanos() as u64;
                    while !stop.load(Ordering::Relaxed) {
                        let mut u = rng.f64();
                        let mut op = mix[mix.len() - 1].0;
                        for (o, p) in mix {
                            if u < *p {
                                op = *o;
                                break;
                            }
                            u -= p;
                        }
                        let key: u64;
                        let (mut field, mut len, mut seen) = (0usize, 0u64, 0u64);
                        // A read-modify-write's read, its end and its rows,
                        // which the update's answer would put out of reach.
                        let mut rmw_read = None;
                        let start = Instant::now();
                        match op {
                            Op::Read => {
                                key = chooser.next(&mut rng, keys);
                                client.read(key)
                            }
                            Op::Update => {
                                key = chooser.next(&mut rng, keys);
                                rng.value(&mut value);
                                field = rng.below(FIELDS as u64) as usize;
                                client.update(key, field, &value);
                            }
                            Op::Rmw => {
                                key = chooser.next(&mut rng, keys);
                                client.read(key);
                                if verify {
                                    rmw_read = Some((Instant::now(), client.got()));
                                }
                                rng.value(&mut value);
                                field = rng.below(FIELDS as u64) as usize;
                                client.update(key, field, &value);
                            }
                            Op::Scan => {
                                key = chooser.next(&mut rng, keys);
                                len = rng.below(MAX_SCAN) + 1;
                                if verify {
                                    // The key was in when it was drawn.
                                    seen = keys.last().max(key);
                                }
                                client.scan(key, len);
                            }
                            Op::Insert => {
                                // Published before the key is taken, a
                                // bound below it: a reader that sees the
                                // key handed out sees this too.
                                let below = keys.next.load(Ordering::SeqCst);
                                keys.in_flight[t].store(below, Ordering::SeqCst);
                                key = keys.next.fetch_add(1, Ordering::SeqCst);
                                keys.in_flight[t].store(key, Ordering::SeqCst);
                                client.insert(key, &record_values(key));
                                keys.in_flight[t].store(u64::MAX, Ordering::SeqCst);
                            }
                        }
                        let end = Instant::now();
                        if measuring.load(Ordering::Relaxed) {
                            let ns = (end - start).as_nanos() as u64;
                            hist.entry(op).or_insert_with(Hist::new).record(ns);
                            ops += 1;
                        }
                        if !verify {
                            continue;
                        }
                        // What the operation was answered, kept after the
                        // timed span; the warm-up's writes are logged too,
                        // since the reads after them see them.
                        let (s, e) = (at(start), at(end));
                        let write =
                            |client: &mut Box<dyn Client>, s, field: usize, d| Entry::Write {
                                key,
                                field: field as u8,
                                value: d,
                                s,
                                e,
                                affected: client.affected(),
                            };
                        match op {
                            Op::Read => log.push(Entry::read(key, s, e, client.got())),
                            Op::Rmw => {
                                let (mid, rows) = rmw_read.take().unwrap();
                                log.push(Entry::read(key, s, at(mid), rows));
                                log.push(write(
                                    &mut client,
                                    at(mid),
                                    field,
                                    digest(value.as_bytes()),
                                ));
                            }
                            Op::Update => {
                                log.push(write(&mut client, s, field, digest(value.as_bytes())))
                            }
                            Op::Insert => log.push(write(&mut client, s, FIELDS, 0)),
                            Op::Scan => log.push(Entry::Scan {
                                key,
                                len,
                                seen,
                                next: keys.next.load(Ordering::SeqCst),
                                rows: client.got().iter().map(|(k, d)| (*k, combine(d))).collect(),
                            }),
                        }
                    }
                    (ops, hist, log)
                })
            })
            .collect();
        std::thread::sleep(cfg.warmup);
        measuring.store(true, Ordering::Relaxed);
        std::thread::sleep(cfg.run);
        measuring.store(false, Ordering::Relaxed);
        stop.store(true, Ordering::Relaxed);
        for h in handles {
            let (ops, hist, log) = h.join().unwrap();
            cell.ops += ops;
            for (op, h) in hist {
                cell.hist.entry(op).or_insert_with(Hist::new).merge(&h);
            }
            cell.logs.push(log);
        }
    });
    let inserted = keys.next.load(Ordering::Acquire) - (records_now + 1);
    (cell, records_now + inserted)
}

// ----------------------------------------------------------------- verify

/// An operation as `--verify` logged it, its span in ns from the cell's
/// start: what it asked and what it was answered.
enum Entry {
    /// A read: the rows it was answered, the first one's key and digests.
    Read {
        key: u64,
        s: u64,
        e: u64,
        rows: usize,
        got: (u64, [u64; FIELDS]),
    },
    /// A scan: the highest key every key below which was in the database
    /// when it began (`seen`), the next key to be handed out when it ended,
    /// and each row's key and record digest.
    Scan {
        key: u64,
        len: u64,
        seen: u64,
        next: u64,
        rows: Vec<(u64, u64)>,
    },
    /// An update of one field, or an insert (`field` = `FIELDS`) of the
    /// record `record_values` draws for its key.
    Write {
        key: u64,
        field: u8,
        value: u64,
        s: u64,
        e: u64,
        affected: u64,
    },
}

impl Entry {
    fn read(key: u64, s: u64, e: u64, rows: Vec<(u64, [u64; FIELDS])>) -> Entry {
        Entry::Read {
            key,
            s,
            e,
            rows: rows.len(),
            got: rows.first().copied().unwrap_or((u64::MAX, [0; FIELDS])),
        }
    }
}

/// A write to one field as the checker holds it.
#[derive(Clone, Copy)]
struct W {
    s: u64,
    e: u64,
    d: u64,
}

/// The writes a cell made to one field of one record, by when they ended,
/// and the latest start among the first `i + 1` of them.
struct Reg {
    ws: Vec<W>,
    smax: Vec<u64>,
}

impl Reg {
    /// The latest start of a write that ended before `t`, if one did.
    fn latest_start_before(&self, t: u64) -> Option<u64> {
        let n = self.ws.partition_point(|w| w.e < t);
        n.checked_sub(1).map(|i| self.smax[i])
    }
}

/// What each field holds as a cell begins, where a cell before it in the
/// phase wrote it: the values it can hold, more than one only where the
/// writes that could have been the last overlapped.
#[derive(Default)]
struct Model {
    fields: std::collections::HashMap<(u64, u8), Vec<u64>>,
    base: std::collections::HashMap<u64, [u64; FIELDS]>,
}

impl Model {
    fn initial(&mut self, key: u64, field: u8) -> Vec<u64> {
        if let Some(v) = self.fields.get(&(key, field)) {
            return v.clone();
        }
        vec![self.base(key)[field as usize]]
    }
    fn base(&mut self, key: u64) -> [u64; FIELDS] {
        *self.base.entry(key).or_insert_with(|| base_digests(key))
    }
}

#[derive(Default)]
struct Verdict {
    reads: u64,
    scans: u64,
    rows: u64,
    writes: u64,
    mismatches: u64,
    /// Scan rows whose fields could have been too many combinations of
    /// overlapping writes to try; none in YCSB's E, which updates nothing.
    undecided: u64,
    examples: Vec<String>,
}

impl Verdict {
    fn wrong(&mut self, what: String) {
        self.mismatches += 1;
        if self.examples.len() < 8 {
            self.examples.push(what);
        }
    }
}

/// Holds every logged operation to what was written: a read answered the
/// record it asked for, each field the value of a write that could have
/// been the last before it -- one that ended before the read began and was
/// not followed by another that also did, or one that overlapped it -- or
/// the value the field held as the cell began where no write had ended; a
/// scan answered consecutive keys from its own as far as every key it
/// could see, no more than it asked for, each record as written; a write
/// wrote one record. A key's value is FNV-1a of its bytes (`digest`).
/// The model is moved on to what the fields hold after the cell.
fn check(logs: &[Vec<Entry>], model: &mut Model) -> Verdict {
    let mut v = Verdict::default();
    // Every write, into its field's register.
    let mut regs: std::collections::HashMap<(u64, u8), Reg> = std::collections::HashMap::new();
    for e in logs.iter().flatten() {
        if let Entry::Write {
            key,
            field,
            value,
            s,
            e,
            affected,
        } = *e
        {
            v.writes += 1;
            if affected != 1 {
                v.wrong(format!("write of {key} answered {affected} records"));
            }
            let mut put = |f: u8, d: u64| {
                regs.entry((key, f))
                    .or_insert_with(|| Reg {
                        ws: Vec::new(),
                        smax: Vec::new(),
                    })
                    .ws
                    .push(W { s, e, d })
            };
            if field as usize == FIELDS {
                for (f, d) in model.base(key).into_iter().enumerate() {
                    put(f as u8, d);
                }
            } else {
                put(field, value);
            }
        }
    }
    let mut by_value: std::collections::HashMap<u64, ((u64, u8), usize)> =
        std::collections::HashMap::new();
    for (r, reg) in regs.iter_mut() {
        reg.ws.sort_by_key(|w| (w.e, w.s));
        let mut m = 0;
        reg.smax = reg
            .ws
            .iter()
            .map(|w| {
                m = m.max(w.s);
                m
            })
            .collect();
        for (i, w) in reg.ws.iter().enumerate() {
            by_value.insert(w.d, (*r, i));
        }
    }
    // Whether `d` is a value the field `r` could hold to a read over [s, e].
    let may_hold = |model: &mut Model, r: (u64, u8), s: u64, e: u64, d: u64| -> bool {
        let reg = regs.get(&r);
        let none_ended = reg.is_none_or(|g| g.latest_start_before(s).is_none());
        if none_ended && model.initial(r.0, r.1).contains(&d) {
            return true;
        }
        let (Some(reg), Some(&(at, i))) = (reg, by_value.get(&d)) else {
            return false;
        };
        if at != r {
            return false;
        }
        let w = reg.ws[i];
        let overlaps = w.s <= e && w.e >= s;
        // Ended before the read, and no write began after it ended and
        // ended before the read too.
        let last = w.e < s && reg.latest_start_before(s).is_some_and(|m| m <= w.e);
        overlaps || last
    };
    for e in logs.iter().flatten() {
        match e {
            Entry::Read {
                key,
                s,
                e,
                rows,
                got,
            } => {
                v.reads += 1;
                if *rows != 1 || got.0 != *key {
                    v.wrong(format!(
                        "read of {key} answered {rows} rows, the first {}",
                        got.0
                    ));
                    continue;
                }
                for f in 0..FIELDS {
                    if !may_hold(model, (*key, f as u8), *s, *e, got.1[f]) {
                        v.wrong(format!(
                            "read of {key} [{s}, {e}] ns: field{f} holds no value written to it"
                        ));
                        break;
                    }
                }
            }
            Entry::Scan {
                key,
                len,
                seen,
                next,
                rows,
            } => {
                v.scans += 1;
                v.rows += rows.len() as u64;
                let visible = (seen + 1).saturating_sub(*key).min(*len);
                if (rows.len() as u64) < visible || rows.len() as u64 > *len {
                    v.wrong(format!(
                        "scan of {len} from {key} answered {} rows, {visible} visible",
                        rows.len()
                    ));
                    continue;
                }
                for (i, (k, c)) in rows.iter().enumerate() {
                    let i = i as u64;
                    let in_order = if i < visible {
                        *k == key + i
                    } else {
                        *k > rows[i as usize - 1].0 && *k < *next
                    };
                    if !in_order {
                        v.wrong(format!("scan from {key}: row {i} is key {k}"));
                        break;
                    }
                    // The record as written: each field's starting value or
                    // any written to it in the cell, every combination
                    // tried up to a bound.
                    let cands: Vec<Vec<u64>> = (0..FIELDS as u8)
                        .map(|f| {
                            let mut c = model.initial(*k, f);
                            if let Some(g) = regs.get(&(*k, f)) {
                                c.extend(g.ws.iter().map(|w| w.d));
                            }
                            c
                        })
                        .collect();
                    if cands.iter().map(Vec::len).product::<usize>() > 4096 {
                        v.undecided += 1;
                        continue;
                    }
                    let mut pick = [0usize; FIELDS];
                    let found = 'combos: loop {
                        let mut d = [0; FIELDS];
                        for f in 0..FIELDS {
                            d[f] = cands[f][pick[f]];
                        }
                        if combine(&d) == *c {
                            break true;
                        }
                        for f in 0..=FIELDS {
                            if f == FIELDS {
                                break 'combos false;
                            }
                            pick[f] += 1;
                            if pick[f] < cands[f].len() {
                                break;
                            }
                            pick[f] = 0;
                        }
                    };
                    if !found {
                        v.wrong(format!(
                            "scan from {key}: the record of {k} is not as written"
                        ));
                        break;
                    }
                }
            }
            Entry::Write { .. } => {}
        }
    }
    // What each field written holds now: the writes no other began after.
    for (r, reg) in regs {
        let last = *reg.smax.last().unwrap();
        model.fields.insert(
            r,
            reg.ws.iter().filter(|w| w.e >= last).map(|w| w.d).collect(),
        );
    }
    v
}

/// A fixed piece of work on one core, the best of three, in ms: the
/// machine's speed as it stands. The Air slows every core as it heats.
fn probe() -> f64 {
    let mut best = f64::MAX;
    for _ in 0..3 {
        let t = Instant::now();
        let mut x = 1u64;
        for i in 0..40_000_000u64 {
            x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(i) ^ (x >> 17);
        }
        std::hint::black_box(x);
        best = best.min(t.elapsed().as_secs_f64() * 1e3);
    }
    best
}

/// Waits `least`, then until the probe runs within 4% of the idle speed,
/// 10 minutes at the most; returns the probe's ratio to it when the cell
/// starts.
fn cool(base: f64, least: Duration) -> f64 {
    std::thread::sleep(least);
    let until = Instant::now() + Duration::from_secs(600);
    loop {
        let p = probe();
        let ratio = p / base;
        if ratio <= 1.04 || Instant::now() > until {
            return ratio;
        }
        eprintln!("  warm ({ratio:.2}x), waiting");
        std::thread::sleep(Duration::from_secs(15));
    }
}

const HEADER: &str = "run\tsystem\tmode\tworkload\tthreads\trecords\tseconds\tops\tops_s\tprobe";

fn header() -> String {
    let mut h = HEADER.to_string();
    for op in OPS {
        for q in ["n", "p50", "p95", "p99", "max"] {
            write!(h, "\t{}_{q}", op.name()).unwrap();
        }
    }
    h
}

fn append(path: &Path, line: &str) {
    let new = !path.exists();
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    if new {
        writeln!(f, "{}", header()).unwrap();
    }
    writeln!(f, "{line}").unwrap();
}

fn make_system(name: &str, dir: &Path, cfg: &Config) -> Box<dyn System> {
    match name {
        "fenec" => Box::new(FenecLocal::new(dir, cfg.auto_compact)),
        "sqlite" => Box::new(Sqlite {
            path: dir.join("ycsb.sqlite"),
            durable: false,
        }),
        "server" | "server-docker" => Box::new(Server {
            dir: dir.to_path_buf(),
            docker: name == "server-docker",
            port: http::free_port(),
            proc: None,
            image: cfg.image.clone(),
            auto_compact: cfg.auto_compact,
        }),
        "pg" => {
            // A server of the reader's own when named, else a container
            // started here and removed with its volume after.
            let (url, container) = match std::env::var("FENECBENCH_YCSB_PG") {
                Ok(url) => (url, None),
                Err(_) => (PG_URL.to_string(), Some(Container::start("pg", PG_RUN))),
            };
            let until = Instant::now() + Duration::from_secs(120);
            let mut c = loop {
                match postgres::Client::connect(&url, postgres::NoTls) {
                    Ok(c) => break c,
                    Err(e) => {
                        assert!(Instant::now() < until, "PostgreSQL did not start: {e}");
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
            };
            let v: String = c.query_one("SELECT version()", &[]).unwrap().get(0);
            let sb: String = c.query_one("SHOW shared_buffers", &[]).unwrap().get(0);
            eprintln!("pg: {v}, shared_buffers {sb}");
            Box::new(Pg {
                url,
                durable: false,
                _container: container,
            })
        }
        "mongo" => {
            let (url, container) = match std::env::var("FENECBENCH_YCSB_MONGO") {
                Ok(url) => (url, None),
                Err(_) => (
                    MONGO_URL.to_string(),
                    Some(Container::start("mongo", MONGO_RUN)),
                ),
            };
            let client = mongodb::sync::Client::with_uri_str(url).unwrap();
            let until = Instant::now() + Duration::from_secs(120);
            let info = loop {
                match client
                    .database("admin")
                    .run_command(doc! {"buildInfo": 1})
                    .run()
                {
                    Ok(d) => break d,
                    Err(e) => {
                        assert!(Instant::now() < until, "MongoDB did not start: {e}");
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
            };
            eprintln!("mongo: MongoDB {}", info.get_str("version").unwrap_or("?"));
            Box::new(Mongo {
                client,
                durable: false,
                _container: container,
            })
        }
        other => panic!("no system `{other}`: fenec, sqlite, server, server-docker, pg, mongo"),
    }
}

fn run_system(name: &str, cfg: &Config, base: f64, run: usize) {
    let dir = std::env::temp_dir().join(format!("fenec-ycsb-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut sys = make_system(name, &dir, cfg);
    assert_eq!(sys.name(), name);
    let mut pinged = false;
    for &mode in &cfg.modes {
        // YCSB's own sequence: load, then A, B, C, F and D over that
        // data, then load afresh for E. Run after the others, E's scans
        // read records an update rewrote somewhere else -- fenecdb writes
        // the whole record again at its file's end -- and measured that
        // rather than the workload: its buffered scans went 3 108 -> 795
        // ops/s in the run that showed it.
        for phase in ["ABCFD", "E"] {
            let order: Vec<&Workload> = phase
                .chars()
                .filter(|c| cfg.workloads.contains(c))
                // C writes nothing: one mode measures it.
                .filter(|c| {
                    !(*c == 'C' && mode == Mode::Durable && cfg.modes.contains(&Mode::Buffered))
                })
                .map(|c| WORKLOADS.iter().find(|w| w.name == c).unwrap())
                .collect();
            if order.is_empty() {
                continue;
            }
            let ratio = cool(base, cfg.gap);
            eprintln!("{name}: loading {} records", cfg.records);
            let t = Instant::now();
            sys.load(cfg.records);
            let secs = t.elapsed().as_secs_f64();
            let rate = cfg.records as f64 / secs;
            eprintln!("{name}: loaded at {rate:.0} records/s");
            append(
                &cfg.out,
                &format!(
                    "{}\t{name}\t{}\tload\t1\t{}\t{secs:.2}\t{}\t{rate:.0}\t{ratio:.3}",
                    cfg.run_id,
                    mode.name(),
                    cfg.records,
                    cfg.records
                ),
            );
            if !pinged {
                pinged = true;
                ping(sys.as_ref(), name, cfg, ratio);
            }
            sys.set_mode(mode);
            let mut records = cfg.records;
            // What the load wrote, which each cell's writes move on.
            let mut model = Model::default();
            for w in order {
                for &threads in &cfg.threads {
                    let ratio = cool(base, cfg.gap);
                    let (cell, now) = run_cell(sys.as_ref(), w, threads, records, cfg);
                    records = now;
                    write_cell(name, mode, w, threads, records, &cell, ratio, cfg, run);
                    if let Some(path) = &cfg.verify {
                        let v = check(&cell.logs, &mut model);
                        eprintln!(
                            "verify {name} {} {} x{threads}: {} reads, {} scans ({} rows), {} writes: {} mismatches, {} undecided",
                            mode.name(),
                            w.name,
                            v.reads,
                            v.scans,
                            v.rows,
                            v.writes,
                            v.mismatches,
                            v.undecided
                        );
                        for e in &v.examples {
                            eprintln!("  {e}");
                        }
                        let new = !path.exists();
                        let mut f = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(path)
                            .unwrap();
                        if new {
                            writeln!(f, "run\tsystem\tmode\tworkload\tthreads\treads\tscans\tscan_rows\twrites\tmismatches\tundecided").unwrap();
                        }
                        writeln!(
                            f,
                            "{}\t{name}\t{}\t{}\t{threads}\t{}\t{}\t{}\t{}\t{}\t{}",
                            cfg.run_id,
                            mode.name(),
                            w.name,
                            v.reads,
                            v.scans,
                            v.rows,
                            v.writes,
                            v.mismatches,
                            v.undecided
                        )
                        .unwrap();
                    }
                }
            }
            if let Some(bytes) = sys.size() {
                eprintln!("{name}: {} MB on disk after {phase}", bytes >> 20);
                append(
                    &cfg.out,
                    &format!(
                        "{}\t{name}\t{}\tsize-{phase}\t1\t{records}\t0\t{bytes}\t0\t0",
                        cfg.run_id,
                        mode.name()
                    ),
                );
            }
        }
    }
    drop(sys);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The round trip of the least request, 2 000 of them after 200.
fn ping(sys: &dyn System, name: &str, cfg: &Config, ratio: f64) {
    let mut c = sys.client();
    if !c.ping() {
        return;
    }
    let mut h = Hist::new();
    for i in 0..2_200 {
        let t = Instant::now();
        c.ping();
        if i >= 200 {
            h.record(t.elapsed().as_nanos() as u64);
        }
    }
    eprintln!(
        "{name}: the least request's round trip p50 {:.1} us, p99 {:.1}",
        h.pct(0.5),
        h.pct(0.99)
    );
    let mut line = format!(
        "{}\t{name}\t-\tping\t1\t{}\t0\t{}\t0\t{ratio:.3}",
        cfg.run_id, cfg.records, h.n
    );
    write!(
        line,
        "\t{}\t{:.1}\t{:.1}\t{:.1}\t{:.1}",
        h.n,
        h.pct(0.5),
        h.pct(0.95),
        h.pct(0.99),
        h.max as f64 / 1e3
    )
    .unwrap();
    for _ in 1..OPS.len() {
        line.push_str("\t0\t\t\t\t");
    }
    append(&cfg.out, &line);
}

#[allow(clippy::too_many_arguments)]
fn write_cell(
    name: &str,
    mode: Mode,
    w: &Workload,
    threads: usize,
    records: u64,
    cell: &Cell,
    ratio: f64,
    cfg: &Config,
    run: usize,
) {
    let secs = cfg.run.as_secs_f64();
    let mut line = format!(
        "{}\t{name}\t{}\t{}\t{threads}\t{records}\t{secs:.0}\t{}\t{:.0}\t{ratio:.3}",
        cfg.run_id,
        mode.name(),
        w.name,
        cell.ops,
        cell.ops as f64 / secs
    );
    let mut summary = String::new();
    for op in OPS {
        match cell.hist.get(&op) {
            Some(h) if h.n > 0 => {
                write!(
                    line,
                    "\t{}\t{:.1}\t{:.1}\t{:.1}\t{:.1}",
                    h.n,
                    h.pct(0.5),
                    h.pct(0.95),
                    h.pct(0.99),
                    h.max as f64 / 1e3
                )
                .unwrap();
                write!(summary, " {} p99 {:.0}us", op.name(), h.pct(0.99)).unwrap();
            }
            _ => line.push_str("\t0\t\t\t\t"),
        }
    }
    append(&cfg.out, &line);
    eprintln!(
        "run {run} {name} {} {} x{threads}: {:.0} ops/s{summary} (probe {ratio:.2})",
        mode.name(),
        w.name,
        cell.ops as f64 / secs
    );
}

// ----------------------------------------------------------------- report

/// The median of each cell's runs: ops/s and every op's p99, as the
/// lines `ycsb` wrote them.
fn report(path: &Path) {
    let text = std::fs::read_to_string(path).unwrap();
    let mut lines = text.lines();
    let head: Vec<&str> = lines.next().unwrap().split('\t').collect();
    let col = |name: &str| head.iter().position(|h| *h == name).unwrap();
    // The runs a later one replaced, by run and system (`superseded.tsv`
    // beside the results): they stay in the file, which is only appended
    // to, and count in no median.
    let superseded: Vec<(String, String)> =
        std::fs::read_to_string(path.with_file_name("superseded.tsv"))
            .unwrap_or_default()
            .lines()
            .skip(1)
            .filter_map(|l| {
                let mut f = l.split('\t');
                Some((f.next()?.to_string(), f.next()?.to_string()))
            })
            .collect();
    let lines = lines.filter(|l| {
        let f: Vec<&str> = l.split('\t').collect();
        !superseded.iter().any(|(run, sys)| {
            f.get(col("run")) == Some(&run.as_str()) && f.get(col("system")) == Some(&sys.as_str())
        })
    });
    // A run's cell measured again -- a run cut short and completed by a
    // later invocation with its `--run-id` loads and pings again -- counts
    // once, as its last line: the file is only ever appended to.
    let mut latest: BTreeMap<(String, String, String, String, usize), Vec<String>> =
        BTreeMap::new();
    for l in lines {
        let f: Vec<String> = l.split('\t').map(str::to_string).collect();
        let key = (
            f[col("system")].clone(),
            f[col("mode")].clone(),
            f[col("workload")].clone(),
            f[col("run")].clone(),
            f[col("threads")].parse().unwrap(),
        );
        latest.insert(key, f);
    }
    let mut cells: BTreeMap<(String, String, String, usize), Vec<Vec<String>>> = BTreeMap::new();
    for ((sys, mode, wl, _, threads), f) in latest {
        cells.entry((sys, mode, wl, threads)).or_default().push(f);
    }
    let median = |v: &mut Vec<f64>| -> Option<f64> {
        if v.is_empty() {
            return None;
        }
        v.sort_by(|a, b| a.total_cmp(b));
        Some(v[v.len() / 2])
    };
    println!("system\tmode\tworkload\tthreads\truns\tops_s\tREAD_p99\tUPDATE_p99\tINSERT_p99\tSCAN_p99\tRMW_p99\tREAD_p50\tUPDATE_p50\tINSERT_p50\tSCAN_p50\tRMW_p50\tprobe_max");
    for ((sys, mode, wl, threads), runs) in cells {
        // A size line stops at the probe's column: what it has not got is
        // no figure.
        let num = |f: &Vec<String>, c: &str| f.get(col(c))?.parse::<f64>().ok();
        let mut ops: Vec<f64> = runs.iter().filter_map(|f| num(f, "ops_s")).collect();
        let mut probe: Vec<f64> = runs.iter().filter_map(|f| num(f, "probe")).collect();
        probe.sort_by(|a, b| a.total_cmp(b));
        let mut out = format!(
            "{sys}\t{mode}\t{wl}\t{threads}\t{}\t{:.0}",
            runs.len(),
            median(&mut ops).unwrap()
        );
        for q in ["p99", "p50"] {
            for op in OPS {
                let c = format!("{}_{q}", op.name());
                let mut v: Vec<f64> = runs.iter().filter_map(|f| num(f, &c)).collect();
                match median(&mut v) {
                    Some(x) => write!(out, "\t{x:.0}").unwrap(),
                    None => out.push_str("\t-"),
                }
            }
        }
        write!(out, "\t{:.2}", probe.last().copied().unwrap_or(0.0)).unwrap();
        println!("{out}");
    }
}

// ------------------------------------------------------------------- main

fn list<T>(s: &str, f: impl Fn(&str) -> T) -> Vec<T> {
    s.split(',').filter(|x| !x.is_empty()).map(f).collect()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("report") {
        report(Path::new(args.get(1).expect("ycsb report <file.tsv>")));
        return;
    }
    let mut cfg = Config {
        records: 1_000_000,
        warmup: Duration::from_secs(5),
        run: Duration::from_secs(30),
        threads: vec![1, 4, 16],
        workloads: "ABCDEF".chars().collect(),
        modes: vec![Mode::Durable, Mode::Buffered],
        systems: list("fenec,sqlite,server,pg,mongo", str::to_string),
        out: PathBuf::from("ycsb.tsv"),
        runs: 1,
        probe_base: None,
        gap: Duration::from_secs(10),
        turn_gap: Duration::from_secs(180),
        image: "fenecdb-ycsb".into(),
        run_id: String::new(),
        verify: None,
        auto_compact: true,
    };
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--no-auto-compact" {
            cfg.auto_compact = false;
            i += 1;
            continue;
        }
        let v = args.get(i + 1).cloned().unwrap_or_default();
        match args[i].as_str() {
            "--records" => cfg.records = v.parse().unwrap(),
            "--seconds" => cfg.run = Duration::from_secs(v.parse().unwrap()),
            "--warmup" => cfg.warmup = Duration::from_secs(v.parse().unwrap()),
            "--threads" => cfg.threads = list(&v, |x| x.parse().unwrap()),
            "--workloads" => cfg.workloads = v.chars().filter(|c| *c != ',').collect(),
            "--modes" => {
                cfg.modes = list(&v, |x| match x {
                    "durable" => Mode::Durable,
                    "buffered" => Mode::Buffered,
                    _ => panic!("--modes durable,buffered"),
                })
            }
            "--systems" => cfg.systems = list(&v, str::to_string),
            "--out" => cfg.out = PathBuf::from(&v),
            "--runs" => cfg.runs = v.parse().unwrap(),
            "--probe-base" => cfg.probe_base = Some(v.parse().unwrap()),
            "--gap" => cfg.gap = Duration::from_secs(v.parse().unwrap()),
            "--turn-gap" => cfg.turn_gap = Duration::from_secs(v.parse().unwrap()),
            "--image" => cfg.image = v,
            "--run-id" => cfg.run_id = v,
            "--verify" => cfg.verify = Some(PathBuf::from(&v)),
            other => panic!("unknown argument {other}"),
        }
        i += 2;
    }
    let base = cfg.probe_base.unwrap_or_else(|| {
        // The idle speed: the best of a few probes a few seconds apart.
        let mut best = f64::MAX;
        for _ in 0..5 {
            best = best.min(probe());
            std::thread::sleep(Duration::from_secs(2));
        }
        best
    });
    eprintln!(
        "{} records, {}s + {}s warm-up a cell, threads {:?}, probe at idle {base:.1} ms",
        cfg.records,
        cfg.run.as_secs(),
        cfg.warmup.as_secs(),
        cfg.threads
    );
    let first_id = cfg.run_id.clone();
    for run in 1..=cfg.runs {
        if cfg.runs > 1 || first_id.is_empty() {
            cfg.run_id = format!("{}{run}", first_id);
        }
        for (n, name) in cfg.systems.clone().iter().enumerate() {
            if n > 0 || run > 1 {
                eprintln!("idle {} s before {name}", cfg.turn_gap.as_secs());
                std::thread::sleep(cfg.turn_gap);
            }
            run_system(name, &cfg, base, run);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(key: u64, s: u64, e: u64, fields: [u64; FIELDS]) -> Entry {
        Entry::read(key, s, e, vec![(key, fields)])
    }

    fn update(key: u64, field: u8, value: u64, s: u64, e: u64) -> Entry {
        Entry::Write {
            key,
            field,
            value,
            s,
            e,
            affected: 1,
        }
    }

    #[test]
    fn a_read_sees_the_last_write_or_one_beside_it() {
        let base = base_digests(7);
        let mut new = base;
        new[3] = 42;
        let w = || update(7, 3, 42, 10, 20);
        // After the write ended: its value, never the one before it.
        let ok = check(&[vec![w(), read(7, 30, 40, new)]], &mut Model::default());
        assert_eq!(ok.mismatches, 0);
        let stale = check(&[vec![w(), read(7, 30, 40, base)]], &mut Model::default());
        assert_eq!(stale.mismatches, 1);
        // Overlapping it, either.
        for got in [base, new] {
            let v = check(
                &[vec![w()], vec![read(7, 15, 25, got)]],
                &mut Model::default(),
            );
            assert_eq!(v.mismatches, 0);
        }
        // A write followed by another that ended before the read is gone.
        let v = check(
            &[vec![w(), update(7, 3, 43, 21, 25), read(7, 30, 40, new)]],
            &mut Model::default(),
        );
        assert_eq!(v.mismatches, 1);
        // Another field's or another record's value is no value of this one.
        let mut moved = base;
        moved[4] = 42;
        let v = check(&[vec![w(), read(7, 30, 40, moved)]], &mut Model::default());
        assert_eq!(v.mismatches, 1);
        let v = check(
            &[vec![Entry::read(7, 1, 2, vec![(8, base_digests(8))])]],
            &mut Model::default(),
        );
        assert_eq!(v.mismatches, 1);
    }

    #[test]
    fn the_model_carries_a_cell_into_the_next() {
        let mut model = Model::default();
        check(&[vec![update(7, 3, 42, 10, 20)]], &mut model);
        let mut new = base_digests(7);
        new[3] = 42;
        assert_eq!(check(&[vec![read(7, 1, 2, new)]], &mut model).mismatches, 0);
        assert_eq!(
            check(&[vec![read(7, 1, 2, base_digests(7))]], &mut model).mismatches,
            1
        );
    }

    #[test]
    fn a_scan_answers_every_key_it_could_see_in_order() {
        let row = |k: u64| (k, combine(&base_digests(k)));
        let scan = |rows: Vec<(u64, u64)>| Entry::Scan {
            key: 5,
            len: 3,
            seen: 100,
            next: 101,
            rows,
        };
        let mut m = Model::default();
        assert_eq!(
            check(&[vec![scan(vec![row(5), row(6), row(7)])]], &mut m).mismatches,
            0
        );
        assert_eq!(
            check(&[vec![scan(vec![row(5), row(7), row(8)])]], &mut m).mismatches,
            1
        );
        assert_eq!(
            check(&[vec![scan(vec![row(5), row(6)])]], &mut m).mismatches,
            1
        );
        assert_eq!(
            check(&[vec![scan(vec![row(5), row(6), (7, 1)])]], &mut m).mismatches,
            1
        );
    }

    #[test]
    fn the_servers_rows_are_read_as_written() {
        let v = record_values(3);
        let mut body = String::from("[{\"id\":3");
        for (i, f) in v.iter().enumerate() {
            body.push_str(&format!(",\"field{i}\":\"{f}\""));
        }
        body.push_str("}]");
        assert_eq!(json_rows(body.as_bytes()), vec![(3, base_digests(3))]);
    }
}
