//! A replica applies the records its primary appended, numbered as the
//! primary numbered them, and has to end up answering every query the way
//! the primary does -- through its indexes as well as its documents -- and
//! reopening from its own file at the same change.

use fenec_core::prelude::*;
use std::sync::{Arc, Mutex};

/// Writes with their numbers, as a sink is handed them.
type Numbered = Vec<(u64, Vec<u8>)>;

/// Rows by id, as a query answers them.
type Rows = Vec<(u64, Vec<Value>)>;

/// A file in memory that also keeps the writes it was handed, with their
/// numbers: what a primary's feed would pass on.
#[derive(Clone)]
struct Tap {
    file: Arc<Mutex<Vec<u8>>>,
    writes: Arc<Mutex<Numbered>>,
}

impl Default for Tap {
    /// A new file holds its signature, as `FileSink::open` leaves one.
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
        let mut db = Database::with_sink(Box::new(self.clone()));
        let file = self.file.lock().unwrap().clone();
        if file.len() > 8 {
            db.load(&file).expect("load");
        }
        db
    }
    fn reopen(&self) -> Database {
        let mut db = Database::new();
        db.load(&self.file.lock().unwrap()).expect("reopen");
        db
    }
    /// The records numbered `from + 1` on, one after another.
    fn since(&self, from: u64) -> Numbered {
        let w = self.writes.lock().unwrap();
        w.iter().filter(|(s, _)| *s > from).cloned().collect()
    }
}

fn exec(db: &mut Database, sql: &str, params: &[Value]) {
    db.execute_with(&fenec_ql::parse_one(sql).expect("parse"), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn rows(db: &Database, sql: &str, params: &[Value]) -> Rows {
    let r = db
        .query(&fenec_ql::parse_one(sql).expect("parse"), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    r.rows()
        .unwrap()
        .rows
        .iter()
        .map(|r| (r.id, r.values.clone()))
        .collect()
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn vector(&mut self, dim: usize) -> Value {
        Value::Vector(
            (0..dim)
                .map(|_| (self.below(20_000) as f32) / 10_000.0 - 1.0)
                .collect(),
        )
    }
}

const DIM: usize = 8;

/// Everything a reader could ask for with one right answer: the documents,
/// and what the text, hash and ordered indexes and an exact `near` answer.
/// The graph gets [`graph_holds_the_live_vectors`] instead: it is built from
/// the same vectors but not in the same batches, and an approximate search
/// over a different graph may rank a close call the other way.
fn answers(db: &Database, probe: &Value) -> Vec<(String, Rows)> {
    let mut out = Vec::new();
    for name in db.collection_names() {
        out.push((name.clone(), rows(db, &format!("get {name}"), &[])));
    }
    if db.collection_names().iter().any(|n| n == "docs") {
        for sql in [
            r#"get docs select id match title "alpha""#,
            r#"get docs select id where tag = "t3""#,
            "get docs select id, n where n >= 20 and n < 60 order n desc limit 7",
            "get docs select id, score where score >= 10.0 order score limit 9",
            "get docs select id near v $1 exact limit 5",
        ] {
            out.push((sql.to_string(), rows(db, sql, std::slice::from_ref(probe))));
        }
    }
    out
}

/// The graph holds a node for every document with a vector and none for a
/// document that is gone, and at this size its search finds what an exact
/// one does.
fn graph_holds_the_live_vectors(db: &Database, probe: &Value) {
    let stats = db.stats();
    let docs = stats.iter().find(|c| c.name == "docs").unwrap();
    let with_vector = rows(db, "get docs select v", &[])
        .iter()
        .filter(|(_, v)| matches!(v[0], Value::Vector(_)))
        .count();
    assert_eq!(docs.vector_indexes[0].count, with_vector);
    let exact = rows(
        db,
        "get docs select id near v $1 exact limit 5",
        std::slice::from_ref(probe),
    );
    let ann = rows(
        db,
        "get docs select id near v $1 limit 5",
        std::slice::from_ref(probe),
    );
    assert_eq!(ann, exact);
}

/// A primary's day: collections made and dropped, an index added under
/// load, documents written, rewritten and deleted -- some of them twice in
/// one statement.
fn workload(db: &mut Database, rng: &mut Rng, rounds: usize) {
    exec(
        db,
        &format!(
            "create collection docs (title text @text, tag text @hash, n int @sorted, \
             score float, v vector<{DIM}> @hnsw(cosine, m=8, ef_construction=64))"
        ),
        &[],
    );
    exec(db, "create collection notes (body text)", &[]);
    for round in 0..rounds {
        for _ in 0..20 {
            let word = ["alpha", "beta", "gamma"][rng.below(3) as usize];
            exec(
                db,
                "put docs {title: $1, tag: $2, n: $3, score: $4, v: $5}",
                &[
                    Value::Text(format!("{word} {}", rng.below(100))),
                    Value::Text(format!("t{}", rng.below(5))),
                    Value::Int(rng.below(100) as i64),
                    Value::Float(rng.below(1000) as f64 / 10.0),
                    rng.vector(DIM),
                ],
            );
        }
        // One statement that writes the same document twice, and one that
        // puts a document back after its delete.
        let id = 1 + rng.below(20 * (round as u64 + 1));
        exec(
            db,
            &format!("put docs [{{id: {id}, n: 1, v: $1}}, {{id: {id}, n: 2, v: $2}}]"),
            &[rng.vector(DIM), rng.vector(DIM)],
        );
        exec(
            db,
            &format!(
                "set docs {{n: {}, title: \"gamma set\"}} where n < {}",
                rng.below(100),
                rng.below(30)
            ),
            &[],
        );
        exec(db, &format!("del docs where n = {}", rng.below(100)), &[]);
        exec(
            db,
            &format!(
                "put docs {{id: {}, title: \"alpha back\", v: $1}}",
                1 + rng.below(20)
            ),
            &[rng.vector(DIM)],
        );
        exec(db, "put notes {body: \"x\"}", &[]);
        if round == 1 {
            exec(db, "create index on docs (score) @sorted", &[]);
            exec(db, "drop collection notes", &[]);
            exec(
                db,
                "create collection notes (body text @text, k int @hash)",
                &[],
            );
        }
    }
}

#[test]
fn a_replica_applying_the_writes_answers_as_its_primary_does() {
    let primary_file = Tap::default();
    let mut primary = primary_file.database();
    let mut rng = Rng(0x9e3779b97f4a7c15);
    workload(&mut primary, &mut rng, 4);

    // The replica is fed in pieces of every size, and checked after each.
    let replica_file = Tap::default();
    let mut replica = replica_file.database();
    replica.follow(vec![(7, 0)]).unwrap();
    let writes = primary_file.since(0);
    assert_eq!(writes.len() as u64, primary.change_seq());
    let mut at = 0;
    let probe = rng.vector(DIM);
    while at < writes.len() {
        let n = (1 + rng.below(40) as usize).min(writes.len() - at);
        let mut records = Vec::new();
        for (k, (seq, bytes)) in writes[at..at + n].iter().enumerate() {
            // The primary numbered them on from where the replica stands.
            assert_eq!(*seq, replica.change_seq() + 1 + k as u64);
            records.extend_from_slice(bytes);
        }
        assert_eq!(replica.apply(&records).unwrap(), n);
        at += n;
        assert_eq!(replica.change_seq(), writes[at - 1].0);
    }
    assert_eq!(replica.change_seq(), primary.change_seq());
    assert_eq!(answers(&replica, &probe), answers(&primary, &probe));
    graph_holds_the_live_vectors(&replica, &probe);

    // The replica numbered each record as the primary did, so a replica of
    // the replica is fed the same stream.
    let relayed: Numbered = replica_file.since(0);
    assert_eq!(relayed, writes);

    // Its file reopens at the same change, still following.
    let reopened = replica_file.reopen();
    assert_eq!(reopened.change_seq(), primary.change_seq());
    assert!(reopened.history().following);
    assert_eq!(answers(&reopened, &probe), answers(&primary, &probe));
}

#[test]
fn a_replica_catches_up_from_an_image_then_the_writes_after_it() {
    let primary_file = Tap::default();
    let mut primary = primary_file.database();
    let mut rng = Rng(42);
    workload(&mut primary, &mut rng, 2);
    exec(&mut primary, "compact", &[]);
    let image = primary.snapshot();
    let at = primary.change_seq();
    workload_more(&mut primary, &mut rng);

    // A replica that held something else entirely.
    let replica_file = Tap::default();
    let mut replica = replica_file.database();
    exec(&mut replica, "create collection stale (x int)", &[]);
    exec(&mut replica, "put stale {x: 1}", &[]);
    let mut fresh = Database::new();
    fresh.load(&image).unwrap();
    replica.adopt(fresh, &image).unwrap();
    replica.follow(vec![(9, 0)]).unwrap();
    assert_eq!(replica.change_seq(), at);
    let mut records = Vec::new();
    for (_, bytes) in primary_file.since(at) {
        records.extend_from_slice(&bytes);
    }
    replica.apply(&records).unwrap();

    let probe = rng.vector(DIM);
    assert_eq!(answers(&replica, &probe), answers(&primary, &probe));
    graph_holds_the_live_vectors(&replica, &probe);
    // The file is the image and the records after it.
    let reopened = replica_file.reopen();
    assert_eq!(reopened.change_seq(), primary.change_seq());
    assert_eq!(answers(&reopened, &probe), answers(&primary, &probe));
    assert!(reopened.collection("stale").is_err());
}

fn workload_more(db: &mut Database, rng: &mut Rng) {
    for _ in 0..30 {
        exec(
            db,
            "put docs {title: \"alpha late\", tag: \"t3\", n: $1, v: $2}",
            &[Value::Int(rng.below(100) as i64), rng.vector(DIM)],
        );
    }
    exec(db, "del docs where n < 10", &[]);
    exec(db, "put notes {body: \"late\", k: 3}", &[]);
}

#[test]
fn a_following_database_takes_no_write_of_its_own() {
    let file = Tap::default();
    let mut db = file.database();
    exec(&mut db, "create collection c (x int)", &[]);
    exec(&mut db, "put c {x: 1}", &[]);
    db.follow(vec![(5, 0)]).unwrap();

    let put = fenec_ql::parse_one("put c {x: 2}").unwrap();
    match db.execute(&put) {
        Err(Error::ReadOnly(_)) => {}
        other => panic!("a replica took a write: {other:?}"),
    }
    let drop = fenec_ql::parse_one("drop collection c").unwrap();
    assert!(matches!(db.execute(&drop), Err(Error::ReadOnly(_))));
    // Reads go on, and so does `compact`: it changes no document.
    assert_eq!(rows(&db, "get c", &[]).len(), 1);
    exec(&mut db, "compact", &[]);

    // Promoted, it takes writes again, on a history of its own.
    let seq = db.change_seq();
    db.fork(11).unwrap();
    exec(&mut db, "put c {x: 2}", &[]);
    assert_eq!(db.history().lineage, vec![(5, 0), (11, seq)]);
    assert!(!db.history().following);

    // The history is not a write: it moved no counter, and a checkpoint's
    // image keeps it as the tail after the image does.
    assert_eq!(db.change_seq(), seq + 1);
    assert_eq!(file.reopen().history(), db.history());
    db.checkpoint().unwrap();
    assert_eq!(file.reopen().history(), db.history());
    assert_eq!(file.reopen().change_seq(), seq + 1);
}

#[test]
fn a_write_that_is_not_there_to_apply_is_refused() {
    let primary_file = Tap::default();
    let mut primary = primary_file.database();
    exec(&mut primary, "create collection c (x int @hash)", &[]);
    exec(&mut primary, "put c [{x: 1}, {x: 2}]", &[]);
    let w = primary_file.since(0);

    // A write to a collection the replica never saw.
    let mut replica = Database::new();
    match replica.apply(&w[1].1) {
        Err(Error::Corrupt(_)) => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(replica.change_seq(), 0);

    // A batch cut inside a record: what came before it stays applied.
    let mut records = w[0].1.clone();
    records.extend_from_slice(&w[1].1);
    records.extend_from_slice(&w[2].1[..w[2].1.len() - 1]);
    assert!(replica.apply(&records).is_err());
    assert_eq!(replica.change_seq(), 2);
    assert_eq!(rows(&replica, "get c where x = 1", &[]).len(), 1);
    assert_eq!(rows(&replica, "get c where x = 2", &[]).len(), 0);
    replica.apply(&w[2].1).unwrap();
    assert_eq!(rows(&replica, "get c where x = 2", &[]).len(), 1);

    // A collection made twice.
    assert!(matches!(replica.apply(&w[0].1), Err(Error::Corrupt(_))));
}
