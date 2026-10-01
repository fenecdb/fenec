//! A mapped database hands the documents written since its file was opened
//! over to the file (`Database::hand_over`): its stores read them from the
//! file's pages and let their segments go. Nothing else may change -- not an
//! answer, not a byte of the file, not the image a checkpoint or a compact
//! writes, not what the file opens as -- whatever wrote them: a lone
//! statement, a block across collections, one put back in part or whole, a
//! block a segment's seal split, a compact beside the database, a primary's
//! records applied by a replica. Each is held to a twin that hands nothing
//! over.

#![cfg(all(feature = "std-fs", unix, target_pointer_width = "64"))]

use fenec_core::engine::writes_in;
use fenec_core::fs::{open_in_memory, open_mapped};
use fenec_core::prelude::*;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn run(db: &mut Database, sql: &str) {
    for stmt in fenec_ql::parse(sql).expect("parse") {
        db.execute(&stmt).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

fn exec(db: &mut Database, sql: &str, params: &[Value]) {
    db.execute_with(&fenec_ql::parse_one(sql).expect("parse"), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn answer(db: &Database, sql: &str) -> ResultSet {
    db.query(&fenec_ql::parse_one(sql).expect("parse"), &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .rows()
        .expect("rows")
        .clone()
}

fn tmp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fenecdb-handover-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{tag}.fenec"));
    let _ = std::fs::remove_file(&path);
    path
}

const QUERIES: &[&str] = &[
    "get docs count",
    "get docs order id",
    "get docs where kind = \"b\" order id",
    "get docs where n >= 100 and n < 180 order n desc limit 7",
    "get docs where title ~ \"7\" order id",
    "get docs match body \"alpha gamma\" limit 5",
    "get docs near v [0.3, 0.1, 0.9, 0.2] exact limit 5",
    "get docs select kind, count(*), sum(n) group kind",
];

/// Every query, and every collection whole, answered the same.
fn same(a: &Database, b: &Database) {
    assert_eq!(a.collection_names(), b.collection_names());
    for name in a.collection_names() {
        let sql = format!("get {name} order id");
        assert_eq!(answer(a, &sql), answer(b, &sql), "{sql}");
    }
    if a.collection("docs").is_ok() {
        for q in QUERIES {
            assert_eq!(answer(a, q), answer(b, q), "{q}");
        }
    }
}

/// The record bytes the stores hold in memory.
fn held(db: &Database) -> usize {
    db.collection_names()
        .iter()
        .map(|n| db.collection(n).unwrap().store.heap_bytes())
        .sum()
}

fn put_doc(db: &mut Database, i: usize) {
    let kind = ["a", "b", "c"][i % 3];
    let word = ["alpha", "beta", "gamma", "delta"][i % 4];
    let v = [(i % 7) as f32, (i % 5) as f32, (i % 3) as f32, 1.0];
    exec(
        db,
        "put docs {title: $1, kind: $2, n: $3, body: $4, v: $5}",
        &[
            Value::Text(format!("t{i}")),
            Value::Text(kind.into()),
            Value::Int(i as i64),
            Value::Text(format!("{word} {word} body {i}")),
            Value::Vector(v.to_vec()),
        ],
    );
}

/// Writes of every shape a block takes: lone statements, one of many
/// documents, a block across collections that makes and drops one, a block
/// put back, a collection dropped and made again under its name.
fn workload(db: &mut Database, from: usize) {
    if db.collection("docs").is_err() {
        run(
            db,
            "create collection docs (title text, kind text @hash, n int @sorted, \
             body text @text, v vector<4> @hnsw(cosine, m=8))",
        );
        run(db, "create collection other (x int, note text)");
    }
    for i in from..from + 120 {
        put_doc(db, i);
        if i % 40 == 0 {
            exec(db, "put other {x: $1}", &[Value::Int(i as i64)]);
        }
    }
    let many: Vec<String> = (0..30)
        .map(|k| {
            format!(
                "{{title: \"many {k}\", kind: \"b\", n: {}}}",
                from + 500 + k
            )
        })
        .collect();
    run(db, &format!("put docs [{}]", many.join(", ")));
    run(db, &format!("del docs where n < {}", from + 10));
    run(
        db,
        &format!(
            "set docs {{title: \"changed\"}} where n >= {} and n < {}",
            from + 60,
            from + 70
        ),
    );

    db.begin().unwrap();
    put_doc(db, from + 1000);
    run(db, "put other {x: 7, note: \"in a block\"}");
    run(db, "create collection scratch (y int)");
    run(db, "put scratch {y: 1}");
    run(
        db,
        &format!("set docs {{kind: \"c\"}} where n = {}", from + 20),
    );
    run(db, "drop collection scratch");
    db.commit().unwrap();

    db.begin().unwrap();
    put_doc(db, from + 2000);
    run(db, &format!("del docs where n = {}", from + 30));
    db.rollback();

    db.begin().unwrap();
    run(db, "put other {x: 8}");
    put_doc(db, from + 4000);
    db.commit().unwrap();

    run(db, "drop collection other");
    run(db, "create collection other (x int, note text)");
    run(db, "put other {x: 9, note: \"made again\"}");
}

/// A database that hands over after every block, and its twin that never
/// does, over files of their own.
fn twins(tag: &str) -> (PathBuf, Database, PathBuf, Database) {
    let (a_path, b_path) = (tmp(&format!("{tag}-a")), tmp(&format!("{tag}-b")));
    let mut a = open_mapped(&a_path).unwrap();
    a.set_handover(0);
    let mut b = open_mapped(&b_path).unwrap();
    b.set_handover(u64::MAX);
    (a_path, a, b_path, b)
}

fn same_bytes(a: &PathBuf, b: &PathBuf) {
    let (x, y) = (std::fs::read(a).unwrap(), std::fs::read(b).unwrap());
    assert!(x == y, "{} and {} bytes differ", x.len(), y.len());
}

#[test]
fn every_answer_and_every_byte_stands_after_a_handover() {
    let (a_path, mut a, b_path, mut b) = twins("answers");
    workload(&mut a, 0);
    workload(&mut b, 0);
    same(&a, &b);
    // Everything written is in the file, and read from there.
    assert_eq!(held(&a), 0);
    assert!(held(&b) > 5_000, "{}", held(&b));
    assert!(a.collection("docs").unwrap().store.is_mapped());
    assert!(a.memory_bytes() < b.memory_bytes());
    a.sync().unwrap();
    b.sync().unwrap();
    // The handover wrote nothing of its own.
    same_bytes(&a_path, &b_path);

    // A checkpoint writes each store's frames as they stand, from the file
    // or from memory: the same image.
    a.checkpoint().unwrap();
    b.checkpoint().unwrap();
    same_bytes(&a_path, &b_path);
    same(&a, &b);
    // The records after it are handed over as the ones after an open.
    workload(&mut a, 10_000);
    workload(&mut b, 10_000);
    same(&a, &b);
    assert_eq!(held(&a), 0);
    a.sync().unwrap();
    b.sync().unwrap();
    same_bytes(&a_path, &b_path);

    run(&mut a, "compact");
    run(&mut b, "compact");
    same_bytes(&a_path, &b_path);
    workload(&mut a, 20_000);
    workload(&mut b, 20_000);
    same(&a, &b);
    assert_eq!(held(&a), 0);

    drop((a, b));
    same_bytes(&a_path, &b_path);
    let (a, b) = (
        open_mapped(&a_path).unwrap(),
        open_in_memory(&b_path).unwrap(),
    );
    same(&a, &b);
    let _ = std::fs::remove_file(&a_path);
    let _ = std::fs::remove_file(&b_path);
}

/// A block of more than a segment's worth: its record's frames lie in two
/// segments, one data record of them, or a block record of a record each
/// when it writes another collection too.
#[test]
fn a_block_a_seal_split_is_handed_over_whole() {
    let (a_path, mut a, b_path, mut b) = twins("sealed");
    let body = "x".repeat(4000);
    for db in [&mut a, &mut b] {
        run(db, "create collection big (n int, body text)");
        run(db, "create collection side (n int)");
        for across in [false, true] {
            db.begin().unwrap();
            for n in 0..2500 {
                exec(
                    db,
                    "put big {n: $1, body: $2}",
                    &[Value::Int(n), Value::Text(body.clone())],
                );
                if across && n % 100 == 0 {
                    exec(db, "put side {n: $1}", &[Value::Int(n)]);
                }
            }
            db.commit().unwrap();
            run(db, "set big {body: \"short\"} where n < 50");
        }
    }
    assert!(a.collection("big").unwrap().store.segment_count() <= 1);
    assert!(b.collection("big").unwrap().store.segment_count() > 1);
    assert_eq!(held(&a), 0);
    same(&a, &b);
    a.sync().unwrap();
    b.sync().unwrap();
    same_bytes(&a_path, &b_path);
    a.checkpoint().unwrap();
    b.checkpoint().unwrap();
    same_bytes(&a_path, &b_path);
    drop((a, b));
    same(
        &open_mapped(&a_path).unwrap(),
        &open_mapped(&b_path).unwrap(),
    );
    let _ = std::fs::remove_file(&a_path);
    let _ = std::fs::remove_file(&b_path);
}

/// Nothing is handed over while a block is open -- its writes are in no
/// record yet -- nor before the documents amount to the threshold, nor from
/// a database read into memory, which has no mapping to read them from.
#[test]
fn a_handover_waits_for_its_threshold_and_for_the_block() {
    let path = tmp("threshold");
    let mut db = open_mapped(&path).unwrap();
    run(&mut db, "create collection c (n int, body text)");
    let body = Value::Text("y".repeat(1000));
    db.set_handover(64 * 1024);
    for n in 0..60 {
        exec(
            &mut db,
            "put c {n: $1, body: $2}",
            &[Value::Int(n), body.clone()],
        );
    }
    assert!(held(&db) > 50_000, "{}", held(&db));
    for n in 60..200 {
        exec(
            &mut db,
            "put c {n: $1, body: $2}",
            &[Value::Int(n), body.clone()],
        );
        assert!(held(&db) < 64 * 1024 + 1100, "{}", held(&db));
    }
    db.begin().unwrap();
    exec(
        &mut db,
        "put c {n: 1000, body: $1}",
        std::slice::from_ref(&body),
    );
    assert_eq!(db.hand_over().unwrap(), 0);
    db.commit().unwrap();
    assert!(db.hand_over().unwrap() > 0);
    assert_eq!(held(&db), 0);
    assert_eq!(
        answer(&db, "get c count").rows[0].values[0],
        Value::Int(201)
    );
    drop(db);

    let mut db = open_in_memory(&path).unwrap();
    db.set_handover(0);
    exec(&mut db, "put c {n: 2000, body: $1}", &[body]);
    assert_eq!(db.hand_over().unwrap(), 0);
    assert!(held(&db) > 200_000);
    let _ = std::fs::remove_file(&path);
}

/// The writes made while a compact wrote its file beside the database are
/// in that file, inside the image, and the stores hold them in memory as
/// they would the records of a write: they are handed over the same way.
#[test]
fn a_compact_beside_hands_over_what_was_written_meanwhile() {
    let (a_path, a, b_path, mut b) = twins("beside");
    let a = std::sync::RwLock::new(a);
    workload(&mut a.write().unwrap(), 0);
    workload(&mut b, 0);
    let meanwhile = |db: &mut Database| {
        put_doc(db, 90_000);
        run(db, "set docs {body: \"rewritten\"} where n = 100");
        run(db, "del docs where n = 101");
    };
    // No handover meanwhile: the stores hold what was written, as a
    // server's do between two.
    a.write().unwrap().set_handover(u64::MAX);
    let compact = fenec_ql::parse_one("compact").unwrap();
    Database::maintain_with(&a, &compact, &mut || meanwhile(&mut a.write().unwrap()))
        .unwrap()
        .unwrap();
    let mut a = a.into_inner().unwrap();
    // A compact of a mapped file takes two copies, its graphs' and its
    // file's, and the writes land after each.
    meanwhile(&mut b);
    meanwhile(&mut b);
    run(&mut b, "compact");
    same(&a, &b);
    assert!(held(&a) > 0);
    a.set_handover(0);
    assert!(a.hand_over().unwrap() > 0);
    assert_eq!(held(&a), 0);
    same(&a, &b);
    workload(&mut a, 50_000);
    workload(&mut b, 50_000);
    same(&a, &b);
    assert_eq!(held(&a), 0);
    drop((a, b));
    same(
        &open_mapped(&a_path).unwrap(),
        &open_mapped(&b_path).unwrap(),
    );
    let _ = std::fs::remove_file(&a_path);
    let _ = std::fs::remove_file(&b_path);
}

/// Writes with their numbers, as a primary's feed passes them on.
#[derive(Clone, Default)]
struct Tap {
    writes: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Sink for Tap {
    fn append(&mut self, _bytes: &[u8]) -> fenec_core::error::Result<()> {
        Ok(())
    }
    fn record(&mut self, _seq: u64, bytes: &[u8]) -> fenec_core::error::Result<()> {
        self.writes.lock().unwrap().push(bytes.to_vec());
        Ok(())
    }
    fn rewrite(&mut self, _bytes: &[u8]) -> fenec_core::error::Result<()> {
        Ok(())
    }
}

/// A replica writes its primary's records into its file as they came, and
/// its stores hold the frames they hold: it hands them over as a primary
/// does, and reopens as the primary answers.
#[test]
fn a_replica_hands_over_what_it_applies() {
    let tap = Tap::default();
    let mut primary = Database::with_sink(Box::new(tap.clone()));
    workload(&mut primary, 0);
    workload(&mut primary, 10_000);

    let path = tmp("replica");
    let mut replica = open_mapped(&path).unwrap();
    replica.set_handover(20_000);
    replica.follow(vec![(7, 0)]).unwrap();
    let writes = tap.writes.lock().unwrap().clone();
    for piece in writes.chunks(9) {
        let records: Vec<u8> = piece.concat();
        assert_eq!(replica.apply(&records).unwrap(), piece.len());
        assert!(
            held(&replica) < 20_000 + records.len(),
            "{}",
            held(&replica)
        );
    }
    let counted: u64 = writes.iter().map(|r| writes_in(r).unwrap()).sum();
    assert_eq!(replica.change_seq(), counted);
    same(&replica, &primary);
    replica.set_handover(0);
    replica.hand_over().unwrap();
    assert_eq!(held(&replica), 0);
    same(&replica, &primary);
    drop(replica);
    let reopened = open_mapped(&path).unwrap();
    assert!(reopened.history().following);
    same(&reopened, &primary);
    let _ = std::fs::remove_file(&path);
}
