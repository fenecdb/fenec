//! A rewrite takes a document out of the indexes whose field it changes and
//! leaves the others alone. It took the document out of every index and put
//! it back, and for a vector that is an HNSW insert and a tombstone per
//! update of any other field: 1.89 ms at 20 000 x 768.
//!
//! The replica's side is in `replica.rs`.

use fenec_core::prelude::*;

fn exec(db: &mut Database, sql: &str) {
    db.execute(&fenec_ql::parse_one(sql).expect("parse"))
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn rows(db: &Database, sql: &str) -> Vec<(u64, Vec<Value>)> {
    let r = db
        .query(&fenec_ql::parse_one(sql).expect("parse"), &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    r.rows()
        .unwrap()
        .rows
        .iter()
        .map(|r| (r.id, r.values.clone()))
        .collect()
}

const SCHEMA: &str = "create collection d (title text @text, tag text @hash, n int @sorted, \
                      s sparse<16> @inverted, v vector<4> @hnsw(cosine, m=8))";

fn put_all(db: &mut Database) {
    for i in 0..200 {
        exec(
            db,
            &format!(
                "put d {{title: \"word{} common\", tag: \"t{}\", n: {i}, s: \"{{{}:1.5}}/16\", \
                 v: [{}.0, 1.0, {}.0, 0.5]}}",
                i % 9,
                i % 7,
                1 + i % 16,
                i % 13,
                i % 5
            ),
        );
    }
}

fn seeded() -> Database {
    let mut db = Database::new();
    exec(&mut db, SCHEMA);
    put_all(&mut db);
    db
}

fn dead(db: &Database) -> usize {
    db.collection("d").unwrap().vectors["v"].dead()
}

fn live(db: &Database) -> usize {
    db.collection("d").unwrap().vectors["v"].len()
}

/// What every index answers, against a database built from the documents
/// as they stand: a field left alone must still be found through its index,
/// and a changed one under its new value only.
fn same_answers(db: &Database) {
    let mut fresh = Database::new();
    fresh.load(&db.snapshot()).unwrap();
    for sql in [
        "get d order id",
        "get d select id match title \"word3\"",
        "get d select id match title \"renamed\"",
        "get d select id where tag = \"t2\"",
        "get d select id where tag = \"moved\"",
        "get d select id, n where n >= 40 and n < 90 order n desc limit 9",
        "get d select id near s \"{3:1.0,4:1.0}/16\" limit 9",
        "get d select id near v [2.0, 1.0, 3.0, 0.5] exact limit 9",
    ] {
        assert_eq!(rows(db, sql), rows(&fresh, sql), "{sql}");
    }
}

#[test]
fn a_rewrite_keeps_the_nodes_of_vectors_it_leaves_alone() {
    let mut db = seeded();
    // `set` of other fields, every index but the graph's among them.
    exec(
        &mut db,
        "set d {title: \"renamed\", tag: \"moved\", n: 1000} where n < 50",
    );
    assert_eq!((dead(&db), live(&db)), (0, 200));
    // A `put` over an existing id, the vector written again as it was.
    exec(
        &mut db,
        "put d {id: 60, title: \"again\", tag: \"t4\", n: 59, s: \"{2:1.5}/16\", \
         v: [7.0, 1.0, 4.0, 0.5]}",
    );
    assert_eq!((dead(&db), live(&db)), (0, 200));
    same_answers(&db);

    // A changed vector retires its node; one taken away leaves the graph.
    exec(
        &mut db,
        "set d {v: [1.0, 2.0, 3.0, 4.0]} where n >= 190 and n < 200",
    );
    assert_eq!((dead(&db), live(&db)), (10, 200));
    exec(&mut db, "set d {v: null} where n = 100");
    assert_eq!((dead(&db), live(&db)), (11, 199));
    same_answers(&db);
}

/// A graph restored from the checkpoint takes the writes after it the way
/// the write path took them: an update of another field keeps the node.
#[test]
fn the_tail_after_a_checkpoint_keeps_the_nodes_too() {
    let path = std::env::temp_dir().join(format!("fenec-rewrite-{}.fenec", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let mut db = fenec_core::fs::open(&path).unwrap();
        exec(&mut db, SCHEMA);
        put_all(&mut db);
        db.checkpoint().unwrap();
        exec(&mut db, "set d {title: \"renamed\"} where n < 50");
        exec(&mut db, "set d {v: [1.0, 2.0, 3.0, 4.0]} where n >= 190");
        db.sync().unwrap();
        // Closed without a checkpoint: the writes above are the tail.
    }
    let db = fenec_core::fs::open(&path).unwrap();
    assert_eq!((dead(&db), live(&db)), (10, 200));
    same_answers(&db);
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// A float that changes only its sign bit -- `0.0` to `-0.0`, which the
/// scan and the hash key take for one value -- leaves the index answering
/// as a build from the documents does, both ways round.
#[test]
fn a_sign_bit_is_a_change() {
    let mut db = Database::new();
    exec(&mut db, "create collection t (f float @hash)");
    exec(&mut db, "put t {f: 0.0}");
    exec(&mut db, "set t {f: -0.0}");
    let mut fresh = Database::new();
    fresh.load(&db.snapshot()).unwrap();
    for sql in ["get t count where f = -0.0", "get t count where f = 0.0"] {
        assert_eq!(rows(&db, sql), rows(&fresh, sql), "{sql}");
    }
}
