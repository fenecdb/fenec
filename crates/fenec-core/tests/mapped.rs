//! A file opened mapped (`fs::open_mapped`) answers every query as the same
//! file opened by reading it (`fs::open`): the documents are decoded from the
//! file's pages instead of from copies, and nothing else may differ -- not
//! the checkpoint's image, not the tail after it, not what a write, a
//! checkpoint or a compact on the mapped database leaves behind.

#![cfg(all(feature = "std-fs", unix, target_pointer_width = "64"))]

use fenec_core::fs::{open, open_mapped};
use fenec_core::prelude::*;
use std::path::PathBuf;

fn run(db: &mut Database, sql: &str) {
    for stmt in fenec_ql::parse(sql).expect("parse") {
        db.execute(&stmt).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

fn answer(db: &Database, sql: &str, params: &[Value]) -> ResultSet {
    db.query(&fenec_ql::parse_one(sql).expect("parse"), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .rows()
        .expect("rows")
        .clone()
}

fn tmp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fenecdb-mapped-{}", std::process::id()));
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
    "get docs near v [0.3, 0.1, 0.9, 0.2] limit 5",
    "get docs select kind, count(*), sum(n) group kind",
    "get other order id",
];

fn same(a: &Database, b: &Database) {
    for q in QUERIES {
        assert_eq!(answer(a, q, &[]), answer(b, q, &[]), "{q}");
    }
}

fn fill(path: &PathBuf) {
    let mut db = open(path).unwrap();
    run(
        &mut db,
        "create collection docs (title text, kind text @hash, n int @sorted, \
         body text @text, v vector<4> @hnsw(cosine, m=8))",
    );
    run(&mut db, "create collection other (x int)");
    for i in 0..300 {
        let kind = ["a", "b", "c"][i % 3];
        let word = ["alpha", "beta", "gamma", "delta"][i % 4];
        let v = [(i % 7) as f32, (i % 5) as f32, (i % 3) as f32, 1.0];
        db.execute_with(
            &fenec_ql::parse_one("put docs {title: $1, kind: $2, n: $3, body: $4, v: $5}").unwrap(),
            &[
                Value::Text(format!("t{i}")),
                Value::Text(kind.into()),
                Value::Int(i as i64),
                Value::Text(format!("{word} {word} body {i}")),
                Value::Vector(v.to_vec()),
            ],
        )
        .unwrap();
        if i % 50 == 0 {
            run(&mut db, &format!("put other {{x: {i}}}"));
        }
    }
    run(&mut db, "del docs where n < 20");
    run(&mut db, "set docs {title: \"changed\"} where n >= 290");
    // The image ends here: what follows is the tail, applied on top of it.
    db.checkpoint().unwrap();
    run(&mut db, "del docs where n >= 280 and n < 285");
    run(
        &mut db,
        "put docs {title: \"late\", kind: \"b\", n: 1000, body: \"alpha late\", v: [1, 1, 1, 1]}",
    );
    run(&mut db, "set docs {kind: \"c\"} where n = 150");
    db.sync().unwrap();
}

#[test]
fn a_mapped_open_answers_as_a_read_one() {
    let path = tmp("answers");
    fill(&path);
    let read = open(&path).unwrap();
    let mapped = open_mapped(&path).unwrap();
    same(&read, &mapped);
    // The records stayed in the file; only what was written since is held.
    let held = |db: &Database| db.memory_bytes();
    assert!(
        held(&mapped) < held(&read),
        "{} >= {}",
        held(&mapped),
        held(&read)
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn writes_checkpoints_and_compacts_on_a_mapped_database() {
    let path = tmp("writes");
    fill(&path);
    let twin = tmp("writes-twin");
    std::fs::copy(&path, &twin).unwrap();

    // The same statements on both: one read, one mapped.
    let mut read = open(&twin).unwrap();
    let mut mapped = open_mapped(&path).unwrap();
    for db in [&mut read, &mut mapped] {
        run(db, "put docs {title: \"after\", kind: \"a\", n: 2000, body: \"gamma after\", v: [0, 1, 0, 1]}");
        run(db, "del docs where n >= 200 and n < 210");
        run(
            db,
            "set docs {title: \"again\"} where kind = \"a\" and n < 60",
        );
    }
    same(&read, &mapped);

    // A checkpoint writes the mapped records back out with the new ones:
    // the same file as the read database's checkpoint, byte for byte. Both
    // are reopened to compare: a graph restored from a file numbers its
    // nodes afresh, which orders exact ties apart from the one built write
    // by write, mapped or not.
    mapped.checkpoint().unwrap();
    read.checkpoint().unwrap();
    drop(mapped);
    drop(read);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        std::fs::read(&twin).unwrap(),
        "the checkpoints differ"
    );
    let mut mapped = open_mapped(&path).unwrap();
    let mut read = open(&twin).unwrap();
    same(&read, &mapped);

    // A compact copies the live records out of the mapping.
    run(&mut mapped, "compact");
    run(&mut read, "compact");
    same(&read, &mapped);
    drop(mapped);
    drop(read);
    same(&open(&twin).unwrap(), &open_mapped(&path).unwrap());
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&twin);
}

#[test]
fn an_empty_or_new_file_opens_mapped() {
    let path = tmp("new");
    {
        let mut db = open_mapped(&path).unwrap();
        run(&mut db, "create collection other (x int)");
        run(&mut db, "put other {x: 1}");
        db.sync().unwrap();
    }
    let db = open_mapped(&path).unwrap();
    assert_eq!(answer(&db, "get other select x", &[]).rows.len(), 1);
    let _ = std::fs::remove_file(&path);
}

/// A last record a crash cut short is cut off the file here as `fs::open`
/// cuts it, though the mapping still covers it: no record points there.
#[test]
fn a_record_cut_short_is_cut_off_before_the_next_write() {
    use std::io::Write;
    let path = tmp("torn");
    {
        let mut db = open_mapped(&path).unwrap();
        run(&mut db, "create collection other (x int)");
        run(&mut db, "put other {x: 1}");
        db.sync().unwrap();
    }
    let whole = std::fs::metadata(&path).unwrap().len();
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    f.write_all(&[3, 1, 100, 0, 1, 2, 3]).unwrap();
    drop(f);
    {
        let mut db = open_mapped(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), whole);
        run(&mut db, "put other {x: 2}");
        db.sync().unwrap();
        assert_eq!(answer(&db, "get other select x", &[]).rows.len(), 2);
    }
    let db = open_mapped(&path).unwrap();
    assert_eq!(answer(&db, "get other select x", &[]).rows.len(), 2);
    let _ = std::fs::remove_file(&path);
}
