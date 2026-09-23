//! Sparse vectors: `sparse<N>`, `@inverted`, and `near` by dot product.
//!
//! The index is held to the exact scan throughout -- `near ... exact` scores
//! every document -- through writes, rewrites, deletions, a reopen, a
//! compact and an index built after the fact. The weights are multiples of
//! 1/64, so every dot product is exact in either order of summation and the
//! two paths can be compared row for row, scores included.

use fenec_core::prelude::*;
use std::sync::RwLock;

fn run(db: &mut Database, sql: &str) {
    for stmt in fenec_ql::parse(sql).expect("parse") {
        db.execute(&stmt).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

fn hits(db: &Database, sql: &str, params: &[Value]) -> Vec<(u64, f32)> {
    let r = db
        .query(&fenec_ql::parse_one(sql).expect("parse"), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    r.rows()
        .unwrap()
        .rows
        .iter()
        .map(|r| (r.id, r.score.unwrap_or(f32::NAN)))
        .collect()
}

fn refused(db: &Database, sql: &str, params: &[Value]) -> String {
    match fenec_ql::parse_one(sql) {
        Err(e) => e.to_string(),
        Ok(stmt) => db
            .query(&stmt, params)
            .err()
            .unwrap_or_else(|| panic!("{sql} was answered"))
            .to_string(),
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    /// A sparse vector in pgvector's text form: a skewed handful of the
    /// `dim` dimensions, the weights multiples of 1/64.
    fn sparse(&mut self, dim: u64, most: u64) -> String {
        let n = 1 + self.next() % most;
        let mut idx: Vec<u64> = (0..n)
            .map(|_| 1 + (self.next() % dim).pow(2) / dim)
            .collect();
        idx.sort_unstable();
        idx.dedup();
        let entries: Vec<String> = idx
            .iter()
            .map(|i| format!("{i}:{}", (1 + self.next() % 256) as f64 / 64.0))
            .collect();
        format!("{{{}}}/{dim}", entries.join(","))
    }
}

const DIM: u64 = 300;

fn filled(n: usize, seed: u64) -> Database {
    let mut db = Database::new();
    run(
        &mut db,
        &format!(
            "create collection docs (tag text @hash, body text @text, s sparse<{DIM}> @inverted)"
        ),
    );
    let mut rng = Rng(seed);
    for i in 0..n {
        let s = rng.sparse(DIM, 40);
        run(
            &mut db,
            &format!(
                "put docs {{tag: \"{}\", body: \"word{} common\", s: \"{s}\"}}",
                ["a", "b", "c"][i % 3],
                i % 7
            ),
        );
    }
    db
}

/// The index and the exact scan give the same rows and scores, with and
/// without a filter, over `queries` drawn from `seed`.
fn agree(db: &Database, seed: u64) {
    let mut rng = Rng(seed);
    for _ in 0..25 {
        let q = [Value::Text(rng.sparse(DIM, 20))];
        for filter in ["", " where tag = \"b\""] {
            for limit in [1, 10, 60] {
                let sql = format!("get docs{filter} near s $1 limit {limit}");
                let exact = format!("get docs{filter} near s $1 exact limit {limit}");
                assert_eq!(hits(db, &sql, &q), hits(db, &exact, &q), "{sql} {q:?}");
            }
        }
    }
}

#[test]
fn near_over_a_sparse_field_ranks_by_dot_product() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection docs (t text, s sparse<8> @inverted)",
    );
    run(&mut db, r#"put docs {t: "one", s: "{1:1,2:1}/8"}"#);
    run(&mut db, r#"put docs {t: "two", s: "{2:3}/8"}"#);
    run(&mut db, r#"put docs {t: "far", s: "{8:9}/8"}"#);
    run(&mut db, r#"put docs {t: "tie", s: "{1:2}/8"}"#);
    // 2 scores 3, 1 and 4 tie at 2 and go by id, 3 shares no dimension.
    let q = [Value::Text("{1:1,2:1}/8".into())];
    assert_eq!(
        hits(&db, "get docs near s $1 limit 10", &q),
        vec![(2, 3.0), (1, 2.0), (4, 2.0)]
    );
    // A literal in the query, and the value handed over already parsed.
    assert_eq!(
        hits(&db, r#"get docs near s "{2:1}/8" limit 1"#, &[]),
        vec![(2, 3.0)]
    );
    // Held, the indices count from 0: this is `{1:1}/8`.
    let parsed = [Value::Sparse(8, vec![(0, 1.0)])];
    assert_eq!(
        hits(&db, "get docs near s $1 limit 2", &parsed),
        vec![(4, 2.0), (1, 1.0)]
    );
    // What went in comes back in pgvector's text form, in index order.
    run(
        &mut db,
        r#"put docs {t: "mixed", s: "{3:0.5, 1:0.25, 2:0}/8"}"#,
    );
    let r = db
        .query(
            &fenec_ql::parse_one("get docs select s where t = \"mixed\"").unwrap(),
            &[],
        )
        .unwrap();
    let v = &r.rows().unwrap().rows[0].values[0];
    assert_eq!(*v, Value::Sparse(8, vec![(0, 0.25), (2, 0.5)]));
    assert_eq!(fenec_core::json::to_string(v), r#""{1:0.25,3:0.5}/8""#);
}

#[test]
fn the_index_answers_what_the_exact_scan_does() {
    let mut db = filled(1_500, 0x5a5a);
    agree(&db, 11);
    // Rewrites, a vector taken away, deletions: the index follows them.
    let mut rng = Rng(0x77);
    for id in (1..1_500u64).step_by(7) {
        run(
            &mut db,
            &format!(
                "set docs {{s: \"{}\"}} where id = {id}",
                rng.sparse(DIM, 40)
            ),
        );
    }
    run(&mut db, "set docs {s: null} where id < 40");
    run(&mut db, "del docs where id > 1400");
    agree(&db, 12);
}

#[test]
fn values_that_are_not_a_sparse_vector_are_refused() {
    let mut db = Database::new();
    run(&mut db, "create collection docs (s sparse<4> @inverted)");
    for (bad, why) in [
        (r#"put docs {s: "{1:1}/5"}"#, "dimension"),
        (r#"put docs {s: "{1:1,1:2}/4"}"#, "twice"),
        (r#"put docs {s: "{5:1}/4"}"#, "outside"),
        (r#"put docs {s: [1, 2, 3, 4]}"#, "sparse"),
    ] {
        let err = db
            .execute(&fenec_ql::parse_one(bad).unwrap())
            .err()
            .unwrap_or_else(|| panic!("{bad} went in"))
            .to_string();
        assert!(err.contains(why), "{bad}: {err}");
    }
    run(&mut db, r#"put docs {s: "{1:1}/4"}"#);
    let q = [Value::Text("{1:1}/4".into())];
    assert!(refused(&db, "get docs near s $1 ef 50 limit 3", &q).contains("ef"));
    assert!(refused(
        &db,
        "get docs near s $1 limit 3",
        &[Value::Text("{1:1}/5".into())]
    )
    .contains("dimension"));
    assert!(refused(
        &db,
        "get docs near s $1 limit 3",
        &[Value::Vector(vec![1.0; 4])]
    )
    .contains("sparse"));
    assert!(refused(&db, "get docs near s $1 order id limit 3", &q).contains("order"));

    let mut db = Database::new();
    let err = fenec_ql::parse_one("create collection d (t text @inverted)")
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(err.contains("not sparse<N>"), "{err}");
    let err = fenec_ql::parse_one("create collection d (s sparse<0>)")
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(err.contains("dimension"), "{err}");
    run(&mut db, "create collection d (s sparse<4>)");
    run(&mut db, r#"put d {s: "{2:1}/4"}"#);
    assert!(refused(&db, "get d near s $1 limit 1", &q).contains("@inverted"));
}

#[test]
fn an_index_built_after_the_fact_and_beside_the_database_agrees() {
    let mut plain = Database::new();
    run(
        &mut plain,
        &format!("create collection docs (tag text @hash, body text @text, s sparse<{DIM}>)"),
    );
    let mut rng = Rng(0xbeef);
    for i in 0..600 {
        run(
            &mut plain,
            &format!(
                "put docs {{tag: \"{}\", body: \"b\", s: \"{}\"}}",
                ["a", "b", "c"][i % 3],
                rng.sparse(DIM, 40)
            ),
        );
    }
    let snapshot = plain.snapshot();
    let create = fenec_ql::parse_one("create index on docs (s) @inverted").unwrap();

    // Under the lock.
    let mut under = Database::new();
    under.load(&snapshot).unwrap();
    under.execute(&create).unwrap();
    agree(&under, 21);

    // Beside the database, with writes landing while it builds.
    let mut beside = Database::new();
    beside.load(&snapshot).unwrap();
    let lock = RwLock::new(beside);
    let mut more = Rng(0xfeed);
    Database::maintain_with(&lock, &create, &mut || {
        let mut g = lock.write().unwrap();
        run(
            &mut g,
            &format!("put docs {{tag: \"b\", s: \"{}\"}}", more.sparse(DIM, 40)),
        );
        run(
            &mut g,
            &format!("set docs {{s: \"{}\"}} where id = 5", more.sparse(DIM, 40)),
        );
        run(&mut g, "del docs where id = 9");
    })
    .unwrap()
    .unwrap();
    let beside = lock.into_inner().unwrap();
    agree(&beside, 22);
    assert_eq!(
        beside
            .collection("docs")
            .unwrap()
            .sparse_index("s")
            .unwrap()
            .len(),
        600
    );
}

#[test]
fn fuse_ranks_by_the_words_and_the_sparse_vector_both() {
    let db = filled(400, 0xabc);
    let q = [
        Value::Text("word3 common".into()),
        Value::Text(Rng(5).sparse(DIM, 20)),
    ];
    let fused = hits(&db, "get docs match body $1 near s $2 fuse limit 10", &q);
    assert_eq!(fused.len(), 10);
    let explain = db
        .query(
            &fenec_ql::parse_one("explain get docs near s $2 limit 10").unwrap(),
            &q,
        )
        .unwrap();
    let text = format!("{explain:?}");
    assert!(text.contains("the inverted index on s"), "{text}");
}

#[cfg(feature = "std-fs")]
#[test]
fn the_index_is_built_again_on_open_and_follows_a_compact() {
    let dir = std::env::temp_dir().join(format!("fenecdb-sparse-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sparse.fenec");
    let _ = std::fs::remove_file(&path);
    {
        let mut db = fenec_core::fs::open(&path).unwrap();
        let mut rng = Rng(0x1234);
        run(
            &mut db,
            &format!("create collection docs (tag text @hash, body text @text, s sparse<{DIM}> @inverted)"),
        );
        for i in 0..500 {
            run(
                &mut db,
                &format!(
                    "put docs {{tag: \"{}\", body: \"b\", s: \"{}\"}}",
                    ["a", "b", "c"][i % 3],
                    rng.sparse(DIM, 40)
                ),
            );
        }
        db.checkpoint().unwrap();
        run(&mut db, "del docs where id < 100");
        run(
            &mut db,
            &format!("set docs {{s: \"{}\"}} where id = 300", rng.sparse(DIM, 40)),
        );
        db.sync().unwrap();
    }
    let mut db = fenec_core::fs::open(&path).unwrap();
    agree(&db, 31);
    run(&mut db, "compact");
    agree(&db, 32);
    drop(db);
    let db = fenec_core::fs::open_in_memory(&path).unwrap();
    agree(&db, 33);
    drop(db);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&dir);
}

/// A sparse vector is one `Vec` of pairs beside its dimension, so a `Value`
/// -- held per row and per parameter -- is no larger for having the variant.
#[test]
fn a_value_is_no_larger_for_the_sparse_variant() {
    assert_eq!(std::mem::size_of::<Value>(), 32);
}
