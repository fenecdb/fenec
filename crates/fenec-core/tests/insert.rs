//! `insert` makes documents and refuses one whose id is taken, where `put`
//! writes over it: the statement is a block, so nothing of it is left.

use fenec_core::prelude::*;

fn run(db: &mut Database, sql: &str) -> Result<Response> {
    db.execute(&fenec_ql::parse_one(sql).expect("parse"))
}

fn titles(db: &Database) -> Vec<(u64, String)> {
    let r = db
        .query(
            &fenec_ql::parse_one("get t select title order id").unwrap(),
            &[],
        )
        .unwrap();
    r.rows()
        .unwrap()
        .rows
        .iter()
        .map(|r| match &r.values[0] {
            Value::Text(s) => (r.id, s.clone()),
            v => panic!("{v:?}"),
        })
        .collect()
}

#[test]
fn insert_refuses_a_taken_id_and_leaves_nothing_of_the_statement() {
    let mut db = Database::new();
    run(&mut db, "create collection t (title text @hash)").unwrap();
    run(&mut db, "insert into t {id: 5, title: \"five\"}").unwrap();
    // No id: a new one each time.
    run(&mut db, "insert t {title: \"new\"}").unwrap();
    let e = run(
        &mut db,
        "insert t [{id: 9, title: \"nine\"}, {id: 5, title: \"again\"}]",
    )
    .unwrap_err();
    assert!(matches!(e, Error::Duplicate(_)), "{e}");
    // Nothing of it: not the first document, not the index entry.
    assert_eq!(titles(&db), [(5, "five".into()), (6, "new".into())]);
    let r = db
        .query(
            &fenec_ql::parse_one("get t where title = \"nine\" count").unwrap(),
            &[],
        )
        .unwrap();
    assert_eq!(r.rows().unwrap().rows[0].values, [Value::Int(0)]);
    // The same id twice in one insert: the second is taken by the first.
    assert!(matches!(
        run(
            &mut db,
            "insert t {id: 20, title: \"a\"} {id: 20, title: \"b\"}"
        ),
        Err(Error::Duplicate(_))
    ));
    // `put` writes over as ever.
    run(&mut db, "put t {id: 5, title: \"over\"}").unwrap();
    assert_eq!(titles(&db)[0], (5, "over".into()));
}

#[test]
fn a_statement_says_which_it_is() {
    let is_insert = |sql: &str| match fenec_ql::parse_one(sql).unwrap() {
        Statement::Put { insert, .. } => insert,
        s => panic!("{s:?}"),
    };
    assert!(is_insert("insert into t {a: 1}"));
    assert!(is_insert("INSERT t {a: 1}"));
    assert!(!is_insert("put t {a: 1}"));
    assert!(!is_insert("put into t {a: 1}"));
}
