//! What `memory_bytes` counts, which `--max-memory` goes by before every
//! write that grows the data. The indexes keep their counts as they change
//! -- a walk over a text index of 200 000 documents cost 0.42 ms a call --
//! so these hold each count to what it stands for.

use fenec_core::prelude::*;

fn exec(db: &mut Database, sql: &str) {
    db.execute(&fenec_ql::parse_one(sql).expect("parse"))
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// 2 000 sessions, each with a token of its own.
fn sessions(index: &str) -> Database {
    let mut db = Database::new();
    exec(
        &mut db,
        &format!("create collection s (token text {index}, n int)"),
    );
    let docs = (0..2000)
        .map(|i| {
            vec![
                ("token".into(), Expr::Lit(Value::Text(format!("t{i}")))),
                ("n".into(), Expr::Lit(Value::Int(i))),
            ]
        })
        .collect();
    db.execute(&Statement::Put {
        collection: "s".into(),
        docs,
    })
    .unwrap();
    db
}

fn held(db: &Database) -> usize {
    db.collection("s").unwrap().hashes["token"].memory_bytes()
}

/// A `@hash` index is in the count: two databases that differ by the index
/// alone differ by exactly what it holds. It was left out, and a 1 GB file
/// with one counted 163 MB where the heap held 188.
#[test]
fn a_hash_index_is_counted() {
    let with = sessions("@hash");
    let without = sessions("");
    let ix = held(&with);
    // A key and an id a document at the least.
    assert!(ix > 2000 * (2 + 8), "{ix}");
    assert_eq!(with.memory_bytes() - without.memory_bytes(), ix);
}

/// A value that leaves a hash index takes its bucket with it. Kept, the
/// keys of values that come and go -- a session token, rotated three times
/// here -- piled up until the file was opened again.
#[test]
fn a_bucket_goes_with_its_last_document() {
    let mut db = sessions("@hash");
    let keys = |db: &Database| db.collection("s").unwrap().hashes["token"].len();
    for round in 0..3 {
        for id in 1..=2000 {
            exec(
                &mut db,
                &format!("set s {{token: \"r{round}-{id}\"}} where id = {id}"),
            );
        }
    }
    assert_eq!(keys(&db), 2000);
    let r = db
        .query(
            &fenec_ql::parse_one("get s count where token = \"r2-7\"").unwrap(),
            &[],
        )
        .unwrap();
    assert_eq!(r.rows().unwrap().rows[0].values, vec![Value::Int(1)]);
    exec(&mut db, "del s where n >= 0");
    assert_eq!(keys(&db), 0);
}
