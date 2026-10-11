//! `tests/language.fenec`: a file declaring what a build may go without --
//! a `nocase` field, a collated one, points under `@geo` -- beside a
//! collection declaring none of it, written by a build with every feature.
//! `tests/features.rs` opens it in builds without them, where nothing can
//! write such a file; here the full build holds it to what it answers.
//!
//!     FENEC_WRITE_FIXTURE=1 cargo test -p fenec-core --test all language_file

use fenec_core::prelude::*;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/language.fenec");

/// What the file holds, as statements.
pub const STATEMENTS: &str = r#"
create collection users (email text nocase @unique, n int);
create collection people (name text collate und @sorted, n int);
create collection places (n int, loc geo @geo);
create collection plain (n int @hash, tag text);
put users [{email: "Alice@X.com", n: 1}, {email: "bob@x.com", n: 2}];
put people [{name: "zoe", n: 1}, {name: "Émile", n: 2}];
put places [{n: 1, loc: [13.4, 52.5]}, {n: 2, loc: [2.35, 48.86]}];
put plain [{n: 1, tag: "a"}, {n: 2, tag: "b"}];
"#;

/// Each statement parsed once the ones before it ran: a point is read as
/// written only once its collection says it is one.
fn run(db: &mut Database, sql: &str) -> Result<Response> {
    let mut last = Response::Affected(0);
    for text in sql.split(';').filter(|t| !t.trim().is_empty()) {
        for s in fenec_ql::parse_for(db, text).expect("parse") {
            last = db.execute_with(&s, &[])?;
        }
    }
    Ok(last)
}

fn ints(r: &Response) -> Vec<i64> {
    r.rows()
        .expect("rows")
        .rows
        .iter()
        .map(|row| match &row.values[0] {
            Value::Int(n) => *n,
            other => panic!("not an int: {other:?}"),
        })
        .collect()
}

#[test]
fn the_file_of_every_feature_opens_and_answers() {
    if std::env::var_os("FENEC_WRITE_FIXTURE").is_some() {
        let mut db = Database::new();
        run(&mut db, STATEMENTS).unwrap();
        std::fs::write(FIXTURE, db.snapshot()).unwrap();
    }
    let mut db = Database::new();
    db.load(&std::fs::read(FIXTURE).expect("the fixture"))
        .expect("the fixture opens");
    let r = run(&mut db, r#"get users select n where email = "alice@x.com""#).unwrap();
    assert_eq!(ints(&r), [1]);
    let r = run(&mut db, "get people select n order name").unwrap();
    assert_eq!(ints(&r), [2, 1], "`Émile` before `zoe` in the root order");
    let r = run(&mut db, "get places select n near loc [2.0, 48.0] limit 1").unwrap();
    assert_eq!(ints(&r), [2]);
    let r = run(&mut db, "get plain select n where tag = \"b\"").unwrap();
    assert_eq!(ints(&r), [2]);
}
