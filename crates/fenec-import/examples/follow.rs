//! What following costs, against a live server (`make follow-bench`, after
//! `make pgvector-up`):
//!
//! - how long a committed change takes to show in the mirror, one row a
//!   transaction, one transaction at a time;
//! - how fast a follower drains a burst: one transaction of many rows, and
//!   many transactions of one row;
//! - how long a stream the server cut takes to carry changes again.
//!
//! The mirror is a file, so every confirmation waits for a real fsync.
//!
//! ```text
//! cargo run --release -p fenec-import --example follow -- [rows] [dim]
//! ```

use fenec_core::prelude::*;
use fenec_import::follow::{self, Event, Follow};
use fenec_import::pg::Url;
use fenec_import::Options;
use fenec_pg::client::Client;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

const URL: &str = "postgres://postgres:fenec@127.0.0.1:55432/fenecbench";
const T: &str = "fenec_follow_bench";

fn url() -> Url {
    Url::parse(&std::env::var("FENEC_TEST_PG_URL").unwrap_or_else(|_| URL.into())).unwrap()
}

fn title(db: &RwLock<Database>, id: DocId) -> Option<String> {
    let g = db.read().unwrap();
    let c = g.collection(T).ok()?;
    match c.store.read(&c.schema, id).ok()??.get("title") {
        Some(Value::Text(s)) => Some(s.clone()),
        _ => None,
    }
}

/// Waits until row `id` has `want` as its title; how long that took.
fn until(db: &RwLock<Database>, id: DocId, want: &str, t0: Instant) -> Duration {
    loop {
        if title(db, id).as_deref() == Some(want) {
            return t0.elapsed();
        }
        if t0.elapsed() > Duration::from_secs(60) {
            panic!("row {id} never became `{want}`");
        }
        std::thread::sleep(Duration::from_micros(100));
    }
}

fn pct(sorted: &[Duration], p: f64) -> f64 {
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i].as_secs_f64() * 1e3
}

fn main() {
    let args: Vec<usize> = std::env::args()
        .skip(1)
        .filter_map(|a| a.parse().ok())
        .collect();
    let rows = args.first().copied().unwrap_or(10_000);
    let dim = args.get(1).copied().unwrap_or(384);

    let mut c = Client::connect(&url()).expect("could not connect (make pgvector-up?)");
    for q in [
        "create extension if not exists vector".to_string(),
        format!("select pg_drop_replication_slot('{T}') from pg_replication_slots where slot_name = '{T}'"),
        format!("drop publication if exists {T}"),
        format!("drop table if exists {T}"),
        format!("create table {T} (id bigint primary key, title text, embed vector({dim}))"),
        format!(
            "insert into {T} select g, 'row ' || g, \
             (select array_agg(sin(g * 7919 + i))::vector({dim}) from generate_series(1, {dim}) i) \
             from generate_series(1, {rows}) g"
        ),
    ] {
        c.query(&q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }

    let dir = std::env::temp_dir().join(format!("fenec-follow-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("mirror.fenec");
    let db = Arc::new(RwLock::new(
        fenec_core::fs::open(path.to_str().unwrap()).unwrap(),
    ));
    let stop = Arc::new(AtomicBool::new(false));
    let events = Arc::new(Mutex::new(Vec::<(Instant, String)>::new()));
    let t0 = Instant::now();
    let follower = {
        let (db, stop, events) = (Arc::clone(&db), Arc::clone(&stop), Arc::clone(&events));
        std::thread::spawn(move || {
            let f = Follow {
                slot: T.into(),
                publication: T.into(),
            };
            let mut opts = Options::new(T);
            let spec = VectorIndexSpec {
                metric: Metric::Cosine,
                ..VectorIndexSpec::default()
            };
            opts.indexes.push(("embed".into(), IndexKind::Vector(spec)));
            let mut report = |e: Event| {
                if !matches!(e, Event::Confirmed { .. } | Event::CopyProgress(_)) {
                    events
                        .lock()
                        .unwrap()
                        .push((Instant::now(), format!("{e:?}")));
                }
            };
            follow::run(&url(), T, &db, &opts, &f, &stop, &mut report).unwrap();
        })
    };
    let streaming = |events: &Mutex<Vec<(Instant, String)>>| {
        events
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, e)| e == "Streaming")
            .count()
    };
    while streaming(&events) == 0 {
        std::thread::sleep(Duration::from_millis(5));
    }
    let copied = t0.elapsed();
    println!("{rows} rows x {dim}, mirrored into a file\n");
    println!(
        "copy + index              {:>9.1} ms",
        copied.as_secs_f64() * 1e3
    );

    // One row a transaction: commit, then wait for it in the mirror.
    let n = 300;
    let mut lat = Vec::with_capacity(n);
    for i in 0..n {
        let id = 1 + (i * 37 % rows) as DocId;
        let want = format!("single {i}");
        c.query(&format!("update {T} set title = '{want}' where id = {id}"))
            .unwrap();
        // The update's commit has returned: the clock starts here.
        lat.push(until(&db, id, &want, Instant::now()));
    }
    lat.sort();
    println!(
        "commit -> visible          p50 {:.2} ms, p99 {:.2} ms, max {:.2} ms  ({n} transactions)",
        pct(&lat, 0.5),
        pct(&lat, 0.99),
        pct(&lat, 1.0)
    );

    // One transaction of every row.
    let t = Instant::now();
    c.query(&format!("update {T} set title = 'burst ' || id"))
        .unwrap();
    let committed = t.elapsed();
    let drained = until(&db, rows as DocId, &format!("burst {rows}"), t);
    println!(
        "one transaction, {rows} rows  {:>7.1} ms after its commit, {:.0} rows/s",
        (drained - committed).as_secs_f64() * 1e3,
        rows as f64 / (drained - committed).as_secs_f64()
    );

    // Many transactions of one row, as fast as one client commits them.
    let m = 2_000.min(rows);
    let t = Instant::now();
    for i in 1..=m {
        c.query(&format!("update {T} set title = 'many {i}' where id = {i}"))
            .unwrap();
    }
    let committed = t.elapsed();
    let drained = until(&db, m as DocId, &format!("many {m}"), t);
    println!(
        "{m} transactions of one row  committed in {:.0} ms, all visible {:.1} ms after the last",
        committed.as_secs_f64() * 1e3,
        (drained - committed).as_secs_f64() * 1e3
    );

    // The server cuts the stream; the next change waits out the reconnect.
    c.query(
        "select pg_terminate_backend(pid) from pg_stat_replication \
         where application_name = 'fenec-follow'",
    )
    .unwrap();
    let t = Instant::now();
    c.query(&format!(
        "update {T} set title = 'after the cut' where id = 1"
    ))
    .unwrap();
    let back = until(&db, 1, "after the cut", t);
    println!(
        "after a cut connection     visible in {:.0} ms (the first retry waits 500 ms)",
        back.as_secs_f64() * 1e3
    );

    stop.store(true, Ordering::SeqCst);
    follower.join().unwrap();
    c.query(&format!("select pg_drop_replication_slot('{T}')"))
        .unwrap();
    c.query(&format!("drop publication {T}")).unwrap();
    c.query(&format!("drop table {T}")).unwrap();
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
