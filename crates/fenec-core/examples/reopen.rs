//! What a crash costs the next open: `make reopen-bench`.
//!
//! ```text
//! cargo run --release -p fenec-core --example reopen -- write <path> <rows> <dim> [<checkpoint at>]
//! cargo run --release -p fenec-core --example reopen -- open <path> linked|deferred [<clients> <ms>]
//! ```
//!
//! `write` fills a file the way a server does and leaves it the way a crash
//! does: the graph as of the last checkpoint -- taken at `<checkpoint at>`
//! rows, or never -- and every write after it in the tail. `open` opens it
//! as a server did before (`linked`: the tail's vectors linked into the
//! graph before the open returns) or does now (`deferred`,
//! `fs::open_serving`: linked beside the queries, a slice at a time under
//! the write lock, as `fenec_http::link::beside` does), in a process of its
//! own, and times the open, `near` while the vectors wait, the linking --
//! with `<clients>` threads each sending a `near` every `<ms>` meanwhile,
//! and what they waited -- and `near` after.
//!
//! A client that asks again the moment it is answered keeps the read lock
//! taken: on Linux std's lock lets a waiting writer in first, but on macOS
//! readers slip past it, and four such clients kept the linking from
//! finishing in eleven minutes. Paced clients are what a server sees.

use fenec_core::prelude::*;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

struct Rng(u64);
impl Rng {
    fn f32(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        ((x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32) / ((1u32 << 24) as f32)
    }
    fn gauss(&mut self) -> f32 {
        (0..6).map(|_| self.f32()).sum::<f32>() - 3.0
    }
}

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// 64 centres, each vector one of them plus noise, as `bench` and `quant`
/// make them: row `i` is the same vector every run.
fn vector(centres: &[Vec<f32>], i: u64) -> Vec<f32> {
    let mut rng = Rng(splitmix(i) | 1);
    centres[(i % 64) as usize]
        .iter()
        .map(|x| x + rng.gauss() * 0.35)
        .collect()
}

fn centres(dim: usize) -> Vec<Vec<f32>> {
    let mut rng = Rng(0xDEAD_BEEF);
    (0..64)
        .map(|_| (0..dim).map(|_| rng.gauss()).collect())
        .collect()
}

const QUERY_BASE: u64 = 1 << 40;

fn write(path: &str, rows: u64, dim: usize, checkpoint_at: Option<u64>) {
    let _ = std::fs::remove_file(path);
    let centres = centres(dim);
    let mut db = fenec_core::fs::open(path).expect("open");
    let create = format!("create collection d (e vector<{dim}> @hnsw(cosine))");
    db.execute(&fenec_ql::parse_one(&create).unwrap()).unwrap();
    let t = Instant::now();
    let mut i = 0;
    while i < rows {
        let end = (i + 10_000).min(rows);
        let end = match checkpoint_at {
            Some(c) if i < c => end.min(c),
            _ => end,
        };
        let docs = (i..end)
            .map(|j| {
                vec![(
                    "e".to_string(),
                    Expr::Lit(Value::Vector(vector(&centres, j))),
                )]
            })
            .collect();
        db.execute(&Statement::Put {
            collection: "d".into(),
            docs,
        })
        .unwrap();
        i = end;
        if checkpoint_at == Some(i) {
            db.checkpoint().expect("checkpoint");
        }
    }
    db.sync().expect("sync");
    let tail = rows - checkpoint_at.unwrap_or(0);
    println!(
        "wrote {rows} x {dim} in {:.1} s: the graph checkpointed at {} rows, {tail} in the tail",
        t.elapsed().as_secs_f64(),
        checkpoint_at.unwrap_or(0)
    );
    // Dropped without a checkpoint, as a crash leaves it.
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn near(db: &Database, q: &[f32], exact: bool) -> Vec<u64> {
    let sql = match exact {
        true => "get d select id near e $1 exact limit 10",
        false => "get d select id near e $1 limit 10",
    };
    let r = db
        .query(
            &fenec_ql::parse_one(sql).unwrap(),
            &[Value::Vector(q.to_vec())],
        )
        .unwrap();
    r.rows().unwrap().rows.iter().map(|r| r.id).collect()
}

/// p50 and p99 of `near` over the queries, and recall@10 against the exact
/// scan's ten.
fn measure(db: &Database, queries: &[Vec<f32>], truth: &[Vec<u64>]) -> String {
    let mut lat = Vec::new();
    let mut hit = 0;
    for (q, t) in queries.iter().zip(truth) {
        let t0 = Instant::now();
        let got = near(db, q, false);
        lat.push(ms(t0.elapsed()));
        hit += got.iter().filter(|d| t.contains(d)).count();
    }
    format!(
        "near p50 {:.2} ms, p99 {:.2} ms, recall@10 {:.3}",
        pct(&mut lat, 0.5),
        pct(&mut lat, 0.99),
        hit as f64 / (10 * queries.len()) as f64
    )
}

fn open(path: &str, how: &str, clients: usize, every: Duration) {
    let t = Instant::now();
    let db = match how {
        "linked" => fenec_core::fs::open(path),
        "deferred" => fenec_core::fs::open_serving(path, true, Box::new(Ok)),
        other => panic!("open how? {other}"),
    }
    .expect("open");
    let opened = t.elapsed();
    let dim = db.stats()[0].vector_indexes[0].dim;
    let unlinked = db.unlinked();
    println!(
        "{how:<8} open {:6.2} s   {} documents, {unlinked} vectors not linked",
        opened.as_secs_f64(),
        db.stats()[0].documents,
    );

    let centres = centres(dim);
    let queries: Vec<Vec<f32>> = (0..200).map(|i| vector(&centres, QUERY_BASE + i)).collect();
    let t = Instant::now();
    let truth: Vec<Vec<u64>> = queries.iter().map(|q| near(&db, q, true)).collect();
    let exact_ms = ms(t.elapsed()) / queries.len() as f64;
    if unlinked > 0 {
        println!(
            "         while they wait  {}",
            measure(&db, &queries[..50], &truth[..50])
        );
    }

    let db = Arc::new(RwLock::new(db));
    if unlinked > 0 {
        // The clients' queries while the vectors are linked, as the server's
        // linking thread links them: slices that hold the write lock for
        // about 10 ms each.
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let clients: Vec<_> = (0..clients)
            .map(|c| {
                let (db, done, queries) = (Arc::clone(&db), Arc::clone(&done), queries.clone());
                std::thread::spawn(move || {
                    let mut lat = Vec::new();
                    let mut i = c * 50;
                    while !done.load(std::sync::atomic::Ordering::Relaxed) {
                        let t0 = Instant::now();
                        near(&db.read().unwrap(), &queries[i % queries.len()], false);
                        let took = t0.elapsed();
                        lat.push(ms(took));
                        i += 1;
                        std::thread::sleep(every.saturating_sub(took));
                    }
                    lat
                })
            })
            .collect();
        let t = Instant::now();
        let (mut waits, mut holds) = (Vec::new(), Vec::new());
        let mut nodes = 16;
        loop {
            let t0 = Instant::now();
            let mut g = db.write().unwrap();
            let t1 = Instant::now();
            let left = g.link_pending(nodes);
            let took = t1.elapsed();
            drop(g);
            waits.push(ms(t1 - t0));
            holds.push(ms(took));
            if left == 0 {
                break;
            }
            let pace = took.as_secs_f64() / nodes as f64;
            nodes = ((0.010 / pace.max(1e-6)) as usize).clamp(1, (2 * nodes).min(512));
            std::thread::yield_now();
        }
        let linking = t.elapsed();
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut lat: Vec<f64> = clients
            .into_iter()
            .flat_map(|c| c.join().unwrap())
            .collect();
        println!(
            "         linked in {:.1} s over {} slices: each held the lock {:.1} ms p50, {:.1} max; waited for it {:.1} ms p50, {:.1} max",
            linking.as_secs_f64(),
            holds.len(),
            pct(&mut holds, 0.5),
            pct(&mut holds, 1.0),
            pct(&mut waits, 0.5),
            pct(&mut waits, 1.0),
        );
        if !lat.is_empty() {
            println!(
                "         while linking    {} queries: near p50 {:.2} ms, p99 {:.2} ms, max {:.1} ms",
                lat.len(),
                pct(&mut lat, 0.5),
                pct(&mut lat, 0.99),
                pct(&mut lat, 1.0),
            );
        }
    }
    let g = db.read().unwrap();
    println!(
        "         after            {}",
        measure(&g, &queries, &truth)
    );
    println!("         (the exact scan: {exact_ms:.2} ms a query)");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).map(String::as_str).unwrap_or("");
    match arg(0) {
        "write" => write(
            arg(1),
            arg(2).parse().expect("rows"),
            arg(3).parse().expect("dim"),
            args.get(4).map(|s| s.parse().expect("checkpoint at")),
        ),
        "open" => open(
            arg(1),
            arg(2),
            args.get(3).map_or(0, |s| s.parse().expect("clients")),
            Duration::from_millis(args.get(4).map_or(10, |s| s.parse().expect("ms"))),
        ),
        _ => eprintln!(
            "reopen write <path> <rows> <dim> [<checkpoint at>] | reopen open <path> linked|deferred [<clients> <ms>]"
        ),
    }
}
