//! What `in (get ...)` costs: `make subquery-bench`.
//!
//! Each query is timed three ways where they answer the same question: as
//! written, with the inner `get`'s values written out as `in [$1, ...]`
//! (the parameters made beforehand, so their parse is not timed against the
//! subquery's), and as the `lookup ... required` that asks it from the
//! other side. The median of `RUNS`, after one run to build the indexes.

use fenec_core::prelude::*;
use std::time::Instant;

const RUNS: usize = 21;

fn exec(db: &mut Database, sql: &str) {
    for s in fenec_ql::parse(sql).expect("parse") {
        db.execute(&s).expect("execute");
    }
}

fn median_ms(db: &Database, sql: &str, params: &[Value]) -> (f64, Value) {
    let stmt = fenec_ql::parse_one(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let answer = |db: &Database| match db.query(&stmt, params) {
        Ok(Response::Rows(rs)) => rs.rows[0].values[0].clone(),
        other => panic!("{sql}: {other:?}"),
    };
    let first = answer(db);
    let mut t: Vec<f64> = (0..RUNS)
        .map(|_| {
            let s = Instant::now();
            std::hint::black_box(answer(db));
            s.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    t.sort_by(|a, b| a.total_cmp(b));
    (t[RUNS / 2], first)
}

/// The inner `get`'s values, as parameters and the places that name them.
fn written(db: &Database, inner: &str) -> (Vec<Value>, String) {
    let Ok(Response::Rows(rs)) = db.query(&fenec_ql::parse_one(inner).unwrap(), &[]) else {
        panic!("{inner}");
    };
    let vals: Vec<Value> = rs
        .rows
        .into_iter()
        .map(|r| r.values.into_iter().next().unwrap())
        .filter(|v| !v.is_null())
        .collect();
    let places: Vec<String> = (1..=vals.len()).map(|i| format!("${i}")).collect();
    (vals, format!("[{}]", places.join(", ")))
}

fn row(what: &str, (ms, n): (f64, Value)) {
    let n = match n {
        Value::Int(n) => n.to_string(),
        v => format!("{v:?}"),
    };
    println!("  {what:<44} {ms:>9.3} ms  ({n})");
}

fn main() {
    let customers: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(20_000);
    let orders = customers * 10;
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection customers (country text @hash, tier int, code text);
         create collection orders (customer int @hash, code text, status text @hash,
                                   total float)",
    );
    let mut docs = Vec::with_capacity(customers);
    for i in 0..customers {
        docs.push(format!(
            "{{country: \"c{}\", tier: {}, code: \"k{}\"}}",
            i % 10,
            i % 7,
            i + 1
        ));
    }
    exec(&mut db, &format!("put customers [{}]", docs.join(",")));
    let mut x: u64 = 0x9e3779b97f4a7c15;
    for chunk in 0..orders / 10_000 {
        let mut docs = Vec::with_capacity(10_000);
        for _ in 0..10_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let c = 1 + x % customers as u64;
            let total = (x >> 20) % 100_000;
            // One order in a hundred is rare: the side a `lookup` starts
            // from when its bucket is small.
            let status = if (x >> 40).is_multiple_of(100) {
                "rare"
            } else {
                "open"
            };
            docs.push(format!(
                "{{customer: {c}, code: \"k{c}\", status: \"{status}\", total: {}.5}}",
                total / 100
            ));
        }
        exec(&mut db, &format!("put orders [{}]", docs.join(",")));
        let _ = chunk;
    }
    println!("{customers} customers, {orders} orders; median of {RUNS}");

    println!(
        "orders of one country's customers ({} values)",
        customers / 10
    );
    let inner = r#"get customers select id where country = "c3""#;
    let (vals, list) = written(&db, inner);
    row(
        "`customer in (get ...)`, @hash",
        median_ms(
            &db,
            &format!("get orders where customer in ({inner}) count"),
            &[],
        ),
    );
    row(
        "`customer in [..]` written out",
        median_ms(
            &db,
            &format!("get orders where customer in {list} count"),
            &vals,
        ),
    );
    let inner_code = r#"get customers select code where country = "c3""#;
    let (vals, list) = written(&db, inner_code);
    row(
        "`code in (get ...)`, no index: the scan",
        median_ms(
            &db,
            &format!("get orders where code in ({inner_code}) count"),
            &[],
        ),
    );
    row(
        "`code in [..]` written out",
        median_ms(
            &db,
            &format!("get orders where code in {list} count"),
            &vals,
        ),
    );

    println!("customers with an order past 990 (the inner `get` scans orders)");
    let inner = "get orders select customer where total > 990";
    let (vals, list) = written(&db, inner);
    println!("  ({} values)", vals.len());
    row(
        "`id in (get ...)`",
        median_ms(
            &db,
            &format!("get customers where id in ({inner}) count"),
            &[],
        ),
    );
    row(
        "`id in [..]` written out",
        median_ms(
            &db,
            &format!("get customers where id in {list} count"),
            &vals,
        ),
    );
    row(
        "`lookup orders ... required where total > 990`",
        median_ms(
            &db,
            "get customers count lookup orders on customer required where total > 990",
            &[],
        ),
    );

    println!("customers with a rare order (the inner `get` reads a bucket)");
    let inner = r#"get orders select customer where status = "rare""#;
    let (vals, list) = written(&db, inner);
    println!("  ({} values)", vals.len());
    row(
        "`id in (get ...)`",
        median_ms(
            &db,
            &format!("get customers where id in ({inner}) count"),
            &[],
        ),
    );
    row(
        "`id in [..]` written out",
        median_ms(
            &db,
            &format!("get customers where id in {list} count"),
            &vals,
        ),
    );
    row(
        "`lookup orders ... required where status = rare`",
        median_ms(
            &db,
            r#"get customers count lookup orders on customer required where status = "rare""#,
            &[],
        ),
    );
    row(
        "  and tier = 3 on the parent: `id in (get ...)`",
        median_ms(
            &db,
            &format!("get customers where tier = 3 and id in ({inner}) count"),
            &[],
        ),
    );
    row(
        "  and tier = 3: `lookup ... required`",
        median_ms(
            &db,
            r#"get customers where tier = 3 count lookup orders on customer required where status = "rare""#,
            &[],
        ),
    );
}
