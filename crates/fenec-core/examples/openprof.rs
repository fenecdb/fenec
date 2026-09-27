// Scratch: makes a checkpointed file like `make compare`'s, and times opens.
use fenec_core::prelude::*;
use std::time::Instant;
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = &args[1];
    if args[0] == "write" {
        let _ = std::fs::remove_file(path);
        let mut db = fenec_core::fs::open(path).unwrap();
        db.execute(&fenec_ql::parse_one("create collection bench (category text @hash, score int, embed vector<128> @hnsw(cosine))").unwrap()).unwrap();
        let mut x = 7u64;
        let mut f = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 40) as f32 / (1u64 << 24) as f32 - 0.5
        };
        let cats = ["a", "b", "c", "d"];
        for b in 0..50 {
            let docs: Vec<_> = (0..2000)
                .map(|i| {
                    let n = b * 2000 + i;
                    vec![
                        (
                            "category".to_string(),
                            Expr::Lit(Value::Text(cats[n % 4].into())),
                        ),
                        (
                            "score".to_string(),
                            Expr::Lit(Value::Int((n % 1000) as i64)),
                        ),
                        (
                            "embed".to_string(),
                            Expr::Lit(Value::Vector((0..128).map(|_| f()).collect())),
                        ),
                    ]
                })
                .collect();
            db.execute(&Statement::Put {
                collection: "bench".into(),
                docs,
            })
            .unwrap();
        }
        db.checkpoint().unwrap();
        println!("written");
        return;
    }
    let n: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);
    let mut v = Vec::new();
    let mem = args.get(3).is_some_and(|m| m == "mem");
    let bytes = std::fs::read(path).unwrap();
    for _ in 0..n {
        let t = Instant::now();
        let db = match mem {
            true => {
                let mut db = Database::new();
                db.load(&bytes).unwrap();
                db
            }
            false => fenec_core::fs::open(path).unwrap(),
        };
        v.push(t.elapsed().as_secs_f64() * 1e3);
        drop(db);
    }
    v.sort_by(|a, b| a.total_cmp(b));
    println!("open p50 {:.1} ms", v[n / 2]);
}
