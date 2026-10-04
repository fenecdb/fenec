//! `require <n>`: a write that must write exactly `n` rows, or is refused
//! and its block put back whole. A `set` whose `where` matched nothing
//! answered `affected 0` and its `/batch` went on, so a transfer's debit
//! that found too little money was passed over and its credit made money.

use fenec_core::prelude::*;
use std::sync::{Arc, RwLock};

fn parse(sql: &str) -> Statement {
    fenec_ql::parse_one(sql).expect("parse")
}

fn run(db: &mut Database, sql: &str) -> Result<Response> {
    db.execute(&parse(sql))
}

fn one(db: &Database, sql: &str) -> Value {
    let r = db.query(&parse(sql), &[]).unwrap();
    r.rows().unwrap().rows[0].values[0].clone()
}

fn int(v: Value) -> i64 {
    match v {
        Value::Int(i) => i,
        Value::Float(f) => f as i64,
        // A sum over no rows.
        Value::Null => 0,
        v => panic!("{v:?}"),
    }
}

fn ledger(accounts: i64, each: i64) -> Database {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection accounts (name text @unique, balance int)",
    )
    .unwrap();
    run(
        &mut db,
        "create collection journal (tx int @hash, account text @hash, amount int)",
    )
    .unwrap();
    for a in 0..accounts {
        run(
            &mut db,
            &format!("put accounts {{name: \"a{a}\", balance: {each}}}"),
        )
        .unwrap();
    }
    db
}

/// The transfer as a `/batch` holds it: the debit only where the money is,
/// the credit only where the account is, each required to write its row,
/// and the journal's two legs.
const TRANSFER: [&str; 3] = [
    "set accounts {balance: balance - $3} where name = $1 and balance >= $3 require 1",
    "set accounts {balance: balance + $3} where name = $2 require 1",
    "insert journal [{tx: $4, account: $1, amount: 0 - $3}, {tx: $4, account: $2, amount: $3}]",
];

fn transfer(
    db: &mut Database,
    stmts: &[Statement],
    from: &str,
    to: &str,
    amount: i64,
    tx: i64,
) -> std::result::Result<Vec<Response>, (usize, Error)> {
    let params = [
        Value::Text(from.into()),
        Value::Text(to.into()),
        Value::Int(amount),
        Value::Int(tx),
    ];
    let block: Vec<(&Statement, &[Value])> = stmts.iter().map(|s| (s, &params[..])).collect();
    db.execute_block(&block)
}

#[test]
fn require_parses_after_each_write_and_nowhere_else() {
    for sql in [
        "set a {n: 1} where id = 1 require 1",
        "set a {n: 1} require 0",
        "del a where id = 1 require 1",
        "del a require 3",
        "put a {n: 1} require 1",
        "insert a [{n: 1}, {n: 2}] require 2",
        "insert a {n: 1} if absent require 1",
    ] {
        let s = parse(sql);
        let require = match &s {
            Statement::Put { require, .. }
            | Statement::Update { require, .. }
            | Statement::Delete { require, .. } => *require,
            other => panic!("{other:?}"),
        };
        assert!(require.is_some(), "{sql}");
    }
    assert!(matches!(
        parse("set a {n: 1} where id = 1"),
        Statement::Update { require: None, .. }
    ));
    for bad in [
        "set a {n: 1} require",
        "set a {n: 1} require -1",
        "set a {n: 1} require $1",
        "del a require 1 where id = 2",
        "get a require 1",
    ] {
        assert!(fenec_ql::parse_one(bad).is_err(), "{bad}");
    }
}

#[test]
fn a_write_that_misses_its_count_is_refused_and_put_back() {
    let mut db = ledger(2, 100);
    let seq = db.change_seq();
    // A set over two rows that requires one: refused, neither row changed.
    let e = run(&mut db, "set accounts {balance: 0} require 1").unwrap_err();
    assert_eq!(
        e,
        Error::Unmet("`set accounts` wrote 2 rows, and requires 1".into())
    );
    assert_eq!(int(one(&db, "get accounts select sum(balance)")), 200);
    assert_eq!(db.change_seq(), seq, "nothing landed");
    // Met, it answers as before.
    assert!(matches!(
        run(
            &mut db,
            "set accounts {balance: balance + 1} where name = \"a0\" require 1"
        ),
        Ok(Response::Affected(1))
    ));
    // A del, a put and `if absent` count the same way.
    let e = run(&mut db, "del accounts where name = \"nobody\" require 1").unwrap_err();
    assert_eq!(
        e,
        Error::Unmet("`del accounts` wrote 0 rows, and requires 1".into())
    );
    let e = run(
        &mut db,
        "insert accounts {name: \"a0\", balance: 5} if absent require 1",
    )
    .unwrap_err();
    assert!(matches!(e, Error::Unmet(_)), "{e:?}");
    assert_eq!(int(one(&db, "get accounts count")), 2);
    assert!(run(
        &mut db,
        "insert accounts {name: \"a9\", balance: 0} if absent require 1"
    )
    .is_ok());
    // `require 0`: none may match.
    assert!(run(&mut db, "del accounts where balance < 0 require 0").is_ok());
}

/// The whole block goes back: the debit made before the credit that found
/// no account, and the journal's legs after it never run.
#[test]
fn a_credit_to_a_missing_account_puts_the_transfer_back() {
    let mut db = ledger(2, 100);
    let stmts: Vec<Statement> = TRANSFER.iter().map(|s| parse(s)).collect();
    let seq = db.change_seq();
    let (at, e) = transfer(&mut db, &stmts, "a0", "nobody", 30, 1).unwrap_err();
    assert_eq!(at, 1, "the credit");
    assert!(matches!(e, Error::Unmet(_)), "{e:?}");
    assert_eq!(
        one(&db, "get accounts select balance where name = \"a0\""),
        Value::Int(100)
    );
    assert_eq!(int(one(&db, "get journal count")), 0);
    assert_eq!(db.change_seq(), seq);
    // An overdraft: the debit finds no row with the money.
    let (at, _) = transfer(&mut db, &stmts, "a0", "a1", 101, 2).unwrap_err();
    assert_eq!(at, 0, "the debit");
    assert_eq!(int(one(&db, "get accounts select sum(balance)")), 200);
    // Without `require` the same overdraft made money: the debit wrote
    // nothing and the credit went ahead.
    let loose: Vec<Statement> = TRANSFER
        .iter()
        .map(|s| parse(&s.replace(" require 1", "")))
        .collect();
    transfer(&mut db, &loose, "a0", "a1", 101, 3).unwrap();
    assert_eq!(int(one(&db, "get accounts select sum(balance)")), 301);
}

/// Eight threads moving money at random among few accounts, so overdrafts
/// and missing accounts are common: the sum never moves, no balance goes
/// below zero, and the journal holds two legs for each transfer that
/// landed and accounts for every balance.
#[test]
fn concurrent_transfers_neither_make_nor_lose_money() {
    const ACCOUNTS: i64 = 6;
    const START: i64 = 100;
    const THREADS: u64 = 8;
    const EACH: u64 = 600;
    let db = Arc::new(RwLock::new(ledger(ACCOUNTS, START)));
    let stmts: Arc<Vec<Statement>> = Arc::new(TRANSFER.iter().map(|s| parse(s)).collect());
    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let (db, stmts) = (Arc::clone(&db), Arc::clone(&stmts));
            std::thread::spawn(move || {
                let mut seed = 0x9e37_79b9_7f4a_7c15u64 ^ (t + 1);
                let mut next = |n: u64| {
                    seed = seed
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    (seed >> 33) % n
                };
                let (mut landed, mut refused) = (0u64, 0u64);
                for i in 0..EACH {
                    let from = format!("a{}", next(ACCOUNTS as u64));
                    // One in eight credits names an account that is not there.
                    let to = match next(8) {
                        0 => "ghost".to_string(),
                        _ => format!("a{}", next(ACCOUNTS as u64)),
                    };
                    let amount = 1 + next(60) as i64;
                    let tx = (t * EACH + i) as i64;
                    let mut g = db.write().unwrap();
                    match transfer(&mut g, &stmts, &from, &to, amount, tx) {
                        Ok(_) => landed += 1,
                        Err((_, Error::Unmet(_))) => refused += 1,
                        Err((_, e)) => panic!("{e:?}"),
                    }
                }
                (landed, refused)
            })
        })
        .collect();
    let (mut landed, mut refused) = (0, 0);
    for h in handles {
        let (l, r) = h.join().unwrap();
        landed += l;
        refused += r;
    }
    assert_eq!(landed + refused, THREADS * EACH);
    assert!(
        landed > 0 && refused > 0,
        "{landed} landed, {refused} refused"
    );
    let db = db.read().unwrap();
    assert_eq!(
        int(one(&db, "get accounts select sum(balance)")),
        ACCOUNTS * START
    );
    assert_eq!(int(one(&db, "get accounts where balance < 0 count")), 0);
    assert_eq!(int(one(&db, "get journal count")) as u64, 2 * landed);
    assert_eq!(int(one(&db, "get journal select sum(amount)")), 0);
    for a in 0..ACCOUNTS {
        let name = format!("a{a}");
        let balance = int(one(
            &db,
            &format!("get accounts select balance where name = \"{name}\""),
        ));
        let moved = int(one(
            &db,
            &format!("get journal select sum(amount) where account = \"{name}\""),
        ));
        assert_eq!(balance, START + moved, "{name}");
    }
}
