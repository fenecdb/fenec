//! Scale measurement: `cargo run --release -p fenec-core --example bench -- [N] [DIM]`
//!
//! What is measured
//!   * bulk write throughput (document + vector indexing included)
//!   * ANN query latency (p50 / p95 / p99)
//!   * recall against an exact scan (recall@10)
//!   * size of the out-of-memory byte image and the time to reopen it

use fenec_core::prelude::*;
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
    /// Rough normal distribution (via the central limit theorem).
    fn gauss(&mut self) -> f32 {
        (0..6).map(|_| self.f32()).sum::<f32>() - 3.0
    }
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() as f64 - 1.0) * p) as usize]
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let n: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(100_000);
    let dim: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(128);

    // The `--uniform` flag generates uniform random vectors. That is a
    // pathological case for ANN: in high dimensions every distance converges
    // and no neighbourhood structure is left. The default clustered
    // distribution resembles the output of real embedding models.
    let uniform = args.iter().any(|a| a == "--uniform");
    // --efc N : candidate list width during construction
    let ef_search: usize = args
        .iter()
        .position(|a| a == "--ef")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(fenec_core::schema::DEFAULT_EF_SEARCH);
    let efc: usize = args
        .iter()
        .position(|a| a == "--efc")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(VectorIndexSpec::default().ef_construction);

    println!(
        "fenecdb {} — {n} documents × {dim} dimensions — {} distribution — ef_construction={efc} ef_search={ef_search}\n",
        fenec_core::VERSION,
        if uniform { "uniform random (pathological)" } else { "clustered (embedding-like)" }
    );

    let mut db = Database::new();
    db.execute(&Statement::CreateCollection {
        schema: Schema::new(
            "bench",
            vec![
                Field::new("category", DataType::Text).indexed(IndexKind::Hash),
                Field::new("score", DataType::Int),
                Field::new("embed", DataType::Vector(dim, VecPrec::F32)).indexed(
                    IndexKind::Vector(VectorIndexSpec {
                        ef_construction: efc,
                        ef_search,
                        ..VectorIndexSpec::default()
                    }),
                ),
            ],
        )
        .unwrap(),
        if_not_exists: false,
    })
    .unwrap();

    // ---- writing
    let mut rng = Rng(0xDEAD_BEEF);
    let categories = ["a", "b", "c", "d"];
    let centers: Vec<Vec<f32>> = (0..64)
        .map(|_| (0..dim).map(|_| rng.gauss()).collect())
        .collect();
    let mut queries: Vec<Vec<f32>> = Vec::new();
    let t0 = Instant::now();
    let batch = 2_000;
    let mut written = 0usize;
    while written < n {
        let mut docs = Vec::with_capacity(batch);
        for i in written..(written + batch).min(n) {
            let v: Vec<f32> = if uniform {
                (0..dim).map(|_| rng.f32() - 0.5).collect()
            } else {
                let c = &centers[i % centers.len()];
                c.iter().map(|x| x + rng.gauss() * 0.35).collect()
            };
            if i % (n / 100).max(1) == 0 && queries.len() < 100 {
                queries.push(v.clone());
            }
            docs.push(vec![
                (
                    "category".to_string(),
                    Expr::Lit(Value::Text(categories[i % 4].to_string())),
                ),
                (
                    "score".to_string(),
                    Expr::Lit(Value::Int((i % 1000) as i64)),
                ),
                ("embed".to_string(), Expr::Lit(Value::Vector(v))),
            ]);
        }
        written += docs.len();
        db.execute(&Statement::Put {
            collection: "bench".into(),
            docs,
        })
        .unwrap();
    }
    let write = t0.elapsed();
    println!(
        "write      {n} documents  {:.2?}  ({:.0} docs/s)",
        write,
        n as f64 / write.as_secs_f64()
    );

    let st = &db.stats()[0];
    println!(
        "storage    {:.1} MB  ({:.0} bytes/doc, {} segments, no page cache)",
        st.bytes as f64 / 1e6,
        st.bytes as f64 / n as f64,
        st.segments
    );

    // ---- ANN queries
    let mut lat = Vec::new();
    for q in &queries {
        let sel = Select {
            collection: "bench".into(),
            project: Some(vec!["id".into()]),
            near: Some(Near {
                field: "embed".into(),
                vector: Expr::Lit(Value::Vector(q.clone())),
                ef: None,
                exact: false,
            }),
            limit: Some(10),
            ..Default::default()
        };
        let t = Instant::now();
        db.execute(&Statement::Select(sel)).unwrap();
        lat.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "ann        k=10  p50 {:.3} ms  p95 {:.3} ms  p99 {:.3} ms",
        pct(&lat, 0.50),
        pct(&lat, 0.95),
        pct(&lat, 0.99)
    );

    // ---- filtered ANN
    let mut flat = Vec::new();
    for q in queries.iter().take(20) {
        let sel = Select {
            collection: "bench".into(),
            project: Some(vec!["id".into()]),
            filter: Some(Expr::Cmp(
                CmpOp::Eq,
                Box::new(Expr::Field("category".into())),
                Box::new(Expr::Lit(Value::Text("a".into()))),
            )),
            near: Some(Near {
                field: "embed".into(),
                vector: Expr::Lit(Value::Vector(q.clone())),
                ef: Some(200),
                exact: false,
            }),
            limit: Some(10),
            ..Default::default()
        };
        let t = Instant::now();
        db.execute(&Statement::Select(sel)).unwrap();
        flat.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    flat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!(
        "ann+filter category='a' (n/4)  p50 {:.3} ms  p95 {:.3} ms",
        pct(&flat, 0.50),
        pct(&flat, 0.95)
    );

    // ---- recall
    let mut hits = 0usize;
    let sample = queries.iter().take(20);
    let mut total = 0usize;
    for q in sample {
        let mk = |exact: bool| Select {
            collection: "bench".into(),
            project: Some(vec!["id".into()]),
            near: Some(Near {
                field: "embed".into(),
                vector: Expr::Lit(Value::Vector(q.clone())),
                ef: None,
                exact,
            }),
            limit: Some(10),
            ..Default::default()
        };
        let a = db.execute(&Statement::Select(mk(false))).unwrap();
        let e = db.execute(&Statement::Select(mk(true))).unwrap();
        let (a, e) = (a.rows().unwrap(), e.rows().unwrap());
        let exact_ids: Vec<u64> = e.rows.iter().map(|r| r.id).collect();
        hits += a.rows.iter().filter(|r| exact_ids.contains(&r.id)).count();
        total += exact_ids.len();
    }
    println!(
        "recall@10  {:.1}%  (against the exact scan)",
        hits as f64 / total as f64 * 100.0
    );

    // ---- persistence
    let t = Instant::now();
    let image = db.snapshot();
    let snap = t.elapsed();
    let t = Instant::now();
    let mut db2 = Database::new();
    db2.load(&image).unwrap();
    let reload = t.elapsed();
    println!(
        "image      {:.1} MB  snapshot {:.2?}  reopen {:.2?} (index rebuild included)",
        image.len() as f64 / 1e6,
        snap,
        reload
    );
}
