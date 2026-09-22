//! Measures the ef / recall trade-off:
//! `cargo run --release -p fenec-core --example sweep -- [N] [DIM]`
//!
//! Two data distributions are compared:
//!   * uniform random -- the hardest case for ANN (in high dimensions the
//!     distances converge and there is no neighbourhood structure)
//!   * clustered      -- resembles real embedding distributions
//!
//! The latency column is the **mean** over 50 cold queries, not a p50: it is
//! there to show the shape of the trade-off. For the quotable per-query
//! numbers use `--example bench -- N DIM --ef E`, which reports p50/p95/p99.

use fenec_core::schema::{Metric, VectorIndexSpec};
use fenec_core::vector::VectorIndex;
use std::time::Instant;

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

fn build(n: usize, dim: usize, clustered: bool, efc: usize) -> (VectorIndex, Vec<Vec<f32>>, f64) {
    let spec = VectorIndexSpec {
        metric: Metric::Cosine,
        m: 16,
        ef_construction: efc,
        ef_search: 64,
        ..VectorIndexSpec::default()
    };
    let mut ix = VectorIndex::new(dim, spec);
    ix.reserve(n);
    let mut rng = Rng(12345);

    // in clustered data, spread around 64 centres
    let centers: Vec<Vec<f32>> = (0..64)
        .map(|_| (0..dim).map(|_| rng.gauss()).collect())
        .collect();

    let mut queries = Vec::new();
    let t = Instant::now();
    for i in 0..n {
        let v: Vec<f32> = if clustered {
            let c = &centers[i % centers.len()];
            c.iter().map(|x| x + rng.gauss() * 0.35).collect()
        } else {
            (0..dim).map(|_| rng.f32() - 0.5).collect()
        };
        if i % (n / 50).max(1) == 0 && queries.len() < 50 {
            queries.push(v.clone());
        }
        ix.insert(i as u64, &v);
    }
    let secs = t.elapsed().as_secs_f64();
    (ix, queries, secs)
}

fn recall(ix: &VectorIndex, queries: &[Vec<f32>], k: usize, ef: usize) -> (f64, f64) {
    let mut hits = 0usize;
    let mut total = 0usize;
    let mut lat = 0f64;
    for q in queries {
        let t = Instant::now();
        let a = ix.search(q, k, Some(ef), |_| true);
        lat += t.elapsed().as_secs_f64() * 1000.0;
        let e = ix.search_exact(q, k, |_| true);
        let ids: Vec<u64> = e.iter().map(|x| x.0).collect();
        hits += a.iter().filter(|x| ids.contains(&x.0)).count();
        total += ids.len();
    }
    (
        hits as f64 / total as f64 * 100.0,
        lat / queries.len() as f64,
    )
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let n: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(50_000);
    let dim: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(128);

    for (label, clustered) in [
        ("uniform random", false),
        ("clustered (embedding-like)", true),
    ] {
        println!("\n=== {label} — {n} × {dim} ===");
        for efc in [100usize, 200] {
            let (ix, queries, secs) = build(n, dim, clustered, efc);
            println!(
                "\nef_construction={efc}  build {secs:.1}s  ({:.0} vectors/s)",
                n as f64 / secs
            );
            println!("  {:>6}  {:>10}  {:>10}", "ef", "recall@10", "mean lat.");
            for ef in [16usize, 32, 64, 128, 256] {
                let (r, l) = recall(&ix, &queries, 10, ef);
                println!("  {ef:>6}  {:>9.1}%  {:>8.3} ms", r, l);
            }
        }
    }
}
