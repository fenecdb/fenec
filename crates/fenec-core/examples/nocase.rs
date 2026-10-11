//! What comparing by the case fold costs a `nocase` field: `make
//! nocase-bench`.
//!
//! Two collections of the same 100 000 rows, one of plain `text` fields and
//! one of `nocase` ones: an email under `@unique`, a name with no index (a
//! third of them past ASCII: Turkish, German, Greek), and a city under
//! `@hash` (50 of them). The plain collection holds each text as one
//! spelling and is asked that spelling; the `nocase` one holds it in mixed
//! case and is asked another mix -- so each finds the same rows, and the
//! difference is what folding costs. Each query is timed as the median of
//! `RUNS` over 200 of them (10 for those that read every row):
//!
//! - a put of a row, one statement each, asking the `@unique` index;
//! - an equality through the unique index, and a count through a bucket;
//! - an equality and a `~` over every row's name, with no index;
//! - an order by the name, a page of 20, which reads and compares every row;
//! - the rows by city, `group` and `facet`, and a facet of names by the scan.
//!
//! A build that knows no `nocase` prints its plain column alone, so the
//! program measures main's plain fields against this build's. Words after
//! the command keep the cases whose name holds one of them: `make
//! nocase-bench ARGS=scan`.

use fenec_core::prelude::*;
use std::time::Instant;

const ROWS: usize = 100_000;
const RUNS: usize = 11;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    /// `s` with each character's case thrown again.
    fn respell(&mut self, s: &str) -> String {
        let mut out = String::new();
        for c in s.chars() {
            match self.below(3) {
                0 => out.push(c),
                1 => out.extend(c.to_uppercase()),
                _ => out.extend(c.to_lowercase()),
            }
        }
        out
    }
}

const FIRST: [&str; 12] = [
    "Ada",
    "Grace",
    "Ömer",
    "Şule",
    "İsmail",
    "Işık",
    "Jürgen",
    "Straße",
    "Νίκος",
    "Linus",
    "Barbara",
    "Ken",
];

fn exec(db: &mut Database, sql: &str, params: &[Value]) {
    for s in fenec_ql::parse(sql).expect("parse") {
        db.execute_with(&s, params).expect("execute");
    }
}

/// Median milliseconds of running every query once.
fn time(db: &Database, sql: &str, queries: &[Value]) -> f64 {
    let stmt = fenec_ql::parse_one(sql).expect("parse");
    let run = |db: &Database| {
        for q in queries {
            std::hint::black_box(db.query(&stmt, std::slice::from_ref(q)).expect(sql));
        }
    };
    run(db);
    let mut t: Vec<f64> = (0..RUNS)
        .map(|_| {
            let s = Instant::now();
            run(db);
            s.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    t.sort_by(|a, b| a.total_cmp(b));
    t[RUNS / 2]
}

struct Row {
    email: String,
    name: String,
    city: String,
}

fn main() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let cities: Vec<String> = (0..50).map(|i| format!("City{i}")).collect();
    let rows: Vec<Row> = (0..ROWS)
        .map(|i| {
            let first = FIRST[rng.below(FIRST.len())];
            let city = rng.below(cities.len());
            Row {
                email: format!("user{i}@example.com"),
                name: format!("{first} {}", rng.below(1000)),
                city: cities[city].clone(),
            }
        })
        .collect();
    let mut db = Database::new();
    println!("{ROWS} rows; ms are the median of {RUNS} runs; plain against nocase\n");
    let mut put = [0.0; 2];
    let folds = fenec_ql::parse_one("create collection t (a text nocase)").is_ok();
    let kinds: &[(&str, &str)] = match folds {
        true => &[("plain", ""), ("folded", " nocase")],
        false => &[("plain", "")],
    };
    for (k, (coll, mods)) in kinds.iter().enumerate() {
        exec(
            &mut db,
            &format!(
                "create collection {coll} (email text{mods} @unique, name text{mods}, \
                 city text{mods} @hash, n int)"
            ),
            &[],
        );
        let stmt = fenec_ql::parse_one(&format!(
            "put {coll} {{email: $1, name: $2, city: $3, n: $4}}"
        ))
        .unwrap();
        // Spelled before the clock starts.
        let mut text = |t: &str| Value::Text(if k == 1 { rng.respell(t) } else { t.into() });
        let params: Vec<[Value; 4]> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                [
                    text(&r.email),
                    text(&r.name),
                    text(&r.city),
                    Value::Int(i as i64),
                ]
            })
            .collect();
        let s = Instant::now();
        for p in &params {
            db.execute_with(&stmt, p).unwrap();
        }
        put[k] = s.elapsed().as_secs_f64() * 1e9 / ROWS as f64;
    }
    println!(
        "  {:<44} {:>9.0} {:>9.0} ns",
        "a put, asking @unique", put[0], put[1]
    );
    // The plain collection is asked a row's text as written, the `nocase`
    // one the same text respelled: the same rows either way.
    let mut q = Vec::new();
    for _ in 0..200 {
        let r = &rows[rng.below(ROWS)];
        q.push((r.email.clone(), r.name.clone(), r.city.clone()));
    }
    let cases: [(&str, &str, usize); 9] = [
        ("an email by @unique", "get C where email = $1", 0),
        ("a city's rows counted by @hash", "get C where city = $1 count", 2),
        ("a name by the scan", "get C where name = $1 count", 1),
        ("a name's part by `~`", "get C where name ~ $1 count", 1),
        ("an order by name, a page", "get C select id order name limit 20", 1),
        ("rows by city, `group`", "get C select city, count() group city", 2),
        ("rows by city, `facet` by the buckets", "get C facet city limit 0", 2),
        ("a facet of names by the scan", "get C where n < 20000 facet name top 10 limit 0", 1),
        ("`in` a list of 10 names, the scan", "get C where name in [$1, \"a\", \"b\", \"c\", \"d\", \"e\", \"f\", \"g\", \"h\", \"i\"] count", 1),
    ];
    let only: Vec<String> = std::env::args().skip(1).collect();
    for (what, sql, field) in cases {
        if !only.is_empty() && !only.iter().any(|w| what.contains(w.as_str())) {
            continue;
        }
        // An order, a group and a facet read every row once a query: ten
        // queries are enough of them.
        let n = if sql.contains("where") && !sql.contains("facet") {
            200
        } else {
            10
        };
        let mut ms = [0.0; 2];
        for (k, (coll, _)) in kinds.iter().enumerate() {
            let queries: Vec<Value> = q
                .iter()
                .take(n)
                .map(|x| {
                    let t = [&x.0, &x.1, &x.2][field];
                    Value::Text(if k == 1 { rng.respell(t) } else { t.clone() })
                })
                .collect();
            ms[k] = time(&db, &sql.replace(" C ", &format!(" {coll} ")), &queries);
        }
        let per = n as f64;
        let (a, b) = (ms[0] / per, ms[1] / per);
        let unit = if a < 0.01 { "us" } else { "ms" };
        let scale = if unit == "us" { 1e3 } else { 1.0 };
        println!(
            "  {what:<44} {:>9.3} {:>9.3} {unit}  ({:+.0}%)",
            a * scale,
            b * scale,
            (b / a - 1.0) * 100.0
        );
    }
}
