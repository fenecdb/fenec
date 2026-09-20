//! Persistence: record framing and the id counter.
//!
//! The counter is derived from the records; since `compact` throws away the
//! tombstones the derivation falls short there, and the image carries the
//! counter explicitly (`REC_NEXTID`). The tests here hold both ends of that
//! gate: the highest deleted id must not come back, and collections must not
//! interfere with each other's counter.
//!
//! Framing as well: every record is laid out as
//! `[kind][collection-id][length][body]` and the read side *must* consume the
//! length. If it does not, the leftover byte is read as the next record kind
//! -- and the whole file becomes unopenable.

use fenec_core::prelude::*;

fn run(db: &mut Database, sql: &str) {
    for stmt in fenec_ql::parse(sql).expect("parse") {
        db.execute(&stmt).expect("execute");
    }
}

fn names(db: &mut Database) -> Vec<String> {
    let stmt = fenec_ql::parse_one("collections").expect("parse");
    let Response::Schemas(schemas) = db.execute(&stmt).expect("execute") else {
        panic!("expected a schema list");
    };
    schemas.iter().map(|s| s.name.clone()).collect()
}

fn ids(db: &mut Database, collection: &str) -> Vec<u64> {
    let stmt = fenec_ql::parse_one(&format!("get {collection}")).expect("parse");
    let Response::Rows(rs) = db.execute(&stmt).expect("execute") else {
        panic!("expected rows");
    };
    rs.rows.iter().map(|r| r.id).collect()
}

fn reload(db: &Database) -> Database {
    let mut fresh = Database::new();
    fresh.load(&db.snapshot()).expect("load");
    fresh
}

#[cfg(feature = "std-fs")]
fn tmp_path(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("fenecdb-persist-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let path = dir.join("data.fenec");
    let _ = std::fs::remove_file(&path);
    (dir, path)
}

#[cfg(feature = "std-fs")]
fn cleanup(dir: std::path::PathBuf, path: std::path::PathBuf) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_dir(dir);
}

/// Once the document with the highest id is deleted and `compact` runs, that
/// id must never be handed out again -- not even after a restart.
#[test]
fn compaction_keeps_the_id_watermark() {
    let mut db = Database::new();
    run(&mut db, "create collection t (a text)");
    run(&mut db, r#"put t [{a: "one"}, {a: "two"}, {a: "three"}]"#);
    assert_eq!(ids(&mut db, "t"), vec![1, 2, 3]);

    run(&mut db, "del t where id = 3");
    run(&mut db, "compact");

    let mut db = reload(&db);
    run(&mut db, r#"put t {a: "four"}"#);
    assert_eq!(
        ids(&mut db, "t"),
        vec![1, 2, 4],
        "id 3 was handed out again"
    );
}

/// An externally supplied sparse id raises the watermark too, and survives
/// compaction.
#[test]
fn explicit_ids_raise_the_watermark_too() {
    let mut db = Database::new();
    run(&mut db, "create collection t (a text)");
    run(&mut db, r#"put t {id: 9000000000, a: "far"}"#);
    run(&mut db, "del t where id = 9000000000");
    run(&mut db, "compact");

    let mut db = reload(&db);
    run(&mut db, r#"put t {a: "new"}"#);
    assert_eq!(ids(&mut db, "t"), vec![9000000001]);
}

/// The counter is per collection: one must not shift another's ids.
#[test]
fn counters_are_per_collection() {
    let mut db = Database::new();
    run(&mut db, "create collection a (x text)");
    run(&mut db, "create collection b (x text)");
    run(&mut db, r#"put a [{x: "1"}, {x: "2"}, {x: "3"}]"#);
    run(&mut db, r#"put b {x: "1"}"#);
    run(&mut db, "del a where id = 3");
    run(&mut db, "compact");

    let mut db = reload(&db);
    run(&mut db, r#"put a {x: "new"}"#);
    run(&mut db, r#"put b {x: "new"}"#);
    assert_eq!(ids(&mut db, "a"), vec![1, 2, 4]);
    assert_eq!(ids(&mut db, "b"), vec![1, 2]);
}

/// A file with no counter record (WAL tail only, never compacted) must load
/// as before: there the counter is counted from the records.
#[cfg(feature = "std-fs")]
#[test]
fn images_without_the_counter_record_still_load() {
    let (dir, path) = tmp_path("wal");
    {
        let mut db = fenec_core::fs::open(&path).expect("open");
        run(&mut db, "create collection t (a text)");
        run(&mut db, r#"put t [{a: "one"}, {a: "two"}]"#);
        run(&mut db, "del t where id = 2");
        db.sync().expect("sync");
    }
    let mut db = fenec_core::fs::open(&path).expect("reopen");
    assert_eq!(ids(&mut db, "t"), vec![1]);
    run(&mut db, r#"put t {a: "three"}"#);
    assert_eq!(
        ids(&mut db, "t"),
        vec![1, 3],
        "the tombstone must carry the counter"
    );
    cleanup(dir, path);
}

/// The real restart path: compaction rewrites the file and later writes are
/// appended to the tail. Together they must still give the right counter.
#[cfg(feature = "std-fs")]
#[test]
fn watermark_survives_a_file_restart() {
    let (dir, path) = tmp_path("compact");
    {
        let mut db = fenec_core::fs::open(&path).expect("open");
        run(&mut db, "create collection t (a text)");
        run(&mut db, r#"put t [{a: "one"}, {a: "two"}, {a: "three"}]"#);
        run(&mut db, "del t where id = 3");
        run(&mut db, "compact");
        // The write that arrives *after* compaction lands in the tail.
        run(&mut db, r#"put t {a: "four"}"#);
        db.sync().expect("sync");
    }

    let mut db = fenec_core::fs::open(&path).expect("reopen");
    assert_eq!(ids(&mut db, "t"), vec![1, 2, 4]);
    run(&mut db, r#"put t {a: "five"}"#);
    assert_eq!(ids(&mut db, "t"), vec![1, 2, 4, 5]);

    cleanup(dir, path);
}

/// Dropping a collection must not make the file unopenable.
///
/// The `drop` record carries a length field like every other one (even
/// though its body is empty). If the read side does not skip it, the
/// leftover `0` byte is read as a record kind and the *whole* database fails
/// to open with "unknown record kind 0". Since the snapshot path never
/// writes a `drop` record, it fixes itself after a `compact` or a
/// `checkpoint`; the bug only shows up in the raw tail.
#[cfg(feature = "std-fs")]
#[test]
fn dropping_a_collection_keeps_the_file_readable() {
    let (dir, path) = tmp_path("drop");
    {
        let mut db = fenec_core::fs::open(&path).expect("open");
        run(&mut db, "create collection a (x text)");
        run(&mut db, r#"put a {x: "one"}"#);
        run(&mut db, "drop collection a");
        run(&mut db, "create collection b (y text)");
        run(&mut db, r#"put b {y: "two"}"#);
        db.sync().expect("sync");
    }

    let mut db = fenec_core::fs::open(&path).expect("reopen");
    assert_eq!(names(&mut db), vec!["b".to_string()]);
    assert_eq!(ids(&mut db, "b"), vec![1]);
    cleanup(dir, path);
}
