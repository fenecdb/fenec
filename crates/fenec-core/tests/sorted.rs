//! An ordered index never changes an answer.
//!
//! Every query here runs twice: on a collection whose fields carry `@sorted`
//! and on a twin without it, holding the same documents. The twin is the
//! scan the engine had before, so the two results have to agree row for row
//! -- order, ties and page included -- through writes, deletes, a reopen and
//! an index created after the fact.

use fenec_core::prelude::*;

const SCHEMA: &str =
    "(n int, f float, t timestamp, s text, tag text @hash, e vector<3> @hnsw(cosine))";

fn exec(db: &mut Database, sql: &str, params: &[Value]) -> Response {
    db.execute_with(&fenec_ql::parse_one(sql).expect("parse"), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn rows(db: &Database, sql: &str, params: &[Value]) -> Vec<(u64, Vec<Value>)> {
    let r = db
        .query(&fenec_ql::parse_one(sql).expect("parse"), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    r.rows()
        .expect("rows")
        .rows
        .iter()
        .map(|r| (r.id, r.values.clone()))
        .collect()
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn pick<T: Clone>(&mut self, xs: &[T]) -> T {
        xs[(self.next() % xs.len() as u64) as usize].clone()
    }
}

/// Few distinct values, so ties are everywhere; nulls, negatives, ints past
/// 2^53, `-0.0` and `NaN` among them.
fn document(r: &mut Rng) -> Vec<(String, Expr)> {
    let big = 1i64 << 53;
    let n = r.pick(&[
        Value::Null,
        Value::Int(-7),
        Value::Int(0),
        Value::Int(3),
        Value::Int(3),
        Value::Int(42),
        Value::Int(big + 1),
        Value::Int(big + 2),
        Value::Int(i64::MIN),
    ]);
    let f = r.pick(&[
        Value::Null,
        Value::Float(-2.5),
        Value::Float(-0.0),
        Value::Float(0.0),
        Value::Float(1.5),
        Value::Float(1.5),
        Value::Float(1e300),
    ]);
    let t = r.pick(&[
        Value::Null,
        Value::Text("2025-06-01T00:00:00Z".into()),
        Value::Text("2026-01-01T00:00:00Z".into()),
        Value::Text("2026-01-01T00:00:00Z".into()),
        Value::Text("2026-09-01T12:00:00Z".into()),
    ]);
    let s = r.pick(&[
        Value::Null,
        Value::Text(String::new()),
        Value::Text("Ağaç".into()),
        Value::Text("kahve".into()),
        Value::Text("kahve".into()),
        Value::Text("çay".into()),
        Value::Text("zeytin".into()),
    ]);
    let tag = r.pick(&["a", "b", "c"]);
    let e = [0, 1, 2].map(|_| ((r.next() % 1000) as f32) / 500.0 - 1.0);
    let mut doc = Vec::new();
    for (k, v) in [("n", n), ("f", f), ("t", t), ("s", s)] {
        if v != Value::Null {
            doc.push((k.to_string(), Expr::Lit(v)));
        }
    }
    doc.push(("tag".into(), Expr::Lit(Value::Text(tag.into()))));
    doc.push(("e".into(), Expr::Lit(Value::Vector(e.to_vec()))));
    doc
}

/// Two databases with the same documents: `ix` with the ordered indexes,
/// `plain` without.
fn twins(docs: usize) -> (Database, Database) {
    let mut ix = Database::new();
    let mut plain = Database::new();
    exec(
        &mut ix,
        &format!(
            "create collection c {}",
            SCHEMA
                .replace(" int,", " int @sorted,")
                .replace(" float,", " float @sorted,")
                .replace(" timestamp,", " timestamp @sorted,")
                .replace(" s text,", " s text @sorted,")
        ),
        &[],
    );
    exec(&mut plain, &format!("create collection c {SCHEMA}"), &[]);
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    let batch: Vec<_> = (0..docs).map(|_| document(&mut r)).collect();
    for db in [&mut ix, &mut plain] {
        db.execute(&Statement::Put {
            collection: "c".into(),
            docs: batch.clone(),
        })
        .unwrap();
    }
    (ix, plain)
}

fn filters() -> Vec<String> {
    let mut out = vec![String::new()];
    let cmps = ["=", "!=", "<", "<=", ">", ">="];
    let values = [
        (
            "n",
            vec!["-7", "3", "42", "9007199254740993", "-100", "12.5"],
        ),
        ("f", vec!["-0.0", "0", "1.5", "-3", "2"]),
        (
            "t",
            vec![
                "\"2026-01-01\"",
                "\"2026-01-01T00:00:00Z\"",
                "1767225600000",
                "\"garbage\"",
            ],
        ),
        (
            "s",
            vec!["\"kahve\"", "\"\"", "\"b\"", "\"Ağaç\"", "\"zzz\""],
        ),
    ];
    for (field, vals) in &values {
        for v in vals {
            for op in cmps {
                out.push(format!("where {field} {op} {v}"));
            }
            out.push(format!("where {v} < {field}"));
        }
    }
    out.extend(
        [
            "where n >= -7 and n < 42",
            "where n > 3 and n < 3",
            "where n >= 3 and n <= 3 and tag = \"b\"",
            "where n >= 0 and s = \"kahve\"",
            "where n > 0 and n > 3 and n <= 42",
            "where n < 10 or f > 1",
            "where not (n > 3)",
            "where f >= -0.0 and f <= 0",
            "where t >= \"2026-01-01\" and t < \"2026-06-01\"",
            "where s >= \"k\" and s < \"l\"",
            "where s > \"\" and tag in [\"a\", \"c\"]",
            "where n is null",
            "where n >= 3 and n != 42",
            // Filters no index narrows: an ordered walk tests them row by
            // row, and gives up on one that matches too little to fill the
            // page within its budget.
            "where s ~ \"eyt\"",
            "where s ~ \"qqq\"",
            "where n >= 3 and s ~ \"ahv\"",
        ]
        .map(String::from),
    );
    out
}

fn check(ix: &Database, plain: &Database, stage: &str) {
    let orders = [
        "",
        "order n",
        "order n desc",
        "order f desc",
        "order t",
        "order s desc",
        "order s",
        "order n desc, f",
    ];
    let pages = [
        "",
        "limit 1",
        "limit 5",
        "limit 7 offset 3",
        "limit 50 offset 40",
    ];
    for f in filters() {
        let count = format!("get c {f} count");
        assert_eq!(
            rows(ix, &count, &[]),
            rows(plain, &count, &[]),
            "{stage}: {count}"
        );
        for o in orders {
            for p in pages {
                let q = format!("get c select n, f, t, s {f} {o} {p}");
                assert_eq!(rows(ix, &q, &[]), rows(plain, &q, &[]), "{stage}: {q}");
            }
        }
    }
    // The allowed set a filtered `near` searches within.
    let v = Value::Vector(vec![0.3, -0.2, 0.9]);
    for f in [
        "where n >= 3",
        "where t < \"2026-06-01\" and s >= \"k\"",
        "where f > 0",
    ] {
        for mode in ["exact", ""] {
            let q = format!("get c select n {f} near e $1 {mode} limit 10");
            assert_eq!(
                rows(ix, &q, std::slice::from_ref(&v)),
                rows(plain, &q, std::slice::from_ref(&v)),
                "{stage}: {q}"
            );
        }
    }
}

#[test]
fn an_ordered_index_gives_the_scans_answer() {
    let (mut ix, mut plain) = twins(400);
    check(&ix, &plain, "after the load");

    // Writes: overwrite, update to and from null, delete; the index must
    // follow each one.
    for db in [&mut ix, &mut plain] {
        exec(
            db,
            "set c {n: 3, s: \"kahve\"} where tag = \"a\" and n > 40",
            &[],
        );
        exec(db, "set c {f: 1.5} where f < 0", &[]);
        exec(
            db,
            "del c where t = \"2025-06-01T00:00:00Z\" and tag = \"c\"",
            &[],
        );
        exec(db, "put c {id: 7, n: -7, s: \"zeytin\", tag: \"b\"}", &[]);
    }
    check(&ix, &plain, "after writes");

    // A reopen builds the index from the documents.
    let mut reopened = Database::new();
    reopened.load(&ix.snapshot()).unwrap();
    check(&reopened, &plain, "after a reopen");
}

/// An index created over data that is already there is the one built on
/// write.
#[test]
fn an_index_created_later_gives_the_same_answers() {
    let (_, mut plain) = twins(300);
    let mut later = Database::new();
    later.load(&plain.snapshot()).unwrap();
    for field in ["n", "f", "t", "s"] {
        exec(
            &mut later,
            &format!("create index on c ({field}) @sorted"),
            &[],
        );
    }
    check(&later, &plain, "index created later");
    exec(&mut plain, "put c {n: 1}", &[]);
    exec(&mut later, "put c {n: 1}", &[]);
    check(&later, &plain, "index created later, then a write");
}

/// `NaN` compares equal to everything under `cmp_value`, so it matches every
/// inclusive comparison and no strict one, and a walk has no place for it.
#[test]
fn nan_rows_match_what_the_scan_says_they_match() {
    let (mut ix, mut plain) = twins(120);
    for db in [&mut ix, &mut plain] {
        exec(db, "put c {f: $1, tag: \"a\"}", &[Value::Float(f64::NAN)]);
    }
    for f in [
        "where f >= 1",
        "where f > 1",
        "where f = 0",
        "where f <= 1.5 and f >= -1",
        "where f < 2",
    ] {
        for o in ["", "order f desc limit 5", "order f limit 5 offset 2"] {
            let q = format!("get c select n, f {f} {o}");
            let (a, b) = (rows(&ix, &q, &[]), rows(&plain, &q, &[]));
            // NaN != NaN, so compare the ids and treat the values bitwise.
            let key = |rs: Vec<(u64, Vec<Value>)>| {
                rs.into_iter()
                    .map(|(id, vs)| (id, format!("{vs:?}")))
                    .collect::<Vec<_>>()
            };
            assert_eq!(key(a), key(b), "{q}");
        }
    }
}

#[test]
fn only_types_with_an_order_take_the_index() {
    let mut db = Database::new();
    for bad in [
        "create collection x (v vector<3> @sorted)",
        "create collection x (l [int] @sorted)",
        "create collection x (b bool @sorted)",
        "create collection x (y bytes @sorted)",
    ] {
        // The schema refuses it as it is built, which is at parse time.
        let refused = match fenec_ql::parse_one(bad) {
            Err(e) => e.to_string(),
            Ok(stmt) => db.execute(&stmt).unwrap_err().to_string(),
        };
        assert!(refused.contains("no ordered index"), "{bad}: {refused}");
    }
    exec(&mut db, "create collection x (b bool)", &[]);
    let stmt = fenec_ql::parse_one("create index on x (b) @sorted").unwrap();
    assert!(matches!(db.execute(&stmt), Err(Error::Type(_))));
}
