//! An open leaves a collection's hash, text, ordered and sparse indexes for
//! the first statement that reads each to build from the documents
//! (`Derived`), all but an ordered index over a collated field, which the
//! browser's collation chunks need built at the load. Nothing may tell a
//! database opened that way from the one that wrote the file: not a query
//! through any of them, not a write made before the first read, not a block
//! undone after a read built an index, not readers meeting one at once.

use fenec_core::prelude::*;

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

/// Each index read: the hash by an equality, an `in` and a `lookup` from
/// another collection, the ordered ones by a range and by walks for
/// `order`, the text index by `match`, the sparse one by `near`.
const QUERIES: &[&str] = &[
    r#"get docs where kind = "b" order id"#,
    r#"get docs where kind in ["a", "c"] order id"#,
    "get tags lookup docs on kind = tag",
    "get docs where n >= 40 and n < 90 order id",
    "get docs order n desc limit 7",
    "get docs order name limit 9",
    r#"get docs match body "alpha" limit 5"#,
    r#"get docs near s "{1:1,6:2}/8" limit 5"#,
    "get docs count",
];

fn filled() -> Database {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection docs (kind text @hash, n int @sorted, \
         name text collate und @sorted, body text @text, s sparse<8> @inverted, \
         v vector<4> @hnsw(cosine, m=8))",
    );
    run(&mut db, "create collection tags (tag text)");
    let names = ["Ömer", "Zoë", "Émile", "ábaco", "Bob", "çiçek", "Ana"];
    let words = ["alpha", "beta", "gamma", "delta"];
    for i in 0..120 {
        run(
            &mut db,
            &format!(
                r#"put docs {{kind: "{}", n: {}, name: "{}{i}", body: "{} {}", s: "{{{}:1,{}:2}}/8", v: [{}, 0.5, 0.25, 1]}}"#,
                ["a", "b", "c"][i % 3],
                (i * 37) % 100,
                names[i % 7],
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
    db
}

fn reopened(db: &Database) -> Database {
    let mut back = Database::new();
    back.load(&db.snapshot()).unwrap();
    back
}

/// Which of the hash, text, two ordered and sparse indexes are built.
fn built(db: &Database) -> [bool; 5] {
    let c = db.collection("docs").unwrap();
    let sorted = |f: &str| c.sorted.iter().any(|(n, d)| n == f && d.built().is_some());
    [
        c.hashes["kind"].built().is_some(),
        c.texts["body"].built().is_some(),
        sorted("n"),
        sorted("name"),
        c.sparse[0].1.built().is_some(),
    ]
}

#[test]
fn an_open_leaves_each_index_to_the_first_read_of_it() {
    let db = filled();
    let back = reopened(&db);
    assert_eq!(built(&back), [false, false, false, true, false]);
    let unbuilt = back.memory_bytes();
    for q in QUERIES {
        assert_eq!(answer(&back, q), answer(&db, q), "{q}");
    }
    assert_eq!(built(&back), [true; 5]);
    assert!(back.memory_bytes() > unbuilt);
}

/// A write skips an unbuilt index, and the build reads the documents the
/// write left. The filter of the last one reads the hash index, which is
/// built there and kept up by the write.
#[test]
fn a_write_before_the_first_read_is_in_the_index_it_builds() {
    let mut db = filled();
    let mut back = reopened(&db);
    for w in [
        r#"put docs {kind: "b", n: 55, name: "Émile", body: "alpha beta", s: "{1:5}/8"}"#,
        r#"set docs {kind: "c", n: 3, name: "Zeynep", body: "gamma"} where id = 4"#,
        "del docs where id = 9",
        r#"set docs {n: 1, body: "alpha alpha"} where kind = "c""#,
    ] {
        run(&mut db, w);
        run(&mut back, w);
    }
    assert_eq!(built(&back), [true, false, false, true, false]);
    for q in QUERIES {
        assert_eq!(answer(&back, q), answer(&db, q), "{q}");
    }
}

/// A read inside a block builds each index with the block's writes in it,
/// and undoing the block takes them out of it again.
#[test]
fn a_block_undone_after_a_read_built_an_index_leaves_it_as_the_documents_are() {
    let db = filled();
    let mut back = reopened(&db);
    back.begin().unwrap();
    run(
        &mut back,
        r#"put docs {kind: "b", n: 55, name: "Zoë", body: "alpha", s: "{2:1}/8"}"#,
    );
    run(
        &mut back,
        r#"set docs {kind: "a", n: 1, body: "delta"} where id = 3"#,
    );
    run(&mut back, "del docs where id = 5");
    for q in QUERIES {
        answer(&back, q);
    }
    assert_eq!(built(&back), [true; 5]);
    back.rollback();
    for q in QUERIES {
        assert_eq!(answer(&back, q), answer(&db, q), "{q}");
    }
}

/// Readers that meet an unbuilt index at once, as a server's do under its
/// read lock: one builds it, the others wait for that build.
#[test]
fn readers_meeting_an_unbuilt_index_at_once_agree() {
    let db = filled();
    let back = reopened(&db);
    let want: Vec<ResultSet> = QUERIES.iter().map(|q| answer(&db, q)).collect();
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                for (q, w) in QUERIES.iter().zip(&want) {
                    assert_eq!(&answer(&back, q), w, "{q}");
                }
            });
        }
    });
    assert_eq!(built(&back), [true; 5]);
}
