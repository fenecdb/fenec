//! Expressions in a select list, aggregates over them, `group` by several
//! keys and by `bucket`, `count(distinct ...)`, `first` and `last` --
//! each against the same numbers worked out by hand over the rows.

use fenec_core::prelude::*;
use std::collections::{BTreeMap, BTreeSet};

fn exec(db: &mut Database, sql: &str) {
    db.execute(&fenec_ql::parse_one(sql).expect("parse"))
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn rows_with(db: &Database, sql: &str, params: &[Value]) -> (Vec<String>, Vec<Vec<Value>>) {
    let r = db
        .query(&fenec_ql::parse_one(sql).expect("parse"), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    let rs = r.rows().unwrap();
    (
        rs.columns.clone(),
        rs.rows.iter().map(|r| r.values.clone()).collect(),
    )
}

fn rows(db: &Database, sql: &str) -> Vec<Vec<Value>> {
    rows_with(db, sql, &[]).1
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

const T0: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z
const MIN: i64 = 60_000;

/// A tick: its symbol, time, price (in cents, null one time in eleven)
/// and quantity, written in an order other than the time's.
type Tick = (String, i64, Option<i64>, i64);

fn ticks(db: &mut Database, n: usize) -> Vec<Tick> {
    exec(
        db,
        "create collection ticks (sym text @hash, at timestamp @sorted, px int, qty int, \
         meta json)",
    );
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut out = Vec::new();
    for _ in 0..n {
        let sym = ["AAA", "BBB", "CCC"][rng.below(3) as usize].to_string();
        // Times repeat, so `first` and `last` meet ties.
        let at = T0 + rng.below(30 * MIN as u64 / 1000) as i64 * 1000;
        let px = (rng.below(11) != 0).then(|| 10_000 + rng.below(500) as i64);
        let qty = 1 + rng.below(9) as i64;
        let px_sql = px.map_or("null".to_string(), |p| p.to_string());
        exec(
            db,
            &format!(
                "put ticks {{sym: \"{sym}\", at: {at}, px: {px_sql}, qty: {qty}, \
                 meta: {{venue: \"v{}\", size: {qty}}}}}",
                qty % 2
            ),
        );
        out.push((sym, at, px, qty));
    }
    out
}

#[test]
fn a_select_list_works_out_expressions_over_each_row() {
    let mut db = Database::new();
    let all = ticks(&mut db, 50);
    let (cols, got) = rows_with(
        &db,
        "get ticks select sym, px * qty as notional, greatest(px, 10200) as floor, \
         case when qty > 4 then \"big\" else \"small\" end as size, bucket(at, 15m) as q \
         order id limit 50",
        &[],
    );
    assert_eq!(cols, ["sym", "notional", "floor", "size", "q"]);
    for (t, row) in all.iter().zip(&got) {
        assert_eq!(row[0], Value::Text(t.0.clone()));
        assert_eq!(row[1], t.2.map_or(Value::Null, |p| Value::Int(p * t.3)));
        assert_eq!(
            row[2],
            Value::Int(t.2.map_or(10_200, |p| p.max(10_200))),
            "greatest passes nulls over"
        );
        let size = if t.3 > 4 { "big" } else { "small" };
        assert_eq!(row[3], Value::Text(size.into()));
        assert_eq!(row[4], Value::Timestamp(t.1 - (t.1 - T0) % (15 * MIN)));
    }
    // A parameter in a column, past the filter's.
    let (_, got) = rows_with(
        &db,
        "get ticks select qty * $2 as twice where qty > $1 order id limit 1",
        &[Value::Int(0), Value::Int(2)],
    );
    assert_eq!(got[0][0], Value::Int(all[0].3 * 2));
    // A field named again, and a path.
    let (cols, got) = rows_with(
        &db,
        "get ticks select qty as n, meta.size as s order id limit 1",
        &[],
    );
    assert_eq!(cols, ["n", "s"]);
    assert_eq!(got[0], [Value::Int(all[0].3), Value::Int(all[0].3)]);
}

#[test]
fn aggregates_fold_expressions_and_group_by_several_keys() {
    let mut db = Database::new();
    let all = ticks(&mut db, 400);
    // By hand: per symbol and 5-minute bucket.
    // Rows, price times quantity, quantity, the highest price, quantities.
    type Bar = (i64, i64, i64, i64, BTreeSet<i64>);
    let mut want: BTreeMap<(String, i64), Bar> = BTreeMap::new();
    for t in &all {
        let b = t.1 - (t.1 - T0) % (5 * MIN);
        let e = want
            .entry((t.0.clone(), b))
            .or_insert((0, 0, 0, i64::MIN, BTreeSet::new()));
        e.0 += 1;
        if let Some(p) = t.2 {
            e.1 += p * t.3;
            e.2 += t.3;
            e.3 = e.3.max(p);
        }
        e.4.insert(t.3);
    }
    let got = rows(
        &db,
        "get ticks select sym, bucket(at, 5m) as bar, count(*), sum(px * qty) as pv, \
         sum(qty) as vol, max(px), count(distinct qty) group sym, bar",
    );
    assert_eq!(got.len(), want.len());
    for (row, ((sym, b), w)) in got.iter().zip(&want) {
        assert_eq!(row[0], Value::Text(sym.clone()));
        assert_eq!(row[1], Value::Timestamp(*b));
        assert_eq!(row[2], Value::Int(w.0));
        assert_eq!(row[6], Value::Int(w.4.len() as i64));
        // `sum(qty)` counts the null-priced ticks' quantity too.
        let vol: i64 = all
            .iter()
            .filter(|t| t.0 == *sym && t.1 - (t.1 - T0) % (5 * MIN) == *b)
            .map(|t| t.3)
            .sum();
        assert_eq!(row[4], Value::Int(vol));
        if w.3 == i64::MIN {
            assert_eq!(row[3], Value::Null);
            assert_eq!(row[5], Value::Null);
        } else {
            assert_eq!(row[3], Value::Int(w.1));
            assert_eq!(row[5], Value::Int(w.3));
        }
    }
    // An expression over aggregates: VWAP, the keys ordered by it.
    let got = rows(
        &db,
        "get ticks select sym, sum(px * qty) / sum(qty) as vwap where px is not null \
         group sym order vwap desc",
    );
    let mut vwap: Vec<(i64, String)> = ["AAA", "BBB", "CCC"]
        .iter()
        .map(|s| {
            let (pv, q) = all
                .iter()
                .filter(|t| t.0 == *s && t.2.is_some())
                .fold((0, 0), |(pv, q), t| (pv + t.2.unwrap() * t.3, q + t.3));
            (pv / q, s.to_string())
        })
        .collect();
    vwap.sort_by(|a, b| b.cmp(a));
    let want: Vec<Vec<Value>> = vwap
        .into_iter()
        .map(|(v, s)| vec![Value::Text(s), Value::Int(v)])
        .collect();
    assert_eq!(got, want);
    // A group key named only in `group`, a path among them.
    let got = rows(
        &db,
        "get ticks select meta.venue, count(*), sum(meta.size) group meta.venue",
    );
    let odd = all.iter().filter(|t| t.3 % 2 == 1).count() as i64;
    let odd_sum: i64 = all.iter().filter(|t| t.3 % 2 == 1).map(|t| t.3).sum();
    assert_eq!(got.len(), 2);
    assert_eq!(
        got[1],
        [
            Value::Text("v1".into()),
            Value::Int(odd),
            Value::Int(odd_sum)
        ]
    );
}

#[test]
fn first_and_last_take_the_rows_least_and_greatest_by_their_order() {
    let mut db = Database::new();
    let all = ticks(&mut db, 300);
    let got = rows(
        &db,
        "get ticks select bucket(at, 10m) as bar, first(px by at) as open, max(px) as high, \
         min(px) as low, last(px by at) as close, sum(qty) as volume where sym = \"AAA\" \
         group bar",
    );
    // Each tick of a bar: its time, the order it was written in, price, qty.
    type Seen = (i64, usize, Option<i64>, i64);
    let mut bars: BTreeMap<i64, Vec<Seen>> = BTreeMap::new();
    for (i, t) in all.iter().enumerate().filter(|(_, t)| t.0 == "AAA") {
        let b = t.1 - (t.1 - T0) % (10 * MIN);
        bars.entry(b).or_default().push((t.1, i, t.2, t.3));
    }
    assert_eq!(got.len(), bars.len());
    for (row, (b, ts)) in got.iter().zip(&bars) {
        // The rows a null price is no value of are passed over; ties in
        // time go to the row written first, and last to the one after.
        let priced: Vec<_> = ts.iter().filter(|t| t.2.is_some()).collect();
        let open = priced
            .iter()
            .min_by_key(|t| (t.0, t.1))
            .map(|t| t.2.unwrap());
        let close = priced
            .iter()
            .max_by_key(|t| (t.0, t.1))
            .map(|t| t.2.unwrap());
        let opt = |v: Option<i64>| v.map_or(Value::Null, Value::Int);
        assert_eq!(row[0], Value::Timestamp(*b));
        assert_eq!(row[1], opt(open), "open of {b}");
        assert_eq!(row[2], opt(priced.iter().map(|t| t.2.unwrap()).max()));
        assert_eq!(row[3], opt(priced.iter().map(|t| t.2.unwrap()).min()));
        assert_eq!(row[4], opt(close), "close of {b}");
        assert_eq!(row[5], Value::Int(ts.iter().map(|t| t.3).sum()));
    }
    // Without `by`, in the order the rows were written.
    let got = rows(
        &db,
        "get ticks select first(qty), last(qty) where sym = \"BBB\"",
    );
    let b: Vec<_> = all.iter().filter(|t| t.0 == "BBB").collect();
    assert_eq!(got[0], [Value::Int(b[0].3), Value::Int(b[b.len() - 1].3)]);
}

#[test]
fn bucket_truncates_to_fixed_and_calendar_intervals() {
    let mut db = Database::new();
    exec(&mut db, "create collection e (at timestamp)");
    let at = |t: &str| Value::Text(t.into());
    for (t, iv, want) in [
        (
            "2026-03-17T10:47:31.250Z",
            "15m",
            "2026-03-17T10:45:00.000Z",
        ),
        ("2026-03-17T10:47:31.250Z", "1d", "2026-03-17T00:00:00.000Z"),
        // ISO weeks begin on a Monday: 2026-03-16 is one.
        ("2026-03-17T10:47:31.250Z", "1w", "2026-03-16T00:00:00.000Z"),
        (
            "2026-03-17T10:47:31.250Z",
            "1mo",
            "2026-03-01T00:00:00.000Z",
        ),
        (
            "2026-05-17T10:47:31.250Z",
            "3mo",
            "2026-04-01T00:00:00.000Z",
        ),
        ("2026-05-17T10:47:31.250Z", "1y", "2026-01-01T00:00:00.000Z"),
        ("1969-12-31T23:59:59.999Z", "1h", "1969-12-31T23:00:00.000Z"),
        (
            "1969-12-31T23:59:59.999Z",
            "1mo",
            "1969-12-01T00:00:00.000Z",
        ),
    ] {
        let (_, got) = rows_with(
            &db,
            "get e select bucket(timestamp($1), $2) as b, count(*) group b",
            &[at(t), at(iv)],
        );
        assert!(got.is_empty(), "no rows, no groups");
        let r = db
            .query(
                &fenec_ql::parse_one("get e where bucket(timestamp($1), $2) = $3 count").unwrap(),
                &[at(t), at(iv), at(want)],
            )
            .unwrap();
        let _ = r;
        // Through the registry, as a filter or a `set` reaches it.
        let reg = Registry::with_builtins();
        let v = reg
            .call(
                "bucket",
                &[
                    Value::Text(t.into()).coerce(&DataType::Timestamp).unwrap(),
                    at(iv),
                ],
            )
            .unwrap();
        assert_eq!(
            v,
            Value::Text(want.into())
                .coerce(&DataType::Timestamp)
                .unwrap(),
            "{t} {iv}"
        );
    }
    // In a query, the literal duration and a parameter agree.
    exec(&mut db, "put e {at: \"2026-03-17T10:47:31Z\"}");
    let a = rows(&db, "get e select bucket(at, 1h) as h, count(*) group h");
    let (_, b) = rows_with(
        &db,
        "get e select bucket(at, $1) as h, count(*) group h",
        &[Value::Int(3_600_000)],
    );
    assert_eq!(a, b);
    for (iv, says) in [("0m", "interval"), ("5x", "interval"), ("m", "interval")] {
        let e = db
            .query(
                &fenec_ql::parse_one("get e select bucket(at, $1) as h, count(*) group h").unwrap(),
                &[at(iv)],
            )
            .expect_err(iv)
            .to_string();
        assert!(e.contains(says), "{iv}: {e}");
    }
}

#[test]
fn counts_by_bucket_over_a_range_read_the_ordered_index_alone() {
    let mut db = Database::new();
    let all = ticks(&mut db, 300);
    // The same rows in a twin without the index, which reads them.
    exec(&mut db, "create collection twin (at timestamp)");
    for t in &all {
        exec(&mut db, &format!("put twin {{at: {}}}", t.1));
    }
    let from = Value::Timestamp(T0 + 7 * MIN);
    for (q, walks) in [
        ("select bucket(at, 5m) as b, count(*) where at >= $1 group b", true),
        ("select bucket(at, 5m) as b, count(*) as n where at >= $1 and at < $2 group b order n desc, b", true),
        ("select at, count(*) where at > $1 group at", true),
        ("select bucket(at, 5m) as b, bucket(at, 1h) as h, count(*) where $1 <= at group b, h", true),
        // Not the shape: a sum, another field's condition.
        ("select bucket(at, 5m) as b, count(*), max(at) where at >= $1 group b", false),
    ] {
        let params = [from.clone(), Value::Timestamp(T0 + 20 * MIN)];
        let (_, a) = rows_with(&db, &format!("get ticks {q}"), &params);
        let (_, b) = rows_with(&db, &format!("get twin {q}"), &params);
        assert_eq!(a, b, "{q}");
        assert!(!a.is_empty());
        let (_, plan) = rows_with(&db, &format!("explain get ticks {q}"), &params);
        let read = plan.iter().any(|r| format!("{:?}", r[0]).contains("no row read"));
        assert_eq!(read, walks, "{q}: {plan:?}");
    }
    // And under `@ttl`, whose bound is the range's too.
    exec(&mut db, "create collection live (at timestamp @ttl(3650d))");
    for t in &all {
        exec(&mut db, &format!("put live {{at: {}}}", t.1));
    }
    let q = "select bucket(at, 5m) as b, count(*) where at >= $1 group b";
    let (_, a) = rows_with(&db, &format!("get live {q}"), std::slice::from_ref(&from));
    let (_, b) = rows_with(&db, &format!("get twin {q}"), std::slice::from_ref(&from));
    assert_eq!(a, b);
    let (_, plan) = rows_with(&db, &format!("explain get live {q}"), &[from]);
    assert!(
        plan.iter()
            .any(|r| format!("{:?}", r[0]).contains("no row read")),
        "{plan:?}"
    );
}

#[test]
fn case_greatest_and_least_reach_filters_and_writes() {
    let mut db = Database::new();
    exec(
        &mut db,
        "create collection bars (key text @unique, high int, low int)",
    );
    exec(&mut db, "put bars {key: \"a\", high: 10, low: 10}");
    for p in [12, 7, 11] {
        exec(
            &mut db,
            &format!(
                "set bars {{high: greatest(high, {p}), low: least(low, {p})}} where key = \"a\""
            ),
        );
    }
    assert_eq!(
        rows(&db, "get bars select high, low"),
        vec![vec![Value::Int(12), Value::Int(7)]]
    );
    // `case` works out only the branch it takes.
    exec(&mut db, "put bars {key: \"z\", high: 0, low: 0}");
    let got = rows(
        &db,
        "get bars select key, case when low = 0 then null else high / low end as r order key",
    );
    assert_eq!(got[1][1], Value::Null);
    assert_eq!(got[0][1], Value::Int(1));
    let got = rows(
        &db,
        "get bars where case when low = 0 then false else high / low >= 1 end count",
    );
    assert_eq!(got[0][0], Value::Int(1));
}

#[test]
fn what_an_aggregating_list_cannot_answer_is_refused() {
    let mut db = Database::new();
    ticks(&mut db, 5);
    for (sql, says) in [
        ("get ticks select px * qty", "answers under a name"),
        (
            "get ticks select sym, sum(qty) group bucket(at, 1m)",
            "neither aggregated",
        ),
        ("get ticks select sum(max(px)) as x", "inside another"),
        ("get ticks select count(qty)", "count(distinct"),
        (
            "get ticks select distinct(qty) as d, count(*)",
            "inside `count`",
        ),
        ("get ticks select min(px, qty) as m", "least(a, b)"),
        (
            "get ticks select first(px, qty, at) as f",
            "first(<value> [by <key>])",
        ),
        ("get ticks select sum(qty), sum(qty)", "names two columns"),
        (
            "get ticks select count(*) group sum(qty)",
            "an aggregate is the group's",
        ),
        ("get ticks select sum(sym)", "int or float"),
        ("get ticks select sum(nope)", "nope"),
        (
            "get ticks select px * 2 as a, qty as a",
            "names two columns",
        ),
        (
            "get ticks select bucket(at, 1m) as b, count(*) group b order px",
            "not a column",
        ),
        (
            "get ticks select sum(qty) as s, count(*) group s",
            "an aggregate is the group's",
        ),
    ] {
        let e = error(&db, sql);
        assert!(e.contains(says), "{sql}: {e}");
    }
    // A sum of a text worked out over the rows is refused at the first.
    let e = error(&db, "get ticks select sum(sym + 1) as s");
    assert!(e.contains("takes numbers"), "{e}");
    // An int past 64 bits is refused, in a row and in a fold.
    let e = error(&db, "get ticks select qty * 9223372036854775807 as x");
    assert!(e.contains("overflows"), "{e}");
}

#[test]
fn group_by_a_name_of_the_list_and_order_by_any_column() {
    let mut db = Database::new();
    let all = ticks(&mut db, 200);
    let got = rows(
        &db,
        "get ticks select bucket(at, 10m) as bar, sym, count(*) as n group bar, sym \
         order n desc, bar desc limit 2",
    );
    let mut counts: BTreeMap<(i64, String), i64> = BTreeMap::new();
    for t in &all {
        *counts
            .entry((t.1 - (t.1 - T0) % (10 * MIN), t.0.clone()))
            .or_default() += 1;
    }
    let mut want: Vec<_> = counts.into_iter().collect();
    want.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then(b.0 .0.cmp(&a.0 .0))
            .then(a.0 .1.cmp(&b.0 .1))
    });
    let want: Vec<Vec<Value>> = want[..2]
        .iter()
        .map(|((b, s), n)| vec![Value::Timestamp(*b), Value::Text(s.clone()), Value::Int(*n)])
        .collect();
    assert_eq!(got, want);
    // Without `order`, by the keys in the order `group` names them.
    let got = rows(
        &db,
        "get ticks select sym, bucket(at, 10m) as bar, count(*) group sym, bar",
    );
    let keys: Vec<_> = got.iter().map(|r| (r[0].clone(), r[1].clone())).collect();
    let mut sorted = keys.clone();
    sorted.sort_by(|a, b| a.0.cmp_value(&b.0).then(a.1.cmp_value(&b.1)));
    assert_eq!(keys, sorted);
    // `count(distinct ...)` orders by its name.
    let got = rows(
        &db,
        "get ticks select sym, count(distinct qty) group sym order count(distinct qty) desc, sym",
    );
    assert_eq!(got.len(), 3);
}
