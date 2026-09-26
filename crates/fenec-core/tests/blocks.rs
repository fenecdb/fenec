//! A block of writes lands whole or not at all ([`Database::execute_block`]).
//! In memory, one that does not land leaves every document, every index and
//! the change counter as it found them. On disk it is one record, which a
//! crash writes whole or cuts off whole. A replica applies it whole and
//! numbers it as its primary did. A `put` of many documents is a block of
//! its own.

use fenec_core::engine::writes_in;
use fenec_core::plugin::{Hook, Plugin, Registry, WriteOp};
use fenec_core::prelude::*;
use std::sync::{Arc, Mutex};

/// Writes with their numbers, as a sink is handed them.
type Numbered = Vec<(u64, Vec<u8>)>;

/// Rows by id, as a query answers them.
type Rows = Vec<(u64, Vec<Value>)>;

/// A file in memory that keeps the writes it was handed, with their numbers.
#[derive(Clone)]
struct Tap {
    file: Arc<Mutex<Vec<u8>>>,
    writes: Arc<Mutex<Numbered>>,
}

impl Default for Tap {
    fn default() -> Tap {
        Tap {
            file: Arc::new(Mutex::new(fenec_core::engine::MAGIC.to_vec())),
            writes: Arc::default(),
        }
    }
}

impl Sink for Tap {
    fn append(&mut self, bytes: &[u8]) -> fenec_core::error::Result<()> {
        self.file.lock().unwrap().extend_from_slice(bytes);
        Ok(())
    }
    fn record(&mut self, seq: u64, bytes: &[u8]) -> fenec_core::error::Result<()> {
        self.file.lock().unwrap().extend_from_slice(bytes);
        self.writes.lock().unwrap().push((seq, bytes.to_vec()));
        Ok(())
    }
    fn rewrite(&mut self, bytes: &[u8]) -> fenec_core::error::Result<()> {
        *self.file.lock().unwrap() = bytes.to_vec();
        Ok(())
    }
}

impl Tap {
    fn database(&self) -> Database {
        Database::with_sink(Box::new(self.clone()))
    }
    fn bytes(&self) -> Vec<u8> {
        self.file.lock().unwrap().clone()
    }
    fn since(&self, from: u64) -> Numbered {
        let w = self.writes.lock().unwrap();
        w.iter().filter(|(s, _)| *s > from).cloned().collect()
    }
}

fn stmt(sql: &str) -> Statement {
    fenec_ql::parse_one(sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn exec(db: &mut Database, sql: &str, params: &[Value]) {
    db.execute_with(&stmt(sql), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn rows(db: &Database, sql: &str, params: &[Value]) -> Rows {
    db.query(&stmt(sql), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .rows()
        .unwrap()
        .rows
        .iter()
        .map(|r| (r.id, r.values.clone()))
        .collect()
}

/// Runs `sqls` as one block.
fn block(db: &mut Database, sqls: &[&str]) -> std::result::Result<usize, (usize, Error)> {
    let parsed: Vec<Statement> = sqls.iter().map(|s| stmt(s)).collect();
    let stmts: Vec<(&Statement, &[Value])> = parsed.iter().map(|s| (s, &[][..])).collect();
    db.execute_block(&stmts).map(|out| out.len())
}

fn vector(seed: u64) -> String {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let v: Vec<String> = (0..8)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            format!("{:.3}", (x % 2000) as f32 / 1000.0 - 1.0)
        })
        .collect();
    format!("[{}]", v.join(", "))
}

/// Two collections, every kind of index, and 40 documents.
fn seeded(db: &mut Database) {
    exec(
        db,
        "create collection docs (title text @text, tag text @hash, n int @sorted, v vector<8> @hnsw(cosine))",
        &[],
    );
    exec(db, "create collection notes (k text @hash, body text)", &[]);
    for i in 0..40u64 {
        exec(
            db,
            &format!(
                r#"put docs {{title: "alpha {i}", tag: "t{}", n: {i}, v: {}}}"#,
                i % 4,
                vector(i)
            ),
            &[],
        );
    }
    exec(db, r#"put notes {k: "a", body: "one"}"#, &[]);
}

/// Everything a reader could ask, with one right answer each.
fn answers(db: &Database) -> Vec<Rows> {
    let probe = [Value::Vector(vec![
        0.5, -0.25, 0.1, 0.9, -0.7, 0.3, 0.0, 0.2,
    ])];
    [
        "get docs",
        "get notes",
        r#"get docs select id match title "alpha""#,
        r#"get docs select id where tag = "t1""#,
        "get docs select id, n where n >= 10 and n < 30 order n desc",
        "get docs select id near v $1 exact limit 7",
        "get docs select id near v $1 limit 7",
        r#"get notes select id where k = "a""#,
    ]
    .iter()
    .map(|sql| rows(db, sql, &probe))
    .collect()
}

#[test]
fn a_block_that_does_not_land_leaves_every_answer_as_it_was() {
    let tap = Tap::default();
    let mut db = tap.database();
    seeded(&mut db);
    let before = answers(&db);
    let seq = db.change_seq();

    let failed = block(
        &mut db,
        &[
            &format!(
                r#"put docs {{title: "beta", tag: "t9", n: 100, v: {}}}"#,
                vector(100)
            ),
            &format!(
                r#"set docs {{title: "gamma", n: -5, v: {}}} where tag = "t1""#,
                vector(7)
            ),
            "del docs where n < 10",
            r#"put notes {k: "b", body: "two"}"#,
            r#"set notes {k: "c"} where k = "a""#,
            // A read sees the writes before it...
            r#"get docs where tag = "t9""#,
            // ...and the block stops here, having changed nothing.
            r#"put docs {n: "not a number"}"#,
        ],
    );
    assert_eq!(failed.map_err(|(i, _)| i), Err(6));
    assert_eq!(answers(&db), before);
    assert_eq!(db.change_seq(), seq, "the counter moved");
    assert_eq!(db.changed_collections_since(seq), Some(vec![]));
    assert!(tap.since(seq).is_empty(), "a record reached the file");

    // The ids it handed out are handed out again.
    exec(
        &mut db,
        r#"put docs {title: "delta", tag: "t0", n: 1}"#,
        &[],
    );
    assert_eq!(
        rows(&db, r#"get docs select id where title = "delta""#, &[])[0].0,
        41
    );
}

#[test]
fn a_block_that_lands_is_one_record_numbered_as_its_last_write() {
    let tap = Tap::default();
    let mut db = tap.database();
    seeded(&mut db);

    // Across collections: a block record.
    let seq = db.change_seq();
    let landed = block(
        &mut db,
        &[
            r#"put docs {title: "beta", tag: "t9", n: 100}"#,
            r#"set docs {n: 200} where tag = "t9""#,
            r#"get docs where tag = "t9""#,
            r#"put notes {k: "b", body: "two"}"#,
            "del docs where n = 0",
        ],
    );
    assert_eq!(landed.map_err(|(i, e)| (i, e.to_string())), Ok(5));
    let written = tap.since(seq);
    assert_eq!(written.len(), 1, "one record");
    assert_eq!(written[0].0, seq + 4);
    assert_eq!(written[0].1[0], 9, "a block record");
    assert_eq!(writes_in(&written[0].1).unwrap(), 4);
    assert_eq!(db.change_seq(), seq + 4);
    assert_eq!(
        rows(&db, r#"get docs select n where tag = "t9""#, &[])[0].1,
        [Value::Int(200)]
    );

    // Into one collection: one data record of as many frames, the form a
    // version before blocks reads.
    let seq = db.change_seq();
    exec(
        &mut db,
        r#"put docs [{title: "x", n: 1}, {title: "y", n: 2}, {title: "z", n: 3}]"#,
        &[],
    );
    let written = tap.since(seq);
    assert_eq!(written.len(), 1);
    assert_eq!((written[0].0, written[0].1[0]), (seq + 3, 3));
    assert_eq!(writes_in(&written[0].1).unwrap(), 3);

    // A lone write is the record it always was.
    let seq = db.change_seq();
    exec(&mut db, r#"put notes {k: "z"}"#, &[]);
    let written = tap.since(seq);
    assert_eq!(written.len(), 1);
    assert_eq!((written[0].0, written[0].1[0]), (seq + 1, 3));

    // The file reads back to the same answers, at the same change.
    let mut back = Database::new();
    back.load(&tap.bytes()).unwrap();
    assert_eq!(back.change_seq(), db.change_seq());
    assert_eq!(answers(&back), answers(&db));
}

/// Refuses a document titled `bad`, as a scoped token's check refuses one
/// outside its rows.
struct Refuse;
impl Hook for Refuse {
    fn name(&self) -> &str {
        "refuse"
    }
    fn before_write(&self, _: &Schema, _: WriteOp, doc: &mut Document) -> Result<()> {
        match doc.get("title") {
            Some(Value::Text(t)) if t == "bad" => Err(Error::Denied("refused".into())),
            _ => Ok(()),
        }
    }
}
impl Plugin for Refuse {
    fn name(&self) -> &str {
        "refuse"
    }
    fn init(&self, reg: &mut Registry) -> Result<()> {
        reg.register_hook(Arc::new(Refuse));
        Ok(())
    }
}

#[test]
fn a_put_of_many_documents_stopped_half_way_lands_none_of_them() {
    let tap = Tap::default();
    let mut db = tap.database();
    seeded(&mut db);
    db.install_plugin(&Refuse).unwrap();
    let before = answers(&db);
    let seq = db.change_seq();
    let put = stmt(&format!(
        r#"put docs [{{title: "ok one", tag: "t1", v: {}}}, {{title: "ok two", n: 7}}, {{title: "bad"}}]"#,
        vector(3)
    ));
    assert!(db.execute_with(&put, &[]).is_err());
    assert_eq!(answers(&db), before);
    assert_eq!(db.change_seq(), seq);
    assert!(tap.since(seq).is_empty());
    // An update over many documents the same: every one or none.
    let set = stmt(r#"set docs {title: "bad"} where tag = "t2""#);
    assert!(db.execute_with(&set, &[]).is_err());
    assert_eq!(answers(&db), before);
}

#[test]
fn a_block_a_crash_cuts_short_is_cut_off_whole() {
    let tap = Tap::default();
    let mut db = tap.database();
    seeded(&mut db);
    let kept = answers(&db);
    let whole = tap.bytes().len();
    block(
        &mut db,
        &[
            r#"put docs {title: "beta", tag: "t9", n: 100}"#,
            r#"put notes {k: "b", body: "two"}"#,
            "del docs where n < 5",
        ],
    )
    .unwrap();
    let file = tap.bytes();
    assert!(file.len() > whole + 8);
    // Cut anywhere in the block's record, and none of it is there.
    for cut in [
        whole + 1,
        whole + 3,
        (whole + file.len()) / 2,
        file.len() - 1,
    ] {
        let mut back = Database::new();
        assert_eq!(back.load(&file[..cut]).unwrap(), whole, "cut at {cut}");
        assert_eq!(answers(&back), kept, "cut at {cut}");
    }
    let mut back = Database::new();
    back.load(&file).unwrap();
    assert_eq!(answers(&back), answers(&db));
}

#[test]
fn a_replica_applies_a_block_whole_and_numbers_it_as_its_primary() {
    let primary_file = Tap::default();
    let mut primary = primary_file.database();
    seeded(&mut primary);
    block(
        &mut primary,
        &[
            &format!(
                r#"put docs {{title: "beta", tag: "t9", n: 100, v: {}}}"#,
                vector(100)
            ),
            r#"put notes {k: "b", body: "two"}"#,
            r#"set docs {n: -1} where tag = "t3""#,
            "del notes where k = \"a\"",
        ],
    )
    .unwrap();
    exec(
        &mut primary,
        r#"put docs [{title: "x"}, {title: "y"}]"#,
        &[],
    );

    let replica_file = Tap::default();
    let mut replica = replica_file.database();
    replica.follow(vec![(7, 0)]).unwrap();
    let writes = primary_file.since(0);
    for (seq, record) in &writes {
        replica.apply(record).unwrap();
        assert_eq!(replica.change_seq(), *seq, "numbered as the primary did");
    }
    assert_eq!(replica.change_seq(), primary.change_seq());
    assert_eq!(answers(&replica), answers(&primary));
    // Its own file holds the same records, numbered the same: a replica
    // of the replica is fed the same stream.
    assert_eq!(replica_file.since(0), writes);
}

#[test]
fn begin_commit_and_rollback_hold_a_block_open_between_statements() {
    let tap = Tap::default();
    let mut db = tap.database();
    seeded(&mut db);
    let before = answers(&db);
    let seq = db.change_seq();

    db.begin().unwrap();
    assert!(db.begin().is_err(), "a block inside a block");
    exec(&mut db, r#"put notes {k: "b", body: "two"}"#, &[]);
    assert_eq!(
        rows(&db, r#"get notes where k = "b""#, &[]).len(),
        1,
        "a read in it sees it"
    );
    exec(&mut db, "del docs where n >= 20", &[]);
    // A compact is refused, and the block stays open.
    assert!(db.execute_with(&stmt("compact"), &[]).is_err());
    assert!(db.in_block());
    assert!(tap.since(seq).is_empty(), "nothing lands before the commit");
    db.rollback();
    assert!(!db.in_block());
    assert_eq!(answers(&db), before);
    assert_eq!(db.change_seq(), seq);

    db.begin().unwrap();
    exec(&mut db, r#"put notes {k: "b", body: "two"}"#, &[]);
    exec(&mut db, r#"put docs {title: "beta", n: 100}"#, &[]);
    db.commit().unwrap();
    assert_eq!(db.change_seq(), seq + 2);
    assert_eq!(tap.since(seq).len(), 1);
}

#[test]
fn a_compact_is_refused_in_a_block_of_more_than_one() {
    let mut db = Database::new();
    seeded(&mut db);
    let refused = block(&mut db, &[r#"put notes {k: "b"}"#, "compact"]);
    assert_eq!(refused.map_err(|(i, _)| i), Err(1));
    assert!(rows(&db, r#"get notes where k = "b""#, &[]).is_empty());
}

/// Each collection's schema as its record holds it, and what its store
/// holds, by name.
fn shape(db: &Database) -> Vec<(String, Vec<u8>, usize, usize, usize)> {
    db.collection_names()
        .into_iter()
        .map(|n| {
            let c = db.collection(&n).unwrap();
            let st = c.stats();
            (n, c.schema.encode(), st.documents, st.bytes, st.dead_bytes)
        })
        .collect()
}

/// The schema changes of the tests below: a collection made and written,
/// an index built and read through, a collection dropped and another made
/// under its name.
const CHANGES: [&str; 8] = [
    "create collection other (x int @hash)",
    "put other [{x: 1}, {x: 2}]",
    "create index on notes (body) @text",
    r#"put notes {k: "b", body: "two words"}"#,
    r#"get notes select id match body "two""#,
    "drop collection docs",
    "create collection docs (other text @hash)",
    r#"put docs {other: "x"}"#,
];

/// What a reader could ask once the changes landed.
fn changed_answers(db: &Database) -> Vec<Rows> {
    [
        "get other",
        "get notes",
        "get docs",
        "get other select id where x = 2",
        r#"get notes select id match body "two""#,
        r#"get docs select id where other = "x""#,
    ]
    .iter()
    .map(|sql| rows(db, sql, &[]))
    .collect()
}

#[test]
fn a_block_that_does_not_land_puts_its_schema_changes_back() {
    let tap = Tap::default();
    let mut db = tap.database();
    seeded(&mut db);
    let (before, shaped, seq) = (answers(&db), shape(&db), db.change_seq());

    let mut failing = CHANGES.to_vec();
    failing.push(r#"put notes {k: 1}"#);
    let failed = block(&mut db, &failing);
    assert_eq!(failed.map_err(|(i, _)| i), Err(8));
    assert_eq!(shape(&db), shaped);
    assert_eq!(answers(&db), before);
    assert_eq!(db.change_seq(), seq, "the counter moved");
    assert!(tap.since(seq).is_empty(), "a record reached the file");

    // Held open, a read in it sees them all; put back, none of them.
    db.begin().unwrap();
    for sql in CHANGES {
        exec(&mut db, sql, &[]);
    }
    assert_eq!(changed_answers(&db)[4].len(), 1);
    db.rollback();
    assert_eq!(shape(&db), shaped);
    assert_eq!(answers(&db), before);

    // The index is gone, and is built again; the collection is gone, and
    // is made again, under the id it had.
    exec(&mut db, "create index on notes (body) @text", &[]);
    exec(&mut db, "create collection other (x int)", &[]);
    let other = db.collection("other").unwrap().id;
    assert_eq!(other, db.collection("notes").unwrap().id + 1);
}

#[test]
fn a_block_of_schema_changes_lands_as_one_record() {
    let tap = Tap::default();
    let mut db = tap.database();
    seeded(&mut db);
    let seq = db.change_seq();
    assert_eq!(
        block(&mut db, &CHANGES).map_err(|(i, e)| (i, e.to_string())),
        Ok(8)
    );
    let written = tap.since(seq);
    assert_eq!(written.len(), 1, "one record");
    assert_eq!(written[0].1[0], 9, "a block record");
    // A schema change counts as a write, as it does on its own: four of
    // them, and four documents.
    assert_eq!(writes_in(&written[0].1).unwrap(), 8);
    assert_eq!(db.change_seq(), seq + 8);
    let landed = changed_answers(&db);
    assert_eq!(landed[3].len(), 1);
    assert_eq!(landed[4].len(), 1);
    assert_eq!(landed[5].len(), 1);

    // The file reads back to the same collections and answers...
    let mut back = Database::new();
    back.load(&tap.bytes()).unwrap();
    assert_eq!(back.change_seq(), db.change_seq());
    assert_eq!(changed_answers(&back), landed);
    assert_eq!(
        back.collection_names(),
        ["notes", "other", "docs"],
        "in the order they were made"
    );
    // ...and a replica applies them, numbered as its primary numbered them.
    let replica_file = Tap::default();
    let mut replica = replica_file.database();
    replica.follow(vec![(7, 0)]).unwrap();
    for (seq, record) in tap.since(0) {
        replica.apply(&record).unwrap();
        assert_eq!(replica.change_seq(), seq);
    }
    assert_eq!(changed_answers(&replica), landed);
    assert_eq!(replica_file.since(0), tap.since(0));

    // Cut anywhere in it, none of it is there.
    let file = tap.bytes();
    let whole = file.len() - written[0].1.len();
    for cut in [whole + 1, (whole + file.len()) / 2, file.len() - 1] {
        let mut back = Database::new();
        assert_eq!(back.load(&file[..cut]).unwrap(), whole, "cut at {cut}");
        assert_eq!(back.collection_names(), ["docs", "notes"], "cut at {cut}");
    }

    // A lone schema change is the record it always was.
    for (sql, kind) in [
        ("create collection lone (x int)", 1),
        ("create index on lone (x) @hash", 5),
        ("drop collection lone", 2),
    ] {
        let seq = db.change_seq();
        exec(&mut db, sql, &[]);
        let written = tap.since(seq);
        assert_eq!((written.len(), written[0].1[0]), (1, kind), "{sql}");
    }
}

#[test]
fn a_savepoint_puts_back_the_schema_changes_after_it() {
    let tap = Tap::default();
    let mut db = tap.database();
    seeded(&mut db);
    let (before, shaped) = (answers(&db), shape(&db));

    db.begin().unwrap();
    exec(&mut db, "create collection early (x int @sorted)", &[]);
    exec(&mut db, "put early [{x: 1}, {x: 5}]", &[]);
    exec(&mut db, r#"put notes {k: "z", body: "before"}"#, &[]);
    let at = (shape(&db), rows(&db, "get early where x > 2", &[]));
    let point = db.savepoint();
    exec(&mut db, "put early {x: 9}", &[]);
    exec(&mut db, "drop collection early", &[]);
    exec(&mut db, "create collection early (y text)", &[]);
    exec(&mut db, "create index on notes (body) @text", &[]);
    exec(&mut db, r#"put notes {k: "c", body: "after"}"#, &[]);
    db.rollback_to(&point).unwrap();
    assert_eq!((shape(&db), rows(&db, "get early where x > 2", &[])), at);

    // A collection written before a savepoint and dropped before it too
    // comes back whole with the rest, its store as it stood.
    exec(&mut db, "drop collection notes", &[]);
    let dropped = db.savepoint();
    exec(&mut db, "create collection notes (k int)", &[]);
    db.rollback_to(&dropped).unwrap();
    db.rollback();
    assert_eq!(shape(&db), shaped);
    assert_eq!(answers(&db), before);

    // Landed, the file reads back to what it held.
    db.begin().unwrap();
    exec(&mut db, "create collection early (x int @sorted)", &[]);
    exec(&mut db, "put early [{x: 1}, {x: 5}]", &[]);
    let point = db.savepoint();
    exec(&mut db, "drop collection early", &[]);
    db.rollback_to(&point).unwrap();
    db.commit().unwrap();
    let mut back = Database::new();
    back.load(&tap.bytes()).unwrap();
    assert_eq!(
        rows(&back, "get early where x > 2", &[]),
        rows(&db, "get early where x > 2", &[])
    );
    assert_eq!(back.collection_names(), db.collection_names());
}

#[test]
fn a_savepoint_puts_back_only_the_writes_after_it() {
    let tap = Tap::default();
    let mut db = tap.database();
    seeded(&mut db);
    let seq = db.change_seq();

    db.begin().unwrap();
    let start = db.savepoint();
    assert!(start.is_start());
    // Before it: a write into each collection, over documents the writes
    // after it write again.
    exec(&mut db, r#"put notes {k: "b", body: "two"}"#, &[]);
    exec(
        &mut db,
        &format!(
            r#"set docs {{title: "gamma", v: {}}} where tag = "t1""#,
            vector(7)
        ),
        &[],
    );
    let kept = answers(&db);
    let point = db.savepoint();
    assert!(!point.is_start());
    exec(
        &mut db,
        &format!(
            r#"put docs {{title: "beta", tag: "t9", n: 100, v: {}}}"#,
            vector(100)
        ),
        &[],
    );
    exec(
        &mut db,
        &format!(
            r#"set docs {{title: "delta", n: -5, v: {}}} where tag = "t1""#,
            vector(9)
        ),
        &[],
    );
    exec(&mut db, "del docs where n < 10", &[]);
    exec(&mut db, r#"set notes {body: "three"} where k = "b""#, &[]);
    exec(&mut db, r#"del notes where k = "a""#, &[]);
    let later = db.savepoint();
    exec(&mut db, r#"put notes {k: "c"}"#, &[]);
    // A statement stopped half way leaves what it wrote in the block, for
    // a savepoint before it to put back.
    db.install_plugin(&Refuse).unwrap();
    let half = stmt(&format!(
        r#"put docs [{{title: "ok", tag: "t1", v: {}}}, {{title: "bad"}}]"#,
        vector(5)
    ));
    assert!(db.execute_with(&half, &[]).is_err());
    assert_eq!(
        rows(&db, r#"get docs select id where title = "ok""#, &[]).len(),
        1
    );
    db.rollback_to(&point).unwrap();
    assert!(db.in_block());
    assert_eq!(answers(&db), kept);
    // A savepoint after it is over: its writes were put back.
    assert!(db.rollback_to(&later).is_err());

    // Taken back to as often as asked; the ids handed out after it are
    // handed out again.
    exec(&mut db, r#"put docs {title: "epsilon"}"#, &[]);
    assert_eq!(
        rows(&db, r#"get docs select id where title = "epsilon""#, &[])[0].0,
        41
    );
    db.rollback_to(&point).unwrap();
    assert_eq!(answers(&db), kept);
    exec(&mut db, r#"put docs {title: "zeta"}"#, &[]);
    assert_eq!(
        rows(&db, r#"get docs select id where title = "zeta""#, &[])[0].0,
        41
    );
    db.rollback_to(&point).unwrap();

    // It lands with what came before the savepoint: one record, numbered
    // as its last write, which the file reads back.
    assert!(tap.since(seq).is_empty());
    db.commit().unwrap();
    let written = tap.since(seq);
    assert_eq!(written.len(), 1);
    // The note, and the ten documents tagged `t1`.
    assert_eq!(writes_in(&written[0].1).unwrap(), 11);
    assert_eq!(db.change_seq(), seq + 11);
    assert_eq!(answers(&db), kept);
    let mut back = Database::new();
    back.load(&tap.bytes()).unwrap();
    assert_eq!(answers(&back), kept);

    // Its block is over, and so is it.
    assert!(db.rollback_to(&point).is_err());
}

#[test]
fn a_savepoint_at_the_start_puts_back_every_write_and_keeps_the_block() {
    let tap = Tap::default();
    let mut db = tap.database();
    seeded(&mut db);
    let before = answers(&db);
    let seq = db.change_seq();

    // Taken before the block, as a transaction takes one before its first
    // write: the start of whichever block is open.
    let start = db.savepoint();
    db.rollback_to(&start).unwrap();
    db.begin().unwrap();
    exec(&mut db, r#"put notes {k: "b"}"#, &[]);
    exec(&mut db, "del docs where n >= 20", &[]);
    db.rollback_to(&start).unwrap();
    assert!(db.in_block());
    assert_eq!(answers(&db), before);
    db.commit().unwrap();
    assert_eq!(db.change_seq(), seq);
    assert!(tap.since(seq).is_empty());

    // A savepoint of another block is refused.
    db.begin().unwrap();
    exec(&mut db, r#"put notes {k: "b"}"#, &[]);
    let other = db.savepoint();
    db.rollback();
    db.begin().unwrap();
    exec(&mut db, r#"put notes {k: "c"}"#, &[]);
    exec(&mut db, r#"put notes {k: "d"}"#, &[]);
    assert!(db.rollback_to(&other).is_err());
    assert_eq!(
        rows(&db, r#"get notes select id where k = "d""#, &[]).len(),
        1
    );
    db.rollback();
}
