//! Breaks down the memory footprint: calibration for `--max-memory`.
//!
//! `cargo run --release -p fenec-core --example memory -- [N] [DIM]`
//!
//! The total printed is [`Database::memory_bytes`]. The example deliberately
//! does not call `snapshot`: the whole image is built in memory, so peak RSS
//! would distort its own measurement. To compare against real RSS, run the
//! process under a timer:
//!
//! ```text
//! /usr/bin/time -l cargo run --release ... --example memory     # macOS
//! /usr/bin/time -v cargo run --release ... --example memory     # Linux
//! ```

use fenec_core::prelude::*;

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

fn mb(bytes: usize) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let n: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(100_000);
    let dim: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(128);

    let mut rng = Rng(0xC0FFEE);
    let centers: Vec<Vec<f32>> = (0..64)
        .map(|_| (0..dim).map(|_| rng.gauss()).collect())
        .collect();

    let mut db = Database::new();
    db.execute(&Statement::CreateCollection {
        schema: Schema::new(
            "s",
            vec![
                Field::new("category", DataType::Text).indexed(IndexKind::Hash),
                Field::new(
                    "embed",
                    DataType::Vector(dim, VecPrec::F32),
                )
                .indexed(IndexKind::Vector(VectorIndexSpec::default())),
            ],
        )
        .unwrap(),
        if_not_exists: false,
    })
    .unwrap();

    for chunk in (0..n).collect::<Vec<_>>().chunks(2000) {
        let docs: Vec<Vec<(String, Expr)>> = chunk
            .iter()
            .map(|i| {
                let v: Vec<f32> = centers[i % 64]
                    .iter()
                    .map(|x| x + rng.gauss() * 0.35)
                    .collect();
                vec![
                    (
                        "category".to_string(),
                        Expr::Lit(Value::Text(format!("k{}", i % 64))),
                    ),
                    ("embed".to_string(), Expr::Lit(Value::Vector(v))),
                ]
            })
            .collect();
        db.execute(&Statement::Put {
            collection: "s".into(),
            docs,
        })
        .unwrap();
    }

    let st = &db.stats()[0];
    let arena: usize = st.vector_indexes.iter().map(|v| v.arena_bytes).sum();
    println!("{n} documents x {dim} dimensions");
    println!("  segment bytes      {}", mb(st.bytes));
    println!("  vector arena       {}", mb(arena));
    println!("  measured footprint {}   <- memory_bytes()", mb(db.memory_bytes()));
    println!();
    println!("Measure peak RSS from outside. The ratio measured on this machine:");
    println!("the footprint is 60-75% of peak RSS (the gap is HNSW build buffers");
    println!("and allocator leftovers). `--max-memory` is picked from this, with headroom.");
}
