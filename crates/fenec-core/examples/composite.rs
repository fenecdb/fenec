//! What an index over several fields buys and costs: `make composite-bench`.
//!
//! `reads`: a million posts over 10 000 owners, and the page an app reads
//! most -- `where owner = $1 order created desc limit 20` -- through
//! `@sorted(owner, created desc)`, through `created @sorted` alone (its walk
//! tests `owner` row by row), through `owner @hash` alone (its bucket read
//! and sorted), and through the scan; each over a collection of its own, the
//! index built after the load and timed, its memory counted. Then a feed
//! over ten owners (`owner in [..]`), which the index answers as ten spans.
//!
//! `writes`: a lone put's cost beside the index and beside the two single
//! indexes, against none.
//!
//! `unique`: `@unique(org, slug)` against `@unique` on a field holding both
//! joined, the way a two-field key was kept before: a lone put, and a read
//! by both -- whether a hash index over several fields would be worth one.

use fenec_core::prelude::*;
use std::time::Instant;

fn exec(db: &mut Database, sql: &str) -> Response {
    let mut last = Response::Affected(0);
    for s in fenec_ql::parse(sql).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        last = db.execute(&s).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    last
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn put_all(db: &mut Database, collection: &str, docs: &[Vec<(String, Expr)>]) {
    for chunk in docs.chunks(20_000) {
        db.execute(&Statement::Put {
            collection: collection.into(),
            docs: chunk.to_vec(),
            insert: false,
            if_absent: false,
            docs_param: None,
            else_set: None,
            require: None,
        })
        .unwrap();
    }
}

/// p50 and p99 of `n` runs of `stmt`, each handed `params(i)`, in ms.
fn timed(
    db: &Database,
    stmt: &Statement,
    n: usize,
    params: &mut dyn FnMut(usize) -> Vec<Value>,
) -> (f64, f64, usize) {
    let mut t = Vec::with_capacity(n);
    let mut rows = 0;
    for i in 0..n {
        let p = params(i);
        let s = Instant::now();
        let r = db.query(stmt, &p).unwrap();
        t.push(s.elapsed().as_secs_f64() * 1e3);
        rows += r.rows().map_or(0, |r| r.rows.len());
    }
    t.sort_by(|a, b| a.total_cmp(b));
    (t[n / 2], t[(n * 99 / 100).min(n - 1)], rows / n)
}

fn plan(db: &Database, sql: &str, params: &[Value]) -> String {
    let r = db
        .query(
            &fenec_ql::parse_one(&format!("explain {sql}")).unwrap(),
            params,
        )
        .unwrap();
    r.rows()
        .unwrap()
        .rows
        .iter()
        .map(|r| format!("{:?}", r.values[0]))
        .collect::<Vec<_>>()
        .join(" / ")
}

fn reads(n: usize, owners: usize) {
    println!("reads: {n} posts over {owners} owners");
    let mut r = Rng(0x2545_F491_4F6C_DD1D);
    // Created rising with the id, a few seconds apart and jittered, as
    // posts arrive; an owner each at random.
    let t0 = 1_760_000_000_000i64;
    let docs: Vec<Vec<(String, Expr)>> = (0..n)
        .map(|i| {
            vec![
                (
                    "owner".into(),
                    Expr::Lit(Value::Int((r.next() % owners as u64) as i64)),
                ),
                (
                    "created".into(),
                    Expr::Lit(Value::Timestamp(
                        t0 + i as i64 * 3_000 + (r.next() % 2_000) as i64,
                    )),
                ),
                ("title".into(), Expr::Lit(Value::Text(format!("post {i}")))),
            ]
        })
        .collect();
    let configs = [
        (
            "composite",
            "create index on posts (owner, created desc) @sorted",
        ),
        (
            "@sorted(created)",
            "create index on posts (created) @sorted",
        ),
        ("@hash(owner)", "create index on posts (owner) @hash"),
        ("scan", ""),
    ];
    let page = fenec_ql::parse_one(
        "get posts select id, created, title where owner = $1 order created desc limit 20",
    )
    .unwrap();
    let feed = fenec_ql::parse_one(
        "get posts select id, created where owner in [$1, $2, $3, $4, $5, $6, $7, $8, $9, $10] \
         order created desc limit 20",
    )
    .unwrap();
    println!(
        "  {:<18} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "index", "build ms", "MB", "p50 ms", "p99 ms", "feed p50", "feed p99"
    );
    for (name, ddl) in configs {
        let mut db = Database::new();
        exec(
            &mut db,
            "create collection posts (owner int, created timestamp, title text)",
        );
        put_all(&mut db, "posts", &docs);
        let before = db.memory_bytes();
        let s = Instant::now();
        if !ddl.is_empty() {
            exec(&mut db, ddl);
            // The hash and ordered indexes are built by the first read.
            db.warm(&[]).unwrap();
        }
        let build = s.elapsed().as_secs_f64() * 1e3;
        let mb = (db.memory_bytes() - before) as f64 / 1e6;
        let runs = if name == "scan" { 40 } else { 2_000 };
        let mut q = Rng(7);
        let (p50, p99, rows) = timed(&db, &page, runs, &mut |_| {
            vec![Value::Int((q.next() % owners as u64) as i64)]
        });
        assert_eq!(rows, 20, "{name}");
        let mut q = Rng(11);
        let (f50, f99, _) = timed(&db, &feed, runs.min(400), &mut |_| {
            (0..10)
                .map(|_| Value::Int((q.next() % owners as u64) as i64))
                .collect()
        });
        println!("  {name:<18} {build:>9.1} {mb:>9.1} {p50:>9.4} {p99:>9.4} {f50:>9.4} {f99:>9.4}");
        println!(
            "      {}",
            plan(
                &db,
                "get posts where owner = $1 order created desc limit 20",
                &[Value::Int(5)]
            )
        );
    }
}

/// ns a lone put of a post into a collection of `n` holds, median of 5
/// rounds of `per` puts.
fn writes(n: usize, owners: usize) {
    println!("writes: a lone put into {n} posts");
    let configs = [
        ("none", ""),
        ("composite", ", @sorted(owner, created desc)"),
        ("@sorted(created) + @hash(owner)", ""),
        ("@sorted(owner)", ""),
    ];
    for (name, extra) in configs {
        let mut db = Database::new();
        let (created, owner) = match name {
            "@sorted(created) + @hash(owner)" => (" @sorted", " @hash"),
            "@sorted(owner)" => ("", " @sorted"),
            _ => ("", ""),
        };
        exec(
            &mut db,
            &format!(
                "create collection posts (owner int{owner}, created timestamp{created}, \
                 title text{extra})"
            ),
        );
        let mut r = Rng(3);
        let t0 = 1_760_000_000_000i64;
        let doc = |i: usize, r: &mut Rng| {
            vec![
                (
                    "owner".into(),
                    Expr::Lit(Value::Int((r.next() % owners as u64) as i64)),
                ),
                (
                    "created".into(),
                    Expr::Lit(Value::Timestamp(t0 + i as i64 * 3_000)),
                ),
                ("title".into(), Expr::Lit(Value::Text(format!("post {i}")))),
            ]
        };
        let docs: Vec<_> = (0..n).map(|i| doc(i, &mut r)).collect();
        put_all(&mut db, "posts", &docs);
        db.warm(&[]).unwrap();
        let per = 50_000;
        let mut rounds = Vec::new();
        for round in 0..5 {
            let stmts: Vec<Statement> = (0..per)
                .map(|k| Statement::Put {
                    collection: "posts".into(),
                    docs: vec![doc(n + round * per + k, &mut r)],
                    insert: false,
                    if_absent: false,
                    docs_param: None,
                    else_set: None,
                    require: None,
                })
                .collect();
            let s = Instant::now();
            for st in &stmts {
                db.execute(st).unwrap();
            }
            rounds.push(s.elapsed().as_secs_f64() * 1e9 / per as f64);
        }
        rounds.sort_by(|a, b| a.total_cmp(b));
        println!("  {name:<34} {:>7.0} ns a put", rounds[2]);
    }
}

fn unique(n: usize) {
    println!("unique: {n} pages over {} orgs", n / 100);
    let orgs = n / 100;
    let configs = [
        (
            "@unique(org, slug)",
            "create collection pages (org text, slug text, n int, @unique(org, slug))",
        ),
        (
            "key @unique",
            "create collection pages (org text, slug text, n int, key text @unique)",
        ),
        (
            "@sorted(org, slug), no check",
            "create collection pages (org text, slug text, n int, @sorted(org, slug))",
        ),
    ];
    for (name, ddl) in configs {
        let mut db = Database::new();
        exec(&mut db, ddl);
        let joined = name == "key @unique";
        let doc = |i: usize| {
            let (org, slug) = (format!("org{}", i % orgs), format!("page-{}", i / orgs));
            let mut d = vec![
                ("org".into(), Expr::Lit(Value::Text(org.clone()))),
                ("slug".into(), Expr::Lit(Value::Text(slug.clone()))),
                ("n".into(), Expr::Lit(Value::Int(i as i64))),
            ];
            if joined {
                d.push((
                    "key".into(),
                    Expr::Lit(Value::Text(format!("{org}:{slug}"))),
                ));
            }
            d
        };
        let docs: Vec<_> = (0..n).map(doc).collect();
        let s = Instant::now();
        put_all(&mut db, "pages", &docs);
        let load = s.elapsed().as_secs_f64() * 1e3;
        let per = 50_000;
        let mut rounds = Vec::new();
        for round in 0..5 {
            let stmts: Vec<Statement> = (0..per)
                .map(|k| Statement::Put {
                    collection: "pages".into(),
                    docs: vec![doc(n + round * per + k)],
                    insert: false,
                    if_absent: false,
                    docs_param: None,
                    else_set: None,
                    require: None,
                })
                .collect();
            let s = Instant::now();
            for st in &stmts {
                db.execute(st).unwrap();
            }
            rounds.push(s.elapsed().as_secs_f64() * 1e9 / per as f64);
        }
        rounds.sort_by(|a, b| a.total_cmp(b));
        let read = match joined {
            true => "get pages select n where key = $1",
            false => "get pages select n where org = $1 and slug = $2",
        };
        let stmt = fenec_ql::parse_one(read).unwrap();
        let mut q = Rng(5);
        let (p50, p99, rows) = timed(&db, &stmt, 20_000, &mut |_| {
            let i = (q.next() % n as u64) as usize;
            let (org, slug) = (format!("org{}", i % orgs), format!("page-{}", i / orgs));
            match joined {
                true => vec![Value::Text(format!("{org}:{slug}"))],
                false => vec![Value::Text(org), Value::Text(slug)],
            }
        });
        assert_eq!(rows, 1, "{name}");
        println!(
            "  {name:<30} load {load:>7.0} ms, {:>6.0} ns a put, read p50 {:>6.2} us p99 {:>6.2} us, {:.1} MB",
            rounds[2],
            p50 * 1e3,
            p99 * 1e3,
            db.memory_bytes() as f64 / 1e6
        );
    }
}

/// The ordered index a field has alone, whose chunks the index over
/// several fields shares: what it answers and a lone write's cost, for a
/// build before and after a change to the chunks.
fn sorted(n: usize) {
    println!("sorted: {n} rows, `at @sorted` and `k @sorted` (random)");
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection t (at int @sorted, k int @sorted, v int)",
    );
    let mut r = Rng(9);
    let docs: Vec<Vec<(String, Expr)>> = (0..n)
        .map(|i| {
            vec![
                ("at".into(), Expr::Lit(Value::Int(i as i64))),
                (
                    "k".into(),
                    Expr::Lit(Value::Int((r.next() % 1_000_000_000) as i64)),
                ),
                ("v".into(), Expr::Lit(Value::Int((i % 100) as i64))),
            ]
        })
        .collect();
    put_all(&mut db, "t", &docs);
    let s = Instant::now();
    db.warm(&[]).unwrap();
    println!(
        "  build both               {:>8.1} ms",
        s.elapsed().as_secs_f64() * 1e3
    );
    // About a thousand rows a range.
    let width = 1_000_000_000 / n as i64 * 1000;
    for (what, sql, runs) in [
        ("latest 20", "get t select at order at desc limit 20", 5_000),
        (
            "k from $1, 20",
            "get t select k where k >= $1 order k limit 20",
            5_000,
        ),
        (
            "k range count",
            "get t where k >= $1 and k < $2 count",
            2_000,
        ),
        (
            "k range, ~1000 rows",
            "get t select v where k >= $1 and k < $2",
            2_000,
        ),
    ] {
        let stmt = fenec_ql::parse_one(sql).unwrap();
        let mut q = Rng(13);
        let (p50, p99, _) = timed(&db, &stmt, runs, &mut |_| {
            let a = (q.next() % 1_000_000_000) as i64;
            vec![Value::Int(a), Value::Int(a + width)]
        });
        println!(
            "  {what:<24} p50 {:>8.2} us  p99 {:>8.2} us",
            p50 * 1e3,
            p99 * 1e3
        );
    }
    let per = 50_000;
    let mut rounds = Vec::new();
    for round in 0..5 {
        let stmts: Vec<Statement> = (0..per)
            .map(|i| Statement::Put {
                collection: "t".into(),
                docs: vec![vec![
                    (
                        "at".into(),
                        Expr::Lit(Value::Int((n + round * per + i) as i64)),
                    ),
                    (
                        "k".into(),
                        Expr::Lit(Value::Int((r.next() % 1_000_000_000) as i64)),
                    ),
                ]],
                insert: false,
                if_absent: false,
                docs_param: None,
                else_set: None,
                require: None,
            })
            .collect();
        let s = Instant::now();
        for st in &stmts {
            db.execute(st).unwrap();
        }
        rounds.push(s.elapsed().as_secs_f64() * 1e9 / per as f64);
    }
    rounds.sort_by(|a, b| a.total_cmp(b));
    println!("  a lone put               {:>8.0} ns", rounds[2]);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let what = args.next().unwrap_or_else(|| "all".into());
    let n: usize = args
        .next()
        .and_then(|a| a.parse().ok())
        .unwrap_or(1_000_000);
    let owners: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(10_000);
    if what == "reads" || what == "all" {
        reads(n, owners);
    }
    if what == "writes" || what == "all" {
        writes(n, owners);
    }
    if what == "unique" || what == "all" {
        unique(n);
    }
    if what == "sorted" {
        sorted(n);
    }
}
