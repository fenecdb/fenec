//! `sum`, `avg`, `min`, `max` and `count(*)`, whole and per `group`,
//! against the same numbers worked out by hand over the rows.

use fenec_core::prelude::*;
use std::collections::BTreeMap;

fn exec(db: &mut Database, sql: &str) {
    db.execute(&fenec_ql::parse_one(sql).expect("parse"))
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn rows(db: &Database, sql: &str) -> (Vec<String>, Vec<Vec<Value>>) {
    let r = db
        .query(&fenec_ql::parse_one(sql).expect("parse"), &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    let rs = r.rows().unwrap();
    (
        rs.columns.clone(),
        rs.rows.iter().map(|r| r.values.clone()).collect(),
    )
}

fn error(db: &Database, sql: &str) -> String {
    match fenec_ql::parse_one(sql) {
        Err(e) => e.to_string(),
        Ok(stmt) => db.query(&stmt, &[]).expect_err(sql).to_string(),
    }
}

struct Rng(u64);
impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// An order: its status, its total (null one time in seven), its weight.
type Order = (String, Option<i64>, f64);

/// Which orders a filter keeps.
type Keep = fn(&Order) -> bool;

fn orders(db: &mut Database, n: usize) -> Vec<Order> {
    exec(
        db,
        "create collection orders (status text @hash, total int @sorted, weight float, at timestamp)",
    );
    let mut rng = Rng(0x2545f4914f6cdd1d);
    let mut out = Vec::new();
    for i in 0..n {
        let status = ["paid", "open", "void", "held"][rng.below(4) as usize].to_string();
        let total = (rng.below(7) != 0).then(|| rng.below(1000) as i64 - 200);
        let weight = rng.below(10_000) as f64 / 100.0;
        let total_sql = total.map_or("null".to_string(), |t| t.to_string());
        exec(
            db,
            &format!(
                "put orders {{status: \"{status}\", total: {total_sql}, weight: {weight}, \
                 at: \"2026-01-{:02}T00:00:00Z\"}}",
                1 + i % 28
            ),
        );
        out.push((status, total, weight));
    }
    out
}

/// What a group's aggregates should come to, worked out by hand.
fn by_hand(rows: &[&Order]) -> Vec<Value> {
    let totals: Vec<i64> = rows.iter().filter_map(|o| o.1).collect();
    let weights: Vec<f64> = rows.iter().map(|o| o.2).collect();
    let opt = |v: Option<Value>| v.unwrap_or(Value::Null);
    vec![
        Value::Int(rows.len() as i64),
        opt((!totals.is_empty()).then(|| Value::Int(totals.iter().sum()))),
        opt((!totals.is_empty()).then(|| {
            Value::Float(totals.iter().map(|t| *t as f64).sum::<f64>() / totals.len() as f64)
        })),
        opt(totals.iter().min().map(|m| Value::Int(*m))),
        opt(totals.iter().max().map(|m| Value::Int(*m))),
        // A sum over no rows is null, as SQL's is, not 0.
        opt((!weights.is_empty()).then(|| Value::Float(weights.iter().sum()))),
    ]
}

const LIST: &str = "count(*), sum(total), avg(total), min(total), max(total), sum(weight)";

fn close(a: &[Value], b: &[Value]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| match (x, y) {
            (Value::Float(x), Value::Float(y)) => (x - y).abs() <= 1e-9 * x.abs().max(1.0),
            _ => x == y,
        })
}

#[test]
fn aggregates_over_the_rows_match_the_sums_by_hand() {
    let mut db = Database::new();
    let all = orders(&mut db, 2_000);

    // Whole: one row, in the list's order, under the list's labels.
    let (cols, got) = rows(&db, &format!("get orders select {LIST}"));
    assert_eq!(
        cols,
        [
            "count",
            "sum(total)",
            "avg(total)",
            "min(total)",
            "max(total)",
            "sum(weight)"
        ]
    );
    let everything: Vec<&Order> = all.iter().collect();
    assert!(close(&got[0], &by_hand(&everything)), "{got:?}");

    // Filtered -- through the hash index and through the ordered one.
    let filters: [(&str, Keep); 3] = [
        ("status = \"paid\"", |o| o.0 == "paid"),
        ("total >= 500", |o| o.1.is_some_and(|t| t >= 500)),
        ("status = \"none\"", |_| false),
    ];
    for (filter, keep) in filters {
        let (_, got) = rows(&db, &format!("get orders select {LIST} where {filter}"));
        let kept: Vec<&Order> = all.iter().filter(|o| keep(o)).collect();
        assert!(close(&got[0], &by_hand(&kept)), "{filter}: {got:?}");
    }

    // Grouped: a row per status, by status unless ordered otherwise.
    let (cols, got) = rows(
        &db,
        &format!("get orders select status, {LIST} where weight < 80.0 group status"),
    );
    assert_eq!(cols[0], "status");
    let mut want: BTreeMap<&str, Vec<&Order>> = BTreeMap::new();
    for o in all.iter().filter(|o| o.2 < 80.0) {
        want.entry(&o.0).or_default().push(o);
    }
    assert_eq!(got.len(), want.len());
    for (row, (status, group)) in got.iter().zip(&want) {
        assert_eq!(row[0], Value::Text(status.to_string()));
        assert!(close(&row[1..], &by_hand(group)), "{status}: {row:?}");
    }
}

#[test]
fn groups_order_and_page_by_any_column_of_the_list() {
    let mut db = Database::new();
    let all = orders(&mut db, 500);
    let mut sums: Vec<(i64, String)> = ["paid", "open", "void", "held"]
        .iter()
        .map(|s| {
            let sum = all.iter().filter(|o| o.0 == *s).filter_map(|o| o.1).sum();
            (sum, s.to_string())
        })
        .collect();
    sums.sort_by(|a, b| b.cmp(a));

    let (_, got) = rows(
        &db,
        "get orders select status, sum(total) group status order sum(total) desc limit 2",
    );
    let want: Vec<Vec<Value>> = sums[..2]
        .iter()
        .map(|(sum, s)| vec![Value::Text(s.clone()), Value::Int(*sum)])
        .collect();
    assert_eq!(got, want);
    let (_, got) = rows(
        &db,
        "get orders select sum(total), status group by status order sum(total) desc offset 3",
    );
    assert_eq!(
        got,
        vec![vec![Value::Int(sums[3].0), Value::Text(sums[3].1.clone())]]
    );
    // The classic order, and a count ordered on.
    let (_, got) = rows(
        &db,
        "select status, count(*) from orders group status order count(*) desc, status",
    );
    assert_eq!(got.len(), 4);
    assert!(got.windows(2).all(|w| w[0][1].cmp_value(&w[1][1]).is_ge()));
}

#[test]
fn nulls_and_empty_sets_answer_as_sql_does() {
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection t (g text, n int, x float, s text, at timestamp)",
    );
    exec(&mut db, "put t {g: \"a\"}");
    exec(
        &mut db,
        "put t {g: \"a\", n: 3, s: \"pear\", at: \"2026-03-01T00:00:00Z\"}",
    );
    exec(
        &mut db,
        "put t {g: \"b\", n: 5, x: 1.5, s: \"apple\", at: \"2025-01-01T00:00:00Z\"}",
    );
    exec(&mut db, "put t {n: 9}");

    // Rows with no value count as rows and nowhere else.
    let (_, got) = rows(
        &db,
        "get t select count(*), sum(n), avg(x), min(s), max(at) where g = \"a\"",
    );
    assert_eq!(
        got[0],
        [
            Value::Int(2),
            Value::Int(3),
            Value::Null,
            Value::Text("pear".into()),
            Value::Timestamp(1_772_323_200_000)
        ]
    );
    // Nothing matched: a count of 0 and nothing else, as one row; no groups.
    let (_, got) = rows(&db, "get t select count(*), sum(n), max(s) where n > 100");
    assert_eq!(got, vec![vec![Value::Int(0), Value::Null, Value::Null]]);
    let (_, got) = rows(&db, "get t select g, count(*) where n > 100 group g");
    assert!(got.is_empty());
    // The null key is a group of its own, first.
    let (_, got) = rows(&db, "get t select g, count(*), sum(n) group g");
    assert_eq!(
        got,
        vec![
            vec![Value::Null, Value::Int(1), Value::Int(9)],
            vec![Value::Text("a".into()), Value::Int(2), Value::Int(3)],
            vec![Value::Text("b".into()), Value::Int(1), Value::Int(5)],
        ]
    );
    // An int sum past 64 bits is an error, not a wrapped number.
    exec(&mut db, "put t {g: \"c\", n: 9223372036854775807}");
    exec(&mut db, "put t {g: \"c\", n: 1}");
    assert!(error(&db, "get t select sum(n) where g = \"c\"").contains("64-bit"));
}

#[test]
fn what_does_not_combine_is_refused() {
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection t (g text, n int, v vector<2> @hnsw(cosine), body text @text)",
    );
    exec(
        &mut db,
        "put t {g: \"a\", n: 1, v: [1.0, 0.0], body: \"x\"}",
    );
    for (sql, says) in [
        ("get t select sum(g)", "int or float"),
        ("get t select max(v)", "an order"),
        ("get t select sum(n) near v [1.0, 0.0]", "near"),
        ("get t select sum(n) match body \"x\"", "match"),
        ("get t select sum(n) order n", "one row"),
        ("get t select sum(n) limit 3", "one row"),
        ("get t select g, sum(n)", "neither aggregated nor grouped"),
        (
            "get t select n, count(*) group g",
            "neither aggregated nor grouped",
        ),
        ("get t select g group g", "needs an aggregate"),
        ("get t select count(n)", "count(*)"),
        ("get t select median(n)", "not an aggregate"),
        ("get t select g, count(*) group g order n", "not a column"),
        ("get t select sum(nope)", "nope"),
    ] {
        let e = error(&db, sql);
        assert!(e.contains(says), "{sql}: {e}");
    }
}

#[test]
fn explain_shows_the_index_and_the_fold() {
    let mut db = Database::new();
    orders(&mut db, 200);
    let (_, got) = rows(
        &db,
        "explain get orders select status, sum(total) where status = \"paid\" group status",
    );
    let plan: Vec<String> = got
        .iter()
        .map(|r| match &r[0] {
            Value::Text(t) => t.clone(),
            v => panic!("{v:?}"),
        })
        .collect();
    assert!(plan.iter().any(|s| s.contains("hash")), "{plan:?}");
    assert!(plan.iter().any(|s| s.starts_with("aggregate:")), "{plan:?}");
}
