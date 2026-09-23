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

/// A crash in the middle of an append leaves the last record cut short, in
/// its body or its header. The open loads what came before it and cuts it
/// off the file: left there, it swallowed the first record appended after
/// it -- a write acknowledged, and gone on the open after -- and a header cut
/// short left the file unopenable.
#[cfg(feature = "std-fs")]
#[test]
fn a_record_cut_short_is_cut_off_before_the_next_write() {
    use std::io::Write;
    let torn: [(&str, &[u8]); 3] = [
        // A data record promising 100 bytes, 8 of them there.
        ("body", &[3, 1, 100, 0, 1, 2, 3, 4, 5, 6, 7, 8]),
        // Its kind and half its collection id.
        ("header", &[3, 0x81]),
        ("kind", &[3]),
    ];
    for (tag, bytes) in torn {
        let (dir, path) = tmp_path(&format!("torn-{tag}"));
        {
            let mut db = fenec_core::fs::open(&path).expect("open");
            run(&mut db, "create collection t (a text)");
            run(&mut db, r#"put t {a: "before"}"#);
            db.sync().expect("sync");
        }
        let whole = std::fs::metadata(&path).unwrap().len();
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(bytes).unwrap();
        drop(f);
        {
            let mut db = fenec_core::fs::open(&path).expect("the open after the crash");
            assert_eq!(ids(&mut db, "t"), vec![1], "{tag}");
            assert_eq!(std::fs::metadata(&path).unwrap().len(), whole, "{tag}");
            run(&mut db, r#"put t {a: "after"}"#);
            db.sync().expect("sync");
        }
        let mut db = fenec_core::fs::open(&path).expect("the open after that");
        assert_eq!(ids(&mut db, "t"), vec![1, 2], "{tag}");
        cleanup(dir, path);
    }
}

/// A record cut short inside the checkpoint image is not a crash's torn
/// tail -- an image is written beside the file and renamed over it whole --
/// but a damaged or truncated file. The open refuses it and leaves every
/// byte where it was: cut there as a torn tail is, one flipped bit in an
/// image's length deleted every record after it, intact ones included.
#[cfg(feature = "std-fs")]
#[test]
fn a_record_cut_short_inside_the_image_refuses_the_open_and_cuts_nothing() {
    let (dir, path) = tmp_path("torn-image");
    {
        let mut db = fenec_core::fs::open(&path).expect("open");
        run(&mut db, "create collection t (a text)");
        run(&mut db, r#"put t {a: "one"}"#);
        run(&mut db, r#"put t {a: "two"}"#);
        db.checkpoint().expect("checkpoint");
        run(&mut db, r#"put t {a: "three"}"#);
        db.sync().expect("sync");
    }
    let good = std::fs::read(&path).unwrap();
    // The counter header, then the image's records: find its data record
    // and have its length promise more than the file holds.
    let mut pos = 8 + 17;
    let data = loop {
        let kind = good[pos];
        let mut p = pos + 1;
        let _cid = varint(&good, &mut p);
        let len_at = p;
        let len = varint(&good, &mut p);
        if kind == 3 {
            break len_at;
        }
        pos = p + len as usize;
    };
    assert_eq!(good[data] & 0x80, 0, "a one-byte length");
    let mut bad = good.clone();
    bad[data] = 0x7f;
    std::fs::write(&path, &bad).unwrap();
    let err = fenec_core::fs::open(&path)
        .err()
        .expect("the damaged image opened");
    assert!(err.to_string().contains("checkpoint image"), "{err}");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        bad,
        "the open wrote to the file"
    );

    // A file cut between two of the image's records: every record there is
    // whole, and only the image's stated length can tell.
    std::fs::write(&path, &good[..data - 2]).unwrap();
    let err = fenec_core::fs::open(&path)
        .err()
        .expect("the truncated image opened");
    assert!(err.to_string().contains("checkpoint image"), "{err}");
    assert_eq!(std::fs::metadata(&path).unwrap().len(), (data - 2) as u64);

    // Mended, every row is there, the one after the image included.
    std::fs::write(&path, &good).unwrap();
    let mut db = fenec_core::fs::open(&path).expect("the mended file");
    assert_eq!(ids(&mut db, "t"), vec![1, 2, 3]);
    cleanup(dir, path);
}

#[cfg(feature = "std-fs")]
fn varint(b: &[u8], p: &mut usize) -> u64 {
    let (mut v, mut shift) = (0u64, 0);
    loop {
        let byte = b[*p];
        *p += 1;
        v |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return v;
        }
        shift += 7;
    }
}

/// A tool that only looks at a file -- `fenec types` -- opens it read-only:
/// a server may be in the middle of appending to it, and from outside that
/// looks like a torn last record. Nothing is cut, created or written, and a
/// write is refused.
#[cfg(feature = "std-fs")]
#[test]
fn a_read_only_open_leaves_the_file_as_it_is() {
    use std::io::Write;
    let (dir, path) = tmp_path("read-only");
    {
        let mut db = fenec_core::fs::open(&path).expect("open");
        run(&mut db, "create collection t (a text)");
        run(&mut db, r#"put t {a: "before"}"#);
        db.sync().expect("sync");
    }
    // A record the writer has only half appended.
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    f.write_all(&[3, 1, 100, 0, 1, 2]).unwrap();
    drop(f);
    let before = std::fs::read(&path).unwrap();
    let mut db = fenec_core::fs::open_read_only(&path).expect("read-only open");
    assert_eq!(ids(&mut db, "t"), vec![1]);
    let err = db
        .execute(&fenec_ql::parse_one(r#"put t {a: "no"}"#).unwrap())
        .err()
        .expect("a write went through");
    assert!(
        matches!(err, fenec_core::error::Error::ReadOnly(_)),
        "{err}"
    );
    drop(db);
    assert_eq!(std::fs::read(&path).unwrap(), before);

    let missing = dir.join("missing.fenec");
    assert!(fenec_core::fs::open_read_only(&missing).is_err());
    assert!(!missing.exists(), "the read-only open created the file");
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
