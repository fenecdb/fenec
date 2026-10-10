//! A file under updates, compacted on its own or not:
//! `cargo run --release -p fenec-core --example compaction -- [options]`
//!
//! YCSB's records -- ten fields of 100 random characters, about 1 KB --
//! loaded, then updated a field at a time at keys drawn uniformly, while
//! one thread reads whole records by key as fast as it can; then the reads
//! alone for a while. Every second a line: the file's size and what it
//! holds live, the updates and reads that second, the reads' p50, p99 and
//! longest, and whether a compact ran. With `--auto on` a thread looks
//! every 5 s and compacts beside the database as `compact_when_due` says
//! (the policy every host applies); with `--auto off` nothing does. At the
//! end, the reads' percentiles while a compact ran, while none did, and
//! after the updates, and how long each compact held the write lock.
//!
//! The updates are also counted in 100 ms buckets, with how long the
//! writers waited for the write lock and held it: at the end, the update
//! rate while a compact ran -- the median bucket and the worst -- against
//! the rate while none did, and with `--series <file>` every bucket as a
//! line, the reads' p99 and longest beside it.
//!
//! Options: `--records 1000000 --updates 5000000 --mode buffered|durable
//! --auto on|off --writers 1 --after 30 --dir <scratch> --series <file>`.
//! Durable is a flush and its fsync after each write, outside the lock,
//! with enough writers to share the fsyncs; buffered a flush every 250 ms,
//! as `--sync 250` does.

use fenec_core::engine::{compact_when_due, CompactPolicy};
use fenec_core::prelude::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

const FIELDS: usize = 10;
const FIELD_LEN: usize = 100;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        ((self.next() as u128 * n as u128) >> 64) as u64
    }
    fn value(&mut self) -> String {
        const ABC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        (0..FIELD_LEN)
            .map(|_| ABC[self.below(ABC.len() as u64) as usize] as char)
            .collect()
    }
}

struct Opts {
    records: u64,
    updates: u64,
    durable: bool,
    auto: bool,
    writers: usize,
    after: u64,
    dir: std::path::PathBuf,
    series: Option<std::path::PathBuf>,
}

/// A bucket's length: the series' step.
const BUCKET: Duration = Duration::from_millis(100);

/// What the writers did in one bucket: the updates, and the nanoseconds
/// they waited for the write lock and held it, in all and at the most, and
/// held it in updates of a millisecond or more -- the stalls.
#[derive(Default, Clone, Copy)]
struct Bucket {
    updates: u64,
    wait: u64,
    hold: u64,
    max_wait: u64,
    max_hold: u64,
    stalled: u64,
}

impl Bucket {
    fn add(&mut self, o: &Bucket) {
        self.updates += o.updates;
        self.wait += o.wait;
        self.hold += o.hold;
        self.max_wait = self.max_wait.max(o.max_wait);
        self.max_hold = self.max_hold.max(o.max_hold);
        self.stalled += o.stalled;
    }
}

/// The writers' buckets, by index from the start.
#[derive(Default)]
struct Series(Mutex<Vec<Bucket>>);

impl Series {
    fn add(&self, at: usize, b: &Bucket) {
        let mut v = self.0.lock().unwrap();
        if v.len() <= at {
            v.resize(at + 1, Bucket::default());
        }
        v[at].add(b);
    }
}

fn opts() -> Opts {
    let mut o = Opts {
        records: 1_000_000,
        updates: 5_000_000,
        durable: false,
        auto: true,
        writers: 1,
        after: 30,
        dir: std::env::temp_dir().join("fenec-compaction"),
        series: None,
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let v = args.get(i + 1).cloned().unwrap_or_default();
        match args[i].as_str() {
            "--records" => o.records = v.parse().unwrap(),
            "--updates" => o.updates = v.parse().unwrap(),
            "--mode" => o.durable = v == "durable",
            "--auto" => o.auto = v == "on",
            "--writers" => o.writers = v.parse().unwrap(),
            "--after" => o.after = v.parse().unwrap(),
            "--dir" => o.dir = v.into(),
            "--series" => o.series = Some(v.into()),
            a => panic!("unknown option {a}"),
        }
        i += 2;
    }
    o
}

/// Microseconds at the given share of `v`, sorted in place.
fn pct(v: &mut [u32], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_unstable();
    v[((v.len() - 1) as f64 * p).round() as usize] as f64 / 1e3
}

fn summary(label: &str, v: &mut [u32]) {
    let (p50, p99) = (pct(v, 0.5), pct(v, 0.99));
    let max = v.last().copied().unwrap_or(0) as f64 / 1e3;
    println!(
        "# {label}: {} reads, p50 {p50:.1} us, p99 {p99:.1} us, max {max:.1} us",
        v.len()
    );
}

/// The update rate a bucket at a time: while a compact ran -- the buckets
/// wholly inside one -- against while none did, up to the last whole
/// bucket of updates; and with `--series`, every bucket as a line.
fn rates(series: &[Bucket], spans: &[(Duration, Duration)], reads: &[(f64, f64, usize)], o: &Opts) {
    let per_s = |b: &Bucket| b.updates as f64 / BUCKET.as_secs_f64();
    let bounds = |i: usize| (BUCKET * i as u32, BUCKET * (i as u32 + 1));
    let inside = |i: usize| {
        let (from, to) = bounds(i);
        spans.iter().any(|&(s, e)| s <= from && to <= e)
    };
    let touches = |i: usize| {
        let (from, to) = bounds(i);
        spans.iter().any(|&(s, e)| s < to && from < e)
    };
    // The bucket the writers finished in is cut short.
    let last = series.len().saturating_sub(1);
    let (mut during, mut beside) = (Vec::new(), Vec::new());
    for (i, b) in series[..last].iter().enumerate() {
        match (inside(i), touches(i)) {
            (true, _) => during.push(per_s(b)),
            (false, false) => beside.push(per_s(b)),
            _ => {}
        }
    }
    let median = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v.get(v.len() / 2).copied().unwrap_or(0.0)
    };
    let worst = during.iter().cloned().fold(f64::INFINITY, f64::min);
    println!(
        "# updates while a compact ran: {} buckets of {} ms, {:.0}/s at the median, {:.0}/s in the worst",
        during.len(),
        BUCKET.as_millis(),
        median(&mut during),
        if worst.is_finite() { worst } else { 0.0 }
    );
    println!(
        "# updates while none ran: {} buckets, {:.0}/s at the median",
        beside.len(),
        median(&mut beside)
    );
    for (s, e) in spans {
        println!(
            "# a compact from {:.1} s to {:.1} s: {:.2} s",
            s.as_secs_f64(),
            e.as_secs_f64(),
            (*e - *s).as_secs_f64()
        );
    }
    let Some(path) = &o.series else {
        return;
    };
    let mut out = String::from(
        "t_s\tupdates_s\twait_ms\thold_ms\tmax_wait_ms\tmax_hold_ms\tstalled_ms\treads\tread_p99_us\tread_max_us\tcompacting\n",
    );
    for i in 0..series.len().max(reads.len()) {
        let b = series.get(i).copied().unwrap_or_default();
        let (p99, max, n) = reads.get(i).copied().unwrap_or((0.0, 0.0, 0));
        out.push_str(&format!(
            "{:.1}\t{:.0}\t{:.1}\t{:.1}\t{:.2}\t{:.2}\t{:.1}\t{n}\t{p99:.1}\t{max:.0}\t{}\n",
            bounds(i).0.as_secs_f64(),
            per_s(&b),
            b.wait as f64 / 1e6,
            b.hold as f64 / 1e6,
            b.max_wait as f64 / 1e6,
            b.max_hold as f64 / 1e6,
            b.stalled as f64 / 1e6,
            touches(i) as u8,
        ));
    }
    std::fs::write(path, out).unwrap();
}

fn main() {
    let o = opts();
    std::fs::create_dir_all(&o.dir).unwrap();
    let path = o.dir.join("compaction.fenec");
    let _ = std::fs::remove_file(&path);
    let mut db = fenec_core::fs::open(&path).unwrap();
    let fields: Vec<String> = (0..FIELDS).map(|i| format!("field{i} text")).collect();
    db.execute(
        &fenec_ql::parse_one(&format!(
            "create collection usertable ({})",
            fields.join(", ")
        ))
        .unwrap(),
    )
    .unwrap();
    let t = Instant::now();
    let mut rng = Rng(0x5943_5342);
    let mut key = 1;
    while key <= o.records {
        let end = (key + 1_000).min(o.records + 1);
        let docs = (key..end)
            .map(|k| {
                let mut d = vec![("id".to_string(), Expr::Lit(Value::Int(k as i64)))];
                for i in 0..FIELDS {
                    d.push((format!("field{i}"), Expr::Lit(Value::Text(rng.value()))));
                }
                d
            })
            .collect();
        db.execute(&Statement::Put {
            collection: "usertable".into(),
            docs,
            insert: true,
            if_absent: false,
            docs_param: None,
            else_set: None,
            require: None,
        })
        .unwrap();
        key = end;
    }
    db.sync().unwrap();
    println!(
        "# loaded {} records in {:.1} s, the file {:.0} MB; {} updates, {}, auto-compact {}, {} writer(s)",
        o.records,
        t.elapsed().as_secs_f64(),
        db.garbage().file as f64 / 1e6,
        o.updates,
        if o.durable { "durable" } else { "buffered" },
        if o.auto { "on" } else { "off" },
        o.writers
    );
    let db = Arc::new(RwLock::new(db));
    let stop = Arc::new(AtomicBool::new(false));
    let updating = Arc::new(AtomicBool::new(true));
    let compacting = Arc::new(AtomicBool::new(false));
    let updates = Arc::new(AtomicU64::new(0));
    let series = Arc::new(Series::default());
    // When each compact began and ended, from the start.
    let spans: Arc<Mutex<Vec<(Duration, Duration)>>> = Arc::default();
    let start = Instant::now();
    let mut threads = Vec::new();

    // Buffered: the bytes go to disk every 250 ms, the fsync outside the lock.
    if !o.durable {
        let (db, stop) = (Arc::clone(&db), Arc::clone(&stop));
        threads.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(250));
                let d = db.write().unwrap().flush().unwrap();
                if let Some(d) = d {
                    d().unwrap();
                }
            }
        }));
    }
    // The compactor: a look every 5 s, as the server and the library look.
    let swaps = Arc::new(Mutex::new(Vec::new()));
    // The looks that found a compact due and passed it over.
    let passed = Arc::new(AtomicU64::new(0));
    if o.auto {
        let (db, stop, compacting, swaps, spans, passed) = (
            Arc::clone(&db),
            Arc::clone(&stop),
            Arc::clone(&compacting),
            Arc::clone(&swaps),
            Arc::clone(&spans),
            Arc::clone(&passed),
        );
        threads.push(std::thread::spawn(move || {
            let policy = CompactPolicy::default();
            let mut slept = Duration::ZERO;
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
                slept += Duration::from_millis(100);
                if slept < fenec_core::engine::AUTO_COMPACT_EVERY {
                    continue;
                }
                slept = Duration::ZERO;
                if !db.read().unwrap().compact_due(&policy) {
                    continue;
                }
                let before = db.read().unwrap().garbage();
                compacting.store(true, Ordering::Relaxed);
                let t = Instant::now();
                let r = compact_when_due(&db, &policy);
                let took = t.elapsed();
                compacting.store(false, Ordering::Relaxed);
                let Some(r) = r else {
                    // Due, and the look found the lock taken.
                    passed.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                {
                    r.unwrap();
                    spans.lock().unwrap().push((t - start, t - start + took));
                    let g = db.read().unwrap();
                    let held = g.last_compact_held();
                    println!(
                        "# compact: {:.0} -> {:.0} MB in {:.2} s, the write lock held {:.1} ms",
                        before.file as f64 / 1e6,
                        g.garbage().file as f64 / 1e6,
                        took.as_secs_f64(),
                        held.as_secs_f64() * 1e3
                    );
                    swaps.lock().unwrap().push(held.as_secs_f64() * 1e3);
                }
            }
        }));
    }
    // The writers: a field of a record at a time.
    let sets: Arc<Vec<Statement>> = Arc::new(
        (0..FIELDS)
            .map(|i| {
                fenec_ql::parse_one(&format!("set usertable {{field{i}: $2}} where id = $1"))
                    .unwrap()
            })
            .collect(),
    );
    let mut writers = Vec::new();
    for w in 0..o.writers {
        let (db, sets, updates, updating, series) = (
            Arc::clone(&db),
            Arc::clone(&sets),
            Arc::clone(&updates),
            Arc::clone(&updating),
            Arc::clone(&series),
        );
        let (records, total, durable) = (o.records, o.updates, o.durable);
        writers.push(std::thread::spawn(move || {
            let mut rng = Rng(0x1234_5678 + w as u64 * 7919);
            let mut args = vec![Value::Null, Value::Null];
            let (mut at, mut bucket) = (0, Bucket::default());
            while updates.fetch_add(1, Ordering::Relaxed) < total {
                args[0] = Value::Int(1 + rng.below(records) as i64);
                args[1] = Value::Text(rng.value());
                let f = rng.below(FIELDS as u64) as usize;
                let asked = Instant::now();
                let (d, taken) = {
                    let mut g = db.write().unwrap();
                    let taken = Instant::now();
                    g.execute_with(&sets[f], &args).unwrap();
                    let d = match durable {
                        true => g.flush().unwrap(),
                        false => None,
                    };
                    (d, taken)
                };
                let held = Instant::now();
                if let Some(d) = d {
                    d().unwrap();
                }
                let done = Instant::now();
                let now = ((done - start).as_nanos() / BUCKET.as_nanos()) as usize;
                if now != at {
                    series.add(at, &bucket);
                    (at, bucket) = (now, Bucket::default());
                }
                let wait = (taken - asked).as_nanos() as u64;
                let hold = (held - taken).as_nanos() as u64;
                bucket.add(&Bucket {
                    updates: 1,
                    wait,
                    hold,
                    max_wait: wait,
                    max_hold: hold,
                    stalled: if hold >= 1_000_000 { hold } else { 0 },
                });
            }
            series.add(at, &bucket);
            updating.store(false, Ordering::Relaxed);
        }));
    }
    // The reader: whole records by key, its latencies sent a second at a time.
    let (tx, rx) = mpsc::channel::<(usize, Vec<u32>)>();
    {
        let (db, stop) = (Arc::clone(&db), Arc::clone(&stop));
        let records = o.records;
        threads.push(std::thread::spawn(move || {
            let get = fenec_ql::parse_one("get usertable where id = $1").unwrap();
            let mut rng = Rng(0xfeed);
            let mut mine = Vec::with_capacity(1 << 20);
            let mut at = 0;
            let mut args = [Value::Null];
            while !stop.load(Ordering::Relaxed) {
                args[0] = Value::Int(1 + rng.below(records) as i64);
                let t = Instant::now();
                let r = db.read().unwrap().query(&get, &args).unwrap();
                let done = Instant::now();
                mine.push((done - t).as_nanos().min(u32::MAX as u128) as u32);
                drop(r);
                // A batch a bucket, sent as the next begins.
                let now = ((done - start).as_nanos() / BUCKET.as_nanos()) as usize;
                if now != at {
                    let v = std::mem::replace(&mut mine, Vec::with_capacity(1 << 18));
                    let _ = tx.send((at, v));
                    at = now;
                }
            }
        }));
    }

    println!("t\tfile_MB\tlive_MB\tupdates_s\treads_s\tread_p50_us\tread_p99_us\tread_max_us\tcompacting\tcompactions");
    let (mut during, mut beside, mut after) = (Vec::new(), Vec::new(), Vec::new());
    // The reads' p99 and longest a bucket, in microseconds, and how many.
    let mut read_buckets: Vec<(f64, f64, usize)> = Vec::new();
    let mut updated_at: Option<Instant> = None;
    let mut last_updates = 0;
    let mut second = 1;
    loop {
        let mut window: Vec<u32> = Vec::new();
        let mut was_compacting = false;
        while start.elapsed() < Duration::from_secs(second) {
            if let Ok((at, mut v)) = rx.recv_timeout(Duration::from_millis(20)) {
                if read_buckets.len() <= at {
                    read_buckets.resize(at + 1, (0.0, 0.0, 0));
                }
                let max = v.iter().max().copied().unwrap_or(0) as f64 / 1e3;
                read_buckets[at] = (pct(&mut v, 0.99), max, v.len());
                let c = compacting.load(Ordering::Relaxed);
                was_compacting |= c;
                match (updated_at.is_some(), c) {
                    (true, _) => after.extend_from_slice(&v),
                    (false, true) => during.extend_from_slice(&v),
                    (false, false) => beside.extend_from_slice(&v),
                }
                window.extend(v);
            }
        }
        let g = db.read().unwrap().garbage();
        let compactions = db.read().unwrap().compactions();
        let done = updates.load(Ordering::Relaxed).min(o.updates);
        let n = window.len();
        let max = window.iter().max().copied().unwrap_or(0) as f64 / 1e3;
        println!(
            "{second}\t{:.0}\t{:.0}\t{}\t{n}\t{:.1}\t{:.1}\t{max:.0}\t{}\t{compactions}",
            g.file as f64 / 1e6,
            g.live as f64 / 1e6,
            done - last_updates,
            pct(&mut window, 0.5),
            pct(&mut window, 0.99),
            was_compacting as u8,
        );
        last_updates = done;
        second += 1;
        if updated_at.is_none()
            && !updating.load(Ordering::Relaxed)
            && writers.iter().all(|w| w.is_finished())
        {
            updated_at = Some(Instant::now());
            println!(
                "# the updates are done after {:.1} s",
                start.elapsed().as_secs_f64()
            );
        }
        if updated_at.is_some_and(|t| t.elapsed() >= Duration::from_secs(o.after)) {
            break;
        }
    }
    stop.store(true, Ordering::Relaxed);
    for w in writers {
        w.join().unwrap();
    }
    for t in threads {
        t.join().unwrap();
    }
    summary("reads while a compact ran", &mut during);
    summary("reads beside the updates, no compact running", &mut beside);
    summary("reads after the updates", &mut after);
    println!(
        "# {} looks found a compact due and passed it over",
        passed.load(Ordering::Relaxed)
    );
    let swaps = swaps.lock().unwrap();
    if !swaps.is_empty() {
        let most = swaps.iter().cloned().fold(0.0, f64::max);
        println!(
            "# {} compacts, the write lock held {:.1} ms at the most, {:.1} on average",
            swaps.len(),
            most,
            swaps.iter().sum::<f64>() / swaps.len() as f64
        );
    }
    let g = db.read().unwrap().garbage();
    println!(
        "# at the end the file is {:.0} MB, {:.0} MB of it live",
        g.file as f64 / 1e6,
        g.live as f64 / 1e6
    );
    rates(
        &series.0.lock().unwrap(),
        &spans.lock().unwrap(),
        &read_buckets,
        &o,
    );
    drop(db);
    let _ = std::fs::remove_file(&path);
}
