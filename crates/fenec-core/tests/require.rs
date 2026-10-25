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
        // A write's clauses come in any order, as a `get`'s do.
        "del a require 1 where id = 2",
        "set a {n: 1} require 1 where id = 1 order id limit 1",
    ] {
        let s = parse(sql);
        let require = match &s {
            Statement::Put { require, .. }
            | Statement::Update { require, .. }
            | Statement::Delete { require, .. } => require.clone(),
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
        "del a require 1 require",
        "get a require",
        "get a require $1",
        "get a count require 1",
        "get a select sum(n) require 1",
        "get a lookup b on x require 1",
        "get b where x in (get a select id require 1)",
    ] {
        assert!(fenec_ql::parse_one(bad).is_err(), "{bad}");
    }
}

/// `get ... require <n>`: the rows a `get` answers -- after `limit` -- must
/// number `n`, or the block it is in is put back, as a write's count is.
/// A checkout asks "is the price still 12" beside the writes it guards.
#[test]
fn a_get_that_misses_its_count_puts_the_block_back() {
    for (sql, want) in [
        ("get a where n = 1 require 1", 1),
        ("get a limit 1 require 1", 1),
        ("get a require 0", 0),
        ("get a select n, count(*) group n require 2", 2),
        ("get a order n limit 5 require 1 lookup b on x", 1),
    ] {
        let Statement::Select(sel) = parse(sql) else {
            panic!("{sql}");
        };
        assert_eq!(sel.require.map(|r| r.n), Some(want), "{sql}");
    }

    let mut db = Database::new();
    for sql in [
        "create collection products (sku text @unique, price int, stock int)",
        "create collection orders (sku text @hash, price int)",
        "put products [{sku: \"tee\", price: 12, stock: 3}, {sku: \"cap\", price: 8, stock: 0}]",
    ] {
        run(&mut db, sql).unwrap();
    }
    let checkout: Vec<Statement> = [
        "get products where sku = $1 and price = $2 and stock > 0 require 1",
        "insert orders {sku: $1, price: $2}",
        "set products {stock: stock - 1} where sku = $1 require 1",
    ]
    .iter()
    .map(|s| parse(s))
    .collect();
    let buy = |db: &mut Database, sku: &str, price: i64| {
        let params = [Value::Text(sku.into()), Value::Int(price)];
        let block: Vec<(&Statement, &[Value])> =
            checkout.iter().map(|s| (s, &params[..])).collect();
        db.execute_block(&block)
    };
    // The price seen is the price paid.
    let answers = buy(&mut db, "tee", 12).unwrap();
    assert_eq!(answers[0].rows().unwrap().rows.len(), 1);
    // A price that moved, or no stock: refused at the read, nothing written.
    let seq = db.change_seq();
    for (sku, price) in [("tee", 11), ("cap", 8), ("hat", 5)] {
        let (at, e) = buy(&mut db, sku, price).unwrap_err();
        assert_eq!(at, 0);
        assert!(matches!(e, Error::Unmet(_)), "{e:?}");
    }
    assert_eq!(
        Error::Unmet("`get products` answered 0 rows, and requires 1".into()),
        buy(&mut db, "tee", 11).unwrap_err().1
    );
    assert_eq!(db.change_seq(), seq, "nothing landed");
    assert_eq!(int(one(&db, "get orders count")), 1);

    // Read in the block, it sees the writes before it there.
    let block: Vec<Statement> = [
        "set products {price: 13} where sku = \"tee\"",
        "get products where price = 13 require 1",
    ]
    .iter()
    .map(|s| parse(s))
    .collect();
    let refs: Vec<(&Statement, &[Value])> = block.iter().map(|s| (s, &[][..])).collect();
    assert!(db.execute_block(&refs).is_ok());

    // On its own, a read: the count after `limit`, refused as unmet.
    let e = db.query(&parse("get products require 1"), &[]).unwrap_err();
    assert_eq!(
        e,
        Error::Unmet("`get products` answered 2 rows, and requires 1".into())
    );
    assert!(db
        .query(&parse("get products limit 1 require 1"), &[])
        .is_ok());
    assert!(db
        .query(&parse("get products where sku = \"tee\" require 1"), &[])
        .is_ok());
    let mut out = String::new();
    assert_eq!(
        db.query_json(&parse("get products require 1"), &[], &mut out)
            .unwrap(),
        None,
        "the quick path counts no rows"
    );
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

/// The checkout of `fenecql-writing.html#checkout`, as written there: a
/// moved price, a used-up coupon and no stock each stop it at their line,
/// nothing written.
#[test]
fn the_docs_checkout_stops_at_the_line_that_fails() {
    let mut db = Database::new();
    for sql in [
        "create collection products (sku text @unique, price int, stock int)",
        "create collection coupons (code text @unique, off int, uses int, max_uses int)",
        "create collection orders (ref text @unique, sku text @hash, paid int)",
        "put products {sku: \"tee\", price: 12, stock: 1}",
        "put coupons {code: \"C\", off: 2, uses: 0, max_uses: 1}",
    ] {
        run(&mut db, sql).unwrap();
    }
    let lines: Vec<Statement> = [
        "get products where sku = $1 and price = $2 require 1",
        "get coupons where code = $3 and uses < max_uses require 1",
        "set products {stock: stock - 1} where sku = $1 and stock > 0 require 1",
        "set coupons {uses: uses + 1} where code = $3 require 1",
        "insert orders {ref: $4, sku: $1, paid: $2 - $5}",
    ]
    .iter()
    .map(|s| parse(s))
    .collect();
    let buy = |db: &mut Database, price: i64, code: &str, r: &str| {
        let params = [
            Value::Text("tee".into()),
            Value::Int(price),
            Value::Text(code.into()),
            Value::Text(r.into()),
            Value::Int(2),
        ];
        let block: Vec<(&Statement, &[Value])> = lines.iter().map(|s| (s, &params[..])).collect();
        db.execute_block(&block).map(|_| ()).map_err(|(at, _)| at)
    };
    assert_eq!(buy(&mut db, 11, "C", "o1"), Err(0));
    assert_eq!(buy(&mut db, 12, "X", "o1"), Err(1));
    assert_eq!(buy(&mut db, 12, "C", "o1"), Ok(()));
    assert_eq!(buy(&mut db, 12, "C", "o2"), Err(1), "the coupon is used up");
    run(&mut db, "set coupons {max_uses: 5}").unwrap();
    assert_eq!(buy(&mut db, 12, "C", "o2"), Err(2), "no stock");
    assert_eq!(int(one(&db, "get orders select sum(paid)")), 10);
}

/// `require <n> else '<reason>'`: the reason, written in the statement,
/// after the count wherever a `require` goes, in either quote.
#[test]
fn a_require_takes_its_reason_after_its_count() {
    let reason = |sql: &str| -> Option<(u64, String)> {
        let r = match parse(sql) {
            Statement::Put { require, .. }
            | Statement::Update { require, .. }
            | Statement::Delete { require, .. } => require,
            Statement::Select(sel) => sel.require,
            other => panic!("{other:?}"),
        }?;
        (!r.reason.is_empty()).then(|| (r.n, r.reason.into()))
    };
    for (sql, n, why) in [
        (
            "set a {n: n - 1} where id = 1 and n > 0 require 1 else 'insufficient funds'",
            1,
            "insufficient funds",
        ),
        (
            "del a where id = 1 require 1 else \"no such row\"",
            1,
            "no such row",
        ),
        ("put a {n: 1} require 1 else 'x'", 1, "x"),
        (
            "insert a {n: 1} if absent require 1 else 'taken' returning id",
            1,
            "taken",
        ),
        (
            "insert a {n: 1} returning id require 1 else 'taken'",
            1,
            "taken",
        ),
        (
            "del a require 0 else 'not empty' where n < 0",
            0,
            "not empty",
        ),
        (
            "get a where id = 1 limit 1 require 1 else 'no such account'",
            1,
            "no such account",
        ),
        ("get a require 1 else 'none' lookup b on x", 1, "none"),
        (
            "select n from a where id = 1 require 1 else 'none'",
            1,
            "none",
        ),
        // SQL's quote doubled, a backslash's escape, and letters past ASCII.
        ("get a require 0 else 'it''s held'", 0, "it's held"),
        ("get a require 0 else 'a \\\\ b'", 0, "a \\ b"),
        (
            "update a set n = 1 where id = 2 require 1 else 'yok böyle bir hesap'",
            1,
            "yok böyle bir hesap",
        ),
    ] {
        assert_eq!(reason(sql), Some((n, why.to_string())), "{sql}");
    }
    // Without `else`, none, as before.
    assert_eq!(reason("set a {n: 1} where id = 1 require 1"), None);
    // As long as a reason goes: 200 bytes, here 100 letters of two each.
    let long = "ş".repeat(100);
    assert_eq!(
        reason(&format!("get a require 1 else '{long}'")),
        Some((1, long.clone()))
    );
    // A sync replica writes the clause back as the parser reads it.
    for sql in [
        "put a {n: 1} require 1 else 'it''s \\\\ held'",
        "put a {n: 1} require 2",
    ] {
        let Statement::Put {
            require: Some(r), ..
        } = parse(sql)
        else {
            panic!("{sql}")
        };
        let mut text = String::from("put a {n: 1}");
        r.clause_into(&mut text);
        assert_eq!(text, sql);
    }
}

/// What a reason may not be, each refused naming what to write: a
/// parameter (the statement's own words, as its count is), empty, past
/// 200 bytes, or anything but a text.
#[test]
fn a_reason_is_a_text_of_the_statement() {
    let refused = |sql: &str| fenec_ql::parse(sql).unwrap_err().to_string();
    let e = refused("set a {n: 1} where id = 1 require 1 else $1");
    assert!(e.contains("never a parameter"), "{e}");
    let e = refused("get a require 1 else ''");
    assert!(e.contains("reason is empty"), "{e}");
    let e = refused(&format!("get a require 1 else '{}'", "x".repeat(201)));
    assert!(e.contains("at most 200 bytes, not 201"), "{e}");
    let e = refused(&format!("get a require 1 else '{}x'", "ş".repeat(100)));
    assert!(e.contains("at most 200 bytes, not 201"), "{e}");
    // A backtick, which the refusal quotes its statement with: the reason
    // is read back as the message up to the first `` (` ``.
    for sql in [
        "get a require 1 else 'see (`docs`)'",
        "set a {n: 1} where id = 1 require 1 else 'a ) (` b'",
        "del a where id = 1 require 1 else \"`\"",
    ] {
        let e = refused(sql);
        assert!(e.contains("reason holds no backtick"), "{sql}: {e}");
    }
    for (sql, found) in [
        ("del a where id = 1 require 1 else", "end of input"),
        ("del a where id = 1 require 1 else 5", "5"),
        ("del a where id = 1 require 1 else no", "`no`"),
        ("put a {n: 1} require 1 else set {n: 2}", "`set`"),
    ] {
        let e = refused(sql);
        assert!(e.contains("takes its reason in quotes"), "{sql}: {e}");
        assert!(e.ends_with(&format!("found {found}")), "{sql}: {e}");
    }
    // `else` with no `require` before it is no clause: as before, a word
    // that begins no statement.
    let e = refused("del a where id = 1 else 'x'");
    assert!(e.contains("unknown command `else`"), "{e}");
}

/// The reason in the refusal: ahead of the count's words, and whole in
/// `Error::reason`, for a write and for a `get`; the block put back.
#[test]
fn an_unmet_require_says_its_reason() {
    let mut db = ledger(2, 100);
    let seq = db.change_seq();
    let e = run(
        &mut db,
        "set accounts {balance: balance - 500} where name = \"a0\" and balance >= 500 \
         require 1 else 'insufficient funds'",
    )
    .unwrap_err();
    assert_eq!(e.reason(), Some("insufficient funds"));
    assert_eq!(
        e.to_string(),
        "unmet: insufficient funds (`set accounts` wrote 0 rows, and requires 1)"
    );
    // One without a reason says none.
    let e = run(&mut db, "del accounts where name = \"nobody\" require 1").unwrap_err();
    assert_eq!(e.reason(), None);
    // A `get`'s, alone, parentheses and all.
    let e = db
        .query(
            &parse("get accounts where name = \"nobody\" limit 1 require 1 else 'no (such) one'"),
            &[],
        )
        .unwrap_err();
    assert_eq!(e.reason(), Some("no (such) one"));
    assert_eq!(
        e,
        Error::Unmet("no (such) one (`get accounts` answered 0 rows, and requires 1)".into())
    );
    // The transfer, each guard saying why: the block stops at the one that
    // failed, says its reason, and nothing of it lands.
    let stmts: Vec<Statement> = [
        "get accounts where name = $1 limit 1 require 1 else 'no such account'",
        "set accounts {balance: balance - $3} where name = $1 and balance >= $3 require 1 \
         else 'insufficient funds'",
        "set accounts {balance: balance + $3} where name = $2 require 1 else 'no account to credit'",
        "insert journal [{tx: $4, account: $1, amount: 0 - $3}, {tx: $4, account: $2, amount: $3}]",
    ]
    .iter()
    .map(|s| parse(s))
    .collect();
    for (from, to, amount, at, why) in [
        ("ghost", "a1", 10, 0, "no such account"),
        ("a0", "a1", 101, 1, "insufficient funds"),
        ("a0", "ghost", 10, 2, "no account to credit"),
    ] {
        let (stopped, e) = transfer(&mut db, &stmts, from, to, amount, 1).unwrap_err();
        assert_eq!((stopped, e.reason()), (at, Some(why)), "{from} -> {to}");
    }
    assert_eq!(db.change_seq(), seq, "nothing landed");
    assert_eq!(int(one(&db, "get accounts select sum(balance)")), 200);
    assert_eq!(int(one(&db, "get journal count")), 0);
    assert!(transfer(&mut db, &stmts, "a0", "a1", 10, 1).is_ok());
    // A quoted collection's name holding the count's opening, `` (` ``:
    // the reason is the message up to the first, never the last, which
    // read it as "my reason (`set x".
    run(&mut db, "create collection \"x (`y\" (a int)").unwrap();
    for sql in [
        "set \"x (`y\" {a: 1} where a = 5 require 1 else 'my reason'",
        "del \"x (`y\" where a = 5 require 1 else 'my reason'",
        "select a from \"x (`y\" where a = 5 require 1 else 'my reason'",
    ] {
        let e = run(&mut db, sql).unwrap_err();
        assert_eq!(e.reason(), Some("my reason"), "{sql}: {e}");
    }
    let e = run(&mut db, "del \"x (`y\" where a = 5 require 1").unwrap_err();
    assert_eq!(e.reason(), None, "{e}");
}
