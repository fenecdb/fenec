//! A block its owner keeps open between statements is parked for readers
//! ([`Database::park`]): its writes put back, so the database answers as
//! what has landed, and kept, so the owner's next statement writes them
//! again ([`Database::unpark`]). Nothing may tell a parked block from a
//! block never written to, nor, once it lands, a block parked on the way
//! from one that never was: not an answer, not a byte of the file.

use fenec_core::prelude::*;
use std::sync::{Arc, Mutex};

/// A file in memory, as `fs::open` makes a new one.
#[derive(Clone)]
struct File(Arc<Mutex<Vec<u8>>>);

impl Default for File {
    fn default() -> File {
        File(Arc::new(Mutex::new(fenec_core::engine::MAGIC.to_vec())))
    }
}

impl Sink for File {
    fn append(&mut self, bytes: &[u8]) -> fenec_core::error::Result<()> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(())
    }
    fn rewrite(&mut self, bytes: &[u8]) -> fenec_core::error::Result<()> {
        *self.0.lock().unwrap() = bytes.to_vec();
        Ok(())
    }
}

impl File {
    fn database(&self) -> Database {
        Database::with_sink(Box::new(self.clone()))
    }
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
    }
}

fn run(db: &mut Database, sql: &str) {
    for stmt in fenec_ql::parse(sql).expect("parse") {
        db.execute(&stmt).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

fn answer(db: &Database, sql: &str) -> ResultSet {
    db.query(&fenec_ql::parse_one(sql).expect("parse"), &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .rows()
        .expect("rows")
        .clone()
}

/// Every index read, and the scans: equalities, an `in`, ranges and
/// orders, `count`, a group, a `lookup` with and without `required`,
/// `match`, `near` exact and by the graph, a sparse `near`.
const QUERIES: &[&str] = &[
    "get docs order id",
    r#"get docs where kind = "b" order id"#,
    r#"get docs where kind in ["a", "c"] order id"#,
    "get docs where n >= 40 and n < 90 order id",
    "get docs order n desc limit 7",
    "get docs count",
    r#"get docs count where kind = "a""#,
    "get docs select kind, count(*), sum(n) group kind",
    r#"get docs match body "alpha" limit 5"#,
    r#"get docs near v [0.3, 0.1, 0.9, 0.2] exact limit 5"#,
    r#"get docs near v [0.3, 0.1, 0.9, 0.2] limit 5"#,
    r#"get docs near s "{1:1,6:2}/8" limit 5"#,
    "get tags order id lookup docs on kind = tag",
    "get tags order id lookup docs on kind = tag required",
    "get notes order id",
    r#"get notes where k = "x" order id"#,
];

fn same(a: &Database, b: &Database, what: &str) {
    for q in QUERIES {
        assert_eq!(answer(a, q), answer(b, q), "{what}: {q}");
    }
}

fn filled(file: &File) -> Database {
    let mut db = file.database();
    run(
        &mut db,
        "create collection docs (kind text @hash, n int @sorted, body text @text, \
         s sparse<8> @inverted, v vector<4> @hnsw(cosine, m=8))",
    );
    run(&mut db, "create collection tags (tag text)");
    run(&mut db, "create collection notes (k text @hash, body text)");
    let words = ["alpha", "beta", "gamma", "delta"];
    for i in 0..120 {
        run(
            &mut db,
            &format!(
                r#"put docs {{kind: "{}", n: {}, body: "{} {}", s: "{{{}:1,{}:2}}/8", v: [{}, 0.5, 0.25, 1]}}"#,
                ["a", "b", "c"][i % 3],
                (i * 37) % 100,
                words[i % 4],
                words[(i / 4) % 4],
                i % 4 + 1,
                i % 4 + 5,
                i as f32 / 120.0
            ),
        );
    }
    run(&mut db, r#"put tags {tag: "a"}"#);
    run(&mut db, r#"put tags {tag: "c"}"#);
    for i in 0..10 {
        run(
            &mut db,
            &format!(r#"put notes {{k: "{}", body: "n{i}"}}"#, ["x", "y"][i % 2]),
        );
    }
    db
}

/// Writes that leave every graph as it is: new documents with no vector,
/// rewrites that keep theirs, deletions of documents with none.
const WRITES: &[&str] = &[
    r#"put notes {k: "x", body: "late"}"#,
    r#"set docs {kind: "c", n: 3, body: "gamma alpha"} where id = 4"#,
    r#"set docs {n: 1} where kind = "a" and n < 30"#,
    r#"del notes where id = 2"#,
    r#"put tags {tag: "b"}"#,
    r#"set notes {k: "y"} where k = "x" and id < 6"#,
];

/// A block parked for readers answers as what landed, and once written
/// again and landed, it is the block that never was parked: the answers,
/// and the file's bytes.
#[test]
fn a_parked_block_reads_as_what_landed_and_lands_as_it_would_have() {
    let (file, twin_file) = (File::default(), File::default());
    let mut db = filled(&file);
    let mut twin = filled(&twin_file);
    let landed = file.bytes();
    let mut before = File::default().database();
    before.load(&landed).unwrap();

    db.begin().unwrap();
    twin.begin().unwrap();
    for (i, w) in WRITES.iter().enumerate() {
        run(&mut db, w);
        run(&mut twin, w);
        assert!(db.park(), "{w}");
        assert!(db.reads_landed());
        same(&db, &before, &format!("parked after write {i}"));
        // The owner's next statement writes the block again first.
        if i % 2 == 0 {
            db.unpark().unwrap();
            same(&db, &twin, &format!("written again after write {i}"));
        }
    }
    db.commit().unwrap();
    twin.commit().unwrap();
    same(&db, &twin, "landed");
    assert_eq!(file.bytes(), twin_file.bytes(), "the file");
}

/// A savepoint and a rollback to it, around parks: the writes after it go,
/// the ones before stay, as in a block never parked.
#[test]
fn savepoints_hold_across_parks() {
    let (file, twin_file) = (File::default(), File::default());
    let mut db = filled(&file);
    let mut twin = filled(&twin_file);
    for d in [&mut db, &mut twin] {
        d.begin().unwrap();
        run(d, WRITES[0]);
        run(d, WRITES[1]);
    }
    assert!(db.park());
    db.unpark().unwrap();
    let (sp, sp_twin) = (db.savepoint(), twin.savepoint());
    for d in [&mut db, &mut twin] {
        run(d, WRITES[2]);
        run(d, WRITES[3]);
    }
    assert!(db.park());
    db.rollback_to(&sp).unwrap();
    twin.rollback_to(&sp_twin).unwrap();
    same(&db, &twin, "rolled back to the savepoint");
    for d in [&mut db, &mut twin] {
        run(d, WRITES[4]);
    }
    assert!(db.park());
    db.commit().unwrap();
    twin.commit().unwrap();
    same(&db, &twin, "landed");
    assert_eq!(file.bytes(), twin_file.bytes(), "the file");
}

/// A block rolled back while parked leaves what landed, with nothing to
/// write again.
#[test]
fn a_parked_block_rolled_back_leaves_what_landed() {
    let file = File::default();
    let mut db = filled(&file);
    let landed = file.bytes();
    let mut before = File::default().database();
    before.load(&landed).unwrap();
    db.begin().unwrap();
    for w in WRITES {
        run(&mut db, w);
    }
    assert!(db.park());
    db.rollback();
    same(&db, &before, "rolled back");
    assert_eq!(file.bytes(), landed);
    // And the next block writes where the first would have.
    run(&mut db, WRITES[0]);
    run(&mut before, WRITES[0]);
    same(&db, &before, "written after");
}

/// A block that changed a graph -- a vector written, or a document holding
/// one deleted -- or the schema is not parked: put back and written again,
/// each of its nodes would be a tombstone and another node. Readers wait
/// for it, as they waited for every block.
#[test]
fn a_block_that_changed_a_graph_or_the_schema_is_not_parked() {
    for write in [
        "put docs {kind: \"a\", n: 5, v: [1, 0, 0, 0]}",
        "set docs {v: [0, 1, 0, 0]} where id = 3",
        "del docs where id = 7",
        "create collection more (x int)",
    ] {
        let file = File::default();
        let mut db = filled(&file);
        db.begin().unwrap();
        run(&mut db, WRITES[0]);
        run(&mut db, write);
        assert!(!db.park(), "{write}");
        assert!(!db.reads_landed(), "{write}");
        db.commit().unwrap();
    }
}

/// An index a reader builds while the block is parked -- one the open left
/// unbuilt -- is built from what landed, and kept up when the block is
/// written again.
#[test]
fn an_index_built_while_parked_takes_the_block_when_written_again() {
    let (file, twin_file) = (File::default(), File::default());
    filled(&file);
    filled(&twin_file);
    let mut db = File::default().database();
    db.load(&file.bytes()).unwrap();
    let mut twin = File::default().database();
    twin.load(&twin_file.bytes()).unwrap();
    db.begin().unwrap();
    twin.begin().unwrap();
    for w in WRITES {
        run(&mut db, w);
        run(&mut twin, w);
    }
    assert!(db.park());
    // Built here, by readers, from what landed.
    answer(&db, r#"get notes where k = "x" order id"#);
    answer(&db, r#"get docs where kind = "b" order id"#);
    db.unpark().unwrap();
    same(&db, &twin, "written again");
    db.commit().unwrap();
    twin.commit().unwrap();
    same(&db, &twin, "landed");
}

/// A block its owner left between statements ([`Database::leave_block`])
/// is not written into by anyone else, and not read from until parked:
/// a statement through `&mut self` is refused rather than joined into the
/// block, and a `query` rather than shown its writes. The owner, back,
/// goes on with it.
#[test]
fn a_left_block_is_neither_joined_nor_read_until_parked() {
    let file = File::default();
    let mut db = filled(&file);
    db.begin().unwrap();
    run(&mut db, WRITES[0]);
    db.leave_block();
    assert!(db.block_left());
    let stmt = |sql: &str| fenec_ql::parse_one(sql).unwrap();
    for sql in [r#"put notes {k: "z"}"#, "get notes count"] {
        assert!(db.execute_with(&stmt(sql), &[]).is_err(), "{sql}");
    }
    assert!(db
        .execute_block(&[(&stmt("get notes count"), &[])])
        .is_err());
    assert!(db.query(&stmt("get notes count"), &[]).is_err());
    assert!(db.checkpoint().is_err());
    // Parked, readers read what landed.
    assert!(db.park());
    assert_eq!(
        answer(&db, "get notes count").rows[0].values,
        vec![Value::Int(10)]
    );
    // The owner goes on: its write is there again.
    db.rejoin_block();
    run(&mut db, WRITES[4]);
    assert_eq!(
        answer(&db, "get notes count").rows[0].values,
        vec![Value::Int(11)]
    );
    db.commit().unwrap();
    assert!(!db.block_left());
}

/// A statement that fails in a block leaves the block's writes before it,
/// as a failed pg transaction keeps them for a `ROLLBACK TO`: parked, and
/// taken back to a savepoint before the failure, the block lands what came
/// before the savepoint.
#[test]
fn a_block_a_statement_failed_in_is_parked_and_taken_back() {
    let file = File::default();
    let mut db = file.database();
    run(&mut db, "create collection t (name text)");
    db.begin().unwrap();
    run(&mut db, r#"put t {name: "a"}"#);
    let sp = db.savepoint();
    run(&mut db, r#"put t {name: "b"}"#);
    let bad = fenec_ql::parse_one(r#"put t [{name: "c"}, {name: 1}]"#).unwrap();
    assert!(db.execute_with(&bad, &[]).is_err());
    db.leave_block();
    assert!(!db.reads_landed());
    assert!(db.park());
    assert_eq!(
        answer(&db, "get t count").rows[0].values,
        vec![Value::Int(0)]
    );
    db.rejoin_block();
    db.rollback_to(&sp).unwrap();
    db.commit().unwrap();
    assert_eq!(
        answer(&db, "get t count").rows[0].values,
        vec![Value::Int(1)]
    );
}
