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
//! Options: `--records 1000000 --updates 5000000 --mode buffered|durable
//! --auto on|off --writers 1 --after 30 --dir <scratch>`. Durable is a
//! flush and its fsync after each write, outside the lock, with enough
//! writers to share the fsyncs; buffered a flush every 250 ms, as
//! `--sync 250` does.

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
    if o.auto {
        let (db, stop, compacting, swaps) = (
            Arc::clone(&db),
            Arc::clone(&stop),
            Arc::clone(&compacting),
            Arc::clone(&swaps),
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
                if let Some(r) = r {
                    r.unwrap();
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
        let (db, sets, updates, updating) = (
            Arc::clone(&db),
            Arc::clone(&sets),
            Arc::clone(&updates),
            Arc::clone(&updating),
        );
        let (records, total, durable) = (o.records, o.updates, o.durable);
        writers.push(std::thread::spawn(move || {
            let mut rng = Rng(0x1234_5678 + w as u64 * 7919);
            let mut args = vec![Value::Null, Value::Null];
            while updates.fetch_add(1, Ordering::Relaxed) < total {
                args[0] = Value::Int(1 + rng.below(records) as i64);
                args[1] = Value::Text(rng.value());
                let f = rng.below(FIELDS as u64) as usize;
                let d = {
                    let mut g = db.write().unwrap();
                    g.execute_with(&sets[f], &args).unwrap();
                    match durable {
                        true => g.flush().unwrap(),
                        false => None,
                    }
                };
                if let Some(d) = d {
                    d().unwrap();
                }
            }
            updating.store(false, Ordering::Relaxed);
        }));
    }
    // The reader: whole records by key, its latencies sent a second at a time.
    let (tx, rx) = mpsc::channel::<Vec<u32>>();
    {
        let (db, stop) = (Arc::clone(&db), Arc::clone(&stop));
        let records = o.records;
        threads.push(std::thread::spawn(move || {
            let get = fenec_ql::parse_one("get usertable where id = $1").unwrap();
            let mut rng = Rng(0xfeed);
            let mut mine = Vec::with_capacity(1 << 20);
            let mut since = Instant::now();
            let mut args = [Value::Null];
            while !stop.load(Ordering::Relaxed) {
                args[0] = Value::Int(1 + rng.below(records) as i64);
                let t = Instant::now();
                let r = db.read().unwrap().query(&get, &args).unwrap();
                mine.push(t.elapsed().as_nanos().min(u32::MAX as u128) as u32);
                drop(r);
                if since.elapsed() >= Duration::from_millis(100) {
                    since = Instant::now();
                    let _ = tx.send(std::mem::replace(&mut mine, Vec::with_capacity(1 << 18)));
                }
            }
        }));
    }

    println!("t\tfile_MB\tlive_MB\tupdates_s\treads_s\tread_p50_us\tread_p99_us\tread_max_us\tcompacting\tcompactions");
    let start = Instant::now();
    let (mut during, mut beside, mut after) = (Vec::new(), Vec::new(), Vec::new());
    let mut updated_at: Option<Instant> = None;
    let mut last_updates = 0;
    let mut second = 1;
    loop {
        let mut window: Vec<u32> = Vec::new();
        let mut was_compacting = false;
        while start.elapsed() < Duration::from_secs(second) {
            if let Ok(v) = rx.recv_timeout(Duration::from_millis(20)) {
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
    drop(db);
    let _ = std::fs::remove_file(&path);
}
