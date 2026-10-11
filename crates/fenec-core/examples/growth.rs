//! What an index costs the writes as it grows: `make growth-bench`.
//!
//! ```text
//! cargo run --release -p fenec-core --example growth -- unique <rows>
//! cargo run --release -p fenec-core --example growth -- text <rows> [<new terms a row>]
//! cargo run --release -p fenec-core --example growth -- vector <dim> <rows> [<ef_construction>]
//! ```
//!
//! Each takes `rows` puts one at a time into a mapped file, each timed:
//! p50, p99, p99.99 and the longest, with where it fell, and every put past
//! a bound. A table that outgrew itself moved every key into one twice its
//! size in the put that found it full, so the longest puts were the
//! doublings, each twice the last.
//!
//! - `unique`: a ledger's journal -- an entry id `@unique`, an account
//!   `@hash` of a thousand values. Past 114 688 keys the table is shards
//!   that grow on their own (`maps::Sharded`). Then the file is opened again
//!   three times: the unique index built from the documents, as a first read
//!   builds it, and the account's; a read by an entry id through the index,
//!   and a lookup in it alone.
//! - `text`: a `@text` field whose every row brings new terms -- ids, codes,
//!   names -- beside two words every row holds, so the term map grows by
//!   `new terms` a put. Then opened again three times: the index built, and
//!   `match` over a new term and over the common words.
//! - `vector`: a `vector<dim>` under `@hnsw(cosine)`, a vector a row, the
//!   table that finds a vector written again (`Same`) growing with the
//!   nodes. Then opened again three times: the open, the first put after
//!   it, which makes that table, and `near`.

use fenec_core::prelude::*;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let num = |i: usize, or: usize| -> usize {
        args.get(i)
            .and_then(|a| a.replace('_', "").parse().ok())
            .unwrap_or(or)
    };
    let dir = std::env::temp_dir().join(format!("fenec-growth-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    match args.first().map(String::as_str) {
        Some("text") => text(&dir, num(1, 1_000_000), num(2, 4)),
        Some("vector") => vector(&dir, num(1, 128), num(2, 1_100_000), num(3, 200)),
        Some("unique") => unique(&dir, num(1, 4_000_000)),
        _ => unique(&dir, num(0, 4_000_000)),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// `n` statements put one at a time, each timed, and what they took: every
/// put past `bound_us` listed with the row it fell at.
fn timed(
    db: &mut Database,
    stmt: &Statement,
    n: usize,
    bound_us: f64,
    mut params: impl FnMut(usize) -> Vec<Value>,
) {
    let mut times: Vec<f64> = Vec::with_capacity(n);
    let t0 = Instant::now();
    for i in 0..n {
        let p = params(i);
        let t = Instant::now();
        db.execute_with(stmt, &p).unwrap();
        times.push(t.elapsed().as_secs_f64() * 1e6);
    }
    let total = t0.elapsed().as_secs_f64();
    let (at, longest) = times
        .iter()
        .copied()
        .enumerate()
        .fold((0, 0.0), |m, (i, t)| if t > m.1 { (i, t) } else { m });
    let long: Vec<String> = (times.iter().enumerate())
        .filter(|(_, &t)| t > bound_us)
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
    println!(
        "  past {} ms ({}): {}",
        bound_us / 1e3,
        long.len(),
        long.join(", ")
    );
}

fn unique(dir: &std::path::Path, n: usize) {
    let path = dir.join("journal.fenec");
    let mut db = fenec_core::fs::open(&path).unwrap();
    let create = "create collection j (entry text @unique, tx int, account text @hash)";
    db.execute(&fenec_ql::parse_one(create).unwrap()).unwrap();
    let put = fenec_ql::parse_one("put j {entry: $1, tx: $2, account: $3}").unwrap();
    timed(&mut db, &put, n, 5e3, |i| {
        vec![
            Value::Text(format!("{i}:dr")),
            Value::Int(i as i64),
            Value::Text(format!("a{}", i % 1000)),
        ]
    });
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
}

/// Row `i`'s text: `new` terms no other row holds, then two every row does.
fn body(i: usize, new: usize) -> String {
    let mut s = String::new();
    for t in 0..new {
        s.push_str(&format!("k{i}x{t} "));
    }
    s.push_str("order shipped");
    s
}

fn text(dir: &std::path::Path, n: usize, new: usize) {
    let path = dir.join("text.fenec");
    let mut db = fenec_core::fs::open(&path).unwrap();
    let create = "create collection t (body text @text, n int)";
    db.execute(&fenec_ql::parse_one(create).unwrap()).unwrap();
    // A `match` builds the index, empty; every put keeps it up after.
    let first = fenec_ql::parse_one("get t select id match body \"order\" limit 1").unwrap();
    db.query(&first, &[]).unwrap();
    let put = fenec_ql::parse_one("put t {body: $1, n: $2}").unwrap();
    timed(&mut db, &put, n, 5e3, |i| {
        vec![Value::Text(body(i, new)), Value::Int(i as i64)]
    });
    let ix = db.collection("t").unwrap().text("body").unwrap().unwrap();
    println!(
        "  {} terms, {:.0} MB",
        ix.terms(),
        ix.memory_bytes() as f64 / 1e6
    );
    db.sync().unwrap();
    drop(db);

    let one = fenec_ql::parse_one("get t select id match body $1 limit 10").unwrap();
    for _ in 0..3 {
        let db = fenec_core::fs::open(&path).unwrap();
        let t = Instant::now();
        db.warm_index("t", "body").unwrap();
        let built = t.elapsed().as_secs_f64() * 1e3;
        let q = 200_000;
        let t = Instant::now();
        for i in 0..q {
            let term = format!("k{}x0", (i * 7_919) % n);
            let r = db.query(&one, &[Value::Text(term)]).unwrap();
            assert_eq!(r.rows().map_or(0, |r| r.rows.len()), 1);
        }
        let rare = t.elapsed().as_secs_f64() * 1e9 / q as f64;
        let t = Instant::now();
        for i in 0..q {
            let term = format!("k{}x0 k{}x1 nothing", (i * 7_919) % n, (i * 104_729) % n);
            db.query(&one, &[Value::Text(term)]).unwrap();
        }
        let three = t.elapsed().as_secs_f64() * 1e9 / q as f64;
        let t = Instant::now();
        for _ in 0..20 {
            db.query(&one, &[Value::Text("order shipped".into())])
                .unwrap();
        }
        let common = t.elapsed().as_secs_f64() * 1e3 / 20.0;
        println!(
            "  opened: the text index built in {built:.0} ms; a match of a new term {rare:.0} ns, \
             of three terms {three:.0} ns, of the two every row holds {common:.1} ms"
        );
    }
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

/// `from`'s bytes written into `to`, a mebibyte at a time.
fn written(from: &std::path::Path, to: &std::path::Path) {
    use std::io::{Read, Write};
    let (mut r, mut w) = (
        std::fs::File::open(from).unwrap(),
        std::fs::File::create(to).unwrap(),
    );
    let mut buf = vec![0u8; 1 << 20];
    loop {
        match r.read(&mut buf).unwrap() {
            0 => break,
            n => w.write_all(&buf[..n]).unwrap(),
        }
    }
}

/// 64 centres, each vector one of them plus noise, as `reopen` makes them:
/// row `i` is the same vector every run.
fn centred(centres: &[Vec<f32>], i: u64) -> Vec<f32> {
    let mut rng = Rng(splitmix(i) | 1);
    centres[(i % 64) as usize]
        .iter()
        .map(|x| x + rng.gauss() * 0.35)
        .collect()
}

fn vector(dir: &std::path::Path, dim: usize, n: usize, efc: usize) {
    let path = dir.join("vector.fenec");
    let mut rng = Rng(0xDEAD_BEEF);
    let centres: Vec<Vec<f32>> = (0..64)
        .map(|_| (0..dim).map(|_| rng.gauss()).collect())
        .collect();
    let mut db = fenec_core::fs::open(&path).unwrap();
    let create =
        format!("create collection d (e vector<{dim}> @hnsw(cosine, ef_construction={efc}))");
    db.execute(&fenec_ql::parse_one(&create).unwrap()).unwrap();
    let put = fenec_ql::parse_one("put d {e: $1}").unwrap();
    timed(&mut db, &put, n, 10e3, |i| {
        vec![Value::Vector(centred(&centres, i as u64))]
    });
    // What a server leaves for the next open: the graph in the file. Each
    // round opens a copy of it, since its puts land in the tail, which the
    // next open would link -- and the first of them would place.
    db.checkpoint().unwrap();
    drop(db);
    let copy = dir.join("opened.fenec");

    let near = fenec_ql::parse_one("get d select id near e $1 limit 10").unwrap();
    for round in 0..3 {
        let _ = std::fs::remove_file(&copy);
        // Written a mebibyte at a time rather than copied: on APFS a copy
        // is a clone, whose pages the open reads from the disk, where a
        // server's file is in the cache.
        written(&path, &copy);
        let t = Instant::now();
        let mut db = fenec_core::fs::open(&copy).unwrap();
        let opened = t.elapsed().as_secs_f64() * 1e3;
        let t = Instant::now();
        let p = [Value::Vector(centred(&centres, (n + round) as u64))];
        db.execute_with(&put, &p).unwrap();
        let first = t.elapsed().as_secs_f64() * 1e3;
        let mut next: Vec<f64> = (1..20)
            .map(|j| {
                let t = Instant::now();
                let p = [Value::Vector(centred(
                    &centres,
                    (n + 100 * j + round) as u64,
                ))];
                db.execute_with(&put, &p).unwrap();
                t.elapsed().as_secs_f64() * 1e3
            })
            .collect();
        next.sort_by(f64::total_cmp);
        let second = next[next.len() / 2];
        let q = 2_000;
        let t = Instant::now();
        for i in 0..q {
            let v = centred(&centres, (1u64 << 40) + i as u64);
            db.query(&near, &[Value::Vector(v)]).unwrap();
        }
        let searched = t.elapsed().as_secs_f64() * 1e3 / q as f64;
        println!(
            "  opened in {opened:.1} ms: the first put {first:.1} ms, the next 19 {second:.2} ms p50; near {searched:.3} ms"
        );
    }
}
