//! A crash leaves a checkpoint's image and a tail of writes after it.
//! Reopening that has to restore the graph the image holds and apply the
//! tail to it: rebuilding the graph is what a checkpoint exists to avoid,
//! and at 100 000 x 768 a rebuild kept a restarted server's port closed for
//! about 54 s.
//!
//! A restored graph shows itself in its arena. It keeps the tombstones of
//! the writes it took, as the live index does; a rebuilt one holds live
//! vectors only.

use fenec_core::prelude::*;
use std::sync::{Arc, Mutex};

/// A file in memory: appends go to its end, a rewrite replaces it.
#[derive(Clone, Default)]
struct File(Arc<Mutex<Vec<u8>>>);

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

fn exec(db: &mut Database, sql: &str, params: &[Value]) {
    db.execute_with(&fenec_ql::parse_one(sql).expect("parse"), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn ids(db: &Database, sql: &str, params: &[Value]) -> Vec<(u64, f32)> {
    let r = db
        .query(&fenec_ql::parse_one(sql).expect("parse"), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    r.rows()
        .unwrap()
        .rows
        .iter()
        .map(|r| (r.id, r.score.unwrap_or(0.0)))
        .collect()
}

struct Rng(u64);
impl Rng {
    fn vector(&mut self, dim: usize) -> Vec<f32> {
        (0..dim)
            .map(|_| {
                self.0 ^= self.0 << 13;
                self.0 ^= self.0 >> 7;
                self.0 ^= self.0 << 17;
                (self.0 % 20_000) as f32 / 10_000.0 - 1.0
            })
            .collect()
    }
}

fn arena(db: &Database, collection: &str) -> (usize, VecPrec) {
    let stats = db.stats();
    let c = stats
        .iter()
        .find(|c| c.name == collection)
        .expect("collection");
    let v = &c.vector_indexes[0];
    (v.arena_bytes, v.precision)
}

fn reopen(file: &File) -> Database {
    let mut db = Database::new();
    db.load(&file.0.lock().unwrap()).expect("load");
    db
}

const DIM: usize = 16;

/// What `crashed` leaves: the database as it ran, the file as it stands,
/// and the tail's updates (id, old vector, new) and deletes (id, vector).
struct Crash {
    live: Database,
    file: File,
    rng: Rng,
    updated: Vec<(u64, Vec<f32>, Vec<f32>)>,
    deleted: Vec<(u64, Vec<f32>)>,
}

/// Writes before the checkpoint that leave tombstones and documents without
/// a vector in the image, then a tail of every kind of write.
fn crashed(ty: &str) -> Crash {
    crashed_over(ty, "none")
}

/// [`crashed`], the index over `quant` codes.
fn crashed_over(ty: &str, quant: &str) -> Crash {
    let file = File::default();
    let mut db = Database::with_sink(Box::new(file.clone()));
    exec(
        &mut db,
        &format!(
            "create collection d (tag text, e {ty} @hnsw(cosine, m=8, ef_construction=64, quant={quant}))"
        ),
        &[],
    );
    let mut r = Rng(0x2545_F491_4F6C_DD1D);
    let mut vectors = vec![Vec::new()];
    for i in 1..=2000u64 {
        let v = r.vector(DIM);
        if i % 97 == 0 {
            exec(&mut db, "put d {tag: \"no vector\"}", &[]);
        } else {
            exec(
                &mut db,
                "put d {tag: \"x\", e: $1}",
                &[Value::Vector(v.clone())],
            );
        }
        vectors.push(v);
    }
    // Before the checkpoint: a deleted document and an updated one, whose
    // tombstones the image carries.
    exec(&mut db, "del d where id = 5", &[]);
    let v = r.vector(DIM);
    exec(
        &mut db,
        "set d {e: $1} where id = 6",
        &[Value::Vector(v.clone())],
    );
    vectors[6] = v;
    db.checkpoint().expect("checkpoint");

    // The tail: new documents, updated vectors, deletes, a document without
    // a vector. Each document is touched once, so the live index and the
    // restored one hold the same nodes.
    let mut updated = Vec::new();
    for id in (100..=2000u64).step_by(95).filter(|i| i % 97 != 0) {
        let new = r.vector(DIM);
        exec(
            &mut db,
            "set d {e: $1} where id = $2",
            &[Value::Vector(new.clone()), Value::Int(id as i64)],
        );
        updated.push((id, vectors[id as usize].clone(), new));
    }
    let mut deleted = Vec::new();
    for id in (150..=2000u64)
        .step_by(190)
        .filter(|i| i % 97 != 0 && i % 95 != 5)
    {
        exec(&mut db, "del d where id = $1", &[Value::Int(id as i64)]);
        deleted.push((id, vectors[id as usize].clone()));
    }
    for _ in 0..100 {
        exec(
            &mut db,
            "put d {tag: \"late\", e: $1}",
            &[Value::Vector(r.vector(DIM))],
        );
    }
    exec(&mut db, "put d {tag: \"late, no vector\"}", &[]);
    Crash {
        live: db,
        file,
        rng: r,
        updated,
        deleted,
    }
}

#[test]
fn a_crash_after_a_checkpoint_keeps_the_graph() {
    for ty in [format!("vector<{DIM}>"), format!("vector<{DIM}, f16>")] {
        let Crash {
            live,
            file,
            rng: mut r,
            updated,
            deleted,
        } = crashed(&ty);
        let back = reopen(&file);

        // Restored, not rebuilt: the same nodes as the live index, and the
        // field's own precision.
        assert_eq!(arena(&back, "d"), arena(&live, "d"), "{ty}");
        let docs = |db: &Database| ids(db, "get d select id", &[]).len();
        assert_eq!(docs(&back), docs(&live));

        // An updated document is found at its new vector and not at its old
        // one; a deleted one is not found at all.
        for (id, old, new) in &updated {
            let hit = ids(
                &back,
                "get d select id near e $1 limit 1",
                &[Value::Vector(new.clone())],
            );
            assert_eq!(hit[0].0, *id, "{ty}: document {id} at its new vector");
            let near_old = ids(
                &back,
                "get d select id near e $1 limit 5",
                &[Value::Vector(old.clone())],
            );
            assert!(
                near_old.iter().all(|(d, s)| d != id || *s < 0.999),
                "{ty}: {id} at its old vector"
            );
        }
        for (id, v) in &deleted {
            let hit = ids(
                &back,
                "get d select id near e $1 limit 10",
                &[Value::Vector(v.clone())],
            );
            assert!(
                hit.iter().all(|(d, _)| d != id),
                "{ty}: deleted {id} came back"
            );
        }

        // The ANN over the restored graph finds what an exact scan finds.
        let mut found = 0;
        for _ in 0..20 {
            let q = Value::Vector(r.vector(DIM));
            let ann = ids(
                &back,
                "get d select id near e $1 limit 10",
                std::slice::from_ref(&q),
            );
            let exact = ids(
                &back,
                "get d select id near e $1 exact limit 10",
                std::slice::from_ref(&q),
            );
            found += ann
                .iter()
                .filter(|a| exact.iter().any(|e| e.0 == a.0))
                .count();
        }
        assert!(found >= 190, "{ty}: recall {found}/200");
    }
}

/// What a server opens its file with (`fs::open_serving`): the graph the
/// checkpoint holds, and the tail's vectors left out of it.
fn reopen_serving(file: &File) -> Database {
    let bytes = file.0.lock().unwrap().clone();
    let mut db = Database::with_sink(Box::new(file.clone()));
    db.defer_linking();
    db.load(&bytes).expect("load");
    db
}

fn explain(db: &Database, sql: &str, params: &[Value]) -> Vec<String> {
    let r = db
        .query(
            &fenec_ql::parse_one(&format!("explain {sql}")).expect("parse"),
            params,
        )
        .unwrap_or_else(|e| panic!("explain {sql}: {e}"));
    let rs = r.rows().expect("rows");
    rs.rows
        .iter()
        .map(|r| match &r.values[..] {
            [Value::Text(s)] => s.clone(),
            other => panic!("a plan row holds {other:?}"),
        })
        .collect()
}

/// The answers a crashed file has to give: a document the tail rewrote at
/// its new vector, one it deleted nowhere, and the walk what the exact scan
/// finds.
fn answers_the_tail(db: &Database, crash: &Crash, what: &str) {
    for (id, _, new) in &crash.updated {
        let hit = ids(
            db,
            "get d select id near e $1 limit 1",
            &[Value::Vector(new.clone())],
        );
        assert_eq!(hit[0].0, *id, "{what}: document {id} at its new vector");
    }
    for (id, v) in &crash.deleted {
        let hit = ids(
            db,
            "get d select id near e $1 limit 10",
            &[Value::Vector(v.clone())],
        );
        assert!(
            hit.iter().all(|(d, _)| d != id),
            "{what}: deleted {id} came back"
        );
    }
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    let mut found = 0;
    for _ in 0..20 {
        let q = Value::Vector(r.vector(DIM));
        let ann = ids(
            db,
            "get d select id near e $1 limit 10",
            std::slice::from_ref(&q),
        );
        let exact = ids(
            db,
            "get d select id near e $1 exact limit 10",
            std::slice::from_ref(&q),
        );
        found += ann
            .iter()
            .filter(|a| exact.iter().any(|e| e.0 == a.0))
            .count();
    }
    assert!(found >= 190, "{what}: recall {found}/200");
}

/// A server opens a crashed file without linking the tail's vectors, and
/// links them beside the queries: at 100 000 x 768 linking them first kept
/// the port closed for as long as they took. Until they are linked `near`
/// measures each, so the answers are the ones a linked graph gives; a
/// checkpoint in the meantime keeps them waiting, and an open that does not
/// defer links them there.
#[test]
fn a_server_links_the_tail_after_the_open() {
    for quant in ["none", "int8"] {
        let what = format!("quant={quant}");
        let crash = crashed_over(&format!("vector<{DIM}>"), quant);
        let mut back = reopen_serving(&crash.file);
        // Every vector the tail wrote waits: 100 new, the rewritten ones.
        let waiting = back.unlinked();
        assert_eq!(waiting, 100 + crash.updated.len(), "{what}");
        assert_eq!(arena(&back, "d"), arena(&crash.live, "d"), "{what}");
        answers_the_tail(&back, &crash, &what);
        let q = [Value::Vector(vec![0.5; DIM])];
        let steps = explain(&back, "get d select id near e $1 limit 10", &q);
        let note =
            format!("near: {waiting} vectors of e not linked into the graph yet, each measured");
        assert!(steps.contains(&note), "{what}: {steps:?}");

        back.checkpoint().expect("checkpoint");
        assert_eq!(reopen_serving(&crash.file).unlinked(), waiting, "{what}");
        let plain = reopen(&crash.file);
        assert_eq!(plain.unlinked(), 0, "{what}");
        answers_the_tail(&plain, &crash, &what);

        let mut slices = 0;
        while back.link_pending(7) > 0 {
            slices += 1;
        }
        assert_eq!(slices, waiting.div_ceil(7) - 1, "{what}");
        answers_the_tail(&back, &crash, &what);
        assert!(!explain(&back, "get d select id near e $1 limit 10", &q)
            .iter()
            .any(|s| s.contains("not linked")));
    }
}

/// A file with no graph in it -- never checkpointed -- opens with every
/// vector waiting, and `near` is the exact scan until they are linked.
#[test]
fn a_file_never_checkpointed_opens_with_every_vector_waiting() {
    // What `fs::open` writes into a new file, and nothing written over it.
    let file = File(Arc::new(Mutex::new(fenec_core::engine::MAGIC.to_vec())));
    let mut db = Database::with_sink(Box::new(file.clone()));
    exec(
        &mut db,
        "create collection d (e vector<8> @hnsw(cosine, m=8, ef_construction=64))",
        &[],
    );
    let mut r = Rng(21);
    for _ in 0..1500 {
        exec(&mut db, "put d {e: $1}", &[Value::Vector(r.vector(8))]);
    }
    let mut back = reopen_serving(&file);
    assert_eq!(back.unlinked(), 1500);
    for _ in 0..10 {
        let q = [Value::Vector(r.vector(8))];
        assert_eq!(
            ids(&back, "get d select id near e $1 limit 10", &q),
            ids(&back, "get d select id near e $1 exact limit 10", &q)
        );
    }
    while back.link_pending(100) > 0 {}
    let mut found = 0;
    for _ in 0..20 {
        let q = [Value::Vector(r.vector(8))];
        let ann = ids(&back, "get d select id near e $1 limit 10", &q);
        let exact = ids(&back, "get d select id near e $1 exact limit 10", &q);
        found += ann
            .iter()
            .filter(|a| exact.iter().any(|e| e.0 == a.0))
            .count();
    }
    assert!(found >= 190, "recall {found}/200");
}

/// A checkpoint with nothing after it restores as well, tombstones and
/// documents without a vector included -- before, either one rebuilt the
/// graph on every open.
#[test]
fn tombstones_and_missing_vectors_do_not_cost_a_rebuild() {
    let file = File::default();
    let mut db = Database::with_sink(Box::new(file.clone()));
    exec(
        &mut db,
        "create collection d (e vector<4> @hnsw(cosine, m=8))",
        &[],
    );
    let mut r = Rng(9);
    for _ in 0..300 {
        exec(&mut db, "put d {e: $1}", &[Value::Vector(r.vector(4))]);
    }
    exec(&mut db, "put d {}", &[]);
    exec(&mut db, "del d where id = 10", &[]);
    exec(
        &mut db,
        "set d {e: $1} where id = 11",
        &[Value::Vector(r.vector(4))],
    );
    db.checkpoint().unwrap();
    let back = reopen(&file);
    // 300 vectors written, one replaced: 301 nodes, tombstones included.
    assert_eq!(arena(&back, "d"), (301 * 4 * 4, VecPrec::F32));
    assert_eq!(arena(&back, "d"), arena(&db, "d"));
}

/// An index added in the tail leaves the graph the checkpoint holds, as it
/// leaves the live one: the documents are the same. Reset with the rest,
/// every `create index` had the next open build the collection's graphs
/// again.
#[test]
fn an_index_added_in_the_tail_keeps_the_graph() {
    let file = File::default();
    let mut db = Database::with_sink(Box::new(file.clone()));
    exec(
        &mut db,
        "create collection d (k int, e vector<4> @hnsw(cosine, m=8))",
        &[],
    );
    let mut r = Rng(3);
    for k in 0..200 {
        exec(
            &mut db,
            "put d {k: $1, e: $2}",
            &[Value::Int(k), Value::Vector(r.vector(4))],
        );
    }
    // A tombstone, which a restored graph carries and a rebuilt one has not.
    exec(
        &mut db,
        "set d {e: $1} where id = 3",
        &[Value::Vector(r.vector(4))],
    );
    db.checkpoint().unwrap();
    exec(&mut db, "create index on d (k) @hash", &[]);
    exec(&mut db, "put d {k: 7, e: [1, 0, 0, 0]}", &[]);
    let back = reopen(&file);
    let hit = ids(
        &back,
        "get d select id where k = 7 near e $1 limit 2",
        &[Value::Vector(vec![1.0, 0.0, 0.0, 0.0])],
    );
    assert_eq!(hit[0].0, 201);
    assert_eq!(arena(&back, "d").0, 202 * 4 * 4);
    assert_eq!(arena(&back, "d"), arena(&db, "d"));
}

/// A new file, as `fs::open` makes one, and a database writing into it
/// that appends a graph once `changes` nodes changed and the file grew by
/// `growth` times the record.
fn saving(changes: u64, growth: u64) -> (File, Database) {
    let file = File(Arc::new(Mutex::new(fenec_core::engine::MAGIC.to_vec())));
    let mut db = Database::with_sink(Box::new(file.clone()));
    db.set_graph_saves(changes, growth);
    exec(
        &mut db,
        &format!("create collection d (e vector<{DIM}> @hnsw(cosine, m=8, ef_construction=64))"),
        &[],
    );
    (file, db)
}

fn put(db: &mut Database, r: &mut Rng, n: usize) -> Vec<Vec<f32>> {
    (0..n)
        .map(|_| {
            let v = r.vector(DIM);
            exec(db, "put d {e: $1}", &[Value::Vector(v.clone())]);
            v
        })
        .collect()
}

/// A server appends its graphs to the tail now and then (`save_graphs`),
/// since it checkpoints only on its way down: a crash then opens with the
/// graph as the last record has it, and only what was written after it
/// waits to be linked -- where a file never checkpointed had every vector
/// wait. The record is not a write, and moves no counter.
#[test]
fn a_graph_saved_in_the_tail_is_what_a_crash_opens_with() {
    let (file, mut db) = saving(500, 1);
    let mut r = Rng(33);
    let mut vectors = vec![Vec::new()];
    vectors.extend(put(&mut db, &mut r, 1500));
    let seq = db.change_seq();
    assert!(db.graphs_due());
    assert_eq!(db.save_graphs().unwrap().0, 1);
    assert_eq!(db.change_seq(), seq);
    assert!(!db.graphs_due());
    assert_eq!(db.save_graphs().unwrap().0, 0);

    // After the record: new documents, rewritten vectors, deletes.
    put(&mut db, &mut r, 200);
    let mut updated = Vec::new();
    for id in (10..=1500u64).step_by(97) {
        let new = r.vector(DIM);
        exec(
            &mut db,
            "set d {e: $1} where id = $2",
            &[Value::Vector(new.clone()), Value::Int(id as i64)],
        );
        updated.push((id, vectors[id as usize].clone(), new));
    }
    let mut deleted = Vec::new();
    for id in (40..=1500u64).step_by(151) {
        exec(&mut db, "del d where id = $1", &[Value::Int(id as i64)]);
        deleted.push((id, vectors[id as usize].clone()));
    }
    let crash = Crash {
        live: db,
        file,
        rng: r,
        updated,
        deleted,
    };
    let mut back = reopen_serving(&crash.file);
    assert_eq!(back.unlinked(), 200 + crash.updated.len());
    answers_the_tail(&back, &crash, "saved in the tail, waiting");
    answers_the_tail(&reopen(&crash.file), &crash, "saved in the tail, linked");

    // Nothing is saved while vectors wait to be linked: the record would
    // have them wait again after the next crash.
    back.set_graph_saves(100, 0);
    assert!(!back.graphs_due());
    while back.link_pending(64) > 0 {}
    assert!(back.graphs_due());
    assert_eq!(back.save_graphs().unwrap().0, 1);
    assert_eq!(reopen_serving(&crash.file).unlinked(), 0);
    answers_the_tail(
        &reopen_serving(&crash.file),
        &crash,
        "saved after the linking",
    );
}

/// A graph is appended once the file grew since its last record by the
/// record's size times the growth asked for, not before: the record holds
/// the whole graph. Only the last of its records is restored -- an earlier
/// one would read every vector again for nothing -- and one a crash cut
/// short is cut off the file, the one before it restored.
#[test]
fn the_last_whole_graph_record_is_the_one_restored() {
    let (file, mut db) = saving(300, 1);
    let mut r = Rng(34);
    put(&mut db, &mut r, 1000);
    assert_eq!(db.save_graphs().unwrap().0, 1);
    let first = file.0.lock().unwrap().len();
    put(&mut db, &mut r, 400);
    assert!(
        !db.graphs_due(),
        "400 writes outweigh no graph of 1400 nodes"
    );
    put(&mut db, &mut r, 800);
    assert!(db.graphs_due());
    let before = file.0.lock().unwrap().len();
    assert_eq!(db.save_graphs().unwrap().0, 1);
    let record = file.0.lock().unwrap().len() - before;
    assert!(before - first >= record, "{} < {record}", before - first);
    put(&mut db, &mut r, 50);
    assert_eq!(reopen_serving(&file).unlinked(), 50);

    let torn = File(Arc::new(Mutex::new(
        file.0.lock().unwrap()[..before + record / 2].to_vec(),
    )));
    let back = reopen_serving(&torn);
    assert_eq!(back.unlinked(), 1200);
    assert_eq!(ids(&back, "get d select id limit 5000", &[]).len(), 2200);
}

/// `rerank` reads the stored vectors, and a `vector<N, f16>` field is
/// stored halved under its own tag: it used to read as empty, and the
/// rerank returned no rows.
#[test]
fn rerank_reads_a_half_precision_field() {
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection d (body text @text, e vector<4, f16>)",
        &[],
    );
    exec(
        &mut db,
        "put d [{body: \"a b\", e: [1, 0, 0, 0]}, {body: \"a c\", e: [0, 1, 0, 0]}]",
        &[],
    );
    let hit = ids(
        &db,
        "get d select id match body \"a\" rerank e $1 limit 2",
        &[Value::Vector(vec![0.0, 1.0, 0.0, 0.0])],
    );
    assert_eq!(hit.iter().map(|h| h.0).collect::<Vec<_>>(), vec![2, 1]);
}
