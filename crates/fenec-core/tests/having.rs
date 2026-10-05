//! `having` keeps the groups a condition holds for, over their keys, the
//! list's columns by name and any aggregate; `count` after it counts them.
//! The ordered funnel -- visitors whose first finish is no earlier than
//! their first start -- shipped a row a visitor to the client to compare.

use fenec_core::prelude::*;

fn run(db: &mut Database, sql: &str) {
    db.execute(&fenec_ql::parse_one(sql).expect("parse"))
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    let stmt = fenec_ql::parse_one(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    match db.query(&stmt, &[]) {
        Ok(Response::Rows(rs)) => rs.rows.into_iter().map(|r| r.values).collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn err(db: &Database, sql: &str) -> String {
    match fenec_ql::parse_one(sql) {
        Err(e) => e.to_string(),
        Ok(s) => db.query(&s, &[]).unwrap_err().to_string(),
    }
}

fn funnel() -> Database {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection e (user text, name text, at int)",
    );
    // u1 starts then finishes, u2 finishes before it starts, u3 only
    // starts, u4 starts twice and finishes after the first, u5 views.
    run(
        &mut db,
        r#"put e [{user: "u1", name: "start", at: 1}, {user: "u1", name: "finish", at: 5},
                  {user: "u2", name: "finish", at: 2}, {user: "u2", name: "start", at: 3},
                  {user: "u3", name: "start", at: 4},
                  {user: "u4", name: "start", at: 9}, {user: "u4", name: "start", at: 2},
                  {user: "u4", name: "finish", at: 6}, {user: "u5", name: "view", at: 1}]"#,
    );
    db
}

#[test]
fn having_keeps_the_groups_it_holds_for_and_count_counts_them() {
    let db = funnel();
    let firsts = "get e select user, min(case when name = \"start\" then at end) as a, \
                  min(case when name = \"finish\" then at end) as b \
                  where name in [\"start\", \"finish\"] group user";
    assert_eq!(
        rows(&db, &format!("{firsts} having b >= a")),
        [
            vec![Value::Text("u1".into()), Value::Int(1), Value::Int(5)],
            vec![Value::Text("u4".into()), Value::Int(2), Value::Int(6)],
        ]
    );
    // Those who started at all: a null is not a value.
    assert_eq!(
        rows(&db, &format!("{firsts} having a != null count")),
        [vec![Value::Int(4)]]
    );
    // The funnel's last step in one statement: one row, not one a visitor.
    assert_eq!(
        rows(&db, &format!("{firsts} having b >= a count")),
        [vec![Value::Int(2)]]
    );
    // An aggregate the list does not hold, a key, and `and`.
    assert_eq!(
        rows(
            &db,
            "get e select user, max(at) as last group user having count(*) >= 2 and user != \"u2\""
        ),
        [
            vec![Value::Text("u1".into()), Value::Int(5)],
            vec![Value::Text("u4".into()), Value::Int(9)],
        ]
    );
    // Before the order and the page.
    assert_eq!(
        rows(
            &db,
            "get e select user, count(*) as n group user having n >= 2 order n desc limit 1"
        ),
        [vec![Value::Text("u4".into()), Value::Int(3)]]
    );
    // None passing: no rows, and a count of none.
    assert!(rows(
        &db,
        "get e select user, count(*) as n group user having n > 9"
    )
    .is_empty());
    assert_eq!(
        rows(
            &db,
            "get e select user, count(*) group user having count(*) > 9 count"
        ),
        [vec![Value::Int(0)]]
    );
    // `count` after `group` without `having` counts every group.
    assert_eq!(
        rows(&db, "get e select user, count(*) group user count"),
        [vec![Value::Int(5)]]
    );
}

#[test]
fn what_having_refuses() {
    let db = funnel();
    for (sql, says) in [
        (
            "get e select count(*) having count(*) > 1",
            "follows `group`",
        ),
        (
            "get e select user, count(*) group user having at > 1",
            "neither aggregated",
        ),
        (
            "get e select user, count(*) group user having count(*) > 1 limit 2 count",
            "`count` cannot be used together with `limit`",
        ),
    ] {
        let e = err(&db, sql);
        assert!(e.contains(says), "{sql}: {e}");
    }
}
