//! `returning` on `put` and `insert`: the rows written answered rather
//! than counted, ids and all, in the order of the documents -- a put over
//! an id, an upsert's row set and a row made, the documents of a
//! parameter -- with `require` counting them, and a statement refused
//! writing nothing.

use fenec_core::plugin::{Hook, WriteOp};
use fenec_core::prelude::*;
use std::sync::{Arc, Mutex};

fn run(db: &mut Database, sql: &str, params: &[Value]) -> Result<Response> {
    let stmt = fenec_ql::parse_one_for(db, sql)?;
    db.execute_with(&stmt, params)
}

fn rows(db: &mut Database, sql: &str, params: &[Value]) -> ResultSet {
    match run(db, sql, params).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        Response::Rows(rs) => rs,
        other => panic!("{sql}: {other:?}"),
    }
}

fn table(rs: &ResultSet) -> Vec<(u64, Vec<Value>)> {
    rs.rows.iter().map(|r| (r.id, r.values.clone())).collect()
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

fn fixture() -> Database {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection users (email text nocase @unique, name text, n int)",
        &[],
    )
    .unwrap();
    db
}

fn count(db: &mut Database) -> Value {
    match db
        .query(&fenec_ql::parse_one("get users count").unwrap(), &[])
        .unwrap()
    {
        Response::Rows(rs) => rs.rows[0].values[0].clone(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_insert_answers_the_ids_it_made() {
    let mut db = fixture();
    let rs = rows(
        &mut db,
        "insert users {email: \"a@x\", name: \"Ada\"} returning id",
        &[],
    );
    assert_eq!(rs.columns, ["id"]);
    assert_eq!(table(&rs), [(1, vec![Value::Int(1)])]);
    // Several, in the order written, every field after the id with `*`.
    let rs = rows(
        &mut db,
        "put users [{email: \"b@x\", name: \"Bo\"}, {email: \"c@x\", n: 3}] returning *",
        &[],
    );
    assert_eq!(rs.columns, ["id", "email", "name", "n"]);
    assert_eq!(
        table(&rs),
        [
            (2, vec![Value::Int(2), text("b@x"), text("Bo"), Value::Null]),
            (
                3,
                vec![Value::Int(3), text("c@x"), Value::Null, Value::Int(3)]
            ),
        ]
    );
    // A put over an id answers the row as it now is.
    let rs = rows(
        &mut db,
        "put users {id: 2, email: \"b@x\", name: \"Bea\"} returning name, n",
        &[],
    );
    assert_eq!(table(&rs), [(2, vec![text("Bea"), Value::Null])]);
    // The documents of a parameter.
    let docs =
        fenec_core::json::parse_json(r#"[{"email": "d@x"}, {"email": "e@x", "n": 5}]"#).unwrap();
    let rs = rows(&mut db, "insert users $1 returning id, n", &[docs]);
    assert_eq!(
        table(&rs),
        [
            (4, vec![Value::Int(4), Value::Null]),
            (5, vec![Value::Int(5), Value::Int(5)])
        ]
    );
    // `require` before or after it counts the rows answered.
    let rs = rows(
        &mut db,
        "insert users {email: \"f@x\"} require 1 returning id",
        &[],
    );
    assert_eq!(table(&rs), [(6, vec![Value::Int(6)])]);
    let rs = rows(
        &mut db,
        "insert users {email: \"g@x\"} returning id require 1",
        &[],
    );
    assert_eq!(table(&rs), [(7, vec![Value::Int(7)])]);
    let e = run(
        &mut db,
        "insert users [{email: \"h@x\"}, {email: \"i@x\"}] returning id require 1",
        &[],
    )
    .unwrap_err();
    assert!(matches!(e, Error::Unmet(_)), "{e}");
    assert_eq!(count(&mut db), Value::Int(7), "the unmet insert put back");
}

/// `if absent` answers the rows it made, not those it passed over; `else
/// set` the row it set, in the documents' order, a key twice its row
/// twice, as the statement left it.
#[test]
fn an_upsert_answers_the_rows_it_made_and_set() {
    let mut db = fixture();
    run(
        &mut db,
        "put users [{email: \"a@x\", n: 1}, {email: \"b@x\", n: 1}]",
        &[],
    )
    .unwrap();
    let rs = rows(
        &mut db,
        "put users [{email: \"A@X\", n: 9}, {email: \"c@x\", n: 1}] if absent returning id, email",
        &[],
    );
    assert_eq!(table(&rs), [(3, vec![Value::Int(3), text("c@x")])]);
    let rs = rows(
        &mut db,
        "put users [{email: \"d@x\", n: 1}, {email: \"B@x\", n: 5}, {email: \"d@x\", n: 2}] \
         if absent else set {n: n + new.n} returning id, email, n",
        &[],
    );
    assert_eq!(
        table(&rs),
        [
            (4, vec![Value::Int(4), text("d@x"), Value::Int(3)]),
            (2, vec![Value::Int(2), text("b@x"), Value::Int(6)]),
            (4, vec![Value::Int(4), text("d@x"), Value::Int(3)]),
        ]
    );
    // The count without `returning` is the same.
    let r = run(
        &mut db,
        "put users [{email: \"e@x\", n: 1}, {email: \"e@x\", n: 1}] if absent else set {n: n + 1}",
        &[],
    )
    .unwrap();
    assert_eq!(r, Response::Affected(2));
}

#[test]
fn a_put_refused_answers_nothing_and_writes_nothing() {
    let mut db = fixture();
    run(&mut db, "insert users {email: \"a@x\"}", &[]).unwrap();
    // A field that is not there, refused by name, over the rows written.
    let e = run(&mut db, "insert users {email: \"b@x\"} returning nope", &[]).unwrap_err();
    assert!(e.to_string().contains("nope"), "{e}");
    // A clash, its batch put back whole.
    let e = run(
        &mut db,
        "insert users [{email: \"c@x\"}, {email: \"A@x\"}] returning id",
        &[],
    )
    .unwrap_err();
    assert!(matches!(e, Error::Duplicate(_)), "{e}");
    assert_eq!(count(&mut db), Value::Int(1));
    // In a block put back, the rows it answered are gone with it.
    db.begin().unwrap();
    let rs = rows(&mut db, "insert users {email: \"d@x\"} returning id", &[]);
    assert_eq!(table(&rs), [(2, vec![Value::Int(2)])]);
    db.rollback();
    assert_eq!(count(&mut db), Value::Int(1));
    // Each row is handed to the hooks as the write it was -- a scoped
    // token's read rules (`fenec_http::access`) -- and one refused refuses
    // the write.
    let seen = Arc::new(Mutex::new(Vec::new()));
    db.registry_mut()
        .register_hook(Arc::new(Answering(seen.clone())));
    let e = run(
        &mut db,
        "insert users [{email: \"e@x\", n: 1}, {email: \"f@x\", n: 2}] returning id",
        &[],
    )
    .unwrap_err();
    assert!(matches!(e, Error::Denied(_)), "{e}");
    assert_eq!(count(&mut db), Value::Int(1), "nothing written");
    seen.lock().unwrap().clear();
    run(&mut db, "insert users {email: \"g@x\", n: 1}", &[]).unwrap();
    let rs = rows(
        &mut db,
        "put users [{email: \"G@x\", n: 1}, {email: \"h@x\", n: 1}] \
         if absent else set {n: n} returning email",
        &[],
    );
    assert_eq!(
        table(&rs)
            .iter()
            .map(|r| r.1[0].clone())
            .collect::<Vec<_>>(),
        [text("g@x"), text("h@x")]
    );
    assert_eq!(
        *seen.lock().unwrap(),
        [WriteOp::Update, WriteOp::Insert],
        "the row set an update, the row made an insert"
    );
}

/// Refuses a row answered whose `n` is not 1, as a scoped token's read
/// rules refuse one outside them, and keeps the write each was.
struct Answering(Arc<Mutex<Vec<WriteOp>>>);
impl Hook for Answering {
    fn name(&self) -> &str {
        "answering"
    }
    fn answering(&self, _: &Schema, op: WriteOp, doc: &Document) -> Result<()> {
        self.0.lock().unwrap().push(op);
        match doc.get("n") {
            Some(Value::Int(1)) => Ok(()),
            _ => Err(Error::Denied("not readable".into())),
        }
    }
}

#[test]
fn returning_on_a_put_is_parsed_after_its_clauses_alone() {
    for (sql, why) in [
        ("put users {email: \"a\"} order id returning id", "order"),
        ("put users {email: \"a\"} returning", ""),
        ("put users {email: \"a\"} limit 1 returning id", "limit"),
    ] {
        let e = fenec_ql::parse_one(sql).unwrap_err().to_string();
        assert!(e.contains(why), "{sql}: {e}");
    }
    let stmt = fenec_ql::parse_one("put users {a: 1} if absent returning *").unwrap();
    assert!(stmt.returns_rows());
    let Statement::Put { pick, .. } = stmt else {
        panic!()
    };
    assert_eq!(pick.unwrap().returning, Some(None));
}
