//! What `highlight()`, `snippet()` and `facet` cost: `make search-bench`.
//!
//! 100 000 documents of 30 words each over a vocabulary of 5 000, the
//! commoner words more common, a `brand` under `@hash` (40 values), a
//! `color` with no index (12), a `price` and a list of `tags`. Each query
//! is timed as the median of `RUNS` over 200 two-word queries:
//!
//! - the text index's build, the first `match` after the writes;
//! - `match ... limit 10` alone, then with `highlight(body)`, with its tags
//!   and with `snippet(body, 12)` -- the difference over the ten rows is
//!   what a row's marks cost;
//! - a facet over every row, by the buckets and by the scan, then over a
//!   filter keeping about half, and over what a `match` selects.
//!
//! A statement this build cannot parse is reported `n/a`, so the program
//! runs against a build from before the features as well: the build and
//! `match` alone are the numbers that must not move.

use fenec_core::prelude::*;
use std::time::Instant;

const RUNS: usize = 15;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    /// A word: its rank drawn so the first ranks come up most.
    fn word(&mut self) -> String {
        let u = (self.next() % 1_000_000) as f64 / 1_000_000.0;
        format!("w{}", (u * u * u * 5_000.0) as u64)
    }
}

fn exec(db: &mut Database, sql: &str, params: &[Value]) {
    for s in fenec_ql::parse(sql).expect("parse") {
        db.execute_with(&s, params).expect("execute");
    }
}

/// Median milliseconds of running every query once, a run being all of
/// them; `None` when this build does not parse the statement.
fn time(db: &Database, sql: &str, queries: &[Value]) -> Option<f64> {
    let stmt = fenec_ql::parse_one(sql).ok()?;
    let run = |db: &Database| {
        for q in queries {
            std::hint::black_box(db.query(&stmt, std::slice::from_ref(q)).ok()?);
        }
        Some(())
    };
    run(db)?;
    let mut t: Vec<f64> = (0..RUNS)
        .map(|_| {
            let s = Instant::now();
            run(db);
            s.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    t.sort_by(|a, b| a.total_cmp(b));
    Some(t[RUNS / 2])
}

fn show(what: &str, ms: Option<f64>, per: f64, unit: &str) {
    match ms {
        Some(ms) => println!("  {what:<52} {:>9.3} {unit}", ms / per),
        None => println!("  {what:<52} {:>9}", "n/a"),
    }
}

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(100_000);
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection d (body text @text, brand text @hash, color text, price int, tags [text])",
        &[],
    );
    let mut rng = Rng(0x2545f4914f6cdd1d);
    let mut batch = Vec::new();
    for i in 0..n {
        let body: Vec<String> = (0..30).map(|_| rng.word()).collect();
        let tags: Vec<String> = (0..rng.below(4))
            .map(|_| format!("\"t{}\"", rng.below(20)))
            .collect();
        batch.push(format!(
            "{{body: \"{}\", brand: \"b{}\", color: \"c{}\", price: {}, tags: [{}]}}",
            body.join(" "),
            rng.below(40),
            rng.below(12),
            rng.below(1000),
            tags.join(", ")
        ));
        if batch.len() == 5_000 || i + 1 == n {
            exec(&mut db, &format!("put d [{}]", batch.join(", ")), &[]);
            batch.clear();
        }
    }
    let queries: Vec<Value> = (0..200)
        .map(|_| Value::Text(format!("{} {}", rng.word(), rng.word())))
        .collect();

    println!("{n} documents x 30 words, 200 two-word queries, the median of {RUNS} runs:");
    // Opened from its image, the text index is built by the first read:
    // the tokenizer over every document.
    let image = db.snapshot();
    let mut builds: Vec<f64> = (0..5)
        .map(|_| {
            let mut back = Database::new();
            back.load(&image).expect("load");
            let s = Instant::now();
            exec(&mut back, "get d match body \"w1\" limit 1", &[]);
            s.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    builds.sort_by(|a, b| a.total_cmp(b));
    show(
        "text index built by the first match",
        Some(builds[2]),
        1.0,
        "ms",
    );

    let q = queries.len() as f64;
    let plain = time(&db, "get d select id match body $1 limit 10", &queries);
    show("match limit 10", plain, q, "ms a query");
    for (what, sql) in [
        (
            "  + highlight(body)",
            "get d select id, highlight(body) match body $1 limit 10",
        ),
        (
            "  + highlight(body, \"<mark>\", \"</mark>\")",
            "get d select id, highlight(body, \"<mark>\", \"</mark>\") match body $1 limit 10",
        ),
        (
            "  + snippet(body, 12)",
            "get d select id, snippet(body, 12, \"…\") match body $1 limit 10",
        ),
    ] {
        let with = time(&db, sql, &queries);
        show(what, with, q, "ms a query");
        if let (Some(a), Some(b)) = (plain, with) {
            println!(
                "  {:<52} {:>9.2} us",
                "    a row's marks",
                (b - a) / q / 10.0 * 1e3
            );
        }
    }

    let one = [Value::Null];
    for (what, sql) in [
        (
            "facet brand (@hash), every row",
            "get d limit 10 facet brand",
        ),
        (
            "facet color (scan), every row",
            "get d limit 10 facet color",
        ),
        (
            "facet tags (a list, scan), every row",
            "get d limit 10 facet tags",
        ),
        (
            "count, for the scan's floor",
            "get d where price < 500 count",
        ),
        (
            "facet brand (@hash), price < 500",
            "get d where price < 500 limit 10 facet brand",
        ),
        (
            "facet color (scan), price < 500",
            "get d where price < 500 limit 10 facet color",
        ),
    ] {
        show(what, time(&db, sql, &one), 1.0, "ms");
    }
    let plain = time(&db, "get d select id match body $1 limit 10", &queries);
    show("match limit 10", plain, q, "ms a query");
    show(
        "match limit 10 facet brand, color",
        time(
            &db,
            "get d select id match body $1 limit 10 facet brand, color",
            &queries,
        ),
        q,
        "ms a query",
    );
}
