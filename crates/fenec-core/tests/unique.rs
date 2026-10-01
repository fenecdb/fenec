//! `@unique` refuses a second document holding a value one holds already,
//! on every write that can set it -- `put`, `insert`, `set`, and a block's
//! writes against the block's own -- with `null` no value, and the
//! statement put back whole. Replicas take what their primary took, and a
//! file reopened refuses as the database that wrote it did.

use fenec_core::prelude::*;
use fenec_core::schema::IndexKind;
use std::sync::{Arc, Mutex};

/// A file in memory, and the records it was handed with their numbers.
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
    /// The file opened again, as a process that starts over it would: cut
    /// where the load says a crash left a record short, as `fs::open` cuts.
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
    db.execute(&fenec_ql::parse_one(sql).expect("parse"))
}

fn ok(db: &mut Database, sql: &str) {
    run(db, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn duplicate(db: &mut Database, sql: &str) {
    match run(db, sql) {
        Err(Error::Duplicate(_)) => {}
        other => panic!("{sql}: {other:?}"),
    }
}

/// `(id, email)` of every document, in id order.
fn emails(db: &Database) -> Vec<(u64, Value)> {
    let r = db
        .query(
            &fenec_ql::parse_one("get users select email order id").unwrap(),
            &[],
        )
        .unwrap();
    r.rows()
        .unwrap()
        .rows
        .iter()
        .map(|r| (r.id, r.values[0].clone()))
        .collect()
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

fn users(db: &mut Database) {
    ok(
        db,
        "create collection users (email text @unique, name text)",
    );
    ok(db, r#"put users {email: "a@x", name: "a"}"#);
    ok(db, r#"put users {email: "b@x", name: "b"}"#);
}

#[test]
fn put_insert_and_set_refuse_a_value_another_document_holds() {
    let mut db = Database::new();
    users(&mut db);
    duplicate(&mut db, r#"put users {email: "a@x", name: "c"}"#);
    duplicate(&mut db, r#"insert users {email: "b@x"}"#);
    // Writing over a document with a value another holds.
    duplicate(&mut db, r#"put users {id: 2, email: "a@x"}"#);
    // An update makes one as well, at its second document.
    duplicate(
        &mut db,
        r#"set users {email: "z@x"} where name = "a" or name = "b""#,
    );
    duplicate(&mut db, r#"set users {email: "a@x"} where id = 2"#);
    // Nothing of any of them: the statement is put back whole.
    assert_eq!(emails(&db), [(1, text("a@x")), (2, text("b@x"))]);
    // The index answers as it did, nothing of the refused writes in it.
    let r = db
        .query(
            &fenec_ql::parse_one(r#"get users where email = "z@x" count"#).unwrap(),
            &[],
        )
        .unwrap();
    assert_eq!(r.rows().unwrap().rows[0].values, [Value::Int(0)]);
    // A value let go of is free again.
    ok(&mut db, r#"set users {email: "c@x"} where id = 1"#);
    ok(&mut db, r#"insert users {email: "a@x", name: "new"}"#);
    assert_eq!(
        emails(&db),
        [(1, text("c@x")), (2, text("b@x")), (3, text("a@x"))]
    );
    // And so is a deleted document's.
    ok(&mut db, "del users where id = 2");
    ok(&mut db, r#"put users {email: "b@x"}"#);
}

#[test]
fn a_document_keeping_its_own_value_is_no_duplicate() {
    let mut db = Database::new();
    users(&mut db);
    ok(
        &mut db,
        r#"put users {id: 1, email: "a@x", name: "renamed"}"#,
    );
    ok(&mut db, r#"set users {name: "again"} where id = 1"#);
    ok(&mut db, r#"set users {email: "a@x"} where id = 1"#);
    assert_eq!(emails(&db), [(1, text("a@x")), (2, text("b@x"))]);
}

#[test]
fn null_is_no_value_and_never_collides() {
    let mut db = Database::new();
    users(&mut db);
    ok(&mut db, r#"put users {name: "no email"}"#);
    ok(&mut db, r#"put users {name: "nor this one"}"#);
    ok(&mut db, r#"put users {email: null, name: "said so"}"#);
    ok(&mut db, r#"set users {email: null} where id = 1"#);
    assert_eq!(
        emails(&db)
            .iter()
            .filter(|(_, e)| *e == Value::Null)
            .count(),
        4
    );
    // A null let go of for a value is checked as any value is.
    duplicate(&mut db, r#"set users {email: "b@x"} where id = 3"#);
    ok(&mut db, r#"set users {email: "a@x"} where id = 3"#);
}

#[test]
fn equal_values_collide_as_equality_finds_them() {
    // `-0.0` and `0.0` are one value to `=`, and so one to `@unique`.
    let mut db = Database::new();
    ok(&mut db, "create collection p (price float @unique)");
    ok(&mut db, "put p {price: 0.0}");
    duplicate(&mut db, "put p {price: -0.0}");
    // A value coerced to the field's type: `1` is the float `1.0`.
    ok(&mut db, "put p {price: 1.0}");
    duplicate(&mut db, "put p {price: 1}");
}

#[test]
fn one_statement_and_one_block_are_checked_against_their_own_writes() {
    let mut db = Database::new();
    users(&mut db);
    // Two documents of one put.
    duplicate(&mut db, r#"put users [{email: "n@x"}, {email: "n@x"}]"#);
    // Two statements of a block: the second sees the first's.
    let put = fenec_ql::parse_one(r#"put users {email: "m@x"}"#).unwrap();
    let again = fenec_ql::parse_one(r#"insert users {email: "m@x", name: "2"}"#).unwrap();
    let r = db.execute_block(&[(&put, &[]), (&again, &[])]);
    assert!(matches!(r, Err((1, Error::Duplicate(_)))), "{r:?}");
    assert_eq!(emails(&db), [(1, text("a@x")), (2, text("b@x"))]);
    // A block that frees a value and takes it again goes through.
    let free = fenec_ql::parse_one(r#"set users {email: "old@x"} where id = 1"#).unwrap();
    let take = fenec_ql::parse_one(r#"put users {email: "a@x", name: "new"}"#).unwrap();
    db.execute_block(&[(&free, &[]), (&take, &[])]).unwrap();
    assert_eq!(
        emails(&db),
        [(1, text("old@x")), (2, text("b@x")), (3, text("a@x"))]
    );
    // A transaction's: begun, a write, then a duplicate refused alone --
    // the block goes on with the first, and its rollback takes it back.
    db.begin().unwrap();
    ok(&mut db, r#"put users {email: "t@x"}"#);
    duplicate(&mut db, r#"put users {email: "t@x"}"#);
    db.rollback();
    ok(&mut db, r#"put users {email: "t@x"}"#);
}

#[test]
fn a_reopened_file_refuses_as_the_database_that_wrote_it() {
    let tap = Tap::new();
    let mut db = tap.database();
    users(&mut db);
    duplicate(&mut db, r#"put users {email: "a@x"}"#);
    drop(db);
    // The hash index is built by the first write's check after the open,
    // so it holds every document the file does.
    let mut db = tap.reopen();
    duplicate(&mut db, r#"put users {email: "a@x"}"#);
    duplicate(&mut db, r#"put users {email: "b@x"}"#);
    ok(&mut db, r#"put users {email: "c@x"}"#);
    // A checkpoint's image carries the index's kind as the log did.
    db.checkpoint().unwrap();
    drop(db);
    let mut db = tap.reopen();
    duplicate(&mut db, r#"set users {email: "c@x"} where id = 1"#);
    let schema = db.collection("users").unwrap().schema.clone();
    assert_eq!(schema.field("email").unwrap().index, IndexKind::UNIQUE);
}

#[test]
fn a_crash_after_a_write_reopens_refusing_it() {
    let tap = Tap::new();
    let mut db = tap.database();
    users(&mut db);
    ok(&mut db, r#"put users {email: "c@x"}"#);
    drop(db);
    // The last record cut short, as a crash in the middle of its append
    // leaves it: the open cuts it off, and `c@x` is free again.
    {
        let mut f = tap.file.lock().unwrap();
        let len = f.len();
        f.truncate(len - 3);
    }
    let mut db = tap.reopen();
    assert_eq!(emails(&db), [(1, text("a@x")), (2, text("b@x"))]);
    ok(&mut db, r#"put users {email: "c@x"}"#);
    duplicate(&mut db, r#"put users {email: "a@x"}"#);
}

#[test]
fn create_index_unique_refuses_documents_holding_a_value_twice_and_names_it() {
    let mut db = Database::new();
    ok(&mut db, "create collection users (email text, name text)");
    ok(&mut db, r#"put users {email: "a@x"}"#);
    ok(&mut db, r#"put users {email: "b@x"}"#);
    ok(&mut db, r#"put users {email: "a@x"}"#);
    ok(&mut db, r#"put users {name: "null"}"#);
    ok(&mut db, r#"put users {name: "null too"}"#);
    let e = run(&mut db, "create index on users (email) @unique").unwrap_err();
    match &e {
        Error::Duplicate(m) => {
            assert!(m.contains("\"a@x\""), "{m}");
            assert!(m.contains("1 and 3"), "{m}");
        }
        e => panic!("{e:?}"),
    }
    // Nothing of it: no index, and writes go on as before.
    let schema = db.collection("users").unwrap().schema.clone();
    assert_eq!(schema.field("email").unwrap().index, IndexKind::None);
    ok(&mut db, r#"put users {email: "a@x"}"#);
    // Once the value is held once, the index is made -- the nulls are none.
    ok(&mut db, r#"del users where email = "a@x""#);
    ok(&mut db, "create index on users (email) @unique");
    duplicate(&mut db, r#"put users {email: "b@x"}"#);
}

#[test]
fn create_index_unique_beside_the_database_counts_the_writes_made_meanwhile() {
    let mut db = Database::new();
    ok(&mut db, "create collection users (email text)");
    ok(&mut db, r#"put users {email: "a@x"}"#);
    let lock = std::sync::RwLock::new(db);
    let index = fenec_ql::parse_one("create index on users (email) @unique").unwrap();
    // The build sees one `a@x`; a write made while it runs makes a second.
    let r = Database::maintain_with(&lock, &index, &mut || {
        ok(&mut lock.write().unwrap(), r#"put users {email: "a@x"}"#);
    })
    .unwrap();
    assert!(matches!(r, Err(Error::Duplicate(_))), "{r:?}");
    let db = lock.read().unwrap();
    let schema = db.collection("users").unwrap().schema.clone();
    assert_eq!(schema.field("email").unwrap().index, IndexKind::None);
    drop(db);
    let mut db = lock.into_inner().unwrap();
    ok(&mut db, "del users where id = 2");
    let lock = std::sync::RwLock::new(db);
    Database::maintain(&lock, &index).unwrap().unwrap();
    duplicate(&mut lock.write().unwrap(), r#"put users {email: "a@x"}"#);
}

#[test]
fn a_replica_applies_what_its_primary_took_without_asking_again() {
    let tap = Tap::new();
    let mut primary = tap.database();
    users(&mut primary);
    ok(&mut primary, r#"set users {email: "c@x"} where id = 1"#);
    ok(&mut primary, r#"put users {email: "a@x"}"#);
    duplicate(&mut primary, r#"put users {email: "b@x"}"#);
    let mut replica = Database::new();
    for r in tap.records.lock().unwrap().iter() {
        replica.apply(r).unwrap();
    }
    assert_eq!(emails(&replica), emails(&primary));
    // Promoted, it refuses as its primary did.
    duplicate(&mut replica, r#"put users {email: "a@x"}"#);
    duplicate(&mut replica, r#"put users {email: "c@x"}"#);
}

#[test]
fn a_unique_index_is_a_kind_of_its_own_in_the_file() {
    // The schema writes `@unique` as index kind 8, which a binary from
    // before it does not know and refuses the file for, rather than open
    // it as a plain hash and take the duplicates it would have refused.
    let schema = |kind| {
        Schema::new(
            "u",
            vec![fenec_core::schema::Field::new("e", DataType::Text).indexed(kind)],
        )
        .unwrap()
        .encode()
    };
    let (plain, unique) = (schema(IndexKind::HASH), schema(IndexKind::UNIQUE));
    assert_eq!(plain.len(), unique.len());
    let at = plain.iter().zip(&unique).position(|(a, b)| a != b).unwrap();
    assert_eq!((plain[at], unique[at]), (1, 8));
    assert_eq!(
        Schema::decode(&unique, &mut 0).unwrap().fields[0].index,
        IndexKind::UNIQUE
    );
    // An index kind this binary does not know is refused, not misread.
    let mut later = unique.clone();
    later[at] = 200;
    assert!(matches!(
        Schema::decode(&later, &mut 0),
        Err(Error::Corrupt(_))
    ));
}

#[test]
fn a_lookup_and_a_filter_take_a_unique_index_as_a_hash() {
    let mut db = Database::new();
    users(&mut db);
    ok(&mut db, "create collection orders (email text, total int)");
    ok(&mut db, r#"put orders {email: "a@x", total: 5}"#);
    // The child key needs a hash index, which a unique one is.
    let r = db
        .query(
            &fenec_ql::parse_one("get orders lookup users on email = email").unwrap(),
            &[],
        )
        .unwrap();
    let rs = r.rows().unwrap();
    let users = rs.nested.as_ref().expect("users");
    assert_eq!(users.group(0).len(), 1);
    let plan = db
        .query(
            &fenec_ql::parse_one(r#"explain get users where email = "a@x""#).unwrap(),
            &[],
        )
        .unwrap();
    let plan = format!("{:?}", plan.rows().unwrap().rows);
    assert!(plan.contains("hash"), "{plan}");
}
