//! `put ... if absent else set {..}` is an upsert: a document whose id or
//! `@unique` value a live row holds sets that row as `set` would, `new.f`
//! the document's own `f`; any other is written. One statement under the
//! write lock, so a counter kept this way loses no addition.
//!
//! `put <name> $n` takes its documents from a parameter: an object or a
//! list of objects, read as a body's document is.

use fenec_core::prelude::*;
use std::sync::{Arc, RwLock};

fn run(db: &mut Database, sql: &str) -> Result<Response> {
    db.execute(&fenec_ql::parse_one(sql).expect("parse"))
}

fn run_with(db: &mut Database, sql: &str, params: &str) -> Result<Response> {
    let params = fenec_core::json::parse_params(params).expect("params");
    db.execute_with(&fenec_ql::parse_one(sql).expect("parse"), &params)
}

fn affected(r: Result<Response>) -> usize {
    match r.unwrap_or_else(|e| panic!("{e}")) {
        Response::Affected(n) => n,
        other => panic!("{other:?}"),
    }
}

/// `key -> n`, every row, by key.
fn counts(db: &Database, c: &str) -> Vec<(String, i64)> {
    let r = db
        .query(
            &fenec_ql::parse_one(&format!("get {c} select key, n order key")).unwrap(),
            &[],
        )
        .unwrap();
    r.rows()
        .unwrap()
        .rows
        .iter()
        .map(|r| match (&r.values[0], &r.values[1]) {
            (Value::Text(k), Value::Int(n)) => (k.clone(), *n),
            other => panic!("{other:?}"),
        })
        .collect()
}

fn rollups() -> Database {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection m (key text @unique, n int, at timestamp)",
    )
    .unwrap();
    db
}

#[test]
fn an_upsert_adds_to_the_row_holding_the_unique_value_and_makes_the_rest() {
    let mut db = rollups();
    let up = "put m {key: $1, n: $2} if absent else set {n: n + new.n}";
    assert_eq!(affected(run_with(&mut db, up, r#"["a", 2]"#)), 1);
    assert_eq!(affected(run_with(&mut db, up, r#"["a", 3]"#)), 1);
    assert_eq!(affected(run_with(&mut db, up, r#"["b", 7]"#)), 1);
    assert_eq!(counts(&db, "m"), [("a".into(), 5), ("b".into(), 7)]);
    // A page in one statement, a key twice in it: the second adds to the
    // row the first made.
    let page = "put m [{key: \"a\", n: 1}, {key: \"c\", n: 4}, {key: \"c\", n: 6}] \
                if absent else set {n: n + new.n}";
    assert_eq!(affected(run(&mut db, page)), 3);
    assert_eq!(
        counts(&db, "m"),
        [("a".into(), 6), ("b".into(), 7), ("c".into(), 10)]
    );
    // The set's values read the row and the document both, and any
    // expression: a parameter of the statement, a function.
    run_with(
        &mut db,
        "put m {key: \"b\", n: 100} if absent else set {n: greatest(n, new.n) + $1}",
        "[1]",
    )
    .unwrap();
    assert_eq!(counts(&db, "m")[1], ("b".into(), 101));
    // Fields the set does not name stay as the row holds them.
    run(&mut db, "set m {at: 5} where key = \"a\"").unwrap();
    run(
        &mut db,
        "put m {key: \"a\", n: 1, at: 9} if absent else set {n: n + 1}",
    )
    .unwrap();
    let r = db
        .query(
            &fenec_ql::parse_one("get m select at where key = \"a\"").unwrap(),
            &[],
        )
        .unwrap();
    assert_eq!(r.rows().unwrap().rows[0].values, [Value::Timestamp(5)]);
}

#[test]
fn an_upsert_by_id_and_its_counts_and_refusals() {
    let mut db = rollups();
    run(&mut db, "put m {id: 3, key: \"x\", n: 1}").unwrap();
    // By the id: the row holding it is set.
    assert_eq!(
        affected(run(
            &mut db,
            "put m {id: 3, key: \"x\", n: 4} if absent else set {n: n + new.n} require 1"
        )),
        1
    );
    assert_eq!(counts(&db, "m"), [("x".into(), 5)]);
    // `new.id` is the id it names; one naming none reads null.
    run(
        &mut db,
        "put m {id: 3, n: 0} if absent else set {at: new.id}",
    )
    .unwrap();
    run(
        &mut db,
        "put m {key: \"x\", n: 0} if absent else set {n: coalesce(new.id, 40)}",
    )
    .unwrap();
    assert_eq!(counts(&db, "m"), [("x".into(), 40)]);
    // A field the collection does not have, read as the document's.
    let e = run(
        &mut db,
        "put m {key: \"x\", n: 0} if absent else set {n: new.nope}",
    )
    .unwrap_err();
    assert!(matches!(e, Error::NotFound(_)), "{e}");
    // `require` counts the rows set and made together, and puts the
    // statement back when they are not that many.
    let e = run(
        &mut db,
        "put m [{key: \"x\", n: 1}, {key: \"y\", n: 1}] if absent else set {n: n + 1} require 1",
    )
    .unwrap_err();
    assert!(matches!(e, Error::Unmet(_)), "{e}");
    assert_eq!(counts(&db, "m"), [("x".into(), 40)]);
    // `else set` follows `if absent` alone.
    assert!(fenec_ql::parse_one("put m {key: \"x\"} else set {n: 1}").is_err());
    assert!(fenec_ql::parse_one("put m {key: \"x\"} if absent else {n: 1}").is_err());
}

#[test]
fn a_block_put_back_puts_back_the_rows_an_upsert_set() {
    let mut db = rollups();
    run(&mut db, "put m {key: \"a\", n: 1}").unwrap();
    db.begin().unwrap();
    run(
        &mut db,
        "put m [{key: \"a\", n: 5}, {key: \"b\", n: 2}] if absent else set {n: n + new.n}",
    )
    .unwrap();
    assert_eq!(counts(&db, "m"), [("a".into(), 6), ("b".into(), 2)]);
    db.rollback();
    assert_eq!(counts(&db, "m"), [("a".into(), 1)]);
    // The unique index holds what it held: "b" is free again.
    run(&mut db, "insert m {key: \"b\", n: 9}").unwrap();
}

#[test]
fn a_row_past_its_time_is_made_again_not_set() {
    let mut db = Database::new();
    db.set_clock(Some(10_000_000));
    run(
        &mut db,
        "create collection m (key text @unique, n int, at timestamp @ttl(1h))",
    )
    .unwrap();
    run(&mut db, "put m {key: \"a\", n: 7, at: 1}").unwrap();
    run(
        &mut db,
        "put m {key: \"a\", n: 1, at: 9000000} if absent else set {n: n + new.n}",
    )
    .unwrap();
    assert_eq!(counts(&db, "m"), [("a".into(), 1)]);
}

/// Eight writers adding to the same few keys, each its own statement under
/// the lock: every addition lands, none is lost between a read and a write.
#[test]
fn upserts_from_many_threads_lose_no_addition() {
    let db = Arc::new(RwLock::new(rollups()));
    let stmt = Arc::new(
        fenec_ql::parse_one("put m {key: $1, n: 1} if absent else set {n: n + new.n}").unwrap(),
    );
    let threads: Vec<_> = (0..8)
        .map(|t| {
            let (db, stmt) = (db.clone(), stmt.clone());
            std::thread::spawn(move || {
                for i in 0..2_000 {
                    let key = Value::Text(format!("k{}", (i + t) % 4));
                    db.write().unwrap().execute_with(&stmt, &[key]).unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let total: i64 = counts(&db.read().unwrap(), "m")
        .iter()
        .map(|(_, n)| n)
        .sum();
    assert_eq!(total, 16_000);
    assert_eq!(counts(&db.read().unwrap(), "m").len(), 4);
}

#[test]
fn an_upsert_that_sets_a_vector_a_statement_wrote_keeps_the_last() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection v (key text @unique, e vector<2> @hnsw(l2))",
    )
    .unwrap();
    run(
        &mut db,
        "put v [{key: \"a\", e: [0.0, 0.0]}, {key: \"a\", e: [9.0, 9.0]}] \
         if absent else set {e: new.e}",
    )
    .unwrap();
    let r = db
        .query(
            &fenec_ql::parse_one("get v select key near e [9.0, 9.0] limit 1").unwrap(),
            &[],
        )
        .unwrap();
    let rows = &r.rows().unwrap().rows;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].score, Some(0.0), "{rows:?}");
}

// ------------------------------------------------------------ put $n

fn events() -> Database {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection e (eid text @unique, name text, n int, e vector<2>, props json)",
    )
    .unwrap();
    db
}

fn all(db: &Database) -> String {
    let r = db
        .query(&fenec_ql::parse_one("get e order eid").unwrap(), &[])
        .unwrap();
    let mut out = String::new();
    fenec_core::json::rows_array_into(&mut out, r.rows().unwrap());
    out
}

#[test]
fn a_parameter_holds_the_documents_of_a_put() {
    let mut a = events();
    let mut b = events();
    // The same documents written out and as a parameter.
    run(
        &mut a,
        r#"put e [{eid: "1", name: "view", n: 3, e: [0.5, 0.25], props: {price: 19.99, tags: [1, 2.5]}},
                  {eid: "2", name: "buy"}]"#,
    )
    .unwrap();
    let n = affected(run_with(
        &mut b,
        "put e $1",
        r#"[[{"eid": "1", "name": "view", "n": 3, "e": [0.5, 0.25],
              "props": {"price": 19.99, "tags": [1, 2.5]}},
             {"eid": "2", "name": "buy"}]]"#,
    ));
    assert_eq!(n, 2);
    assert_eq!(all(&a), all(&b));
    // The numbers of a json field as written: an int stays one.
    assert!(all(&b).contains(r#""tags":[1,2.5]"#), "{}", all(&b));
    // One object, and `if absent` over the list.
    run_with(&mut b, "put e $1", r#"[{"eid": "3", "n": 1}]"#).unwrap();
    let n = affected(run_with(
        &mut b,
        "put e $1 if absent",
        r#"[[{"eid": "3"}, {"eid": "4"}]]"#,
    ));
    assert_eq!(n, 1);
    // An upsert from a parameter: a page of keys and what to add to each.
    let n = affected(run_with(
        &mut b,
        "put e $2 if absent else set {n: coalesce(n, 0) + new.n + $1}",
        r#"[10, [{"eid": "3", "n": 2}, {"eid": "5", "n": 1}]]"#,
    ));
    assert_eq!(n, 2);
    let r = b
        .query(
            &fenec_ql::parse_one("get e select eid, n where eid in [\"3\", \"5\"] order eid")
                .unwrap(),
            &[],
        )
        .unwrap();
    let ns: Vec<&Value> = r
        .rows()
        .unwrap()
        .rows
        .iter()
        .map(|r| &r.values[1])
        .collect();
    assert_eq!(ns, [&Value::Int(13), &Value::Int(1)]);
}

#[test]
fn a_documents_parameter_that_is_not_one_is_refused() {
    let mut db = events();
    for (params, what) in [
        ("[5]", "int"),
        (r#"["x"]"#, "text"),
        (r#"[[{"eid": "1"}, 2]]"#, "a list holding int"),
        (r#"[[{"eid": "1"}, {"nope": 1}]]"#, "field"),
    ] {
        let e = run_with(&mut db, "put e $1", params).unwrap_err();
        assert!(e.to_string().contains(what), "{params}: {e}");
    }
    let e = db
        .execute_with(&fenec_ql::parse_one("put e $1").unwrap(), &[])
        .unwrap_err();
    assert!(e.to_string().contains("not bound"), "{e}");
    // Nothing of a refused list was written.
    assert_eq!(all(&db), "[]");
    // An empty list writes nothing, and says so.
    assert_eq!(affected(run_with(&mut db, "put e $1", "[[]]")), 0);
}
