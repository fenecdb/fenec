//! `create index` and `compact` beside the database: the writes made while
//! the build runs have to come out as they would have under the write lock.
//! Each test makes its writes land during the build through
//! `maintain_with`, then compares against a twin that made the same writes
//! and ran the statement under the write lock.

use fenec_core::prelude::*;
use std::sync::RwLock;
use std::time::{Duration, Instant};

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

fn seeded() -> Database {
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection c (tag text, n int, body text, v vector<4>)",
    );
    for i in 0..300 {
        exec(
            &mut db,
            &format!(
                "put c {{tag: \"t{}\", n: {}, body: \"word{} common\", v: [{}.0, 1.0, {}.0, 0.5]}}",
                i % 7,
                i % 50,
                i % 11,
                i % 13,
                i % 5
            ),
        );
    }
    db
}

/// Writes made while a build runs: a new document, rewrites of existing
/// ones -- one of them twice -- deletes, and a document both made and
/// deleted.
fn meanwhile(db: &mut Database) {
    exec(
        db,
        "put c {tag: \"t1\", n: 7, body: \"fresh words\", v: [9.0, 1.0, 0.0, 0.5]}",
    );
    exec(
        db,
        "set c {tag: \"t3\", n: 49, body: \"rewritten\"} where n = 3",
    );
    exec(
        db,
        "set c {tag: \"t0\", n: 1, v: [0.0, 0.0, 1.0, 0.5]} where id = 10",
    );
    exec(db, "set c {n: 2} where id = 10");
    exec(db, "del c where n = 5");
    exec(db, "put c {id: 1000, tag: \"t9\", n: 9}");
    exec(db, "del c where id = 1000");
}

fn answers(db: &Database) -> Vec<Vec<(u64, Vec<Value>)>> {
    let mut queries = vec![
        "get c",
        "get c where tag = \"t3\"",
        "get c where tag = \"t1\"",
        "get c select id, n where n >= 2 and n < 9 order n desc",
    ];
    // `match` and `near` need the index they read.
    let schema = &db.collection("c").unwrap().schema;
    if matches!(schema.field("body").unwrap().index, IndexKind::Text(_)) {
        queries.push("get c select id match body \"rewritten\"");
        queries.push("get c select id match body \"common\"");
    }
    if matches!(schema.field("v").unwrap().index, IndexKind::Vector(_)) {
        queries.push("get c select id near v [0.0, 0.0, 1.0, 0.5] exact limit 5");
    }
    queries.iter().map(|q| rows(db, q)).collect()
}

/// The graph holds a node for every document with a vector and none for
/// one that is gone, and at this size finds the distances an exact search
/// does -- the distances, not the ids: the data repeats vectors, and which
/// of several documents at one distance comes back is the search's choice.
fn graph_is_whole(db: &Database) {
    let stats = db.stats();
    let Some(ix) = stats[0].vector_indexes.first() else {
        return;
    };
    let live = rows(db, "get c select v")
        .iter()
        .filter(|r| matches!(r.1[0], Value::Vector(_)))
        .count();
    assert_eq!(ix.count, live);
    let scores = |sql: &str| -> Vec<f32> {
        let r = db.query(&fenec_ql::parse_one(sql).unwrap(), &[]).unwrap();
        r.rows()
            .unwrap()
            .rows
            .iter()
            .map(|r| r.score.unwrap())
            .collect()
    };
    for probe in ["[9.0, 1.0, 0.0, 0.5]", "[2.5, 1.0, 3.5, 0.5]"] {
        let q = format!("get c select id near v {probe}");
        assert_eq!(
            scores(&format!("{q} limit 8")),
            scores(&format!("{q} exact limit 8"))
        );
    }
}

#[test]
fn an_index_built_beside_catches_up_with_the_writes_made_during_it() {
    for (field, kind) in [
        ("tag", "@hash"),
        ("n", "@sorted"),
        ("body", "@text"),
        ("v", "@hnsw(l2, m=8)"),
    ] {
        let stmt = fenec_ql::parse_one(&format!("create index on c ({field}) {kind}")).unwrap();
        let db = RwLock::new(seeded());
        let r = Database::maintain_with(&db, &stmt, &mut || meanwhile(&mut db.write().unwrap()));
        r.unwrap().unwrap();

        let mut twin = seeded();
        meanwhile(&mut twin);
        twin.execute(&stmt).unwrap();

        let db = db.into_inner().unwrap();
        assert_eq!(answers(&db), answers(&twin), "{field} {kind}");
        // The index is in the schema, and its record in the change counter.
        assert_eq!(
            db.collection("c")
                .unwrap()
                .schema
                .field(field)
                .unwrap()
                .index,
            twin.collection("c")
                .unwrap()
                .schema
                .field(field)
                .unwrap()
                .index
        );
        assert_eq!(db.change_seq(), twin.change_seq());
        graph_is_whole(&db);
    }
}

fn indexed(db: &mut Database) {
    exec(db, "create index on c (tag) @hash");
    exec(db, "create index on c (n) @sorted");
    exec(db, "create index on c (body) @text");
    exec(db, "create index on c (v) @hnsw(l2, m=8)");
    exec(db, "del c where n > 40");
}

/// An id handed out and deleted during the build leaves no record in the
/// copy; compacted, it must not be handed out again.
fn handed_out_and_deleted(db: &mut Database) {
    exec(db, "put c {tag: \"gone\"}");
    exec(db, "del c where tag = \"gone\"");
}

#[test]
fn a_compact_beside_catches_up_and_hands_out_no_id_again() {
    let mut db = seeded();
    indexed(&mut db);
    let db = RwLock::new(db);
    let compact = fenec_ql::parse_one("compact").unwrap();
    let r = Database::maintain_with(&db, &compact, &mut || {
        let mut g = db.write().unwrap();
        meanwhile(&mut g);
        handed_out_and_deleted(&mut g);
    });
    r.unwrap().unwrap();

    let mut twin = seeded();
    indexed(&mut twin);
    meanwhile(&mut twin);
    handed_out_and_deleted(&mut twin);
    exec(&mut twin, "compact");

    let mut db = db.into_inner().unwrap();
    assert_eq!(answers(&db), answers(&twin));
    graph_is_whole(&db);
    exec(&mut db, "put c {tag: \"next\"}");
    exec(&mut twin, "put c {tag: \"next\"}");
    assert_eq!(
        rows(&db, "get c select id where tag = \"next\""),
        rows(&twin, "get c select id where tag = \"next\"")
    );
}

/// Every rewrite of a document with a vector leaves a tombstone in the
/// graph, and a compact is the one thing that takes them out: by statement
/// and beside the database alike. A compact that kept them let the graph
/// grow with every update, and the tombstones crowd the beam `near` walks.
#[test]
fn a_compact_takes_the_tombstones_out_of_the_graph() {
    let dead = |db: &Database| db.collection("c").unwrap().vectors["v"].dead();
    let compact = fenec_ql::parse_one("compact").unwrap();
    for beside in [false, true] {
        let mut db = seeded();
        exec(&mut db, "create index on c (v) @hnsw(l2, m=8)");
        exec(&mut db, "set c {n: 1}");
        assert_eq!(dead(&db), 300);
        if beside {
            let lock = RwLock::new(db);
            Database::maintain(&lock, &compact).unwrap().unwrap();
            db = lock.into_inner().unwrap();
        } else {
            db.execute(&compact).unwrap();
        }
        assert_eq!(dead(&db), 0, "beside: {beside}");
        assert_eq!(db.collection("c").unwrap().vectors["v"].len(), 300);
        graph_is_whole(&db);
        // A graph with nothing to take out is left as it is.
        exec(&mut db, "compact");
        assert_eq!(dead(&db), 0);
    }
}

#[test]
fn a_schema_change_meanwhile_is_reported_rather_than_built_over() {
    let db = RwLock::new(seeded());
    let index = fenec_ql::parse_one("create index on c (n) @sorted").unwrap();
    let err = Database::maintain_with(&db, &index, &mut || {
        exec(&mut db.write().unwrap(), "create index on c (tag) @hash")
    })
    .unwrap()
    .unwrap_err();
    assert!(
        err.to_string().contains("run `create index` again"),
        "{err}"
    );
    let g = db.read().unwrap();
    let schema = &g.collection("c").unwrap().schema;
    assert_eq!(schema.field("n").unwrap().index, IndexKind::None);
    assert_eq!(schema.field("tag").unwrap().index, IndexKind::Hash);
    drop(g);

    let compact = fenec_ql::parse_one("compact c").unwrap();
    let r = Database::maintain_with(&db, &compact, &mut || {
        exec(&mut db.write().unwrap(), "drop collection c")
    });
    assert!(r.unwrap().is_err());
    // Nothing is left collecting ids once they are done: the next write
    // goes on as if neither had run.
    exec(&mut db.write().unwrap(), "create collection d (x int)");
    exec(&mut db.write().unwrap(), "put d {x: 1}");
}

#[test]
fn a_replica_compacts_but_builds_no_index() {
    let mut db = seeded();
    db.follow(vec![(1, 0)]).unwrap();
    let db = RwLock::new(db);
    let index = fenec_ql::parse_one("create index on c (n) @sorted").unwrap();
    assert!(matches!(
        Database::maintain(&db, &index),
        Some(Err(Error::ReadOnly(_)))
    ));
    let compact = fenec_ql::parse_one("compact").unwrap();
    assert!(Database::maintain(&db, &compact).unwrap().is_ok());
}

/// With real threads: while an HNSW index builds, reads are answered and
/// writes are taken -- neither waits for the build to end.
#[test]
fn reads_and_writes_go_on_while_an_index_builds() {
    let mut db = Database::new();
    exec(&mut db, "create collection c (n int, v vector<16>)");
    let docs: Vec<String> = (0..3_000)
        .map(|i| {
            let v: Vec<String> = (0..16)
                .map(|d| format!("{}", ((i * 31 + d * 7) % 97) as f32 / 97.0))
                .collect();
            format!("{{n: {i}, v: [{}]}}", v.join(","))
        })
        .collect();
    for chunk in docs.chunks(1_000) {
        exec(&mut db, &format!("put c [{}]", chunk.join(",")));
    }
    let db = std::sync::Arc::new(RwLock::new(db));
    let stmt = fenec_ql::parse_one("create index on c (v) @hnsw(cosine)").unwrap();
    let build = {
        let db = std::sync::Arc::clone(&db);
        std::thread::spawn(move || {
            let t = Instant::now();
            Database::maintain(&db, &stmt).unwrap().unwrap();
            t.elapsed()
        })
    };
    let (mut reads, mut writes) = (0, 0);
    let t = Instant::now();
    while !build.is_finished() {
        let _ = rows(&db.read().unwrap(), "get c where n = 7");
        reads += 1;
        exec(&mut db.write().unwrap(), "put c {n: -1}");
        writes += 1;
        std::thread::sleep(Duration::from_millis(1));
    }
    let took = build.join().unwrap();
    assert!(
        took > Duration::from_millis(50),
        "the build was too quick to tell ({took:?})"
    );
    assert!(
        reads > 5 && writes > 5,
        "{reads} reads, {writes} writes in {:?}",
        t.elapsed()
    );
    let g = db.read().unwrap();
    let v = g.stats()[0].vector_indexes[0].count;
    assert_eq!(v, 3_000, "every document with a vector is in the graph");
    assert_eq!(rows(&g, "get c where n = -1").len(), writes);
}
