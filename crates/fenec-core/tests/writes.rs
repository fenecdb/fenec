//! Expressions in a `set`, and the writes that say whether they were made:
//! `{n: n + 1}` over the row as it was, under the write lock, so an
//! increment is atomic; `put ... if absent`, which answers 0 rather than
//! refusing; and a row past its `@ttl` out of a write's way, which is what
//! makes a lock that expires.

use fenec_core::prelude::*;
use std::sync::{Arc, Mutex};

fn run(db: &mut Database, sql: &str) -> Result<Response> {
    db.execute(&fenec_ql::parse_one(sql).expect("parse"))
}

fn run_with(db: &mut Database, sql: &str, params: &[Value]) -> Result<Response> {
    db.execute_with(&fenec_ql::parse_one(sql).expect("parse"), params)
}

fn affected(r: Result<Response>) -> usize {
    match r.unwrap() {
        Response::Affected(n) => n,
        r => panic!("{r:?}"),
    }
}

/// The one value of `get <sql>`'s first row.
fn one(db: &Database, sql: &str) -> Value {
    let r = db.query(&fenec_ql::parse_one(sql).unwrap(), &[]).unwrap();
    r.rows().unwrap().rows[0].values[0].clone()
}

fn counters() -> Database {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection hits (key text @hash, n int, f float, s text, at timestamp, j json)",
    )
    .unwrap();
    run(
        &mut db,
        r#"put hits {id: 1, key: "ip1", n: 1, f: 0.5, s: "a"}"#,
    )
    .unwrap();
    run(&mut db, r#"put hits {id: 2, key: "ip2"}"#).unwrap();
    db
}

#[test]
fn a_set_reads_the_row_it_writes() {
    let mut db = counters();
    assert_eq!(
        affected(run(&mut db, r#"set hits {n: n + 1} where key = "ip1""#)),
        1
    );
    assert_eq!(one(&db, "get hits select n where id = 1"), Value::Int(2));
    // A parameter, and the other operators, by precedence.
    run_with(
        &mut db,
        "set hits {n: n * $1 - 1} where id = 1",
        &[Value::Int(10)],
    )
    .unwrap();
    assert_eq!(one(&db, "get hits select n where id = 1"), Value::Int(19));
    run(&mut db, "set hits {n: (n + 1) / 4} where id = 1").unwrap();
    assert_eq!(one(&db, "get hits select n where id = 1"), Value::Int(5));
    // Written without spaces, and a negative number where a value starts.
    run(&mut db, "set hits {n: n-1, f: -2.5} where id = 1").unwrap();
    assert_eq!(one(&db, "get hits select n where id = 1"), Value::Int(4));
    assert_eq!(
        one(&db, "get hits select f where id = 1"),
        Value::Float(-2.5)
    );
    run(&mut db, "set hits {n: n - -1, f: -f} where id = 1").unwrap();
    assert_eq!(one(&db, "get hits select n where id = 1"), Value::Int(5));
    assert_eq!(
        one(&db, "get hits select f where id = 1"),
        Value::Float(2.5)
    );
    // Every pair reads the row as it was, not as the pairs before left it.
    run(&mut db, "set hits {n: n + 1, f: n} where id = 1").unwrap();
    assert_eq!(
        one(&db, "get hits select f where id = 1"),
        Value::Float(5.0)
    );
    // A string stays a string; a name on the value side is the field.
    run(&mut db, r#"set hits {s: "n", key: s} where id = 1"#).unwrap();
    assert_eq!(
        one(&db, "get hits select s where id = 1"),
        Value::Text("n".into())
    );
    assert_eq!(
        one(&db, "get hits select key where id = 1"),
        Value::Text("a".into())
    );
    // `id` is the row's.
    run(&mut db, "set hits {n: id * 100} where id = 2").unwrap();
    assert_eq!(one(&db, "get hits select n where id = 2"), Value::Int(200));
    // A filter takes arithmetic too.
    assert_eq!(one(&db, "get hits where n + 1 > 100 count"), Value::Int(1));
    assert_eq!(one(&db, "get hits where n = 2 * 3 count"), Value::Int(1));
}

#[test]
fn null_plus_one_is_null_and_coalesce_counts_from_nothing() {
    let mut db = counters();
    run(&mut db, "set hits {n: n + 1} where id = 2").unwrap();
    assert_eq!(one(&db, "get hits select n where id = 2"), Value::Null);
    run(&mut db, "set hits {n: coalesce(n, 0) + 1} where id = 2").unwrap();
    run(&mut db, "set hits {n: coalesce(n, 0) + 1} where id = 2").unwrap();
    assert_eq!(one(&db, "get hits select n where id = 2"), Value::Int(2));
}

#[test]
fn types_follow_the_field_and_overflow_is_refused() {
    let mut db = counters();
    // int and float make a float, which an int field takes when whole...
    run(&mut db, "set hits {n: n * 2.0} where id = 1").unwrap();
    assert_eq!(one(&db, "get hits select n where id = 1"), Value::Int(2));
    // ... and refuses when not, as it refuses the literal.
    let e = run(&mut db, "set hits {n: n + 0.5} where id = 1").unwrap_err();
    assert!(matches!(e, Error::Type(_)), "{e}");
    let e = run(&mut db, "set hits {n: 2.5} where id = 1").unwrap_err();
    assert!(matches!(e, Error::Type(_)), "{e}");
    // A float field takes an int's result as a float.
    run(&mut db, "set hits {f: n + 1} where id = 1").unwrap();
    assert_eq!(
        one(&db, "get hits select f where id = 1"),
        Value::Float(3.0)
    );
    // Whole division is whole, toward zero.
    run(&mut db, "set hits {n: -7 / 2} where id = 1").unwrap();
    assert_eq!(one(&db, "get hits select n where id = 1"), Value::Int(-3));
    // Past 64 bits, refused rather than wrapped, and nothing written.
    run(&mut db, "set hits {n: 9223372036854775807} where id = 1").unwrap();
    let e = run(&mut db, "set hits {n: n + 1} where id = 1").unwrap_err();
    assert!(e.to_string().contains("overflows"), "{e}");
    assert_eq!(
        one(&db, "get hits select n where id = 1"),
        Value::Int(i64::MAX)
    );
    let e = run(&mut db, "set hits {n: n / 0} where id = 1").unwrap_err();
    assert!(e.to_string().contains("division by zero"), "{e}");
    let e = run(&mut db, "set hits {f: f / 0} where id = 1").unwrap_err();
    assert!(e.to_string().contains("division by zero"), "{e}");
    // Text and a number: a type error.
    let e = run(&mut db, "set hits {n: s + 1} where id = 1").unwrap_err();
    assert!(matches!(e, Error::Type(_)), "{e}");
    // Over no rows, no error: it comes at the first row, as before.
    assert_eq!(
        affected(run(&mut db, "set hits {n: s + 1} where id = 99")),
        0
    );
    assert_eq!(
        affected(run(&mut db, r#"set hits {n: "x"} where id = 99"#)),
        0
    );
    // A field the collection has not: refused by name, as before.
    let e = run(&mut db, "set hits {nope: n + 1} where id = 1").unwrap_err();
    assert!(matches!(e, Error::NotFound(_)), "{e}");
}

/// `+` joins two texts, in a put's values, a `set` and a filter: a
/// ledger's entry ids were made of their movement's (`$1 + ":dr"`) and
/// travelled as parameters of their own. Text and a number stay a type
/// error, either way round, and so does `-` between texts.
#[test]
fn plus_joins_two_texts_and_nothing_else() {
    let mut db = counters();
    run_with(
        &mut db,
        r#"put hits {id: 3, key: $1 + ":dr", s: "a" + "" + "b"}"#,
        &[Value::Text("tx7".into())],
    )
    .unwrap();
    assert_eq!(
        one(&db, "get hits select key where id = 3"),
        Value::Text("tx7:dr".into())
    );
    assert_eq!(
        one(&db, "get hits select s where id = 3"),
        Value::Text("ab".into())
    );
    run(&mut db, r#"set hits {s: s + "-" + key} where id = 1"#).unwrap();
    assert_eq!(
        one(&db, "get hits select s where id = 1"),
        Value::Text("a-ip1".into())
    );
    assert_eq!(
        one(&db, r#"get hits select id where key = "tx" + "7:dr""#),
        Value::Int(3)
    );
    // A null is null, as in arithmetic.
    run(&mut db, r#"set hits {s: s + "x"} where id = 2"#).unwrap();
    assert_eq!(one(&db, "get hits select s where id = 2"), Value::Null);
    for bad in [
        r#"set hits {s: s + 1} where id = 1"#,
        r#"set hits {s: 1 + s} where id = 1"#,
        r#"set hits {s: s - "a"} where id = 1"#,
        r#"set hits {s: s * "a"} where id = 1"#,
    ] {
        let e = run(&mut db, bad).unwrap_err();
        assert!(matches!(e, Error::Type(_)), "{bad}: {e}");
    }
}

#[test]
fn now_is_the_clock_once_a_statement_and_a_timestamp_moves_by_milliseconds() {
    let mut db = counters();
    db.set_clock(Some(1_000_000));
    run(&mut db, "set hits {at: now() + 30000} where key = \"ip1\"").unwrap();
    assert_eq!(
        one(&db, "get hits select at where id = 1"),
        Value::Timestamp(1_030_000)
    );
    run(&mut db, "set hits {n: at - now()} where id = 1").unwrap();
    assert_eq!(
        one(&db, "get hits select n where id = 1"),
        Value::Int(30_000)
    );
    // The same time on every row.
    run(&mut db, "set hits {at: now()} where id > 0").unwrap();
    let r = db
        .query(&fenec_ql::parse_one("get hits select at").unwrap(), &[])
        .unwrap();
    for row in &r.rows().unwrap().rows {
        assert_eq!(row.values[0], Value::Timestamp(1_000_000));
    }
}

#[test]
fn a_path_reads_and_writes_inside_a_json_field() {
    let mut db = counters();
    run(&mut db, r#"set hits {j: {"views": 1}} where id = 1"#).unwrap();
    run(&mut db, "set hits {j.views: j.views + 1} where id = 1").unwrap();
    assert_eq!(
        one(&db, "get hits select j.views where id = 1"),
        Value::Int(2)
    );
}

#[test]
fn the_texts_that_parsed_before_parse_as_they_did() {
    use fenec_core::query::{Expr, Statement};
    let p = |s: &str| fenec_ql::parse_one(s).unwrap();
    // A negative number after `=`, `,`, `[`, `:` and a keyword is a literal.
    let Statement::Select(sel) = p("get t where x = -1 and -2 < y and z in [-3, 4]") else {
        panic!()
    };
    let text = format!("{:?}", sel.filter.unwrap());
    assert!(text.contains("Int(-1)") && text.contains("Int(-2)") && text.contains("Int(-3)"));
    assert!(!text.contains("Arith"), "{text}");
    let Statement::Put { docs, .. } = p("put t {a: -1, b: [-1.5, 2], c: \"x - 1\"}") else {
        panic!()
    };
    assert!(docs[0].iter().all(|(_, e)| matches!(e, Expr::Lit(_))));
    // `select *` and `count(*)` stay what they were.
    assert!(matches!(p("get t select *"), Statement::Select(_)));
    assert!(matches!(p("get t select count(*)"), Statement::Select(_)));
    // A comment is not a minus.
    assert!(matches!(p("get t -- n - 1\n"), Statement::Select(_)));
}

#[test]
fn put_if_absent_writes_only_what_is_absent_and_says_so() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection locks (name text @unique, owner text)",
    )
    .unwrap();
    let take = "put locks {id: 1, owner: $1} if absent";
    assert_eq!(
        affected(run_with(&mut db, take, &[Value::Text("a".into())])),
        1
    );
    assert_eq!(
        affected(run_with(&mut db, take, &[Value::Text("b".into())])),
        0
    );
    assert_eq!(
        one(&db, "get locks select owner where id = 1"),
        Value::Text("a".into())
    );
    // By a `@unique` value as well, and no id handed out for nothing.
    let by_name = "insert locks {name: \"job\", owner: $1} if absent";
    assert_eq!(
        affected(run_with(&mut db, by_name, &[Value::Text("a".into())])),
        1
    );
    assert_eq!(
        affected(run_with(&mut db, by_name, &[Value::Text("b".into())])),
        0
    );
    assert_eq!(
        one(&db, "get locks where name = \"job\" count"),
        Value::Int(1)
    );
    // Several: the absent written, the held passed over, in one statement.
    assert_eq!(
        affected(run(
            &mut db,
            "put locks [{id: 1, owner: \"c\"}, {id: 7, owner: \"c\"}] if absent"
        )),
        1
    );
    // Plain `insert` still refuses, and `put` still writes over.
    assert!(matches!(
        run(&mut db, "insert locks {id: 7, owner: \"d\"}"),
        Err(Error::Duplicate(_))
    ));
    assert_eq!(affected(run(&mut db, "put locks {id: 7, owner: \"d\"}")), 1);
}

#[test]
fn an_insert_takes_the_place_of_a_row_past_its_time() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection locks (name text @unique, owner text, at timestamp @ttl(30s))",
    )
    .unwrap();
    db.set_clock(Some(1_000_000));
    let take = "insert locks {name: \"job\", owner: $1, at: now()} if absent";
    let by_id = "insert locks {id: 50, owner: $1, at: now()}";
    assert_eq!(
        affected(run_with(&mut db, take, &[Value::Text("a".into())])),
        1
    );
    run_with(&mut db, by_id, &[Value::Text("a".into())]).unwrap();
    // Held: the second is passed over, a plain insert refused.
    db.set_clock(Some(1_029_999));
    assert_eq!(
        affected(run_with(&mut db, take, &[Value::Text("b".into())])),
        0
    );
    assert!(run_with(&mut db, by_id, &[Value::Text("b".into())]).is_err());
    // Renewed while held, by its holder alone.
    let renew = "set locks {at: now()} where name = \"job\" and owner = $1";
    assert_eq!(
        affected(run_with(&mut db, renew, &[Value::Text("b".into())])),
        0
    );
    assert_eq!(
        affected(run_with(&mut db, renew, &[Value::Text("a".into())])),
        1
    );
    // Past its time from the renewal: anyone takes it, the old row let go.
    db.set_clock(Some(1_029_999 + 30_000));
    assert_eq!(
        affected(run_with(&mut db, renew, &[Value::Text("a".into())])),
        0
    );
    assert_eq!(
        affected(run_with(&mut db, take, &[Value::Text("b".into())])),
        1
    );
    assert_eq!(
        affected(run_with(&mut db, by_id, &[Value::Text("b".into())])),
        1
    );
    assert_eq!(
        one(&db, "get locks where name = \"job\" count"),
        Value::Int(1)
    );
    assert_eq!(
        one(&db, "get locks select owner where name = \"job\""),
        Value::Text("b".into())
    );
    assert_eq!(
        one(&db, "get locks select owner where id = 50"),
        Value::Text("b".into())
    );
}

/// N threads race for one lock: exactly one wins, and once its time is
/// past, another takes it.
#[test]
fn of_threads_racing_for_a_lock_exactly_one_wins() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection locks (name text @unique, owner text, at timestamp @ttl(30s))",
    )
    .unwrap();
    db.set_clock(Some(1_000_000));
    let db = Arc::new(Mutex::new(db));
    let race = |round: usize| {
        let wins: Vec<usize> = (0..16)
            .map(|t| {
                let db = db.clone();
                std::thread::spawn(move || {
                    let st = fenec_ql::parse_one(
                        "insert locks {name: \"job\", owner: $1, at: now()} if absent",
                    )
                    .unwrap();
                    let owner = Value::Text(format!("{round}-{t}"));
                    match db.lock().unwrap().execute_with(&st, &[owner]).unwrap() {
                        Response::Affected(n) => n,
                        r => panic!("{r:?}"),
                    }
                })
            })
            .map(|h| h.join().unwrap())
            .collect();
        wins.iter().sum::<usize>()
    };
    assert_eq!(race(0), 1);
    assert_eq!(race(1), 0);
    db.lock().unwrap().set_clock(Some(1_000_000 + 30_000));
    assert_eq!(race(2), 1);
    let d = db.lock().unwrap();
    let owner = one(&d, "get locks select owner where name = \"job\"");
    assert!(
        matches!(owner, Value::Text(ref s) if s.starts_with("2-")),
        "{owner:?}"
    );
}

/// Sixteen threads incrementing one counter: each read and write is under
/// the write lock, so none is lost.
#[test]
fn increments_from_many_threads_lose_none() {
    let mut db = counters();
    run(&mut db, "set hits {n: 0} where id = 1").unwrap();
    let db = Arc::new(Mutex::new(db));
    let st = Arc::new(fenec_ql::parse_one("set hits {n: n + 1} where key = $1").unwrap());
    let hs: Vec<_> = (0..16)
        .map(|_| {
            let (db, st) = (db.clone(), st.clone());
            std::thread::spawn(move || {
                for _ in 0..500 {
                    db.lock()
                        .unwrap()
                        .execute_with(&st, &[Value::Text("ip1".into())])
                        .unwrap();
                }
            })
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
    assert_eq!(
        one(&db.lock().unwrap(), "get hits select n where id = 1"),
        Value::Int(8_000)
    );
}

/// The record holds the value worked out, never the expression: a file
/// read again, and a replica handed the records, hold the same number.
#[test]
fn the_record_carries_the_result() {
    let dir = std::env::temp_dir().join(format!("fenecdb-writes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("db.fenec");
    {
        let mut db = fenec_core::fs::open(&path).unwrap();
        run(&mut db, "create collection hits (n int)").unwrap();
        run(&mut db, "put hits {id: 1, n: 41}").unwrap();
        db.set_clock(Some(5));
        run(&mut db, "set hits {n: n + 1} where id = 1").unwrap();
        db.sync().unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();
    // The statement's text is nowhere in the file; the result is.
    assert!(!bytes.windows(5).any(|w| w == b"n + 1"));
    let db = fenec_core::fs::open(&path).unwrap();
    assert_eq!(one(&db, "get hits select n where id = 1"), Value::Int(42));
    let _ = std::fs::remove_dir_all(&dir);
}
