//! `explain` names the path a query took, one step a row. Every plan the
//! engine can choose is run here once, and its steps are checked for the
//! words that tell the paths apart -- so a path that stops reporting itself,
//! or reports another's name, fails here rather than in someone's tuning.

use fenec_core::prelude::*;

fn exec(db: &mut Database, sql: &str) {
    db.execute(&fenec_ql::parse_one(sql).expect("parse"))
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// The plan's steps, and the row count of the same query run plainly.
fn explain(db: &Database, sql: &str, params: &[Value]) -> Vec<String> {
    let stmt = fenec_ql::parse_one(&format!("explain {sql}")).expect("parse");
    let r = db
        .query(&stmt, params)
        .unwrap_or_else(|e| panic!("explain {sql}: {e}"));
    let rs = r.rows().expect("rows");
    assert_eq!(rs.columns, vec!["plan".to_string()]);
    let steps: Vec<String> = rs
        .rows
        .iter()
        .map(|r| match &r.values[..] {
            [Value::Text(s)] => s.clone(),
            other => panic!("{sql}: a plan row holds {other:?}"),
        })
        .collect();
    // The last step is the answer's size, and it is the answer the query
    // gives without `explain`.
    let plain = db
        .query(&fenec_ql::parse_one(sql).expect("parse"), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    let plain = plain.rows().expect("rows");
    let last = steps.last().expect("a plan has steps");
    match &plain.rows.first().map(|r| &r.values[..]) {
        Some([Value::Int(n)]) if plain.columns == ["count"] => {
            assert_eq!(last, &format!("count: {n}"), "{sql}")
        }
        _ => assert_eq!(last, &format!("rows: {}", plain.rows.len()), "{sql}"),
    }
    steps
}

/// Each expected phrase is found in the steps, in order; two phrases may
/// come from the same step.
fn check(db: &Database, sql: &str, params: &[Value], expected: &[&str]) {
    let steps = explain(db, sql, params);
    let mut at = 0;
    for want in expected {
        match steps[at..].iter().position(|s| s.contains(want)) {
            Some(i) => at += i,
            None => panic!(
                "{sql}: no step says `{want}` from step {at} on:\n{}",
                steps.join("\n")
            ),
        }
    }
}

fn db() -> Database {
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection a (title text @hash, year int @sorted, views int, tag text, \
         body text @text, e vector<3> @hnsw(cosine, m=8))",
    );
    exec(
        &mut db,
        "create collection r (article_id int @hash, stars int)",
    );
    let words = ["rust", "borrow", "ownership", "vector", "index", "query"];
    let docs: Vec<Vec<(String, Expr)>> = (0..2000usize)
        .map(|i| {
            let x = (i % 97) as f32 / 97.0;
            vec![
                (
                    "title".into(),
                    Expr::Lit(Value::Text(format!("t{}", i % 50))),
                ),
                ("year".into(), Expr::Lit(Value::Int(2000 + (i % 26) as i64))),
                ("views".into(), Expr::Lit(Value::Int(i as i64))),
                (
                    "tag".into(),
                    Expr::Lit(Value::Text(["a", "b", "c"][i % 3].into())),
                ),
                (
                    "body".into(),
                    Expr::Lit(Value::Text(format!("{} {}", words[i % 6], words[i % 5]))),
                ),
                (
                    "e".into(),
                    Expr::Lit(Value::Vector(vec![x, 1.0 - x, (i % 7) as f32])),
                ),
            ]
        })
        .collect();
    db.execute(&Statement::Put {
        collection: "a".into(),
        docs,
    })
    .unwrap();
    let children: Vec<Vec<(String, Expr)>> = (0..4000usize)
        .map(|i| {
            vec![
                (
                    "article_id".into(),
                    Expr::Lit(Value::Int((i % 1500 + 1) as i64)),
                ),
                ("stars".into(), Expr::Lit(Value::Int((i % 5 + 1) as i64))),
            ]
        })
        .collect();
    db.execute(&Statement::Put {
        collection: "r".into(),
        docs: children,
    })
    .unwrap();
    db
}

#[test]
fn every_filter_path_names_itself() {
    let db = db();
    check(&db, "get a limit 5", &[], &["filter: none", "rows: 5"]);
    check(
        &db,
        "get a where views >= 1990 limit 5",
        &[],
        &["filter: a full scan", "stopped at the page", "rows: 5"],
    );
    check(
        &db,
        "get a where tag ~ \"b\"",
        &[],
        &["filter: a full scan, 2000 of 2000 rows tested, 667 matched"],
    );
    check(
        &db,
        "get a where title = \"t7\"",
        &[],
        &[
            "filter: the hash index on title, 40 rows, which is the answer",
            "rows: 40",
        ],
    );
    check(
        &db,
        "get a where title = \"t7\" and views > 1000",
        &[],
        &[
            "filter: the hash index on title, 40 rows",
            "filter: 40 candidates tested, 20 matched",
        ],
    );
    check(
        &db,
        "get a where title in [\"t1\", \"t2\"]",
        &[],
        &["filter: the hash index, `in`, on title, 80 rows, which is the answer"],
    );
    check(
        &db,
        "get a where id = 7",
        &[],
        &["filter: the id index, 1 rows, which is the answer"],
    );
    check(
        &db,
        "get a where year >= 2024",
        &[],
        &["filter: the ordered index on year, 152 rows, which is the answer"],
    );
    check(
        &db,
        "get a where year >= 2001",
        &[],
        &["filter: the ordered index on year not used, its range holds more than 1000 rows"],
    );
    check(&db, "get a where year = 2010 count", &[], &["count: 77"]);
}

#[test]
fn every_order_path_names_itself() {
    let db = db();
    check(
        &db,
        "get a order year desc limit 5",
        &[],
        &["order: walked the ordered index on year desc, 5 rows"],
    );
    check(
        &db,
        "get a where tag ~ \"b\" order year limit 5",
        &[],
        &[
            "order: walked the ordered index on year, ",
            "rows tested, 5 kept",
        ],
    );
    check(
        &db,
        "get a where tag ~ \"zz\" order year limit 5",
        &[],
        &[
            "order: walked the ordered index on year, gave up after 250 rows",
            "filter: a full scan",
            "order: year, every key read, 0 matches",
        ],
    );
    check(
        &db,
        "get a where title = \"t7\" order year desc limit 5",
        &[],
        &[
            "order: the ordered index on year not walked, an equality names fewer rows",
            "filter: the hash index on title",
            "order: year desc, every key read, 40 matches, the first 5 put in order",
        ],
    );
    check(
        &db,
        "get a order views desc, id limit 3",
        &[],
        &[
            "filter: none",
            "order: views desc, id, every key read, 2000 matches, the first 3",
        ],
    );

    // `NaN` has no place in an order, so an index holding one is not walked.
    let mut db = Database::new();
    exec(&mut db, "create collection f (x float @sorted)");
    exec(&mut db, "put f [{x: 1.5}, {x: 2.5}]");
    db.execute_with(
        &fenec_ql::parse_one("put f {x: $1}").unwrap(),
        &[Value::Float(f64::NAN)],
    )
    .unwrap();
    check(
        &db,
        "get f order x limit 2",
        &[],
        &[
            "order: the ordered index on x not walked, it holds a NaN",
            "order: x, every key read",
        ],
    );
}

#[test]
fn every_near_path_names_itself() {
    let db = db();
    let q = [Value::Vector(vec![0.3, 0.7, 2.0])];
    check(
        &db,
        "get a near e $1 limit 3",
        &q,
        &["near: ANN over e, ef 100", "rows: 3"],
    );
    check(
        &db,
        "get a near e $1 ef 2 limit 3",
        &q,
        &["near: ANN over e, ef 2, widened to the 3 rows asked for"],
    );
    check(
        &db,
        "get a near e $1 exact limit 3",
        &q,
        &["near: exact scan over every vector in e"],
    );
    // Fewer rows than the ANN would measure: found whole, searched exactly.
    check(
        &db,
        "get a where views < 10 near e $1 limit 3",
        &q,
        &[
            "filter: probed 2000 of 2000 rows, 10 matched, the whole set",
            "near: the 10 rows searched exactly, under the ANN budget of 1600",
        ],
    );
    // More rows than the budget: the probe stops, the ANN tests its candidates.
    check(
        &db,
        "get a where views >= 100 near e $1 ef 4 limit 3",
        &q,
        &[
            "matched, more than the ANN budget of 64",
            "near: ANN over e, ef 4, ",
            "candidates tested, 3 kept",
        ],
    );
    // A filter the ANN's candidates all fail: the probe finishes and the set
    // is searched exactly.
    check(
        &db,
        "get a where tag = \"a\" and views < 300 and views > 200 near e $1 ef 1 limit 20",
        &q,
        &[
            "more than the ANN budget of 16",
            "near: the ANN came up short, the probe finished",
        ],
    );
    // An index hands over the whole set; past the budget the ANN tests it.
    check(
        &db,
        "get a where title = \"t7\" near e $1 ef 1 limit 3",
        &q,
        &[
            "filter: the hash index on title, 40 rows, which is the answer",
            "near: ANN over e, the set as a test",
        ],
    );
    check(
        &db,
        "get a where title = \"t7\" near e $1 exact limit 3",
        &q,
        &["near: exact scan over every vector in e, the set as a test"],
    );
}

#[test]
fn match_lookup_and_required_name_themselves() {
    let db = db();
    let q = [Value::Vector(vec![0.3, 0.7, 2.0])];
    check(
        &db,
        "get a match body \"rust\" limit 5",
        &[],
        &["match: the text index on body, 5 ranked"],
    );
    check(
        &db,
        "get a match body \"rust\" rerank e $1 candidates 50 limit 5",
        &q,
        &[
            "match: the text index on body, 50 candidates",
            "rerank: e, exact distance",
        ],
    );
    check(
        &db,
        "get a where title = \"t7\" limit 3 lookup r on article_id where stars >= 4 limit 2",
        &[],
        &[
            "filter: the hash index on title",
            "lookup: r on article_id, the hash index probed for 3 parents",
        ],
    );
    check(
        &db,
        "get r where id < 4 limit 3 lookup a on id = article_id",
        &[],
        &["lookup: a on id, the id probed for 3 parents, 3 children"],
    );
    check(
        &db,
        "get a count lookup r on article_id required where stars = 5",
        &[],
        &[
            "filter: none",
            "required: from the parent side, 2000 parents probed in r",
        ],
    );
    check(
        &db,
        "get a where views < 10 count lookup r on article_id required where article_id = 3",
        &[],
        &["required: from the child side"],
    );
}

#[test]
fn explain_takes_only_a_read() {
    for bad in [
        "explain put a {views: 1}",
        "explain del a",
        "explain explain get a",
    ] {
        let e = fenec_ql::parse_one(bad).unwrap_err().to_string();
        assert!(
            e.contains("`explain` takes a `get` or `select`"),
            "{bad}: {e}"
        );
    }
    let stmt = fenec_ql::parse_one("explain select views from a where views < $1").unwrap();
    assert!(stmt.is_read_only());
    assert_eq!(stmt.max_param(), 1);
}
