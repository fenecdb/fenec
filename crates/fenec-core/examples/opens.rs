//! What opening a checkpointed file costs, and its first read: `make
//! opens-bench`.
//!
//! ```text
//! cargo run --release -p fenec-core --example opens -- write <path> <rows> <dim>
//! cargo run --release -p fenec-core --example opens -- open <path> <opens>
//! cargo run --release -p fenec-core --example opens -- loop <path> <seconds> [first]
//! cargo run --release -p fenec-core --example opens -- checkpoint <path>
//! ```
//!
//! `write` makes the file `make compare` reopens -- `category text @hash,
//! score int, body text` and, with a `<dim>` past 0, `embed vector<dim>
//! @hnsw` over vectors round 64 centres -- loaded first, indexed after, and
//! checkpointed, so the file is one image. `open` opens it `<opens>` times
//! in this process, its pages in the page cache as they are right after a
//! close, and gives the quartiles of the open alone, of the open and a read
//! by id, of the open and a count by the hash index, and of the open and a
//! `near` -- what an embedded database is asked first. `loop` opens it (and
//! with `first` reads a row by id) for `<seconds>`, for a profiler.
//! `checkpoint` writes a file's image again as this build writes one, so a
//! build is timed opening its own -- each build of a comparison opening a
//! copy of one file it checkpointed.

use fenec_core::prelude::*;
use std::time::{Duration, Instant};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn f32(&mut self) -> f32 {
        ((self.next() >> 40) as f32) / ((1u32 << 24) as f32)
    }
    fn gauss(&mut self) -> f32 {
        (0..6).map(|_| self.f32()).sum::<f32>() - 3.0
    }
}

const CATEGORIES: [&str; 4] = ["a", "b", "c", "d"];

fn run(db: &mut Database, sql: &str) -> Response {
    let stmt = fenec_ql::parse_one(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    db.execute(&stmt).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn query(db: &Database, sql: &str, params: &[Value]) -> usize {
    let stmt = fenec_ql::parse_one(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let r = db
        .query(&stmt, params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    r.rows().map_or(0, |r| r.rows.len())
}

fn write(path: &str, rows: u64, dim: usize) {
    let _ = std::fs::remove_file(path);
    let mut db = fenec_core::fs::open(path).unwrap();
    let embed = match dim {
        0 => String::new(),
        d => format!(", embed vector<{d}>"),
    };
    run(
        &mut db,
        &format!("create collection docs (category text, score int, body text{embed})"),
    );
    let mut rng = Rng(0xC0FFEE);
    let centres: Vec<Vec<f32>> = (0..64)
        .map(|_| (0..dim).map(|_| rng.gauss()).collect())
        .collect();
    let t = Instant::now();
    let mut next = 0u64;
    while next < rows {
        let n = (rows - next).min(2_000);
        let docs = (next..next + n)
            .map(|i| {
                let mut d = vec![
                    (
                        "category".to_string(),
                        Expr::Lit(Value::Text(CATEGORIES[(i % 4) as usize].into())),
                    ),
                    (
                        "score".to_string(),
                        Expr::Lit(Value::Int((rng.next() % 1000) as i64)),
                    ),
                    (
                        "body".to_string(),
                        Expr::Lit(Value::Text(format!("row {i} of the opens bench"))),
                    ),
                ];
                if dim > 0 {
                    let c = &centres[(i % 64) as usize];
                    let v = c.iter().map(|x| x + rng.gauss() * 0.35).collect();
                    d.push(("embed".to_string(), Expr::Lit(Value::Vector(v))));
                }
                d
            })
            .collect();
        db.execute(&Statement::Put {
            collection: "docs".into(),
            docs,
            insert: false,
            if_absent: false,
            docs_param: None,
            else_set: None,
            require: None,
            pick: None,
        })
        .unwrap();
        next += n;
    }
    let loaded = t.elapsed();
    run(&mut db, "create index on docs (category) @hash");
    if dim > 0 {
        run(&mut db, "create index on docs (embed) @hnsw");
    }
    db.checkpoint().unwrap();
    drop(db);
    println!(
        "wrote {rows} rows x {dim} in {loaded:.2?} (+ indexes {:.2?}), {:.1} MB",
        t.elapsed() - loaded,
        std::fs::metadata(path).unwrap().len() as f64 / 1e6
    );
}

fn quartiles(mut v: Vec<Duration>) -> String {
    v.sort();
    let at = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize].as_secs_f64() * 1e3;
    format!(
        "p25 {:7.3} ms  p50 {:7.3} ms  p75 {:7.3} ms",
        at(0.25),
        at(0.5),
        at(0.75)
    )
}

/// A case of [`open`]: its name, and what it reads after the open -- a
/// statement and its parameters -- if anything.
type Case = (&'static str, Option<(&'static str, Vec<Value>)>);

/// What a fresh process asks first: each case opens the file and runs its
/// read, and only the open and the read are timed, never the close.
fn open(path: &str, opens: usize) {
    let probe = fenec_core::fs::open(path).unwrap();
    let docs = probe.collection("docs").unwrap();
    let rows = docs.store.len() as u64;
    let dim = match docs.schema.field("embed").map(|f| &f.ty) {
        Some(DataType::Vector(d, _)) => *d,
        _ => 0,
    };
    let vectors = dim > 0;
    drop(probe);
    let mut rng = Rng(0x0BE5);
    let q: Vec<f32> = (0..dim).map(|_| rng.gauss()).collect();
    let mut cases: Vec<Case> = vec![
        ("open", None),
        (
            "open + get by id",
            Some(("get docs where id = $1", vec![Value::Int(0)])),
        ),
        (
            "open + count by @hash",
            Some((
                "get docs where category = $1 count",
                vec![Value::Text("a".into())],
            )),
        ),
    ];
    if vectors {
        cases.push((
            "open + near, top 10",
            Some((
                "get docs select id near embed $1 limit 10",
                vec![Value::Vector(q)],
            )),
        ));
    }
    // In turns, so that each case meets the machine as the others do.
    let mut times: Vec<Vec<Duration>> = vec![Vec::new(); cases.len()];
    for round in 0..opens {
        for (i, (_, read)) in cases.iter().enumerate() {
            let id = 1 + (rng.next() + round as u64) % rows.max(1);
            let t = Instant::now();
            let db = fenec_core::fs::open(path).unwrap();
            if let Some((sql, params)) = read {
                let mut params = params.clone();
                if let Some(Value::Int(p)) = params.first_mut() {
                    *p = id as i64;
                }
                let n = query(&db, sql, &params);
                assert!(n > 0, "{sql} answered nothing");
            }
            times[i].push(t.elapsed());
            drop(db);
        }
    }
    println!(
        "{rows} rows, {:.1} MB, {opens} opens a case",
        std::fs::metadata(path).unwrap().len() as f64 / 1e6
    );
    for ((name, _), t) in cases.iter().zip(times) {
        println!("{name:<24}{}", quartiles(t));
    }
}

fn spin(path: &str, secs: f64, first: bool) {
    let until = Instant::now() + Duration::from_secs_f64(secs);
    let mut n = 0u64;
    while Instant::now() < until {
        let db = fenec_core::fs::open(path).unwrap();
        if first {
            query(
                &db,
                "get docs where id = $1",
                &[Value::Int(1 + n as i64 % 1000)],
            );
        }
        drop(db);
        n += 1;
    }
    println!("{n} opens");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).map(String::as_str).unwrap_or("");
    match arg(0) {
        "write" => write(arg(1), arg(2).parse().unwrap(), arg(3).parse().unwrap()),
        "open" => open(arg(1), arg(2).parse().unwrap()),
        "loop" => spin(arg(1), arg(2).parse().unwrap(), arg(3) == "first"),
        "checkpoint" => fenec_core::fs::open(arg(1)).unwrap().checkpoint().unwrap(),
        _ => eprintln!(
            "opens write <path> <rows> <dim> | open <path> <opens> \
             | loop <path> <seconds> [first] | checkpoint <path>"
        ),
    }
}
