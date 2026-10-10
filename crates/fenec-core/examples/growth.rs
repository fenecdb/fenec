//! What a `@unique` field's index costs the writes as it grows: `make
//! growth-bench` (`cargo run --release -p fenec-core --example growth --
//! [ROWS]`).
//!
//! A ledger's journal -- an entry id `@unique`, an account `@hash` of a
//! thousand values -- takes `ROWS` puts one at a time into a mapped file,
//! each timed: p50, p99, p99.99 and the longest, with where it fell, and
//! every put past 5 ms. A hash index's table that outgrew itself moved every
//! key into one twice its size in the put that found it full, so the
//! longest puts were the doublings, each twice the last; past 114 688 keys
//! the table is shards that grow on their own (`maps::Sharded`). Then the
//! file is opened again three times: the unique index built from the
//! documents, as a first read builds it, and the account's; a read by an
//! entry id through the index, and a lookup in it alone.

use fenec_core::prelude::*;
use std::time::Instant;

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(4_000_000);
    let dir = std::env::temp_dir().join(format!("fenec-growth-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("journal.fenec");
    let mut db = fenec_core::fs::open(&path).unwrap();
    let create = "create collection j (entry text @unique, tx int, account text @hash)";
    db.execute(&fenec_ql::parse_one(create).unwrap()).unwrap();
    let put = fenec_ql::parse_one("put j {entry: $1, tx: $2, account: $3}").unwrap();
    let mut times: Vec<f64> = Vec::with_capacity(n);
    let t0 = Instant::now();
    for i in 0..n {
        let params = [
            Value::Text(format!("{i}:dr")),
            Value::Int(i as i64),
            Value::Text(format!("a{}", i % 1000)),
        ];
        let t = Instant::now();
        db.execute_with(&put, &params).unwrap();
        times.push(t.elapsed().as_secs_f64() * 1e6);
    }
    let total = t0.elapsed().as_secs_f64();
    let (at, longest) = times
        .iter()
        .copied()
        .enumerate()
        .fold((0, 0.0), |m, (i, t)| if t > m.1 { (i, t) } else { m });
    let long: Vec<String> = (times.iter().enumerate())
        .filter(|(_, &t)| t > 5e3)
        .map(|(i, &t)| format!("{:.1} ms at {i}", t / 1e3))
        .collect();
    let mut sorted = times;
    sorted.sort_by(|a, b| a.total_cmp(b));
    let pct = |p: f64| sorted[((sorted.len() as f64 * p) as usize).min(sorted.len() - 1)];
    println!(
        "{n} puts in {total:.2} s: p50 {:.2} us, p99 {:.2} us, p99.99 {:.1} us, the longest {:.1} ms at {at}",
        pct(0.5),
        pct(0.99),
        pct(0.9999),
        longest / 1e3,
    );
    println!("  past 5 ms: {}", long.join(", "));
    db.sync().unwrap();
    drop(db);

    let keys: Vec<Vec<u8>> = (0..1_000_000usize)
        .map(|i| {
            let mut k = Vec::new();
            let v = Value::Text(format!("{}:dr", (i * 7_919) % n));
            fenec_core::codec::encode_value(&mut k, &v);
            k
        })
        .collect();
    let read = fenec_ql::parse_one("get j where entry = $1 select tx").unwrap();
    for _ in 0..3 {
        let db = fenec_core::fs::open(&path).unwrap();
        let t = Instant::now();
        db.warm_index("j", "entry").unwrap();
        let entry = t.elapsed().as_secs_f64() * 1e3;
        let t = Instant::now();
        db.warm_index("j", "account").unwrap();
        let account = t.elapsed().as_secs_f64() * 1e3;
        let t = Instant::now();
        for i in (0..n).step_by(7) {
            db.query(&read, &[Value::Text(format!("{i}:dr"))]).unwrap();
        }
        let get = t.elapsed().as_secs_f64() * 1e9 / n.div_ceil(7) as f64;
        let ix = db.collection("j").unwrap().hash("entry").unwrap().unwrap();
        let mut found = 0;
        let t = Instant::now();
        for k in &keys {
            found += ix.get(k).map_or(0, |b| b.len());
        }
        let lookup = t.elapsed().as_secs_f64() * 1e9 / keys.len() as f64;
        assert_eq!(found, keys.len());
        println!(
            "  opened: the unique index built in {entry:.0} ms, the hash in {account:.0} ms; \
             a read by an entry {get:.0} ns, a lookup {lookup:.1} ns"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
