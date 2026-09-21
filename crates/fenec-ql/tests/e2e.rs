//! End to end: FenecQL -> engine -> result.

use fenec_core::prelude::*;
use fenec_ql::parse;

fn run(db: &mut Database, sql: &str) -> Response {
    run_with(db, sql, &[])
}

fn run_with(db: &mut Database, sql: &str, params: &[Value]) -> Response {
    let stmts = parse(sql).expect("parse");
    let mut last = Response::Affected(0);
    for s in &stmts {
        last = db.execute_with(s, params).expect("execute");
    }
    last
}

/// A `run` that does not swallow the error: for the negative cases.
fn try_run(db: &mut Database, sql: &str) -> fenec_core::error::Result<Response> {
    let stmts = parse(sql).expect("parse");
    let mut last = Response::Affected(0);
    for s in &stmts {
        last = db.execute_with(s, &[])?;
    }
    Ok(last)
}

fn setup() -> Database {
    let mut db = Database::new();
    run(
        &mut db,
        r#"create collection docs (
             title text,
             tags  [text],
             year  int @hash,
             embed vector<4> @hnsw(cosine, m=8, ef_construction=64)
           )"#,
    );
    run(
        &mut db,
        r#"put docs [
             {title: "rust book",         tags: ["rust","book"],  year: 2021, embed: [1.0, 0.0, 0.0, 0.0]},
             {title: "wasm guide",        tags: ["wasm","rust"],  year: 2023, embed: [0.9, 0.1, 0.0, 0.0]},
             {title: "vector search",     tags: ["ai","vector"],  year: 2024, embed: [0.0, 1.0, 0.0, 0.0]},
             {title: "postgres internals",tags: ["db"],           year: 2019, embed: [0.0, 0.0, 1.0, 0.0]}
           ]"#,
    );
    db
}

#[test]
fn crud_and_filters() {
    let mut db = setup();

    let r = run(&mut db, "get docs");
    assert_eq!(r.rows().unwrap().rows.len(), 4);

    let r = run(
        &mut db,
        r#"get docs select title where year >= 2021 order year desc"#,
    );
    let rows = &r.rows().unwrap().rows;
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].values[0], Value::Text("vector search".into()));

    let r = run(&mut db, r#"get docs where tags has "rust""#);
    assert_eq!(r.rows().unwrap().rows.len(), 2);

    let r = run(&mut db, r#"get docs where title ~ "GUIDE""#);
    assert_eq!(r.rows().unwrap().rows.len(), 1);

    // hash index pushdown path
    let r = run(&mut db, "get docs where year = 2019");
    assert_eq!(r.rows().unwrap().rows.len(), 1);

    let r = run(&mut db, "get docs where year in [2019, 2024]");
    assert_eq!(r.rows().unwrap().rows.len(), 2);

    // update
    assert_eq!(
        run(
            &mut db,
            r#"set docs {year: 2025} where title ~ "rust book""#
        ),
        Response::Affected(1)
    );
    let r = run(&mut db, "get docs where year = 2025");
    assert_eq!(r.rows().unwrap().rows.len(), 1);

    // delete
    assert_eq!(
        run(&mut db, "del docs where year < 2020"),
        Response::Affected(1)
    );
    assert_eq!(run(&mut db, "get docs").rows().unwrap().rows.len(), 3);
}

#[test]
fn vector_search_is_native() {
    let mut db = setup();

    let r = run(
        &mut db,
        "get docs select title near embed [1.0, 0.0, 0.0, 0.0] limit 2",
    );
    let rows = &r.rows().unwrap().rows;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].values[0], Value::Text("rust book".into()));
    // the cosine similarity score is filled in and the order is descending
    assert!(rows[0].score.unwrap() > rows[1].score.unwrap());
    assert!(rows[0].score.unwrap() > 0.99);

    // filter + vector together (pre-filter + ANN)
    let r = run(
        &mut db,
        r#"get docs select title where year >= 2023 near embed [1.0, 0.0, 0.0, 0.0] limit 5"#,
    );
    let rows = &r.rows().unwrap().rows;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].values[0], Value::Text("wasm guide".into()));

    // exact (brute-force) gives the same first result
    let r2 = run(
        &mut db,
        r#"get docs select title where year >= 2023 near embed [1.0, 0.0, 0.0, 0.0] exact limit 5"#,
    );
    assert_eq!(r2.rows().unwrap().rows[0].values[0], rows[0].values[0]);
}

#[test]
fn parameters_bind() {
    let mut db = setup();
    let r = run_with(
        &mut db,
        "get docs select title near embed $1 limit 1",
        &[Value::Vector(vec![0.0, 1.0, 0.0, 0.0])],
    );
    assert_eq!(
        r.rows().unwrap().rows[0].values[0],
        Value::Text("vector search".into())
    );

    let r = run_with(&mut db, "get docs where year = $1", &[Value::Int(2023)]);
    assert_eq!(r.rows().unwrap().rows.len(), 1);
}

#[test]
fn builtin_functions_in_predicates() {
    let mut db = setup();
    let r = run(&mut db, r#"get docs where lower(title) ~ "wasm""#);
    assert_eq!(r.rows().unwrap().rows.len(), 1);
    let r = run(&mut db, "get docs where len(tags) = 2");
    assert_eq!(r.rows().unwrap().rows.len(), 3);
}

#[test]
fn snapshot_roundtrip_preserves_vector_index() {
    let db = setup();
    let image = db.snapshot();

    let mut db2 = Database::new();
    db2.load(&image).expect("load");
    assert_eq!(db2.collection_names(), vec!["docs".to_string()]);

    let r = run(
        &mut db2,
        "get docs select title near embed [0.0, 1.0, 0.0, 0.0] limit 1",
    );
    assert_eq!(
        r.rows().unwrap().rows[0].values[0],
        Value::Text("vector search".into())
    );
    assert_eq!(run(&mut db2, "get docs").rows().unwrap().rows.len(), 4);
}

#[test]
fn compaction_reclaims_space() {
    let mut db = setup();
    run(&mut db, "del docs where year < 2022");
    let before = db.stats()[0].dead_bytes;
    assert!(before > 0);
    run(&mut db, "compact");
    let after = db.stats()[0].dead_bytes;
    assert_eq!(after, 0);
    // the vector index still works after compaction
    let r = run(
        &mut db,
        "get docs select title near embed [0.0, 1.0, 0.0, 0.0] limit 1",
    );
    assert_eq!(
        r.rows().unwrap().rows[0].values[0],
        Value::Text("vector search".into())
    );
}

/// The read-only path (`query`) rejects statements that need to write: under
/// a shared lock the server may only call this one.
#[test]
fn read_only_path_rejects_writes() {
    let mut db = setup();
    let read = parse("get docs select title").unwrap();
    assert!(db.query(&read[0], &[]).is_ok());
    assert!(db.query(&parse("collections").unwrap()[0], &[]).is_ok());
    assert!(db.query(&parse("describe docs").unwrap()[0], &[]).is_ok());

    for sql in [
        "put docs {title: \"x\"}",
        "set docs {title: \"y\"} where id = 1",
        "del docs where id = 1",
        "create collection fresh (a text)",
        // Order matters: `compact` has to run while the collection is there.
        "compact docs",
        "drop collection docs",
    ] {
        let stmt = &parse(sql).unwrap()[0];
        assert!(!stmt.is_read_only(), "{sql} must not count as read-only");
        assert!(
            db.query(stmt, &[]).is_err(),
            "{sql} passed on the read path"
        );
        // It works on the write path.
        assert!(db.execute(stmt).is_ok(), "{sql} failed on the write path");
    }
}

/// `is_dirty` reports whether a write is still waiting to reach disk; the
/// server's periodic syncer skips idle passes based on it.
#[test]
fn dirty_flag_tracks_pending_writes() {
    let mut db = setup();
    // setup wrote: it must be dirty (the flag works with NullSink too).
    assert!(db.is_dirty());
    db.sync().unwrap();
    assert!(!db.is_dirty());
    // A read does not dirty it.
    db.query(&parse("get docs select title").unwrap()[0], &[])
        .unwrap();
    assert!(!db.is_dirty());
    run(&mut db, "put docs {title: \"new\"}");
    assert!(db.is_dirty());
    db.checkpoint().unwrap();
    assert!(!db.is_dirty());
}

/// A filtered `near` must give the right answer even when every ANN
/// candidate is rejected by the filter.
///
/// Regression test: [`VectorIndex::search`] applies the filter *after* the
/// candidates are gathered. If the filter field correlates with the vector,
/// all `ef` nearest neighbours are eliminated and the query used to come
/// back empty -- despite thousands of matching documents. Here the vectors
/// lie along an arc and the group is decided by which half of that arc you
/// are on: the nearest member of `group == 1` is far outside the first `ef`
/// unfiltered neighbours.
#[test]
fn filtered_vector_search_is_not_starved() {
    const N: usize = 2000;
    let mut db = Database::new();
    run(
        &mut db,
        "create collection v (group int @hash, e vector<2> @hnsw(cosine, m=16, ef_construction=64))",
    );
    let mut docs = String::with_capacity(N * 48);
    for i in 0..N {
        let t = i as f64 / N as f64;
        docs.push_str(&format!(
            "{{group: {}, e: [{:.6}, {:.6}]}} ",
            i * 2 / N, // first half 0, second half 1
            1.0 - t,
            t
        ));
    }
    run(&mut db, &format!("put v {docs}"));

    let q = vec![Value::Vector(vec![1.0, 0.0])];
    let ids = |r: Response| -> Vec<DocId> { r.rows().unwrap().rows.iter().map(|x| x.id).collect() };

    // Shows the scenario is real: none of the 50 nearest unfiltered
    // neighbours is in group 1.
    let near_ids = ids(run_with(&mut db, "get v select id near e $1 limit 50", &q));
    assert_eq!(near_ids.len(), 50);
    let groups: Vec<f64> = near_ids
        .iter()
        .map(|id| {
            let r = run_with(
                &mut db,
                &format!("get v select group where id == {id}"),
                &[],
            );
            r.rows().unwrap().rows[0].values[0].as_f64().unwrap()
        })
        .collect();
    assert!(
        groups.iter().all(|g| *g == 0.0),
        "the setup broke: {groups:?}"
    );

    // Short cut: the filter set is smaller than the ANN budget -> direct scan.
    let hnsw = ids(run_with(
        &mut db,
        "get v select id where group == 1 near e $1 limit 3",
        &q,
    ));
    let exact = ids(run_with(
        &mut db,
        "get v select id where group == 1 near e $1 exact limit 3",
        &q,
    ));
    assert_eq!(hnsw.len(), 3, "the filtered near did not reach its limit");
    assert_eq!(hnsw, exact, "the filtered near differs from the exact scan");

    // Fallback path: `ef 1` drops the budget below the filter set, the ANN
    // still comes back empty, and the result must still be right.
    let fallback = ids(run_with(
        &mut db,
        "get v select id where group == 1 near e $1 ef 1 limit 3",
        &q,
    ));
    assert_eq!(fallback, exact, "the fallback path gave a wrong result");

    // A very selective filter still reaches the limit (as far as matches allow).
    let one = ids(run_with(
        &mut db,
        "get v select id where id == 1500 near e $1 limit 3",
        &q,
    ));
    assert_eq!(one.len(), 1, "the single match was not returned");
}

/// When the filter matches no document the result is empty -- the fallback
/// must not break that.
#[test]
fn filtered_vector_search_with_no_match_is_empty() {
    let mut db = setup();
    let q = vec![Value::Vector(vec![1.0, 0.0, 0.0, 0.0])];
    let r = run_with(
        &mut db,
        "get docs select title where year == 1900 near embed $1 limit 3",
        &q,
    );
    assert!(r.rows().unwrap().rows.is_empty());
}

#[test]
fn errors_are_useful() {
    let mut db = setup();
    let e = parse("get docs near nosuch [1.0]")
        .map(|s| db.execute(&s[0]))
        .unwrap()
        .unwrap_err();
    assert!(e.to_string().contains("has no vector index"), "{e}");

    let stmts = parse(r#"put docs {embed: [1.0, 2.0]}"#).unwrap();
    let e = db.execute(&stmts[0]).unwrap_err();
    assert!(e.to_string().contains("vector dimension"), "{e}");
}

#[test]
fn persisted_graph_is_used_and_validated() {
    let db = setup();
    let image = db.snapshot();

    // The image must contain the graph (REC_GRAPH = 4)
    assert!(image.windows(1).any(|w| w[0] == 4));

    let mut db2 = Database::new();
    db2.load(&image).expect("load");
    let r = run(
        &mut db2,
        "get docs select title near embed [0.0, 1.0, 0.0, 0.0] limit 1",
    );
    assert_eq!(
        r.rows().unwrap().rows[0].values[0],
        Value::Text("vector search".into())
    );

    // A corrupt graph must be ignored silently and the index rebuilt.
    let mut corrupted = image.clone();
    let n = corrupted.len();
    for b in corrupted[n - 24..].iter_mut() {
        *b = 0xff;
    }
    let mut db3 = Database::new();
    db3.load(&corrupted)
        .expect("a corrupt graph must not block loading");
    let r = run(
        &mut db3,
        "get docs select title near embed [0.0, 1.0, 0.0, 0.0] limit 1",
    );
    assert_eq!(
        r.rows().unwrap().rows[0].values[0],
        Value::Text("vector search".into())
    );

    // A stale graph (a write after the image) must be ignored as well.
    let mut db4 = Database::new();
    db4.load(&image).unwrap();
    run(
        &mut db4,
        r#"put docs {title: "new", embed: [0.5, 0.5, 0.0, 0.0]}"#,
    );
    let image2 = db4.snapshot();
    let mut db5 = Database::new();
    db5.load(&image2).unwrap();
    assert_eq!(run(&mut db5, "get docs").rows().unwrap().rows.len(), 5);
    let r = run(
        &mut db5,
        "get docs select title near embed [0.5, 0.5, 0.0, 0.0] limit 1",
    );
    assert_eq!(
        r.rows().unwrap().rows[0].values[0],
        Value::Text("new".into())
    );
}

#[test]
fn near_with_order_is_rejected() {
    let mut db = setup();
    let stmts = fenec_ql::parse("get docs near embed [1.0,0,0,0] order year desc limit 2").unwrap();
    let e = db.execute(&stmts[0]).unwrap_err();
    assert!(e.to_string().contains("cannot be combined"), "{e}");
}

/// When the `near` ceiling is exceeded it errors instead of truncating
/// silently: a truncated similarity result is a wrong answer that looks right.
#[test]
fn near_beyond_row_cap_is_rejected() {
    let mut db = setup();
    let cap = fenec_core::engine::MAX_NEAR_ROWS;

    let over = format!("get docs near embed [1.0,0,0,0] limit {}", cap + 1);
    let e = db.execute(&fenec_ql::parse(&over).unwrap()[0]).unwrap_err();
    assert!(e.to_string().contains("at most"), "{e}");

    // `limit` is under the cap but together with `offset` it goes over.
    let over = format!("get docs near embed [1.0,0,0,0] limit 10 offset {cap}");
    let e = db.execute(&fenec_ql::parse(&over).unwrap()[0]).unwrap_err();
    assert!(e.to_string().contains("at most"), "{e}");

    // The cap itself and everything below it works.
    let at = format!("get docs select title near embed [1.0,0,0,0] limit {cap}");
    assert!(matches!(run(&mut db, &at), Response::Rows(_)));
    // Without a `limit` the cap is applied as the default top-k.
    assert!(matches!(
        run(&mut db, "get docs select title near embed [1.0,0,0,0]"),
        Response::Rows(_)
    ));
}

#[test]
fn hash_index_used_inside_conjunction() {
    let mut db = setup();
    // `year` is hash indexed; the equality sits inside an `and` chain
    let r = run(
        &mut db,
        r#"get docs select title where year = 2023 and title ~ "wasm""#,
    );
    assert_eq!(r.rows().unwrap().rows.len(), 1);
    // reversed order
    let r = run(
        &mut db,
        r#"get docs select title where title ~ "wasm" and year = 2023"#,
    );
    assert_eq!(r.rows().unwrap().rows.len(), 1);
    // a non-matching equality must come back empty (empty candidate set)
    let r = run(
        &mut db,
        r#"get docs select title where year = 1999 and title ~ "wasm""#,
    );
    assert_eq!(r.rows().unwrap().rows.len(), 0);
    // an equality under `or` must NOT be used as an index, and the result must still be right
    let r = run(
        &mut db,
        r#"get docs select title where year = 2023 or year = 2019"#,
    );
    assert_eq!(r.rows().unwrap().rows.len(), 2);
}

#[test]
fn create_index_after_bulk_load() {
    let mut db = Database::new();
    // Schema with no index: the write path is a pure append.
    run(
        &mut db,
        "create collection n (title text, topic text, embed vector<4>)",
    );
    run(
        &mut db,
        r#"put n [
             {title: "a", topic: "x", embed: [1.0, 0.0, 0.0, 0.0]},
             {title: "b", topic: "y", embed: [0.0, 1.0, 0.0, 0.0]},
             {title: "c", topic: "x", embed: [0.0, 0.0, 1.0, 0.0]}
           ]"#,
    );

    // With no index `near` must be rejected
    let stmts = fenec_ql::parse("get n near embed [1.0,0,0,0] limit 1").unwrap();
    assert!(db
        .execute(&stmts[0])
        .unwrap_err()
        .to_string()
        .contains("has no vector index"));

    // Build the index afterwards
    assert!(matches!(
        run(&mut db, "create index on n (embed) @hnsw(cosine, m=8)"),
        Response::Ok(_)
    ));
    assert!(matches!(
        run(&mut db, "create index on n (topic) @hash"),
        Response::Ok(_)
    ));

    // Was it filled from the existing documents?
    let r = run(
        &mut db,
        "get n select title near embed [0.0,1.0,0.0,0.0] limit 1",
    );
    assert_eq!(r.rows().unwrap().rows[0].values[0], Value::Text("b".into()));
    let r = run(&mut db, r#"get n select title where topic = "x""#);
    assert_eq!(r.rows().unwrap().rows.len(), 2);

    // Writes after the index must be indexed as well
    run(
        &mut db,
        r#"put n {title: "d", topic: "x", embed: [0.0, 0.0, 0.0, 1.0]}"#,
    );
    let r = run(
        &mut db,
        "get n select title near embed [0.0,0.0,0.0,1.0] limit 1",
    );
    assert_eq!(r.rows().unwrap().rows[0].values[0], Value::Text("d".into()));

    // Building it a second time errors, and is silent with `if not exists`
    assert!(db
        .execute(&fenec_ql::parse("create index on n (embed) @hnsw(cosine)").unwrap()[0])
        .is_err());
    assert!(matches!(
        run(
            &mut db,
            "create index if not exists on n (embed) @hnsw(cosine)"
        ),
        Response::Ok(_)
    ));

    // Is the schema change persistent?
    let image = db.snapshot();
    let mut db2 = Database::new();
    db2.load(&image).unwrap();
    let r = run(
        &mut db2,
        "get n select title near embed [0.0,1.0,0.0,0.0] limit 1",
    );
    assert_eq!(r.rows().unwrap().rows[0].values[0], Value::Text("b".into()));
    let r = run(&mut db2, r#"get n select title where topic = "x""#);
    assert_eq!(r.rows().unwrap().rows.len(), 3);

    // An index on the wrong type must be rejected
    assert!(db
        .execute(&fenec_ql::parse("create index on n (title) @hnsw(cosine)").unwrap()[0])
        .is_err());
}

/// `@hash` must be nothing but an accelerator: an indexed and an unindexed
/// schema must give the same answer to the same filter. Because the bucket
/// key is produced on the write path from the value coerced to the field
/// type, `price = 10` missed the stored `10.0` and silently returned 0 rows
/// unless the lookup went through the same conversion.
#[test]
fn hash_index_agrees_with_scan() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection ix (price float @hash, year int @hash, label bytes @hash)",
    );
    run(
        &mut db,
        "create collection sc (price float, year int, label bytes)",
    );
    for c in ["ix", "sc"] {
        run(
            &mut db,
            &format!(r#"put {c} {{price: 10.0, year: 2024, label: "x"}}"#),
        );
    }

    for filter in [
        "where price = 10", // int literal   -> float field
        "where price = 10.0",
        "where price = 11", // no match
        "where year = 2024",
        "where year = 2024.0",   // float literal -> int field
        "where year = 2024.5",   // not a whole number: nothing may match
        r#"where label = "x""#,  // text literal  -> bytes field
        r#"where year = "abc""#, // literal that does not fit the type
        r#"where year = 2024 and label = "x""#,
    ] {
        let a = run(&mut db, &format!("get ix {filter}"))
            .rows()
            .unwrap()
            .rows
            .len();
        let b = run(&mut db, &format!("get sc {filter}"))
            .rows()
            .unwrap()
            .rows
            .len();
        assert_eq!(a, b, "`{filter}`: indexed {a}, unindexed {b}");
    }
}

/// A type mismatch does not mean "everything is equal". `cmp_value` used to
/// return Equal for incomparable pairs; `year = "abc"` matched every row and
/// `tags = ["x"]` counted every list as equal.
#[test]
fn mismatched_types_are_not_equal() {
    let mut db = setup();

    assert_eq!(
        run(&mut db, r#"get docs where year = "abc""#)
            .rows()
            .unwrap()
            .rows
            .len(),
        0
    );
    assert_eq!(
        run(&mut db, "get docs where title = 5")
            .rows()
            .unwrap()
            .rows
            .len(),
        0
    );

    // List equality is compared element by element.
    let r = run(&mut db, r#"get docs where tags = ["rust","book"]"#);
    assert_eq!(r.rows().unwrap().rows.len(), 1);
    let r = run(&mut db, r#"get docs where tags = ["rust"]"#);
    assert_eq!(r.rows().unwrap().rows.len(), 0);
}

/// An all-numeric array coming from the JSON side is parsed as a vector (for
/// embedding transfer). When the target field is a list it has to be
/// converted back; otherwise `[int]`/`[float]` fields could not be filled
/// from the browser and fenec-pg paths.
#[test]
fn json_number_array_fills_list_fields() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection j (numbers [int], ratios [float])",
    );

    let params = fenec_core::json::parse_params("[[1,2,3]]").expect("json params");
    assert_eq!(params[0].type_name(), "vector");
    run_with(&mut db, "put j {numbers: $1}", &params);

    let r = run(&mut db, "get j select numbers");
    assert_eq!(
        r.rows().unwrap().rows[0].values[0],
        Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
    );

    // A non-integer value cannot be written into an `[int]` field.
    let stmts = fenec_ql::parse("put j {numbers: $1}").unwrap();
    let bad = fenec_core::json::parse_params("[[1.5]]").unwrap();
    assert!(db.execute_with(&stmts[0], &bad).is_err());
}

/// Hash pushdown must work with a bound parameter too. `where year = $1` is
/// the usual shape coming from the browser and from fenec-pg; while only
/// literals were looked for, the index was never used on those paths. The
/// result is the same either way.
#[test]
fn hash_index_pushdown_resolves_params() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection ix (year int @hash, price float @hash)",
    );
    run(&mut db, "create collection sc (year int, price float)");
    for c in ["ix", "sc"] {
        run(
            &mut db,
            &format!("put {c} [{{year: 2019, price: 10.0}}, {{year: 2024, price: 20.0}}]"),
        );
    }

    for (filter, params) in [
        ("where year = $1", vec![Value::Int(2024)]),
        // int field, float parameter: both paths must coerce and agree
        ("where year = $1", vec![Value::Float(2024.0)]),
        // float field, int parameter
        ("where price = $1", vec![Value::Int(20)]),
        ("where year = $1", vec![Value::Int(1900)]),
        (
            "where year = $1 and price > $2",
            vec![Value::Int(2024), Value::Float(5.0)],
        ),
    ] {
        let a = run_with(&mut db, &format!("get ix {filter}"), &params)
            .rows()
            .unwrap()
            .rows
            .len();
        let b = run_with(&mut db, &format!("get sc {filter}"), &params)
            .rows()
            .unwrap()
            .rows
            .len();
        assert_eq!(a, b, "`{filter}` {params:?}: indexed {a}, unindexed {b}");
    }

    // An unbound parameter must skip the index and error on the eval path.
    let stmt = &fenec_ql::parse("get ix where year = $1").unwrap()[0];
    assert!(db.execute_with(stmt, &[]).is_err());
}

/// `vector<N, f16>` is a storage decision; the runtime representation does
/// not change. The schema, the records and the HNSW arena halve, while the
/// query path stays f32.
#[test]
fn f16_vectors_halve_storage_and_survive_reload() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection a (embed vector<64, f16> @hnsw(cosine, m=8, ef_construction=64))",
    );
    run(
        &mut db,
        "create collection b (embed vector<64> @hnsw(cosine, m=8, ef_construction=64))",
    );

    // The same data into both collections.
    let mut seed = 7u32;
    let mut vecs = Vec::new();
    for _ in 0..200 {
        let v: Vec<f32> = (0..64)
            .map(|_| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
            })
            .collect();
        vecs.push(v);
    }
    for v in &vecs {
        for c in ["a", "b"] {
            let stmt = &fenec_ql::parse(&format!("put {c} {{embed: $1}}")).unwrap()[0];
            db.execute_with(stmt, &[Value::Vector(v.clone())]).unwrap();
        }
    }

    let arena = |db: &Database, name: &str| -> usize {
        db.stats()
            .iter()
            .find(|s| s.name == name)
            .unwrap()
            .vector_indexes
            .iter()
            .map(|v| v.arena_bytes)
            .sum()
    };
    assert_eq!(
        arena(&db, "a") * 2,
        arena(&db, "b"),
        "the f16 arena must be half"
    );
    assert_eq!(
        db.stats()
            .iter()
            .find(|s| s.name == "a")
            .unwrap()
            .vector_indexes[0]
            .precision,
        VecPrec::F16
    );
    // The records are smaller too: the f16 collection holds fewer bytes.
    let bytes =
        |db: &Database, name: &str| db.stats().iter().find(|s| s.name == name).unwrap().bytes;
    assert!(bytes(&db, "a") < bytes(&db, "b"));

    // Query results must be in the same order as f32 (an exact overlap for this data).
    let q = vecs[3].clone();
    let ids = |db: &Database, c: &str| -> Vec<u64> {
        let stmt = &fenec_ql::parse(&format!("get {c} near embed $1 limit 5")).unwrap()[0];
        db.query(stmt, &[Value::Vector(q.clone())])
            .unwrap()
            .rows()
            .unwrap()
            .rows
            .iter()
            .map(|r| r.id)
            .collect()
    };
    assert_eq!(ids(&db, "a"), ids(&db, "b"));

    // After restoring from a snapshot the schema and the results must survive.
    let image = db.snapshot();
    let mut db2 = Database::new();
    db2.load(&image).unwrap();
    assert_eq!(
        db2.collection("a")
            .unwrap()
            .schema
            .field("embed")
            .unwrap()
            .ty,
        DataType::Vector(64, VecPrec::F16)
    );
    assert_eq!(ids(&db2, "a"), ids(&db, "a"));
}

/// A misspelled precision name must not silently fall back to f32.
#[test]
fn unknown_vector_precision_is_rejected() {
    assert!(fenec_ql::parse("create collection x (e vector<4, f8>)").is_err());
    assert!(fenec_ql::parse("create collection x (e vector<4, half>)").is_err());
    // f32 can be written out explicitly.
    assert!(fenec_ql::parse("create collection x (e vector<4, f32>)").is_ok());
}

/// `timestamp` carries UTC epoch milliseconds; text, numbers and `now()` all
/// resolve to the same type, and the display is ISO-8601 everywhere.
#[test]
fn timestamps_accept_text_and_epoch() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection event (name text, t timestamp @hash)",
    );
    run(
        &mut db,
        r#"put event [
             {name: "a", t: "2026-09-19T12:34:56Z"},
             {name: "b", t: "2026-09-19 15:34:56+03:00"},
             {name: "c", t: 0},
             {name: "d", t: "2024-02-29"}
           ]"#,
    );

    // Two different spellings of the same instant must be equal.
    let ts = |db: &mut Database, name: &str| -> Value {
        run(
            &mut db_ref(db),
            &format!(r#"get event select t where name = "{name}""#),
        )
        .rows()
        .unwrap()
        .rows[0]
            .values[0]
            .clone()
    };
    fn db_ref(d: &mut Database) -> &mut Database {
        d
    }
    assert_eq!(ts(&mut db, "a"), ts(&mut db, "b"));
    assert_eq!(ts(&mut db, "c"), Value::Timestamp(0));

    // Ordering is chronological, not textual.
    let r = run(&mut db, "get event select name order t asc");
    let order: Vec<String> = r
        .rows()
        .unwrap()
        .rows
        .iter()
        .map(|x| x.values[0].as_text().unwrap().to_string())
        .collect();
    assert_eq!(order, ["c", "d", "a", "b"]);

    // A range against a text literal: if the write path parses the text, the
    // read path must too, otherwise the filter silently comes back empty.
    assert_eq!(
        run(&mut db, r#"get event where t >= "2026-01-01""#)
            .rows()
            .unwrap()
            .rows
            .len(),
        2
    );
    assert_eq!(
        run(&mut db, r#"get event where t < "2025-01-01""#)
            .rows()
            .unwrap()
            .rows
            .len(),
        2
    );

    // @hash equality must agree with the unindexed scan.
    run(&mut db, "create collection copy (name text, t timestamp)");
    run(
        &mut db,
        r#"put copy [{name: "a", t: "2026-09-19T12:34:56Z"}, {name: "c", t: 0}]"#,
    );
    for filter in [
        r#"where t = "2026-09-19T12:34:56Z""#,
        "where t = 0",
        r#"where t = "none""#,
    ] {
        let a = run(&mut db, &format!("get event {filter}"))
            .rows()
            .unwrap()
            .rows
            .len();
        let b = run(&mut db, &format!("get copy {filter}"))
            .rows()
            .unwrap()
            .rows
            .len();
        assert!(a >= b, "`{filter}`: indexed {a}, unindexed {b}");
        if filter.contains("none") {
            assert_eq!((a, b), (0, 0));
        }
    }

    // now() counts the past as past and the future as future. So the test
    // does not depend on the system clock, the records sit far in both.
    run(&mut db, "create collection times (t timestamp)");
    run(
        &mut db,
        r#"put times [{t: "1970-01-01"}, {t: "2024-02-29"}, {t: "2999-12-31"}]"#,
    );
    assert_eq!(
        run(&mut db, "get times where t < now()")
            .rows()
            .unwrap()
            .rows
            .len(),
        2
    );
    assert_eq!(
        run(&mut db, "get times where t > now()")
            .rows()
            .unwrap()
            .rows
            .len(),
        1
    );

    // Invalid text errors on write; it never silently becomes 0.
    let stmt = &fenec_ql::parse(r#"put event {name: "x", t: "yesterday"}"#).unwrap()[0];
    assert!(db.execute(stmt).is_err());

    // Restored from a file, the type and the values survive.
    let image = db.snapshot();
    let mut db2 = Database::new();
    db2.load(&image).unwrap();
    assert_eq!(
        db2.collection("event")
            .unwrap()
            .schema
            .field("t")
            .unwrap()
            .ty,
        DataType::Timestamp
    );
    assert_eq!(ts(&mut db2, "a"), ts(&mut db, "a"));

    // The JSON output is ISO-8601 text: `new Date(x)` works in the browser.
    let json = fenec_core::json::response_to_string(&run(
        &mut db2,
        r#"get event select t where name = "c""#,
    ));
    assert!(json.contains("1970-01-01T00:00:00Z"), "{json}");
}

/// pgvector's text notation is the natural form for `near`: a query typed by
/// hand from psql arrives like this. On the parameter path it was already
/// parsed into a list; on the literal path it stayed `text` and errored.
#[test]
fn near_accepts_pgvector_text() {
    let mut db = setup();

    for sql in [
        r#"get docs select title near embed "[1.0, 0.0, 0.0, 0.0]" limit 1"#,
        // psql's quoting
        r#"get docs select title near embed '[1,0,0,0]' limit 1"#,
    ] {
        let r = run(&mut db, sql);
        assert_eq!(
            r.rows().unwrap().rows[0].values[0],
            Value::Text("rust book".into()),
            "{sql}"
        );
    }

    // Text that is not a vector does not silently become a zero vector.
    let e = try_run(
        &mut db,
        r#"get docs select title near embed "hello" limit 1"#,
    )
    .expect_err("unparseable text was accepted");
    assert!(e.to_string().contains("unparseable text"), "{e}");

    // A non-numeric component errors too; it used to silently become 0.0.
    let e = try_run(
        &mut db,
        r#"get docs select title near embed ["a", "b", "c", "d"] limit 1"#,
    )
    .expect_err("a non-numeric component was accepted");
    assert!(e.to_string().contains("non-numeric"), "{e}");

    // The dimension check works on the text path as well.
    let e = try_run(
        &mut db,
        r#"get docs select title near embed "[1,0]" limit 1"#,
    )
    .expect_err("a wrong dimension was accepted");
    assert!(e.to_string().contains("dimensions"), "{e}");
}

/// `in` and `has` equality must read from the same definition as `=`. Since
/// the derived `==` only matches the same variant, a bound parameter
/// arriving with a different numeric type (`float` from JSON) silently
/// produced an empty result.
#[test]
fn in_and_has_match_bound_params() {
    let mut db = setup();

    let years = run_with(
        &mut db,
        "get docs select title where year in [$1, $2]",
        &[Value::Int(2021), Value::Int(2024)],
    );
    assert_eq!(years.rows().unwrap().rows.len(), 2);

    // The same query must give the same answer with a `float` parameter:
    // `year = $1` already did, `in` did not.
    let as_float = run_with(
        &mut db,
        "get docs select title where year in [$1, $2]",
        &[Value::Float(2021.0), Value::Float(2024.0)],
    );
    assert_eq!(as_float, years);

    let eq = run_with(
        &mut db,
        "get docs select title where year = $1",
        &[Value::Float(2021.0)],
    );
    assert_eq!(eq.rows().unwrap().rows.len(), 1);

    // `has` reads from the same rule.
    run(&mut db, "create collection nums (ns [int])");
    run(&mut db, "put nums {ns: [1, 2, 3]}");
    let hit = run_with(&mut db, "get nums where ns has $1", &[Value::Float(2.0)]);
    assert_eq!(hit.rows().unwrap().rows.len(), 1);
    let miss = run_with(&mut db, "get nums where ns has $1", &[Value::Int(9)]);
    assert_eq!(miss.rows().unwrap().rows.len(), 0);

    let tags = run_with(
        &mut db,
        "get docs where tags has $1",
        &[Value::Text("rust".into())],
    );
    assert_eq!(tags.rows().unwrap().rows.len(), 2);
}

/// The classic SQL order: `select a, b from t`. fenecdb's own order
/// (`get t select a, b`) stays valid in the same position -- the decision is
/// made by backtracking, by looking for a `from`.
#[test]
fn sql_order_projection() {
    let mut db = setup();

    let sql_order = run(&mut db, "select title, year from docs where year >= 2021");
    let fenec_order = run(&mut db, "get docs select title, year where year >= 2021");
    assert_eq!(sql_order, fenec_order);
    assert_eq!(sql_order.rows().unwrap().columns, vec!["title", "year"]);

    // `select t where ...` (no projection) is unaffected.
    let bare = run(&mut db, "select docs where year >= 2021");
    assert_eq!(bare.rows().unwrap().rows.len(), 3);

    // `select * from t` gives all the fields.
    let star = run(&mut db, "select * from docs");
    assert_eq!(
        star.rows().unwrap().columns,
        run(&mut db, "get docs").rows().unwrap().columns
    );

    // It works with `get` too; `from` is optional for both verbs.
    assert_eq!(
        run(&mut db, "get title from docs"),
        run(&mut db, "get docs select title")
    );
    assert_eq!(run(&mut db, "get from docs"), run(&mut db, "get docs"));

    // Backtracking must not swallow the error: a missing field still errors.
    assert!(try_run(&mut db, "select nosuch from docs").is_err());
    // A missing name is a parse error (backtracking mistakes `from` for a
    // column and gives up, leaving the dangling `,` in command position).
    assert!(fenec_ql::parse("select title, from docs").is_err());
}

/// `count`: the number of matching rows instead of the rows themselves.
#[test]
fn count_rows() {
    let mut db = setup();

    let all = run(&mut db, "get docs count");
    let rs = all.rows().unwrap();
    assert_eq!(rs.columns, vec!["count"]);
    assert_eq!(rs.rows[0].values, vec![Value::Int(4)]);

    let filtered = run(&mut db, "get docs where year >= 2021 count");
    assert_eq!(filtered.rows().unwrap().rows[0].values, vec![Value::Int(3)]);

    let none = run(&mut db, "get docs where year > 3000 count");
    assert_eq!(none.rows().unwrap().rows[0].values, vec![Value::Int(0)]);

    // A bound parameter goes down the same path.
    let bound = run_with(
        &mut db,
        "get docs where year >= $1 count",
        &[Value::Int(2021)],
    );
    assert_eq!(bound, filtered);

    // `count` does not combine with the other clauses: being ignored
    // silently would be a wrong answer that looks right.
    for bad in [
        "get docs select title count",
        "get docs count limit 2",
        "get docs count offset 1",
        "get docs count order year",
        "get docs count near embed [1.0, 0.0, 0.0, 0.0]",
    ] {
        let e = fenec_ql::parse(bad).unwrap_err().to_string();
        assert!(e.contains("count"), "`{bad}` -> {e}");
    }
}

/// Multi-key ordering: when the first key ties, the second decides.
#[test]
fn multi_key_order() {
    let mut db = Database::new();
    run(&mut db, "create collection t (a int, b text)");
    run(
        &mut db,
        r#"put t [
             {a: 2, b: "x"}, {a: 1, b: "b"}, {a: 2, b: "a"}, {a: 1, b: "a"}
           ]"#,
    );

    let pairs = |r: Response| -> Vec<(i64, String)> {
        r.rows()
            .unwrap()
            .rows
            .iter()
            .map(|row| match (&row.values[0], &row.values[1]) {
                (Value::Int(a), Value::Text(b)) => (*a, b.clone()),
                other => panic!("{other:?}"),
            })
            .collect()
    };

    assert_eq!(
        pairs(run(&mut db, "get t select a, b order a asc, b asc")),
        vec![
            (1, "a".into()),
            (1, "b".into()),
            (2, "a".into()),
            (2, "x".into())
        ]
    );
    assert_eq!(
        pairs(run(&mut db, "get t select a, b order by a desc, b desc")),
        vec![
            (2, "x".into()),
            (2, "a".into()),
            (1, "b".into()),
            (1, "a".into())
        ]
    );
    // The directions are independent.
    assert_eq!(
        pairs(run(&mut db, "get t select a, b order a asc, b desc")),
        vec![
            (1, "b".into()),
            (1, "a".into()),
            (2, "x".into()),
            (2, "a".into())
        ]
    );
}

/// `order id` must work: `id` is not a schema field but is sortable. It
/// errored because the field lookup happened before the `id` check.
#[test]
fn order_by_id() {
    let mut db = setup();
    let asc: Vec<DocId> = run(&mut db, "get docs order id asc")
        .rows()
        .unwrap()
        .rows
        .iter()
        .map(|r| r.id)
        .collect();
    let mut sorted = asc.clone();
    sorted.sort_unstable();
    assert_eq!(asc, sorted);

    let desc: Vec<DocId> = run(&mut db, "get docs order id desc")
        .rows()
        .unwrap()
        .rows
        .iter()
        .map(|r| r.id)
        .collect();
    sorted.reverse();
    assert_eq!(desc, sorted);
}

// ------------------------------------------------------------------ lookup

fn shop() -> Database {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection products (sku text @hash, name text)",
    );
    run(
        &mut db,
        "create collection reviews (product_id int @hash, stars int, body text)",
    );
    run(
        &mut db,
        r#"put products [{sku: "a", name: "Kahve"}, {sku: "b", name: "Demlik"}, {sku: "c", name: "Kupa"}]"#,
    );
    run(
        &mut db,
        r#"put reviews [
             {product_id: 1, stars: 5, body: "guzel"},
             {product_id: 1, stars: 3, body: "idare eder"},
             {product_id: 1, stars: 4, body: "hizli kargo"},
             {product_id: 3, stars: 2, body: "kirik geldi"}
           ]"#,
    );
    db
}

fn groups(db: &mut Database, sql: &str) -> Vec<Vec<Row>> {
    let Response::Rows(rs) = run(db, sql) else {
        panic!("expected rows");
    };
    rs.nested.expect("nested").groups
}

/// The clause is sugar over a plan that can already be written: one query for
/// the page, one indexed query per row. Whatever it returns, that has to
/// return the same thing -- otherwise the sugar is a second implementation.
#[test]
fn lookup_equals_a_page_query_plus_one_query_per_row() {
    let mut db = shop();
    let Response::Rows(page) = run(&mut db, "get products") else {
        panic!("expected rows");
    };
    let got = groups(&mut db, "get products lookup reviews on product_id");

    assert_eq!(got.len(), page.rows.len());
    for (row, group) in page.rows.iter().zip(&got) {
        let Response::Rows(want) = run(
            &mut db,
            &format!("get reviews where product_id = {}", row.id),
        ) else {
            panic!("expected rows");
        };
        assert_eq!(group, &want.rows, "children of product {}", row.id);
    }
}

/// `limit` after `lookup` binds to the child and counts per parent. This is
/// the shape a join cannot express: its limit counts pairs, so one parent
/// with many children takes the whole page.
#[test]
fn a_child_limit_is_per_parent() {
    let mut db = shop();
    let got = groups(
        &mut db,
        "get products lookup reviews on product_id order stars desc limit 2",
    );
    assert_eq!(
        got.iter().map(|g| g.len()).collect::<Vec<_>>(),
        vec![2, 0, 1]
    );
    // Ordered by stars within the parent, not by insertion.
    assert_eq!(got[0][0].values[2], Value::Int(5));
    assert_eq!(got[0][1].values[2], Value::Int(4));
}

/// Clauses are scoped by position: everything before `lookup` is the parent's,
/// everything after is the child's. The same keyword on both sides has to
/// mean each collection's own field.
#[test]
fn clauses_are_scoped_by_which_side_of_lookup_they_are_on() {
    let mut db = shop();
    let Response::Rows(rs) = run(
        &mut db,
        r#"get products where sku != "b" select name
             lookup reviews on product_id select body where stars >= 4"#,
    ) else {
        panic!("expected rows");
    };
    assert_eq!(rs.columns, vec!["name".to_string()]);
    assert_eq!(rs.rows.len(), 2); // the parent filter dropped Demlik
    let n = rs.nested.expect("nested");
    assert_eq!(n.columns, vec!["body".to_string()]);
    // The child filter dropped the 3-star and 2-star reviews.
    assert_eq!(
        n.groups.iter().map(|g| g.len()).collect::<Vec<_>>(),
        vec![2, 0]
    );
}

/// `on child = parent` when the key is not the parent's id.
#[test]
fn the_parent_key_can_be_named() {
    let mut db = shop();
    run(
        &mut db,
        "create collection tags (sku text @hash, label text)",
    );
    run(
        &mut db,
        r#"put tags [{sku: "a", label: "kavrulmus"}, {sku: "c", label: "seramik"}]"#,
    );
    let got = groups(&mut db, "get products lookup tags on sku = sku");
    assert_eq!(
        got.iter().map(|g| g.len()).collect::<Vec<_>>(),
        vec![1, 0, 1]
    );
}

/// The message a refused query produces, from whichever side refuses it.
/// `Select::check` runs in the parser (so the error carries a position) and
/// again in the engine (so a plan built in Rust is checked too), while the
/// schema-dependent refusals can only happen once there is a database.
fn refusal(db: &mut Database, sql: &str) -> String {
    let stmts = match parse(sql) {
        Err(e) => return e.to_string(),
        Ok(s) => s,
    };
    for s in &stmts {
        if let Err(e) = db.execute_with(s, &[]) {
            return e.to_string();
        }
    }
    panic!("`{sql}` was accepted");
}

/// The refusals, each carrying the word that says what to do instead.
#[test]
fn lookup_is_refused_where_it_cannot_be_answered() {
    let mut db = shop();
    run(
        &mut db,
        "create collection plain (product_id int, note text)",
    );
    for (sql, want) in [
        ("get products count lookup reviews on product_id", "count"),
        ("get products lookup products on id", "itself"),
        ("get products lookup plain on product_id", "@hash"),
        (
            "get products lookup reviews on product_id = sku",
            "do not match",
        ),
        ("get products lookup reviews on nope", "nope"),
        ("get products lookup nosuch on product_id", "nosuch"),
    ] {
        let e = refusal(&mut db, sql);
        assert!(e.contains(want), "`{sql}` -> {e}");
    }
}
