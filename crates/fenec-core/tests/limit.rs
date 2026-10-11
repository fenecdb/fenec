//! A block of writes held to a limit as it lands (`Database::set_limit`):
//! what a server's quota of a tenant's file bytes and rows is asked through.
//! The limit is told the record's length to the byte, so a block that would
//! take the file past it is refused before any of it is written; a block
//! that only takes away always lands; a spill is asked before it is written.

#![cfg(all(feature = "std-fs", unix, target_pointer_width = "64"))]

use fenec_core::engine::{Landing, Limit};
use fenec_core::fs::open_mapped;
use fenec_core::prelude::*;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn exec(db: &mut Database, sql: &str) -> Result<Response> {
    db.execute(&fenec_ql::parse_one(sql).expect("parse"))
}

fn count(db: &Database, collection: &str) -> i64 {
    let stmt = fenec_ql::parse_one(&format!("get {collection} count")).unwrap();
    match db.query(&stmt, &[]).unwrap() {
        Response::Rows(rs) => match rs.rows[0].values[0] {
            Value::Int(n) => n,
            ref v => panic!("{v:?}"),
        },
        r => panic!("{r:?}"),
    }
}

fn tmp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fenecdb-limit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{tag}.fenec"));
    let _ = std::fs::remove_file(&path);
    path
}

/// A limit that only notes what it was asked.
fn noting() -> (Limit, Arc<Mutex<Vec<Landing>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = Arc::clone(&seen);
    let limit: Limit = Arc::new(move |_: &Database, l: &Landing| {
        s.lock().unwrap().push(*l);
        Ok(())
    });
    (limit, seen)
}

/// A limit of `max` file bytes for what grows.
fn bytes(max: u64) -> Limit {
    Arc::new(
        move |_: &Database, l: &Landing| match l.grows && l.file + l.adds > max {
            true => Err(format!("bytes: {} past {max}", l.file + l.adds)),
            false => Ok(()),
        },
    )
}

/// What the limit is told a block adds is what the file grows by: a lone
/// document, many of one collection, a block across collections, a schema
/// change, a delete.
#[test]
fn the_limit_is_told_what_the_file_grows_by() {
    let mut db = Database::new();
    let (limit, seen) = noting();
    db.set_limit(Some(limit));
    let cases = [
        "create collection a (t text, n int)",
        "create collection b (t text)",
        "put a {t: \"one\", n: 1}",
        "put a [{t: \"x\", n: 2}, {t: \"y\", n: 3}, {t: \"z\", n: 4}]",
        "set a {n: n + 1} where n > 1",
        "del a where n = 5",
        "drop collection b",
    ];
    for sql in cases {
        let before = db.garbage().file;
        exec(&mut db, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let l = seen
            .lock()
            .unwrap()
            .pop()
            .unwrap_or_else(|| panic!("{sql}"));
        assert_eq!(l.file, before, "{sql}");
        assert_eq!(db.garbage().file - before, l.adds, "{sql}");
    }
    // A block across collections lands as one record of each write's.
    exec(&mut db, "create collection c (t text)").unwrap();
    db.begin().unwrap();
    exec(&mut db, "put a {t: \"w\", n: 9}").unwrap();
    exec(&mut db, "put c {t: \"w\"}").unwrap();
    exec(&mut db, "del a where n = 3").unwrap();
    let before = db.garbage().file;
    db.commit().unwrap();
    let l = seen.lock().unwrap().pop().unwrap();
    assert!(l.grows);
    assert_eq!(db.garbage().file - before, l.adds);
}

/// Whether a block grows: a document written into a collection of the
/// app's, or a schema made or changed -- not deletes, not a drop, not the
/// rows of a collection that is the database's own.
#[test]
fn only_what_writes_a_document_of_the_apps_grows() {
    let mut db = Database::new();
    let (limit, seen) = noting();
    db.set_limit(Some(limit));
    let grows = |db: &mut Database, sql: &str| {
        exec(db, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        seen.lock().unwrap().pop().unwrap().grows
    };
    assert!(grows(&mut db, "create collection a (t text)"));
    // The database's own collections -- the server's idempotency keys,
    // made by the first keyed write -- are none of the app's.
    assert!(!grows(&mut db, "create collection _keys (k text)"));
    assert!(grows(&mut db, "put a {t: \"x\"}"));
    assert!(!grows(&mut db, "put _keys {k: \"x\"}"));
    assert!(grows(&mut db, "set a {t: \"y\"}"));
    assert!(!grows(&mut db, "del a"));
    assert!(!grows(&mut db, "drop collection _keys"));
    // A keyed delete: the delete and the server's own key, in one block.
    exec(&mut db, "put a {t: \"z\"}").unwrap();
    exec(&mut db, "create collection _idempotency (k text)").unwrap();
    db.begin().unwrap();
    exec(&mut db, "del a").unwrap();
    exec(&mut db, "put _idempotency {k: \"k1\"}").unwrap();
    db.commit().unwrap();
    assert!(!seen.lock().unwrap().pop().unwrap().grows);
    assert_eq!(db.documents(), 0, "the database's own rows are not counted");
}

/// A limit of bytes: writes land up to it and the one that would pass it
/// is refused, put back whole, the file not a byte longer; a delete and a
/// drop land past it.
#[test]
fn a_block_past_the_limit_is_put_back_and_deletes_still_land() {
    let mut db = Database::new();
    exec(&mut db, "create collection a (t text, n int)").unwrap();
    let max = db.garbage().file + 2_000;
    db.set_limit(Some(bytes(max)));
    let mut landed = 0;
    let refused = loop {
        let before = db.garbage().file;
        match exec(
            &mut db,
            &format!("put a {{t: \"{}\", n: {landed}}}", "x".repeat(90)),
        ) {
            Ok(_) => landed += 1,
            Err(e) => {
                assert_eq!(db.garbage().file, before, "a refused block wrote");
                break e;
            }
        }
        assert!(db.garbage().file <= max);
    };
    assert!(
        matches!(refused, Error::Quota(ref m) if m.starts_with("bytes: ")),
        "{refused}"
    );
    assert_eq!(count(&db, "a"), landed);
    // Many at once are refused whole: none of them is there.
    let many = (0..20)
        .map(|i| format!("{{t: \"y\", n: {}}}", 1000 + i))
        .collect::<Vec<_>>()
        .join(", ");
    assert!(exec(&mut db, &format!("put a [{many}]")).is_err());
    assert_eq!(count(&db, "a"), landed);
    // A `set` adds a version of every row it matches: refused whole.
    assert!(exec(&mut db, "set a {n: n + 1}").is_err());
    assert_eq!(count(&db, "a where n >= 1000"), 0);
    // Taking away lands past the limit, and so does a compact.
    exec(&mut db, "del a where n < 5").unwrap();
    assert_eq!(count(&db, "a"), landed - 5);
    exec(&mut db, "drop collection a").unwrap();
}

/// Rows, counted over the app's collections as each block leaves them: a
/// `put` of several that would pass the limit lands none of them.
#[test]
fn a_rows_limit_counts_what_the_block_leaves() {
    let mut db = Database::new();
    exec(&mut db, "create collection a (n int)").unwrap();
    exec(&mut db, "create collection _own (n int)").unwrap();
    let rows: Limit = Arc::new(
        |db: &Database, l: &Landing| match l.grows && db.documents() > 3 {
            true => Err(format!("rows: {}", db.documents())),
            false => Ok(()),
        },
    );
    db.set_limit(Some(rows));
    exec(&mut db, "put a [{n: 1}, {n: 2}]").unwrap();
    assert!(exec(&mut db, "put a [{n: 3}, {n: 4}]").is_err());
    exec(&mut db, "put a {n: 3}").unwrap();
    assert!(exec(&mut db, "put a {n: 4}").is_err());
    // An update leaves the count where it was; the database's own rows are
    // none of it.
    exec(&mut db, "set a {n: n * 10}").unwrap();
    exec(&mut db, "put _own [{n: 1}, {n: 2}, {n: 3}]").unwrap();
    assert_eq!(db.documents(), 3);
    exec(&mut db, "del a where n = 10").unwrap();
    exec(&mut db, "put a {n: 4}").unwrap();
    assert_eq!(count(&db, "a"), 3);
}

/// A block too large to hold spills into the file before it lands, and a
/// spill that would pass the limit is refused before it is written: let
/// through and its block refused after, its bytes would stay in the file,
/// dead and past the limit, for every block refused so.
#[test]
fn a_spill_past_the_limit_is_refused_before_it_is_written() {
    let path = tmp("spill");
    let mut db = open_mapped(&path).unwrap();
    exec(&mut db, "create collection a (t text, n int)").unwrap();
    db.set_spill(4096);
    let max = db.garbage().file + 16_384;
    db.set_limit(Some(bytes(max)));
    let row = |i: usize| format!("{{t: \"{}\", n: {i}}}", "z".repeat(200));
    // Under the limit, a spilled block lands whole.
    let small: Vec<String> = (0..40).map(row).collect();
    exec(&mut db, &format!("put a [{}]", small.join(", "))).unwrap();
    assert!(db.garbage().file <= max);
    // Over it, refused before its spill is written.
    let before = std::fs::metadata(&path).unwrap().len();
    let file = db.garbage().file;
    db.begin().unwrap();
    let mut refused = None;
    for chunk in 0..10 {
        let rows: Vec<String> = (0..40).map(|i| row(1000 + chunk * 40 + i)).collect();
        if let Err(e) = exec(&mut db, &format!("put a [{}]", rows.join(", "))) {
            refused = Some(e);
            break;
        }
    }
    db.rollback();
    assert!(matches!(refused, Some(Error::Quota(_))), "{refused:?}");
    assert!(db.garbage().file <= max, "{} past {max}", db.garbage().file);
    assert!(db.garbage().file - file < 16_384);
    assert_eq!(count(&db, "a"), 40);
    db.sync().unwrap();
    assert!(std::fs::metadata(&path).unwrap().len() >= before);
    drop(db);
    let reopened = open_mapped(&path).unwrap();
    assert_eq!(count(&reopened, "a"), 40);
    let _ = std::fs::remove_file(&path);
}
