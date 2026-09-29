//! A block that outgrows its bound spills its frames into the file before it
//! lands (`Database::spill`), and the stores read them from there: the block
//! is held once, and only up to the bound. Nothing else may change. It lands
//! whole or not at all -- a rollback, a savepoint taken back to and a crash
//! leave none of it -- it reopens as it answered, and a replica is sent it
//! as the one block it would have been. Each is held to a twin that never
//! spills.

#![cfg(all(feature = "std-fs", unix, target_pointer_width = "64"))]

use fenec_core::engine::{landed_block, writes_in};
use fenec_core::fs::{open_in_memory, open_mapped, open_with};
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
    let dir = std::env::temp_dir().join(format!("fenecdb-spill-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{tag}.fenec"));
    let _ = std::fs::remove_file(&path);
    path
}

/// Every collection whole, and what the indexes answer -- the graph's
/// exactly: one built in other batches, or again at an open, may rank a tie
/// the other way.
fn same(a: &Database, b: &Database) {
    assert_eq!(a.collection_names(), b.collection_names());
    for name in a.collection_names() {
        let sql = format!("get {name} order id");
        assert_eq!(answer(a, &sql), answer(b, &sql), "{sql}");
    }
    if a.collection("docs").is_ok() {
        for q in [
            "get docs where kind = \"b\" order id",
            "get docs where n >= 100 and n < 180 order n desc limit 7",
            "get docs match body \"alpha gamma\" limit 5",
            "get docs near v [0.3, 0.1, 0.9, 0.2] exact limit 5",
        ] {
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
            Value::Text(format!("t{i} {}", "x".repeat(i % 50))),
            Value::Text(kind.into()),
            Value::Int(i as i64),
            Value::Text(format!("{word} {word} body {i}")),
            Value::Vector(v.to_vec()),
        ],
    );
}

fn create(db: &mut Database) {
    run(
        db,
        "create collection docs (title text, kind text @hash, n int @sorted, \
         body text @text, v vector<4> @hnsw(cosine, m=8))",
    );
    run(db, "create collection other (x int, note text)");
}

/// Blocks of every shape, each past the bound many times over: one of a
/// collection, one across two that makes and drops a third, a lone
/// statement of many documents, and a batch.
fn workload(db: &mut Database, from: usize) {
    db.begin().unwrap();
    for i in from..from + 300 {
        put_doc(db, i);
    }
    db.commit().unwrap();

    db.begin().unwrap();
    for i in from + 300..from + 600 {
        put_doc(db, i);
        if i % 50 == 0 {
            exec(
                db,
                "put other {x: $1, note: \"across\"}",
                &[Value::Int(i as i64)],
            );
        }
        if i == from + 400 {
            run(db, "create collection scratch (y int)");
            run(db, "put scratch {y: 1}");
        }
        if i == from + 500 {
            run(db, "drop collection scratch");
        }
    }
    run(
        db,
        &format!("set docs {{kind: \"c\"}} where n = {}", from + 20),
    );
    run(db, &format!("del docs where n = {}", from + 21));
    db.commit().unwrap();

    let many: Vec<String> = (0..200)
        .map(|k| {
            format!(
                "{{title: \"many {k} {}\", kind: \"b\", n: {}}}",
                "y".repeat(40),
                from + 5000 + k
            )
        })
        .collect();
    run(db, &format!("put docs [{}]", many.join(", ")));

    let put = fenec_ql::parse_one("put other {x: $1, note: $2}").unwrap();
    let params: Vec<Vec<Value>> = (0..200)
        .map(|k| vec![Value::Int(k), Value::Text("batch ".repeat(20))])
        .collect();
    let stmts: Vec<(&Statement, &[Value])> = params.iter().map(|p| (&put, &p[..])).collect();
    db.execute_block(&stmts).unwrap();
}

/// A database that spills every 2 KB of a block, and its twin that never
/// does, over files of their own.
fn twins(tag: &str) -> (PathBuf, Database, PathBuf, Database) {
    let (a_path, b_path) = (tmp(&format!("{tag}-a")), tmp(&format!("{tag}-b")));
    let mut a = open_mapped(&a_path).unwrap();
    a.set_spill(2048);
    let mut b = open_mapped(&b_path).unwrap();
    b.set_spill(u64::MAX);
    create(&mut a);
    create(&mut b);
    (a_path, a, b_path, b)
}

#[test]
fn a_block_that_spills_lands_as_one_that_did_not() {
    let (a_path, mut a, b_path, mut b) = twins("lands");
    workload(&mut a, 0);
    workload(&mut b, 0);
    same(&a, &b);
    a.checkpoint().unwrap();
    b.checkpoint().unwrap();
    same(&a, &b);
    workload(&mut a, 10_000);
    workload(&mut b, 10_000);
    same(&a, &b);
    a.sync().unwrap();
    b.sync().unwrap();
    // The spills and lands hold the blocks the twin's block records do.
    assert_ne!(
        std::fs::read(&a_path).unwrap(),
        std::fs::read(&b_path).unwrap()
    );
    drop((a, b));
    let (a, b) = (
        open_mapped(&a_path).unwrap(),
        open_in_memory(&b_path).unwrap(),
    );
    same(&a, &b);
    assert_eq!(a.change_seq(), b.change_seq());
    let a = open_in_memory(&a_path).unwrap();
    same(&a, &b);
    let _ = std::fs::remove_file(&a_path);
    let _ = std::fs::remove_file(&b_path);
}

/// What a block holds in memory stays under the bound and a statement,
/// however much it writes; its twin holds all of it.
#[test]
fn a_block_holds_its_bound_and_no_more() {
    let (a_path, mut a, b_path, mut b) = twins("bound");
    for db in [&mut a, &mut b] {
        db.set_handover(0);
        db.begin().unwrap();
    }
    for i in 0..600 {
        put_doc(&mut a, i);
        put_doc(&mut b, i);
        assert!(held(&a) < 2048 + 400, "{} at {i}", held(&a));
        assert!(a.memory_bytes() <= b.memory_bytes(), "at {i}");
    }
    // The twin holds the block twice, in its record and in its stores.
    assert!(held(&b) > 10 * 2048, "{}", held(&b));
    assert!(
        a.memory_bytes() + held(&b) < b.memory_bytes(),
        "{} against {}",
        a.memory_bytes(),
        b.memory_bytes()
    );
    a.commit().unwrap();
    b.commit().unwrap();
    same(&a, &b);
    let _ = std::fs::remove_file(&a_path);
    let _ = std::fs::remove_file(&b_path);
}

/// A block put back after it spilled leaves nothing of itself: not an
/// answer, not a document on reopen. Its spills are dead in the file.
#[test]
fn a_block_put_back_after_it_spilled_leaves_nothing() {
    let (a_path, mut a, b_path, mut b) = twins("rollback");
    workload(&mut a, 0);
    workload(&mut b, 0);
    for db in [&mut a, &mut b] {
        db.begin().unwrap();
        for i in 20_000..20_300 {
            put_doc(db, i);
        }
        run(db, "del docs where n < 100");
        run(db, "set docs {title: \"gone\"} where n >= 200 and n < 300");
        run(db, "create collection gone (z int)");
        run(db, "put other {x: -1}");
        db.rollback();
    }
    same(&a, &b);
    workload(&mut a, 30_000);
    workload(&mut b, 30_000);
    same(&a, &b);
    drop((a, b));
    let (a, b) = (open_mapped(&a_path).unwrap(), open_mapped(&b_path).unwrap());
    same(&a, &b);
    assert_eq!(a.change_seq(), b.change_seq());
    let _ = std::fs::remove_file(&a_path);
    let _ = std::fs::remove_file(&b_path);
}

/// A crash in the middle of a block that spilled loses the block, and only
/// it; the spills the file ends with are cut off.
#[test]
fn a_crash_after_spills_loses_the_block_alone() {
    let (a_path, mut a, b_path, mut b) = twins("crash");
    workload(&mut a, 0);
    workload(&mut b, 0);
    a.sync().unwrap();
    let before = std::fs::metadata(&a_path).unwrap().len();
    a.begin().unwrap();
    for i in 40_000..40_300 {
        put_doc(&mut a, i);
    }
    a.sync().unwrap();
    assert!(std::fs::metadata(&a_path).unwrap().len() > before + 10_000);
    // Gone without landing or putting it back.
    drop(a);
    let a = open_mapped(&a_path).unwrap();
    same(&a, &b);
    assert_eq!(std::fs::metadata(&a_path).unwrap().len(), before);
    drop(a);
    let _ = std::fs::remove_file(&a_path);
    let _ = std::fs::remove_file(&b_path);
}

/// A savepoint stops the spills -- its marks are where the stores stood in
/// memory -- and a block taken back to one lands as its twin's does,
/// spilled before it or not.
#[test]
fn a_savepoint_is_taken_back_to_across_the_spills_before_it() {
    let (a_path, mut a, b_path, mut b) = twins("savepoint");
    for db in [&mut a, &mut b] {
        db.begin().unwrap();
        for i in 0..300 {
            put_doc(db, i);
        }
        let sp = db.savepoint();
        for i in 300..600 {
            put_doc(db, i);
        }
        run(db, "del docs where n < 50");
        db.rollback_to(&sp).unwrap();
        for i in 600..700 {
            put_doc(db, i);
        }
        db.commit().unwrap();
    }
    same(&a, &b);
    drop((a, b));
    same(
        &open_mapped(&a_path).unwrap(),
        &open_mapped(&b_path).unwrap(),
    );
    let _ = std::fs::remove_file(&a_path);
    let _ = std::fs::remove_file(&b_path);
}

/// A block that spilled is not put back for a reader: written again, its
/// writes would be read into memory again.
#[test]
fn a_block_that_spilled_is_not_parked() {
    let (a_path, mut a, b_path, mut b) = twins("park");
    run(&mut a, "create collection plain (s text)");
    run(&mut b, "create collection plain (s text)");
    for db in [&mut a, &mut b] {
        db.begin().unwrap();
        for i in 0..200 {
            exec(
                db,
                "put plain {s: $1}",
                &[Value::Text(format!("{i} {}", "p".repeat(60)))],
            );
        }
        db.leave_block();
    }
    assert!(!a.park());
    assert!(b.park());
    let _ = std::fs::remove_file(&a_path);
    let _ = std::fs::remove_file(&b_path);
}

/// The file's sink, with the writes it passes on as a primary's feed does:
/// a block that spilled as the one block record it would have been.
#[derive(Clone, Default)]
struct Feed(Arc<Mutex<Vec<Vec<u8>>>>);

struct Tee {
    file: Box<dyn Sink>,
    feed: Feed,
}

impl Sink for Tee {
    fn append(&mut self, bytes: &[u8]) -> fenec_core::error::Result<()> {
        self.file.append(bytes)
    }
    fn record(&mut self, seq: u64, bytes: &[u8]) -> fenec_core::error::Result<()> {
        self.file.record(seq, bytes)?;
        self.feed.0.lock().unwrap().push(bytes.to_vec());
        Ok(())
    }
    fn land(
        &mut self,
        seq: u64,
        spilled: &[&[u8]],
        record: &[u8],
    ) -> fenec_core::error::Result<()> {
        let block = landed_block(spilled, record)?;
        self.file.land(seq, spilled, record)?;
        self.feed.0.lock().unwrap().push(block);
        Ok(())
    }
    fn rewrite(&mut self, bytes: &[u8]) -> fenec_core::error::Result<()> {
        self.file.rewrite(bytes)
    }
    fn remapped(&mut self) -> Option<fenec_core::store::Base> {
        self.file.remapped()
    }
    fn written_through(&mut self) -> fenec_core::error::Result<Option<fenec_core::store::Base>> {
        self.file.written_through()
    }
    fn sync(&mut self) -> fenec_core::error::Result<()> {
        self.file.sync()
    }
    fn flush(&mut self) -> fenec_core::error::Result<Option<fenec_core::engine::Durability>> {
        self.file.flush()
    }
}

#[test]
fn a_replica_is_sent_a_spilled_block_whole() {
    let path = tmp("primary");
    let feed = Feed::default();
    let tap = feed.clone();
    let mut primary = open_with(
        &path,
        true,
        Box::new(move |file| Ok(Box::new(Tee { file, feed: tap }) as Box<dyn Sink>)),
    )
    .unwrap();
    primary.set_spill(2048);
    create(&mut primary);
    workload(&mut primary, 0);

    let replica_path = tmp("replica");
    let mut replica = open_mapped(&replica_path).unwrap();
    replica.follow(vec![(7, 0)]).unwrap();
    let writes = feed.0.lock().unwrap().clone();
    let counted: u64 = writes.iter().map(|r| writes_in(r).unwrap()).sum();
    assert_eq!(counted, primary.change_seq());
    for record in &writes {
        replica.apply(record).unwrap();
    }
    assert_eq!(replica.change_seq(), primary.change_seq());
    same(&replica, &primary);
    drop(replica);
    same(&open_mapped(&replica_path).unwrap(), &primary);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&replica_path);
}

/// A collection a block that spilled dropped comes back with the block put
/// back, and what landed of it before is still handed over to the file.
#[test]
fn a_collection_dropped_in_a_spilled_block_comes_back_whole() {
    let (a_path, mut a, b_path, mut b) = twins("dropped");
    for db in [&mut a, &mut b] {
        db.set_handover(u64::MAX);
        for i in 0..40 {
            exec(
                db,
                "put other {x: $1, note: $2}",
                &[Value::Int(i), Value::Text("kept ".repeat(10))],
            );
        }
        db.begin().unwrap();
        // Written in the block before it is dropped: its frames spill.
        exec(db, "put other {x: -1, note: $1}", &[Value::Text("block ".repeat(10))]);
        run(db, "drop collection other");
        for i in 0..300 {
            put_doc(db, i);
        }
        db.rollback();
    }
    same(&a, &b);
    let other = |db: &Database| db.collection("other").unwrap().store.heap_bytes();
    assert!(other(&a) > 0);
    a.set_handover(0);
    exec(&mut a, "put other {x: 99}", &[]);
    assert_eq!(other(&a), 0);
    let _ = std::fs::remove_file(&a_path);
    let _ = std::fs::remove_file(&b_path);
}
