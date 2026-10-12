//! `match ... fuzzy` and `match ... prefix` through FenecQL: what they
//! parse to and refuse, what they find and how it ranks beside `where`,
//! `rerank`, `fuse`, `facet`, `highlight()` and `explain`, the words kept
//! up through writes, a block put back and an image loaded, and the bound
//! past which a word is refused. `text.rs` and `fuzzy.rs` hold the walk
//! and the ranking to their references over generated corpora.

use fenec_core::prelude::*;

fn exec(db: &mut Database, sql: &str, params: &[Value]) {
    db.execute_with(&fenec_ql::parse_one(sql).expect("parse"), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn rows(db: &Database, sql: &str, params: &[Value]) -> ResultSet {
    let r = db
        .query(&fenec_ql::parse_one(sql).expect("parse"), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    r.rows().unwrap().clone()
}

/// The `n` of each row, in order.
fn ns(db: &Database, sql: &str, q: &str) -> Vec<i64> {
    rows(db, sql, &[Value::Text(q.into())])
        .rows
        .iter()
        .map(|r| match r.values[0] {
            Value::Int(n) => n,
            ref v => panic!("{v:?}"),
        })
        .collect()
}

fn error(db: &Database, sql: &str, params: &[Value]) -> String {
    match fenec_ql::parse_one(sql) {
        Err(e) => e.to_string(),
        Ok(stmt) => db.query(&stmt, params).expect_err(sql).to_string(),
    }
}

const BODIES: [&str; 6] = [
    "a database of documents",
    "databases and their indexes",
    "the documentation of an index",
    "İstanbul has a database too",
    "nothing to see here",
    "Database DATABASE database",
];

fn db() -> Database {
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection d (n int, body text @text, kind text @hash, v vector<2> @hnsw(l2))",
        &[],
    );
    for (i, b) in BODIES.iter().enumerate() {
        exec(
            &mut db,
            "put d {n: $1, body: $2, kind: $3, v: $4}",
            &[
                Value::Int(i as i64),
                Value::Text(b.to_string()),
                Value::Text(["a", "b"][i % 2].into()),
                Value::Vector(vec![1.0, i as f32]),
            ],
        );
    }
    db
}

#[test]
fn the_options_parse_in_either_order_and_are_refused_past_two() {
    let m = |sql: &str| match fenec_ql::parse_one(sql).expect(sql) {
        Statement::Select(s) => s.matcher.map(|m| (m.fuzzy, m.prefix)),
        other => panic!("{other:?}"),
    };
    assert_eq!(m("get d match body $1"), Some((0, false)));
    assert_eq!(m("get d match body $1 fuzzy"), Some((2, false)));
    assert_eq!(m("get d match body $1 fuzzy 1"), Some((1, false)));
    assert_eq!(m("get d match body $1 fuzzy 0"), Some((0, false)));
    assert_eq!(m("get d match body $1 prefix"), Some((0, true)));
    assert_eq!(m("get d match body $1 fuzzy 1 prefix"), Some((1, true)));
    assert_eq!(
        m("get d match body $1 prefix fuzzy limit 5"),
        Some((2, true))
    );
    assert_eq!(
        m(r#"get d where n > 1 match body "x" prefix fuzzy 2 limit 5"#),
        Some((2, true))
    );
    // The clauses after them are the statement's.
    match fenec_ql::parse_one("get d match body $1 fuzzy limit 3 offset 1").unwrap() {
        Statement::Select(s) => {
            assert_eq!(s.limit, Some(Count::N(3)));
            assert_eq!(s.offset, Count::N(1));
        }
        other => panic!("{other:?}"),
    }
    let db = db();
    for sql in [
        "get d match body $1 fuzzy 3",
        "get d match body $1 fuzzy -1",
        "get d match body $1 fuzzy 1000",
    ] {
        let e = error(&db, sql, &[Value::Text("x".into())]);
        assert!(e.contains("`fuzzy` takes 0 to 2 edits"), "{sql}: {e}");
    }
    // Built in code, the engine refuses it in the same words.
    let mut s = match fenec_ql::parse_one("get d match body $1").unwrap() {
        Statement::Select(s) => s,
        _ => unreachable!(),
    };
    s.matcher.as_mut().unwrap().fuzzy = 3;
    let e = db
        .query(&Statement::Select(s), &[Value::Text("x".into())])
        .unwrap_err();
    assert!(e.to_string().contains("`fuzzy` takes 0 to 2 edits"), "{e}");
}

#[test]
fn a_typo_and_a_word_begun_find_what_they_mean() {
    let db = db();
    let q = "get d select n match body $1 limit 10";
    assert!(ns(&db, q, "databse").is_empty());
    let fuzzy = "get d select n match body $1 fuzzy limit 10";
    // The word as typed first: every document holding `database`, then
    // the one holding only `databases`.
    let got = ns(&db, fuzzy, "databse");
    assert_eq!(got, [5, 0, 3], "seven letters take one edit");
    // Nine letters take two: `database` and `databases` are each one
    // away, one term between them, the shorter document first.
    assert_eq!(ns(&db, fuzzy, "databasse"), [5, 0, 1, 3]);
    // Turkish's I folds before the edits are counted.
    assert_eq!(ns(&db, fuzzy, "İSTANBL"), [3]);
    // Short words take none.
    assert!(ns(&db, fuzzy, "indx").is_empty());
    let prefix = "get d select n match body $1 prefix limit 10";
    assert_eq!(ns(&db, prefix, "datab"), [5, 0, 1, 3]);
    assert_eq!(ns(&db, prefix, "docu"), [0, 2]);
    // The last word alone.
    assert_eq!(ns(&db, prefix, "docu nothing"), [4]);
    assert_eq!(ns(&db, prefix, "nothing docu")[0], 4);
    // A filter, a reranking and a fusion take a loose `match` as any.
    assert_eq!(
        ns(
            &db,
            "get d select n where kind = \"b\" match body $1 fuzzy limit 10",
            "databasse"
        ),
        [5, 1, 3]
    );
    let r = rows(
        &db,
        "get d select n match body $1 fuzzy rerank v $2 limit 2",
        &[Value::Text("databse".into()), Value::Vector(vec![1.0, 0.0])],
    );
    assert_eq!(r.rows.len(), 2);
    let r = rows(
        &db,
        "get d select n match body $1 prefix near v $2 fuse limit 10",
        &[Value::Text("docu".into()), Value::Vector(vec![1.0, 0.0])],
    );
    assert_eq!(r.rows.len(), 6);
}

#[test]
fn a_mark_is_the_word_a_variant_matched() {
    let db = db();
    let r = rows(
        &db,
        r#"get d select n, highlight(body, "[", "]") match body $1 fuzzy prefix limit 10"#,
        &[Value::Text("indexs docum".into())],
    );
    let marked: Vec<(i64, String)> = r
        .rows
        .iter()
        .map(|r| match (&r.values[0], &r.values[1]) {
            (Value::Int(n), Value::Text(t)) => (*n, t.clone()),
            other => panic!("{other:?}"),
        })
        .collect();
    assert!(
        marked.contains(&(1, "databases and their [indexes]".into())),
        "{marked:?}"
    );
    // `indexs` is one edit from `index` as from `indexes`.
    assert!(
        marked.contains(&(2, "the [documentation] of an [index]".into())),
        "{marked:?}"
    );
    assert!(marked.contains(&(0, "a database of [documents]".into())));
    let r = rows(
        &db,
        r#"get d select snippet(body, 3, "…", "<", ">") match body $1 fuzzy limit 1"#,
        &[Value::Text("databse".into())],
    );
    assert_eq!(
        r.rows[0].values[0],
        Value::Text("<Database> <DATABASE> <database>".into())
    );
}

#[test]
fn a_facet_counts_what_a_loose_match_selects() {
    let db = db();
    let r = rows(
        &db,
        "get d select n match body $1 prefix limit 1 facet kind",
        &[Value::Text("datab".into())],
    );
    assert_eq!(
        r.facets[0].values,
        [(Value::Text("b".into()), 3), (Value::Text("a".into()), 1)]
    );
    let r = rows(
        &db,
        "get d select n match body $1 limit 1 facet kind",
        &[Value::Text("datab".into())],
    );
    assert!(r.facets[0].values.is_empty());
}

#[test]
fn explain_says_how_the_words_were_read() {
    let db = db();
    for (sql, says) in [
        (
            "get d match body $1 fuzzy limit 3",
            "match: the text index on body, fuzzy, ",
        ),
        ("get d match body $1 fuzzy 1 limit 3", "on body, fuzzy 1, "),
        ("get d match body $1 prefix limit 3", "on body, prefix, "),
        (
            "get d match body $1 prefix fuzzy limit 3",
            "on body, fuzzy, prefix, ",
        ),
        (
            "get d match body $1 limit 3",
            "match: the text index on body, ",
        ),
    ] {
        let r = rows(
            &db,
            &format!("explain {sql}"),
            &[Value::Text("datab".into())],
        );
        let steps: Vec<String> = r
            .rows
            .iter()
            .map(|r| match &r.values[0] {
                Value::Text(t) => t.clone(),
                v => panic!("{v:?}"),
            })
            .collect();
        assert!(steps.iter().any(|s| s.contains(says)), "{sql}: {steps:?}");
    }
}

#[test]
fn the_words_follow_the_writes() {
    let mut db = db();
    let fuzzy = "get d select n match body $1 fuzzy limit 10";
    // The words are sorted by this first loose `match`; the writes after
    // it keep them up.
    assert!(ns(&db, fuzzy, "telescpoe").is_empty());
    exec(&mut db, r#"put d {n: 10, body: "a telescope"}"#, &[]);
    assert_eq!(ns(&db, fuzzy, "telescpoe"), [10]);
    exec(&mut db, "del d where n = 10", &[]);
    assert!(ns(&db, fuzzy, "telescpoe").is_empty());
    // A block put back takes its words back out.
    let block =
        fenec_ql::parse(r#"put d {n: 11, body: "microscope"}; put d {n: 1, body: "x"} require 2"#)
            .unwrap();
    let block: Vec<(&Statement, &[Value])> = block.iter().map(|s| (s, &[][..])).collect();
    assert!(db.execute_block(&block).is_err());
    assert!(ns(&db, fuzzy, "micrsocope").is_empty());
    // An image loaded has no words sorted, and sorts them when asked.
    exec(&mut db, r#"put d {n: 12, body: "a periscope"}"#, &[]);
    let mut fresh = Database::new();
    fresh.load(&db.snapshot()).expect("load");
    assert_eq!(ns(&fresh, fuzzy, "periscpoe"), [12]);
    assert_eq!(ns(&fresh, fuzzy, "databse"), ns(&db, fuzzy, "databse"));
}

#[test]
fn a_word_reaching_too_many_is_refused_not_cut() {
    let mut db = Database::new();
    exec(&mut db, "create collection d (body text @text)", &[]);
    let words: Vec<String> = (0..10_001).map(|i| format!("pre{i}")).collect();
    for chunk in words.chunks(2_000) {
        exec(
            &mut db,
            "put d $1",
            &[Value::List(
                chunk
                    .iter()
                    .map(|w| Value::object(vec![("body".into(), Value::Text(w.clone()))]).unwrap())
                    .collect(),
            )],
        );
    }
    let e = error(
        &db,
        "get d match body $1 prefix limit 5",
        &[Value::Text("pre".into())],
    );
    assert!(
        e.contains("`match ... prefix` reads `pre` as more than 10000 words")
            && e.contains("refused rather than cut"),
        "{e}"
    );
    // Narrower, it answers.
    let r = rows(
        &db,
        "get d match body $1 prefix limit 5",
        &[Value::Text("pre19".into())],
    );
    assert_eq!(r.rows.len(), 5);
    // The same words plainly are no question of the bound.
    let r = rows(
        &db,
        "get d match body $1 limit 5",
        &[Value::Text("pre".into())],
    );
    assert!(r.rows.is_empty());
}

#[test]
fn an_index_of_prefixes_reads_loosely_as_any() {
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection d (n int, body text @text(prefix=3))",
        &[],
    );
    for (i, b) in ["kitapların fiyatı", "araba kiralama", "kitaplık"]
        .iter()
        .enumerate()
    {
        exec(
            &mut db,
            "put d {n: $1, body: $2}",
            &[Value::Int(i as i64), Value::Text(b.to_string())],
        );
    }
    let q = "get d select n match body $1 fuzzy limit 10";
    // `^`-marked prefixes are no words: a typo reaches the words.
    assert_eq!(ns(&db, q, "kiralma"), [1]);
    assert_eq!(
        ns(
            &db,
            "get d select n match body $1 prefix limit 10",
            "kitapl"
        ),
        [2, 0]
    );
}
