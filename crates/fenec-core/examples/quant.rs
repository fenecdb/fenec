//! Quantized vector indexes at scale: `make quant-bench`.
//!
//! ```text
//! cargo run --release -p fenec-core --example quant -- [N] [DIM] [none,int8,bit] [--rank R] [--f32] [--filter ROWS]
//! ```
//!
//! One database for each quantization, in turn, over the same N vectors,
//! stored as `vector<DIM, f16>` unless `--f32`. For each: the build, the heap
//! the process holds afterwards (a counting allocator: on a machine short of
//! memory the resident set is whatever the pager left), the arena, and
//! recall@10 and latency of `near` over held-out queries at beams of 100, 200
//! and 400, with how many documents' vectors a query read. The exact ten are
//! found once, by brute force over the vectors as the store holds them.
//!
//! `--filter ROWS` puts each row in a group of about ROWS, `g int @hash`,
//! and measures `near` over one group besides: a set under the ANN budget
//! of `ef x 2m`, which is searched without the walk.
//!
//! The vectors gather around 64 centres. By default each is its centre plus
//! noise in every dimension, the generator `bench` uses -- as many degrees of
//! freedom as dimensions, which real embeddings do not have, and which makes
//! the nearest ten of a crowd of equidistant points hard for any graph.
//! `--rank R` spreads them over R directions instead, with a little noise
//! beside: embeddings vary along far fewer directions than they have.
//!
//! A vector is generated from its own seed, so none is kept: the build and
//! the brute force each make them again, a range to a thread.

use fenec_core::codec::{f16_from_f32, f32_from_f16};
use fenec_core::prelude::*;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

struct Counting;

static HEAP: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            HEAP.fetch_add(l.size(), Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        HEAP.fetch_sub(l.size(), Ordering::Relaxed);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            HEAP.fetch_add(new, Ordering::Relaxed);
            HEAP.fetch_sub(l.size(), Ordering::Relaxed);
        }
        q
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn heap_mb() -> f64 {
    HEAP.load(Ordering::Relaxed) as f64 / 1e6
}

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

/// The data: 64 centres, each vector one of them plus noise, as `bench`
/// makes them -- `i` picks both, so any vector can be made on its own.
struct Data {
    centres: Vec<Vec<f32>>,
    /// `rank` directions, each `dim` long, when the spread is low-rank.
    basis: Vec<Vec<f32>>,
    half: bool,
}

impl Data {
    fn new(dim: usize, rank: usize, half: bool) -> Data {
        let mut rng = Rng(0xDEAD_BEEF);
        let centres = (0..64)
            .map(|_| (0..dim).map(|_| rng.gauss()).collect())
            .collect();
        let basis = (0..rank)
            .map(|_| (0..dim).map(|_| rng.gauss()).collect())
            .collect();
        Data {
            centres,
            basis,
            half,
        }
    }

    fn vector(&self, i: u64, out: &mut Vec<f32>) {
        let mut rng = Rng(splitmix(i) | 1);
        let c = &self.centres[(i % 64) as usize];
        out.clear();
        if self.basis.is_empty() {
            out.extend(c.iter().map(|x| x + rng.gauss() * 0.35));
            return;
        }
        // Along the basis, and a twentieth of that beside it.
        let scale = 0.35 * (c.len() as f32 / self.basis.len() as f32).sqrt();
        out.extend(c.iter().map(|x| x + rng.gauss() * 0.0175));
        for b in &self.basis {
            let z = rng.gauss() * scale / (c.len() as f32).sqrt();
            for (o, x) in out.iter_mut().zip(b) {
                *o += z * x;
            }
        }
    }

    /// The vector as the store holds it: through f16 when the field is.
    fn stored(&self, i: u64, out: &mut Vec<f32>) {
        self.vector(i, out);
        if self.half {
            for x in out.iter_mut() {
                *x = f32_from_f16(f16_from_f32(*x));
            }
        }
    }
}

const QUERY_BASE: u64 = 1 << 40;

fn unit(v: &[f32]) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    v.iter().map(|x| x / n).collect()
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0.0f32; 8];
    let ((ca, ra), (cb, rb)) = (a.as_chunks::<8>(), b.as_chunks::<8>());
    for (x, y) in ca.iter().zip(cb) {
        for k in 0..8 {
            acc[k] += x[k] * y[k];
        }
    }
    acc.iter().sum::<f32>() + ra.iter().zip(rb).map(|(x, y)| x * y).sum::<f32>()
}

/// The exact ten of each query, by cosine over the stored vectors: a range
/// of the rows to a thread, each keeping its own ten, merged at the end.
///
/// With `groups`, each query's exact ten within its own group follow the
/// queries' own, query `q` in group `q % groups` as row `i` is in `i % groups`.
fn truth(data: &Data, n: u64, queries: &[Vec<f32>], groups: u64) -> Vec<Vec<u64>> {
    let qs: Vec<Vec<f32>> = queries.iter().map(|q| unit(q)).collect();
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()) as u64;
    let per = n.div_ceil(threads);
    let parts: Vec<Vec<Vec<(f32, u64)>>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let qs = &qs;
                s.spawn(move || {
                    let lists = if groups > 0 { 2 } else { 1 };
                    let mut best: Vec<Vec<(f32, u64)>> = vec![Vec::new(); qs.len() * lists];
                    let mut v = Vec::new();
                    for i in t * per..((t + 1) * per).min(n) {
                        data.stored(i, &mut v);
                        let norm = dot(&v, &v).sqrt();
                        for (j, q) in qs.iter().enumerate() {
                            let sim = dot(q, &v) / norm;
                            let keep = |b: &mut Vec<(f32, u64)>| {
                                if b.len() < 10 || sim > b[9].0 {
                                    b.push((sim, i + 1));
                                    b.sort_by(|x, y| y.0.total_cmp(&x.0));
                                    b.truncate(10);
                                }
                            };
                            keep(&mut best[j]);
                            if groups > 0 && i % groups == j as u64 % groups {
                                keep(&mut best[qs.len() + j]);
                            }
                        }
                    }
                    best
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    (0..parts[0].len())
        .map(|q| {
            let mut all: Vec<(f32, u64)> = parts.iter().flat_map(|p| p[q].clone()).collect();
            all.sort_by(|x, y| y.0.total_cmp(&x.0));
            all.iter().take(10).map(|x| x.1).collect()
        })
        .collect()
}

/// How many documents' vectors the statement read, as `explain` states it:
/// "near: 12 of the 100 candidates of the codes read ...". None when the
/// index holds vectors and reads none.
fn reads(db: &Database, stmt: &Statement, params: &[Value]) -> Option<usize> {
    let Statement::Select(sel) = stmt else {
        return None;
    };
    let r = db.query(&Statement::Explain(sel.clone()), params).unwrap();
    r.rows()
        .unwrap()
        .rows
        .iter()
        .find_map(|row| match &row.values[..] {
            [Value::Text(s)] if s.contains("candidates of the codes") => s
                .trim_start_matches("near: ")
                .split(' ')
                .next()
                .and_then(|n| n.parse().ok()),
            _ => None,
        })
}

fn pct(v: &[f64], p: f64) -> f64 {
    v[((v.len() as f64 - 1.0) * p) as usize]
}

/// recall@10 of `near` over the queries against `want`, the latency of a
/// second pass, and the vectors a query read on average.
fn measure(
    db: &Database,
    near: &Statement,
    queries: &[Vec<f32>],
    want: &[Vec<u64>],
    group: impl Fn(usize) -> Option<Value>,
) -> String {
    let (mut found, mut read, mut lat) = (0, 0, Vec::new());
    for pass in 0..2 {
        for (i, (q, want)) in queries.iter().zip(want).enumerate() {
            let mut params = vec![Value::Vector(q.clone())];
            params.extend(group(i));
            let t = Instant::now();
            let r = db.query(near, &params).unwrap();
            let ms = t.elapsed().as_secs_f64() * 1e3;
            if pass == 0 {
                let rows = &r.rows().unwrap().rows;
                found += rows.iter().filter(|row| want.contains(&row.id)).count();
                read += reads(db, near, &params).unwrap_or(0);
            } else {
                lat.push(ms);
            }
        }
    }
    lat.sort_by(f64::total_cmp);
    format!(
        "recall@10 {:.3}   p50 {:.3} ms, p99 {:.3} ms   {:.1} vectors read",
        found as f64 / (queries.len() * 10) as f64,
        pct(&lat, 0.5),
        pct(&lat, 0.99),
        read as f64 / queries.len() as f64,
    )
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let n: u64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(100_000);
    let dim: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(768);
    let modes: Vec<String> = args
        .get(2)
        .map_or("none,int8,bit", String::as_str)
        .split(',')
        .map(str::to_string)
        .collect();
    let half = !args.iter().any(|a| a == "--f32");
    let rank: usize = args
        .iter()
        .position(|a| a == "--rank")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let rows: u64 = args
        .iter()
        .position(|a| a == "--filter")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let groups = match rows {
        0 => 0,
        r => (n / r).max(1),
    };
    let data = Data::new(dim, rank, half);
    let prec = if half { ", f16" } else { "" };
    let spread = match rank {
        0 => "in every dimension".to_string(),
        r => format!("along {r} directions"),
    };
    println!(
        "fenecdb {} -- {n} x {dim} vectors<{dim}{prec}>, 64 clusters spread {spread}, cosine\n",
        fenec_core::VERSION
    );

    let queries: Vec<Vec<f32>> = (0..200)
        .map(|i| {
            let mut v = Vec::new();
            data.vector(QUERY_BASE + i, &mut v);
            v
        })
        .collect();
    let t = Instant::now();
    let mut exact = truth(&data, n, &queries, groups);
    let within = exact.split_off(queries.len());
    println!(
        "exact ten   {} queries by brute force in {:.1} s\n",
        queries.len(),
        t.elapsed().as_secs_f64()
    );

    for mode in &modes {
        let quant = match mode.as_str() {
            "none" => String::new(),
            q => format!(", quant={q}"),
        };
        let base = heap_mb();
        let mut db = Database::new();
        let group = if groups > 0 { "g int @hash, " } else { "" };
        let create =
            format!("create collection docs ({group}v vector<{dim}{prec}> @hnsw(cosine{quant}))");
        db.execute(&fenec_ql::parse_one(&create).unwrap()).unwrap();

        let t = Instant::now();
        let batch = 10_000u64;
        let mut i = 0;
        while i < n {
            let docs = (i..(i + batch).min(n))
                .map(|j| {
                    let mut v = Vec::with_capacity(dim);
                    data.vector(j, &mut v);
                    let mut doc = vec![("v".to_string(), Expr::Lit(Value::Vector(v)))];
                    if groups > 0 {
                        let g = Value::Int((j % groups) as i64);
                        doc.push(("g".to_string(), Expr::Lit(g)));
                    }
                    doc
                })
                .collect();
            db.execute(&Statement::Put {
                collection: "docs".into(),
                docs,
            })
            .unwrap();
            i += batch;
        }
        let build = t.elapsed().as_secs_f64();
        let st = &db.stats()[0];
        let arena = st.vector_indexes[0].arena_bytes as f64 / 1e6;
        println!(
            "{mode:<5} build {build:6.1} s   heap {:6.0} MB, of it the arena {arena:6.1} MB",
            heap_mb() - base,
        );
        for ef in [100, 200, 400] {
            let near =
                fenec_ql::parse_one(&format!("get docs select id near v $1 ef {ef} limit 10"))
                    .unwrap();
            let run = measure(&db, &near, &queries, &exact, |_| None);
            println!("      ef {ef:3}   {run}");
        }
        if let Some(per) = n.checked_div(groups) {
            let near =
                fenec_ql::parse_one("get docs select id where g = $2 near v $1 ef 100 limit 10")
                    .unwrap();
            let run = measure(&db, &near, &queries, &within, |q| {
                Some(Value::Int((q as u64 % groups) as i64))
            });
            println!("      g = one of {groups}, {per} rows, ef 100   {run}");
        }
        drop(db);
    }
}
