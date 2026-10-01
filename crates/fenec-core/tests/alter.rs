//! `alter collection` adds, drops and renames a field without rewriting a
//! document: a field added reads `null` in the documents written before it,
//! a field dropped leaves a place skipped on read until `compact` takes it
//! out, a rename is the schema alone. Each answers the same after the file
//! is opened again, after a checkpoint, on a replica, and is put back with
//! the block it was in.

use fenec_core::prelude::*;
use std::sync::{Arc, Mutex};

/// A file in memory, and the records it was handed.
#[derive(Clone)]
struct Tap {
    file: Arc<Mutex<Vec<u8>>>,
    records: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Tap {
    fn new() -> Tap {
        Tap {
            file: Arc::new(Mutex::new(fenec_core::engine::MAGIC.to_vec())),
            records: Arc::default(),
        }
    }
    fn database(&self) -> Database {
        Database::with_sink(Box::new(self.clone()))
    }
    /// The file opened again, cut where the load says a crash left a record
    /// short, as `fs::open` cuts it.
    fn reopen(&self) -> Database {
        let mut db = Database::with_sink(Box::new(self.clone()));
        let bytes = self.file.lock().unwrap().clone();
        let whole = db.load(&bytes).expect("reopen");
        self.file.lock().unwrap().truncate(whole);
        db
    }
}

impl Sink for Tap {
    fn append(&mut self, bytes: &[u8]) -> fenec_core::error::Result<()> {
        self.file.lock().unwrap().extend_from_slice(bytes);
        Ok(())
    }
    fn record(&mut self, _seq: u64, bytes: &[u8]) -> fenec_core::error::Result<()> {
        self.file.lock().unwrap().extend_from_slice(bytes);
        self.records.lock().unwrap().push(bytes.to_vec());
        Ok(())
    }
    fn rewrite(&mut self, bytes: &[u8]) -> fenec_core::error::Result<()> {
        *self.file.lock().unwrap() = bytes.to_vec();
        Ok(())
    }
}

fn run(db: &mut Database, sql: &str) -> Result<Response> {
    db.execute(&fenec_ql::parse_one(sql).unwrap_or_else(|e| panic!("{sql}: {e}")))
}

fn ok(db: &mut Database, sql: &str) {
    run(db, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn rows(db: &Database, sql: &str) -> Vec<(u64, Vec<Value>)> {
    db.query(&fenec_ql::parse_one(sql).unwrap(), &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .rows()
        .unwrap()
        .rows
        .iter()
        .map(|r| (r.id, r.values.clone()))
        .collect()
}

fn names(db: &Database, collection: &str) -> Vec<String> {
    let c = db.collection(collection).unwrap();
    c.schema.fields.iter().map(|f| f.name.clone()).collect()
}

fn int(i: i64) -> Value {
    Value::Int(i)
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

/// Three orders, written before anything changes.
fn orders(db: &mut Database) {
    ok(
        db,
        "create collection orders (customer text @hash, total int @sorted, status text)",
    );
    ok(
        db,
        r#"put orders {customer: "a", total: 10, status: "open"}"#,
    );
    ok(
        db,
        r#"put orders {customer: "b", total: 20, status: "paid"}"#,
    );
    ok(
        db,
        r#"put orders {customer: "a", total: 30, status: "paid"}"#,
    );
}

/// What every check reads: each order's fields as the schema has them.
fn everything(db: &Database) -> Vec<(u64, Vec<Value>)> {
    rows(db, "get orders order id")
}

/// The same database, as each way of opening it again gives it: the log
/// replayed, a checkpoint's image, and the image with writes after it.
fn every_reopen(tap: &Tap, check: &dyn Fn(&Database)) {
    let db = tap.reopen();
    check(&db);
    let mut db = tap.reopen();
    db.checkpoint().unwrap();
    drop(db);
    check(&tap.reopen());
}

#[test]
fn an_added_field_reads_null_where_a_document_was_written_before_it() {
    let tap = Tap::new();
    let mut db = tap.database();
    orders(&mut db);
    let r = run(&mut db, "alter collection orders add field note text").unwrap();
    assert!(matches!(r, Response::Ok(_)), "{r:?}");
    assert_eq!(
        names(&db, "orders"),
        ["customer", "total", "status", "note"]
    );
    ok(
        &mut db,
        r#"put orders {customer: "c", total: 40, note: "gift"}"#,
    );
    ok(&mut db, r#"set orders {note: "late"} where id = 2"#);
    let want = |db: &Database| {
        assert_eq!(
            rows(db, "get orders select note order id"),
            [
                (1, vec![Value::Null]),
                (2, vec![text("late")]),
                (3, vec![Value::Null]),
                (4, vec![text("gift")]),
            ]
        );
        // A filter, an order, a count and an aggregate over it.
        assert_eq!(
            rows(db, "get orders select id where note is null order id"),
            [(1, vec![int(1)]), (3, vec![int(3)])]
        );
        assert_eq!(
            rows(db, r#"get orders select id where note = "gift""#),
            [(4, vec![int(4)])]
        );
        assert_eq!(
            rows(
                db,
                "get orders select note where note is not null order note desc"
            ),
            [(2, vec![text("late")]), (4, vec![text("gift")])]
        );
        let groups = db
            .query(
                &fenec_ql::parse_one("get orders select note, count(*) group note").unwrap(),
                &[],
            )
            .unwrap();
        assert_eq!(groups.rows().unwrap().rows.len(), 3);
        // The fields before it read as they did.
        assert_eq!(
            rows(
                db,
                r#"get orders select total where customer = "a" order id"#
            ),
            [(1, vec![int(10)]), (3, vec![int(30)])]
        );
    };
    want(&db);
    every_reopen(&tap, &want);
}

#[test]
fn an_added_field_takes_an_index_and_refuses_what_create_would() {
    let mut db = Database::new();
    orders(&mut db);
    ok(
        &mut db,
        "alter collection orders add field code text @unique",
    );
    ok(&mut db, "alter collection orders add tag text @hash");
    ok(
        &mut db,
        "alter collection orders add field rank int @sorted",
    );
    ok(
        &mut db,
        r#"set orders {code: "x1", tag: "t", rank: 5} where id = 1"#,
    );
    // The unique index holds the documents written before it as null.
    match run(&mut db, r#"set orders {code: "x1"} where id = 2"#) {
        Err(Error::Duplicate(_)) => {}
        r => panic!("{r:?}"),
    }
    assert_eq!(
        rows(&db, r#"get orders select id where tag = "t""#),
        [(1, vec![int(1)])]
    );
    assert_eq!(
        rows(&db, "get orders select id where rank >= 5"),
        [(1, vec![int(1)])]
    );
    // A name taken, `id`, a required field, an index the type cannot have.
    for (sql, what) in [
        ("alter collection orders add field status text", "taken"),
        ("alter collection orders add field id int", "id"),
        (
            "alter collection orders add field must text required",
            "required",
        ),
        ("alter collection orders add field v int @hnsw", "index"),
        (
            "alter collection orders add field c int collate tr",
            "collate",
        ),
        ("alter collection nope add field x int", "collection"),
    ] {
        assert!(run(&mut db, sql).is_err(), "{what}: {sql}");
    }
    assert_eq!(
        names(&db, "orders"),
        ["customer", "total", "status", "code", "tag", "rank"]
    );
}

#[test]
fn a_dropped_field_is_gone_from_reads_writes_and_indexes() {
    let tap = Tap::new();
    let mut db = tap.database();
    orders(&mut db);
    ok(&mut db, "alter collection orders drop field total");
    assert_eq!(names(&db, "orders"), ["customer", "status"]);
    // Its index went with it, and nothing names it.
    assert!(db.collection("orders").unwrap().sorted.is_empty());
    for sql in [
        "put orders {customer: \"z\", total: 1}",
        "set orders {total: 1}",
        "get orders select total",
        "get orders where total > 1",
        "get orders order total",
    ] {
        let r = match fenec_ql::parse_one(sql).unwrap() {
            s if s.is_read_only() => db.query(&s, &[]),
            s => db.execute(&s),
        };
        assert!(r.is_err(), "{sql}: {r:?}");
    }
    ok(&mut db, r#"put orders {customer: "d", status: "open"}"#);
    let want = |db: &Database| {
        // `get` without `select` answers the id and the fields.
        assert_eq!(
            everything(db),
            [
                (1, vec![int(1), text("a"), text("open")]),
                (2, vec![int(2), text("b"), text("paid")]),
                (3, vec![int(3), text("a"), text("paid")]),
                (4, vec![int(4), text("d"), text("open")]),
            ]
        );
        // The field after the dropped one is read from its own place.
        assert_eq!(
            rows(db, r#"get orders select id where status = "paid" order id"#),
            [(2, vec![int(2)]), (3, vec![int(3)])]
        );
        assert_eq!(
            rows(db, r#"get orders select id where customer = "a" order id"#),
            [(1, vec![int(1)]), (3, vec![int(3)])]
        );
    };
    want(&db);
    every_reopen(&tap, &want);
}

#[test]
fn a_field_added_under_a_dropped_name_is_a_new_field() {
    let tap = Tap::new();
    let mut db = tap.database();
    orders(&mut db);
    ok(&mut db, "alter collection orders drop field total");
    ok(&mut db, "alter collection orders add field total text");
    ok(&mut db, r#"set orders {total: "ten"} where id = 1"#);
    let want = |db: &Database| {
        // The old values are in the dropped place, not the new field's.
        assert_eq!(
            rows(db, "get orders select total order id"),
            [
                (1, vec![text("ten")]),
                (2, vec![Value::Null]),
                (3, vec![Value::Null]),
            ]
        );
        assert_eq!(names(db, "orders"), ["customer", "status", "total"]);
    };
    want(&db);
    every_reopen(&tap, &want);
}

#[test]
fn compact_takes_the_dropped_places_out_of_every_document() {
    let tap = Tap::new();
    let mut db = tap.database();
    orders(&mut db);
    ok(&mut db, "alter collection orders drop field status");
    ok(&mut db, "alter collection orders add field note text");
    ok(
        &mut db,
        r#"put orders {customer: "e", total: 50, note: "n"}"#,
    );
    let before = everything(&db);
    let len = tap.file.lock().unwrap().len();
    ok(&mut db, "compact");
    assert!(tap.file.lock().unwrap().len() < len);
    let schema = db.collection("orders").unwrap().schema.clone();
    assert!(schema.dropped.is_empty(), "{schema:?}");
    assert_eq!(everything(&db), before);
    // Written after the compact, read after an open: the same documents.
    ok(&mut db, r#"put orders {customer: "f", total: 60}"#);
    let after = everything(&db);
    let want = |db: &Database| {
        assert_eq!(everything(db), after);
        assert_eq!(
            rows(
                db,
                "get orders select id where total >= 30 order total desc"
            ),
            [(5, vec![int(5)]), (4, vec![int(4)]), (3, vec![int(3)])]
        );
    };
    want(&db);
    every_reopen(&tap, &want);
}

#[cfg(all(feature = "std-fs", unix, target_pointer_width = "64"))]
#[test]
fn compact_over_a_mapped_file_takes_them_out_too() {
    let dir = std::env::temp_dir().join(format!("fenecdb-alter-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("mapped.fenec");
    let _ = std::fs::remove_file(&path);
    let mut db = fenec_core::fs::open_mapped(&path).unwrap();
    orders(&mut db);
    ok(&mut db, "alter collection orders drop field customer");
    db.checkpoint().unwrap();
    drop(db);
    let mut db = fenec_core::fs::open_mapped(&path).unwrap();
    ok(
        &mut db,
        "alter collection orders rename field total to amount",
    );
    let before = everything(&db);
    // Beside the database, as a server runs it: a dropped field's places
    // need the documents read, which holds the lock instead.
    let lock = std::sync::RwLock::new(db);
    Database::maintain(&lock, &fenec_ql::parse_one("compact").unwrap())
        .unwrap()
        .unwrap();
    let db = lock.into_inner().unwrap();
    assert!(db.collection("orders").unwrap().schema.dropped.is_empty());
    assert_eq!(everything(&db), before);
    drop(db);
    let db = fenec_core::fs::open_mapped(&path).unwrap();
    assert_eq!(everything(&db), before);
    assert_eq!(
        rows(&db, "get orders select id where amount > 15 order amount"),
        [(2, vec![int(2)]), (3, vec![int(3)])]
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_rename_moves_the_field_and_its_index() {
    let tap = Tap::new();
    let mut db = tap.database();
    orders(&mut db);
    ok(
        &mut db,
        "alter collection orders rename field customer to buyer",
    );
    ok(&mut db, "alter collection orders rename total to amount");
    assert_eq!(names(&db, "orders"), ["buyer", "amount", "status"]);
    assert!(run(&mut db, "get orders where customer = \"a\"").is_err());
    for (sql, what) in [
        (
            "alter collection orders rename field buyer to status",
            "taken",
        ),
        ("alter collection orders rename field buyer to id", "id"),
        ("alter collection orders rename field nope to x", "missing"),
    ] {
        assert!(run(&mut db, sql).is_err(), "{what}");
    }
    let want = |db: &Database| {
        assert_eq!(
            rows(db, r#"get orders select amount where buyer = "a" order id"#),
            [(1, vec![int(10)]), (3, vec![int(30)])]
        );
        let plan = format!("{:?}", rows(db, r#"explain get orders where buyer = "a""#));
        assert!(plan.contains("hash"), "{plan}");
        let plan = format!(
            "{:?}",
            rows(
                db,
                "explain get orders where amount > 15 order amount limit 5"
            )
        );
        assert!(plan.contains("ordered index on amount"), "{plan}");
    };
    want(&db);
    every_reopen(&tap, &want);
}

#[test]
fn a_renamed_vector_field_keeps_its_graph_through_an_open() {
    let tap = Tap::new();
    let mut db = tap.database();
    ok(
        &mut db,
        "create collection docs (v vector<3> @hnsw(cosine, m=8), t text)",
    );
    for i in 0..50 {
        let (a, b) = ((i % 7) as f32, (i % 5) as f32);
        ok(
            &mut db,
            &format!("put docs {{v: [{a}, {b}, 1.0], t: \"d{i}\"}}"),
        );
    }
    db.checkpoint().unwrap();
    ok(&mut db, "alter collection docs rename field v to embed");
    ok(&mut db, "put docs {embed: [9.0, 9.0, 1.0], t: \"new\"}");
    let q = "get docs select t near embed [9.0, 9.0, 1.0] limit 3";
    let exact = "get docs select t near embed [9.0, 9.0, 1.0] exact limit 3";
    assert_eq!(rows(&db, q), rows(&db, exact));
    let db = tap.reopen();
    let stats = db.stats();
    assert_eq!(stats[0].vector_indexes[0].field, "embed");
    assert_eq!(stats[0].vector_indexes[0].count, 51);
    assert_eq!(rows(&db, q), rows(&db, exact));
    assert_eq!(rows(&db, q)[0].1, [text("new")]);
}

#[test]
fn vectors_a_block_left_waiting_are_linked_before_their_field_is_renamed() {
    let mut db = Database::new();
    ok(
        &mut db,
        "create collection docs (v vector<3> @hnsw(cosine, m=8), t text)",
    );
    db.begin().unwrap();
    for i in 0..20 {
        ok(
            &mut db,
            &format!("put docs {{v: [{i}.0, 1.0, 0.5], t: \"d{i}\"}}"),
        );
    }
    ok(&mut db, "alter collection docs rename field v to embed");
    db.commit().unwrap();
    assert_eq!(db.unlinked(), 0);
    let q = "get docs select t near embed [19.0, 1.0, 0.5] limit 3";
    let exact = "get docs select t near embed [19.0, 1.0, 0.5] exact limit 3";
    assert_eq!(rows(&db, q), rows(&db, exact));
    assert_eq!(db.stats()[0].vector_indexes[0].count, 20);
}

#[test]
fn a_type_change_is_refused_by_the_parser() {
    for sql in [
        "alter collection orders alter field total type text",
        "alter collection orders modify total text",
        "alter collection orders retype total text",
    ] {
        let e = fenec_ql::parse_one(sql).unwrap_err();
        assert!(e.to_string().contains("type"), "{sql}: {e}");
    }
    assert!(fenec_ql::parse_one("alter collection orders shuffle").is_err());
    // A drop of the last field leaves a collection of none, which no
    // collection is.
    let mut db = Database::new();
    ok(&mut db, "create collection one (x int)");
    assert!(run(&mut db, "alter collection one drop field x").is_err());
}

#[test]
fn a_block_puts_every_change_back() {
    let mut db = Database::new();
    orders(&mut db);
    let before = everything(&db);
    let schema = db.collection("orders").unwrap().schema.clone();
    for change in [
        "alter collection orders add field note text @hash",
        "alter collection orders drop field total",
        "alter collection orders drop field customer",
        "alter collection orders rename field customer to buyer",
    ] {
        db.begin().unwrap();
        ok(&mut db, change);
        // Writes after it, by the schema it made.
        ok(&mut db, r#"put orders {status: "x"}"#);
        ok(&mut db, r#"set orders {status: "y"} where id = 1"#);
        db.rollback();
        assert_eq!(db.collection("orders").unwrap().schema, schema, "{change}");
        assert_eq!(everything(&db), before, "{change}");
        // The indexes are the ones the schema has, and answer as before.
        assert_eq!(
            rows(&db, r#"get orders select id where customer = "a" order id"#),
            [(1, vec![int(1)]), (3, vec![int(3)])],
            "{change}"
        );
        assert_eq!(
            rows(
                &db,
                "get orders select id where total > 15 order total desc"
            ),
            [(3, vec![int(3)]), (2, vec![int(2)])],
            "{change}"
        );
    }
    // Two changes in one block, one landing on the other, put back in turn.
    db.begin().unwrap();
    ok(&mut db, "alter collection orders drop field total");
    ok(&mut db, "alter collection orders add field total text");
    ok(&mut db, "alter collection orders rename field total to t2");
    ok(&mut db, r#"put orders {customer: "q", t2: "x"}"#);
    db.rollback();
    assert_eq!(db.collection("orders").unwrap().schema, schema);
    assert_eq!(everything(&db), before);
    // And one that lands, as one record.
    let s1 = fenec_ql::parse_one("alter collection orders add field note text").unwrap();
    let s2 = fenec_ql::parse_one(r#"put orders {customer: "z", note: "n"}"#).unwrap();
    db.execute_block(&[(&s1, &[]), (&s2, &[])]).unwrap();
    assert_eq!(
        rows(&db, "get orders select note order id"),
        [
            (1, vec![Value::Null]),
            (2, vec![Value::Null]),
            (3, vec![Value::Null]),
            (4, vec![text("n")])
        ]
    );
}

#[test]
fn a_replica_applies_each_change_and_reopens_with_it() {
    let tap = Tap::new();
    let mut primary = tap.database();
    orders(&mut primary);
    ok(
        &mut primary,
        "alter collection orders add field note text @hash",
    );
    ok(&mut primary, r#"set orders {note: "n"} where id = 2"#);
    ok(&mut primary, "alter collection orders drop field total");
    ok(
        &mut primary,
        "alter collection orders rename field customer to buyer",
    );
    let s1 = fenec_ql::parse_one("alter collection orders add field rank int @sorted").unwrap();
    let s2 = fenec_ql::parse_one(r#"put orders {buyer: "r", rank: 3}"#).unwrap();
    primary.execute_block(&[(&s1, &[]), (&s2, &[])]).unwrap();
    let replica_tap = Tap::new();
    let mut replica = replica_tap.database();
    for r in tap.records.lock().unwrap().iter() {
        replica.apply(r).unwrap();
    }
    let check = |db: &Database| {
        assert_eq!(everything(db), everything(&primary));
        assert_eq!(
            db.collection("orders").unwrap().schema,
            primary.collection("orders").unwrap().schema
        );
        assert_eq!(
            rows(db, r#"get orders select id where note = "n""#),
            [(2, vec![int(2)])]
        );
        assert_eq!(
            rows(db, "get orders select id where rank >= 3"),
            [(4, vec![int(4)])]
        );
    };
    check(&replica);
    drop(replica);
    check(&replica_tap.reopen());
}

#[test]
fn the_change_stream_reads_each_document_by_the_schema_it_was_written_under() {
    let tap = Tap::new();
    let mut db = tap.database();
    orders(&mut db);
    ok(&mut db, "alter collection orders drop field total");
    ok(&mut db, r#"put orders {customer: "d", status: "s"}"#);
    ok(&mut db, "alter collection orders add field note text");
    ok(&mut db, r#"put orders {customer: "e", note: "n"}"#);
    let records: Vec<u8> = tap.records.lock().unwrap().concat();
    let fresh = Database::new();
    let mut seen = Vec::new();
    fresh
        .changes_in(&records, 1, &mut |c| {
            seen.push(match c.kind {
                fenec_core::engine::ChangeKind::Put(_, Some(d)) => format!("{:?}", d.fields),
                fenec_core::engine::ChangeKind::Alter(s) => format!(
                    "alter {:?}",
                    s.fields.iter().map(|f| f.name.clone()).collect::<Vec<_>>()
                ),
                fenec_core::engine::ChangeKind::Create(_) => "create".into(),
                _ => "other".into(),
            });
            true
        })
        .unwrap();
    let tail: Vec<&str> = seen
        .iter()
        .rev()
        .take(4)
        .rev()
        .map(String::as_str)
        .collect();
    assert_eq!(
        tail,
        [
            "alter [\"customer\", \"status\"]",
            r#"[("customer", Text("d")), ("status", Text("s"))]"#,
            "alter [\"customer\", \"status\", \"note\"]",
            r#"[("customer", Text("e")), ("status", Null), ("note", Text("n"))]"#,
        ]
    );
    assert_eq!(
        fenec_core::engine::writes_in(&tap.records.lock().unwrap()[4]).unwrap(),
        1
    );
}

#[test]
fn an_alter_cut_short_by_a_crash_is_cut_off_at_the_open() {
    let tap = Tap::new();
    let mut db = tap.database();
    orders(&mut db);
    ok(&mut db, "alter collection orders add field note text");
    drop(db);
    {
        let mut f = tap.file.lock().unwrap();
        let len = f.len();
        f.truncate(len - 2);
    }
    let mut db = tap.reopen();
    assert_eq!(names(&db, "orders"), ["customer", "total", "status"]);
    ok(&mut db, "alter collection orders add field note int");
    drop(db);
    let db = tap.reopen();
    assert_eq!(
        names(&db, "orders"),
        ["customer", "total", "status", "note"]
    );
}

#[test]
fn a_binary_from_before_meets_a_kind_it_refuses() {
    // An alter is record kind 12 and a dropped place type tag 12, both
    // unknown to a binary from before them: it refuses the file, as this
    // one refuses a kind it does not know, rather than read the fields
    // after a dropped one a place early.
    let tap = Tap::new();
    let mut db = tap.database();
    orders(&mut db);
    ok(&mut db, "alter collection orders drop field customer");
    let last = tap.records.lock().unwrap().last().unwrap().clone();
    assert_eq!(last[0], 12);
    let schema = db.collection("orders").unwrap().schema.encode();
    assert!(schema.windows(2).any(|w| w == [0, 12]), "{schema:?}");
    let mut file = tap.file.lock().unwrap().clone();
    let at = file.len() - last.len();
    file[at] = 13;
    let e = Database::new().load(&file).unwrap_err();
    assert!(matches!(e, Error::Corrupt(_)), "{e}");
}
