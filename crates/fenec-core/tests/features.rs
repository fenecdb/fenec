//! A build made without the indexes (`Cargo.toml`'s `vector`, `text`,
//! `sparse` and `sorted`): a collection that declares them is made and
//! opened all the same, its documents read and written, its `@sorted`
//! field's comparisons and orders answered by the scan, and what needs one
//! of the others refused, the feature named.
//!
//!     cargo test -p fenec-core --no-default-features --features std-fs --test features

#![cfg(not(any(
    feature = "vector",
    feature = "text",
    feature = "sparse",
    feature = "sorted"
)))]

use fenec_core::prelude::*;

fn run(db: &mut Database, sql: &str) -> Result<Response> {
    let mut last = Response::Affected(0);
    for s in fenec_ql::parse(sql).expect("parse") {
        last = db.execute_with(&s, &[])?;
    }
    Ok(last)
}

fn ints(r: &Response, col: usize) -> Vec<i64> {
    r.rows()
        .expect("rows")
        .rows
        .iter()
        .map(|row| match &row.values[col] {
            Value::Int(n) => *n,
            other => panic!("not an int: {other:?}"),
        })
        .collect()
}

fn declared() -> Database {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection d (year int @sorted, title text @text, \
         embed vector<3> @hnsw(cosine), s sparse<10> @inverted)",
    )
    .expect("a collection declaring every index is made");
    for (year, title) in [(2021, "rust"), (1999, "wasm"), (2010, "search")] {
        run(
            &mut db,
            &format!(
                r#"put d {{year: {year}, title: "{title}", embed: [1.0, 0.0, 0.0], s: "{{1:0.5}}/10"}}"#
            ),
        )
        .expect("put");
    }
    db
}

#[test]
fn documents_read_and_write_and_the_scan_orders_them() {
    let mut db = declared();
    let r = run(&mut db, "get d select year order year").unwrap();
    assert_eq!(ints(&r, 0), [1999, 2010, 2021]);
    let r = run(
        &mut db,
        "get d select year where year > 2000 order year desc",
    )
    .unwrap();
    assert_eq!(ints(&r, 0), [2021, 2010]);
    run(&mut db, "set d {year: 2030} where year = 1999").unwrap();
    run(&mut db, "del d where year = 2010").unwrap();
    let mut back = Database::new();
    back.load(&db.snapshot()).expect("the image opens");
    let r = run(&mut back, "get d select year order year").unwrap();
    assert_eq!(ints(&r, 0), [2021, 2030]);
}

#[test]
fn what_needs_a_missing_index_is_refused() {
    let mut db = declared();
    for (sql, feature) in [
        ("get d near embed [1.0, 0.0, 0.0] limit 1", "`vector`"),
        (r#"get d match title "rust""#, "`text`"),
        (r#"get d near s "{1:0.5}/10" limit 1"#, "`sparse`"),
        ("create index on d (title) @sorted", "`sorted`"),
    ] {
        let e = run(&mut db, sql).unwrap_err().to_string();
        assert!(
            e.contains(feature) && e.contains("this build was made without"),
            "{sql}: {e}"
        );
    }
    // Nothing was built, so nothing is half-built: the collection answers.
    assert_eq!(
        ints(&run(&mut db, "get d select year order year").unwrap(), 0).len(),
        3
    );
}
