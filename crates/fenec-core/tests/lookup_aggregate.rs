//! A `lookup` level whose list aggregates, and a `lookup` hanging off the
//! groups of a `get` or of such a level, never changes an answer.
//!
//! Every query here runs on a database whose children are reached through
//! their indexes -- a `@hash` bucket, an index over several fields probed
//! or walked -- and is held, every level and every fold, to a reference the
//! test works out itself from plain `get`s over a twin with no index: a
//! child is a parent's where each pair of `on` holds equal values and
//! neither side a null, and `count`, `sum`, `avg`, `min`, `max`, `count
//! (distinct)`, `first` and `last` are folded here by hand. Through writes,
//! a block read inside and put back, a reopen, and children past their
//! `@ttl`.

use fenec_core::prelude::*;
use std::cmp::Ordering;

const DAY: i64 = 86_400_000;
/// The clock both databases read: `seen` older than thirty days before it
/// is past its time.
const NOW: i64 = 1_790_000_000_000;

fn schema(indexed: bool) -> Vec<String> {
    let ix = |s: &str| match indexed {
        true => s.to_string(),
        false => String::new(),
    };
    vec![
        format!(
            "create collection couriers (code text{}, city text, name text)",
            ix(" @hash")
        ),
        format!(
            "create collection shops (org int, code text, city text{})",
            ix(", @unique(org, code)")
        ),
        format!(
            "create collection orders (org int, shop text, courier text{}, status text, \
             total float, qty int, distance float, at timestamp, seen timestamp @ttl(30d){})",
            ix(" @hash"),
            ix(", @sorted(org, shop, at desc), @sorted(courier, at desc)")
        ),
    ]
}

fn exec(db: &mut Database, sql: &str) -> Response {
    db.execute_with(&fenec_ql::parse_one(sql).expect("parse"), &[])
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn result(db: &Database, sql: &str, params: &[Value]) -> ResultSet {
    db.query(&fenec_ql::parse_one(sql).expect("parse"), params)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
        .rows()
        .expect("rows")
        .clone()
}

fn error(db: &Database, sql: &str) -> String {
    match fenec_ql::parse_one(sql) {
        Err(e) => e.to_string(),
        Ok(stmt) => db.query(&stmt, &[]).expect_err(sql).to_string(),
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[(self.next() % xs.len() as u64) as usize]
    }
}

/// A document of `coll`, its fields drawn from few values so that keys
/// repeat and ties are everywhere; `null` among them, and `-0.0` beside
/// `0.0`.
fn document(r: &mut Rng, coll: &str) -> String {
    let org = r.pick(&["null", "1", "2", "2"]);
    let code = |r: &mut Rng| r.pick(&["null", "\"a\"", "\"b\"", "\"c\"", "\"d\""]);
    match coll {
        "couriers" => format!(
            "{{code: {}, city: {}, name: {}}}",
            r.pick(&["null", "\"k1\"", "\"k2\"", "\"k3\"", "\"k4\"", "\"k5\"", "\"k9\""]),
            r.pick(&["\"x\"", "\"y\"", "null"]),
            r.pick(&["\"Ada\"", "\"Bo\"", "\"Cy\""])
        ),
        "shops" => format!(
            "{{org: {org}, code: {}, city: {}}}",
            code(r),
            r.pick(&["\"x\"", "\"y\""])
        ),
        _ => {
            let seen = match r.next() % 4 {
                0 => "null".to_string(),
                1 => (NOW - 40 * DAY).to_string(),
                _ => (NOW - (r.next() % 20) as i64 * DAY).to_string(),
            };
            format!(
                "{{org: {org}, shop: {}, courier: {}, status: {}, total: {}, qty: {}, \
                 distance: {}, at: {}, seen: {seen}}}",
                code(r),
                r.pick(&["null", "\"k1\"", "\"k2\"", "\"k2\"", "\"k3\"", "\"k4\"", "\"k7\""]),
                r.pick(&["null", "\"done\"", "\"done\"", "\"open\"", "\"void\""]),
                r.pick(&["null", "-0.0", "0.0", "1.5", "2.25", "10.0", "7.75"]),
                r.pick(&["null", "1", "2", "3", "-1"]),
                r.pick(&["null", "0.5", "1.5", "3.0", "3.0"]),
                r.pick(&[
                    "null",
                    "\"2026-01-01\"",
                    "\"2026-02-01\"",
                    "\"2026-02-01\"",
                    "\"2026-03-15\"",
                    "\"2026-09-01\""
                ]),
            )
        }
    }
}

/// `ix` with the indexes, `plain` without, holding the same documents
/// under the same ids.
fn twins() -> (Database, Database) {
    let (mut ix, mut plain) = (Database::new(), Database::new());
    for (db, indexed) in [(&mut ix, true), (&mut plain, false)] {
        db.set_clock(Some(NOW));
        for s in schema(indexed) {
            exec(db, &s);
        }
    }
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    for (coll, n) in [("couriers", 14), ("shops", 24), ("orders", 320)] {
        let mut docs: Vec<String> = Vec::new();
        for _ in 0..n {
            docs.push(document(&mut r, coll));
        }
        // `@unique(org, code)` takes a pair once: the twin is given the
        // same documents, the clashes left out of both.
        let mut kept = Vec::new();
        for d in docs {
            let put = format!("put {coll} {d}");
            if ix
                .execute_with(&fenec_ql::parse_one(&put).unwrap(), &[])
                .is_ok()
            {
                kept.push(put);
            }
        }
        for put in kept {
            exec(&mut plain, &put);
        }
    }
    (ix, plain)
}

// ------------------------------------------------------------ the reference

/// A row of a plain `get`, by its columns.
#[derive(Clone, Debug)]
struct Doc {
    id: u64,
    cols: Vec<(String, Value)>,
}

impl Doc {
    fn get(&self, name: &str) -> Value {
        if name == "id" {
            return Value::Int(self.id as i64);
        }
        (self.cols.iter().find(|(c, _)| c == name)).map_or(Value::Null, |(_, v)| v.clone())
    }
}

fn docs(plain: &Database, sql: &str) -> Vec<Doc> {
    let rs = result(plain, sql, &[]);
    (rs.rows.iter())
        .map(|r| Doc {
            id: r.id,
            cols: rs.columns.iter().cloned().zip(r.values.clone()).collect(),
        })
        .collect()
}

/// Whether a lookup matches a parent's value with a child's: both there,
/// and equal as a key holds them -- numbers by value, `-0.0` as `0.0`.
fn same_key(a: &Value, b: &Value) -> bool {
    if a.is_null() || b.is_null() {
        return false;
    }
    let num = |v: &Value| match v {
        Value::Int(i) | Value::Timestamp(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    };
    match (num(a), num(b)) {
        (Some(x), Some(y)) => x == y,
        _ => a == b,
    }
}

/// Whether two values are one as a group's key or a distinct count holds
/// them: by their encodings, so `-0.0` and `0.0` are two.
fn enc_eq(a: &Value, b: &Value) -> bool {
    let (mut x, mut y) = (Vec::new(), Vec::new());
    fenec_core::codec::encode_value(&mut x, a);
    fenec_core::codec::encode_value(&mut y, b);
    x == y
}

/// The children of `p` among `kids`, by the pairs of `on`.
fn children<'a>(kids: &'a [Doc], on: &[(&str, &str)], p: &Doc) -> Vec<&'a Doc> {
    (kids.iter())
        .filter(|k| on.iter().all(|(c, q)| same_key(&k.get(c), &p.get(q))))
        .collect()
}

/// The same for a group's row above, by its columns.
fn children_of_row<'a>(
    kids: &'a [Doc],
    on: &[(&str, &str)],
    row: &[(String, Value)],
) -> Vec<&'a Doc> {
    let col = |n: &str| (row.iter().find(|(c, _)| c == n)).map_or(Value::Null, |(_, v)| v.clone());
    (kids.iter())
        .filter(|k| on.iter().all(|(c, q)| same_key(&k.get(c), &col(q))))
        .collect()
}

#[derive(Clone, Copy)]
enum Agg {
    Count,
    Sum(&'static str),
    Avg(&'static str),
    Min(&'static str),
    Max(&'static str),
    Distinct(&'static str),
    /// `first(v by k)`, or `last(v)` by the id when `k` is `None`.
    First(&'static str, Option<&'static str>),
    Last(&'static str, Option<&'static str>),
}

/// An aggregate over `rows`, worked out by hand: nulls skipped by all but
/// the row count; a sum of ints an int and of floats a float, in the
/// order the rows come; `first` the row least by its key and then its id.
fn fold(rows: &[&Doc], a: Agg) -> Value {
    let vals = |f: &str| -> Vec<Value> {
        (rows.iter())
            .map(|r| r.get(f))
            .filter(|v| !v.is_null())
            .collect()
    };
    match a {
        Agg::Count => Value::Int(rows.len() as i64),
        Agg::Sum(f) | Agg::Avg(f) => {
            let v = vals(f);
            if v.is_empty() {
                return Value::Null;
            }
            let ints = v.iter().all(|x| matches!(x, Value::Int(_)));
            if ints && matches!(a, Agg::Sum(_)) {
                let s: i64 = (v.iter())
                    .map(|x| match x {
                        Value::Int(i) => *i,
                        _ => 0,
                    })
                    .sum();
                return Value::Int(s);
            }
            let mut s = 0.0f64;
            for x in &v {
                s += match x {
                    Value::Int(i) => *i as f64,
                    Value::Float(f) => *f,
                    _ => 0.0,
                };
            }
            match a {
                Agg::Avg(_) => Value::Float(s / v.len() as f64),
                _ => Value::Float(s),
            }
        }
        Agg::Min(f) | Agg::Max(f) => {
            let v = vals(f);
            let best = match a {
                Agg::Min(_) => v.into_iter().reduce(|x, y| match y.cmp_value(&x) {
                    Ordering::Less => y,
                    _ => x,
                }),
                _ => v.into_iter().reduce(|x, y| match y.cmp_value(&x) {
                    Ordering::Greater => y,
                    _ => x,
                }),
            };
            best.unwrap_or(Value::Null)
        }
        Agg::Distinct(f) => {
            let mut seen: Vec<Value> = Vec::new();
            for v in vals(f) {
                // As a key holds them: `-0.0` and `0.0` are two encodings,
                // and `count(distinct)` keeps encodings.
                if !seen.iter().any(|s| enc_eq(s, &v)) {
                    seen.push(v);
                }
            }
            Value::Int(seen.len() as i64)
        }
        Agg::First(f, by) | Agg::Last(f, by) => {
            let later = matches!(a, Agg::Last(..));
            let mut best: Option<(Value, u64, Value)> = None;
            for r in rows {
                let v = r.get(f);
                let k = by.map_or(Value::Int(r.id as i64), |b| r.get(b));
                if v.is_null() || k.is_null() {
                    continue;
                }
                let better = match &best {
                    None => true,
                    Some((bk, bid, _)) => {
                        let o = k.cmp_value(bk).then(r.id.cmp(bid));
                        o == if later {
                            Ordering::Greater
                        } else {
                            Ordering::Less
                        }
                    }
                };
                if better {
                    best = Some((k, r.id, v));
                }
            }
            best.map_or(Value::Null, |b| b.2)
        }
    }
}

/// `rows` in the order of `keys` -- each a name and whether it ascends --
/// the rows' own order breaking ties, as a `get`'s `order` puts them.
fn sorted<'a>(rows: &[&'a Doc], keys: &[(&str, bool)]) -> Vec<&'a Doc> {
    let mut out: Vec<&Doc> = rows.to_vec();
    out.sort_by(|a, b| {
        for (k, asc) in keys {
            let o = a.get(k).cmp_value(&b.get(k));
            if o != Ordering::Equal {
                return if *asc { o } else { o.reverse() };
            }
        }
        a.id.cmp(&b.id)
    });
    out
}

/// A level's answer for one parent: its children folded, ungrouped one
/// row; grouped by `group`, a row a group with the key first, held to
/// `having` (a column's least), in `order` (columns, the key breaking
/// ties) and its page.
struct Fold {
    group: Option<&'static str>,
    aggs: Vec<(&'static str, Agg)>,
    having: Option<(&'static str, i64)>,
    order: Vec<(&'static str, bool)>,
    limit: Option<usize>,
    offset: usize,
}

type Cols = Vec<(String, Value)>;

impl Fold {
    fn rows(&self, kids: &[&Doc]) -> Vec<Cols> {
        let row = |key: Option<(&str, Value)>, of: &[&Doc]| -> Cols {
            let mut r: Cols = Vec::new();
            if let Some((k, v)) = key {
                r.push((k.to_string(), v));
            }
            for (name, a) in &self.aggs {
                r.push((name.to_string(), fold(of, *a)));
            }
            r
        };
        let Some(g) = self.group else {
            return vec![row(None, kids)];
        };
        let mut groups: Vec<(Value, Vec<&Doc>)> = Vec::new();
        for k in kids {
            let v = k.get(g);
            match groups.iter_mut().find(|(x, _)| enc_eq(x, &v)) {
                Some((_, of)) => of.push(k),
                None => groups.push((v, vec![k])),
            }
        }
        let mut rows: Vec<Cols> = Vec::new();
        for (v, of) in &groups {
            rows.push(row(Some((g, v.clone())), of));
        }
        let col = |r: &Cols, n: &str| (r.iter().find(|(c, _)| c == n)).unwrap().1.clone();
        if let Some((n, least)) = self.having {
            rows.retain(|r| matches!(col(r, n), Value::Int(i) if i >= least));
        }
        rows.sort_by(|a, b| {
            for (k, asc) in &self.order {
                let o = col(a, k).cmp_value(&col(b, k));
                if o != Ordering::Equal {
                    return if *asc { o } else { o.reverse() };
                }
            }
            col(a, g).cmp_value(&col(b, g))
        });
        rows.into_iter()
            .skip(self.offset)
            .take(self.limit.unwrap_or(usize::MAX))
            .collect()
    }
}

/// `rs`, its rows and every level's, as columns by name: what is held to
/// the reference.
fn shape(rs: &ResultSet) -> Vec<String> {
    let mut out = Vec::new();
    let line = |cols: &[String], vals: &[Value]| -> String {
        let mut s = String::new();
        for (c, v) in cols.iter().zip(vals) {
            s.push_str(&format!("{c}={v:?} "));
        }
        s
    };
    let mut level = rs.nested.as_ref();
    let mut above: Vec<String> = rs
        .rows
        .iter()
        .map(|r| line(&rs.columns, &r.values))
        .collect();
    out.extend(above.iter().map(|s| format!("0| {s}")));
    let mut depth = 1;
    while let Some(n) = level {
        let mut here = Vec::new();
        for (i, g) in n.groups.iter().enumerate() {
            out.push(format!(
                "{depth}| under {}",
                above.get(i).map_or("?", |s| s.as_str())
            ));
            for r in g {
                let s = line(&n.columns, &r.values);
                out.push(format!("{depth}|   {s}"));
                here.push(s);
            }
        }
        above = here;
        level = n.nested.as_deref();
        depth += 1;
    }
    out
}

/// A reference answer, built level by level the way [`shape`] reads one.
struct Expect {
    lines: Vec<String>,
}

impl Expect {
    fn line(cols: &Cols) -> String {
        let mut s = String::new();
        for (c, v) in cols {
            s.push_str(&format!("{c}={v:?} "));
        }
        s
    }
}

fn plain_cols(d: &Doc, fields: &[&str]) -> Cols {
    fields.iter().map(|f| (f.to_string(), d.get(f))).collect()
}

fn same(ix: &Database, sql: &str, params: &[Value], want: Expect, stage: &str) {
    let got = shape(&result(ix, sql, params));
    if got != want.lines {
        let n = got.len().max(want.lines.len());
        for i in 0..n {
            let (g, w) = (got.get(i), want.lines.get(i));
            if g != w {
                panic!(
                    "{stage}: {sql}\n line {i}\n got  {g:?}\n want {w:?}\n\ngot:\n{}\n\nwant:\n{}",
                    got.join("\n"),
                    want.lines.join("\n")
                );
            }
        }
    }
}

const ORDER_FIELDS: &[&str] = &["org", "shop", "courier", "status", "total", "at"];

/// Every query held to its reference over `plain`.
fn check(ix: &Database, plain: &Database, stage: &str) {
    let shops = docs(plain, "get shops");
    let couriers = docs(plain, "get couriers");
    let orders = docs(plain, "get orders");
    let live: Vec<&Doc> = orders.iter().collect();
    let not_void: Vec<Doc> = docs(plain, "get orders where status != \"void\"");
    let shop_key: &[(&str, &str)] = &[("org", "org"), ("shop", "code")];
    let courier_key: &[(&str, &str)] = &[("courier", "code")];

    // An ungrouped fold of every aggregate, over a key of two fields
    // probed in its index, a filter on the level.
    let all = vec![
        ("n", Agg::Count),
        ("t", Agg::Sum("total")),
        ("q", Agg::Avg("qty")),
        ("sq", Agg::Sum("qty")),
        ("a0", Agg::Min("at")),
        ("s1", Agg::Max("status")),
        ("dc", Agg::Distinct("courier")),
        ("ft", Agg::First("total", Some("at"))),
        ("ls", Agg::Last("status", None)),
    ];
    let list = "select count() as n, sum(total) as t, avg(qty) as q, sum(qty) as sq, \
                min(at) as a0, max(status) as s1, count(distinct courier) as dc, \
                first(total by at) as ft, last(status) as ls";
    let every = Fold {
        group: None,
        aggs: all.clone(),
        having: None,
        order: vec![],
        limit: None,
        offset: 0,
    };
    for required in [false, true] {
        let sql = format!(
            "get shops lookup orders on (org, shop) = (org, code) where status != \"void\" \
             {list}{}",
            if required { " required" } else { "" }
        );
        let mut lines = Vec::new();
        let mut kept = Vec::new();
        for s in &shops {
            let kids = children(&not_void, shop_key, s);
            if required && kids.is_empty() {
                continue;
            }
            kept.push((s, kids));
        }
        for (s, _) in &kept {
            lines.push(format!(
                "0| {}",
                Expect::line(&plain_cols(s, &["id", "org", "code", "city"]))
            ));
        }
        for (s, kids) in &kept {
            lines.push(format!(
                "1| under {}",
                Expect::line(&plain_cols(s, &["id", "org", "code", "city"]))
            ));
            for r in every.rows(kids) {
                lines.push(format!("1|   {}", Expect::line(&r)));
            }
        }
        same(ix, &sql, &[], Expect { lines }, stage);
        // Counted: the parents a `required` level keeps.
        if required {
            let n = result(
                ix,
                "get shops count lookup orders on (org, shop) = (org, code) \
                 where status != \"void\" select count() as n required",
                &[],
            );
            assert_eq!(
                n.rows[0].values,
                vec![Value::Int(kept.len() as i64)],
                "{stage}"
            );
        }
    }

    // Ungrouped, `order`, `limit` and `offset` pick the children folded: a
    // walk of `@sorted(courier, at desc)`, and a probe and a sort.
    for (order, keys, limit, offset, walk) in [
        ("order at desc", vec![("at", false)], 3usize, 0usize, true),
        (
            "order distance desc, at",
            vec![("distance", false), ("at", true)],
            2,
            1,
            false,
        ),
    ] {
        let sql = format!(
            "get couriers lookup orders on courier = code where qty != -1 {order} limit $1 \
             offset {offset} select count() as n, avg(distance) as d, sum(total) as t, \
             max(at) as last_at, first(status by at) as fs"
        );
        let picked_fold = Fold {
            group: None,
            aggs: vec![
                ("n", Agg::Count),
                ("d", Agg::Avg("distance")),
                ("t", Agg::Sum("total")),
                ("last_at", Agg::Max("at")),
                ("fs", Agg::First("status", Some("at"))),
            ],
            having: None,
            order: vec![],
            limit: None,
            offset: 0,
        };
        let passing = docs(plain, "get orders where qty != -1");
        let mut lines = Vec::new();
        for c in &couriers {
            lines.push(format!(
                "0| {}",
                Expect::line(&plain_cols(c, &["id", "code", "city", "name"]))
            ));
        }
        for c in &couriers {
            lines.push(format!(
                "1| under {}",
                Expect::line(&plain_cols(c, &["id", "code", "city", "name"]))
            ));
            let kids = sorted(&children(&passing, courier_key, c), &keys);
            let picked: Vec<&Doc> = kids.into_iter().skip(offset).take(limit).collect();
            for r in picked_fold.rows(&picked) {
                lines.push(format!("1|   {}", Expect::line(&r)));
            }
        }
        same(
            ix,
            &sql,
            &[Value::Int(limit as i64)],
            Expect { lines },
            stage,
        );
        let plan: Vec<String> = result(ix, &format!("explain {sql}"), &[Value::Int(limit as i64)])
            .rows
            .iter()
            .map(|r| format!("{:?}", r.values))
            .collect();
        let walked = plan.iter().any(|l| l.contains("walked"));
        assert_eq!(walked, walk, "{stage}: {sql}\n{plan:?}");
        assert!(
            plan.iter().any(|l| l.contains("folded into")),
            "{stage}: {plan:?}"
        );
    }

    // Grouped: each parent's groups, `having`, their order and page.
    for (sql_tail, f) in [
        (
            "select status, count() as n, sum(qty) as q group status order n desc limit 2 offset 1",
            Fold {
                group: Some("status"),
                aggs: vec![("n", Agg::Count), ("q", Agg::Sum("qty"))],
                having: None,
                order: vec![("n", false)],
                limit: Some(2),
                offset: 1,
            },
        ),
        (
            "select courier, count() as n, avg(total) as a group courier having n >= 2 \
             order a desc",
            Fold {
                group: Some("courier"),
                aggs: vec![("n", Agg::Count), ("a", Agg::Avg("total"))],
                having: Some(("n", 2)),
                order: vec![("a", false)],
                limit: None,
                offset: 0,
            },
        ),
    ] {
        let sql = format!("get shops lookup orders on (org, shop) = (org, code) {sql_tail}");
        let mut lines = Vec::new();
        for s in &shops {
            lines.push(format!(
                "0| {}",
                Expect::line(&plain_cols(s, &["id", "org", "code", "city"]))
            ));
        }
        for s in &shops {
            lines.push(format!(
                "1| under {}",
                Expect::line(&plain_cols(s, &["id", "org", "code", "city"]))
            ));
            for r in f.rows(&children(&orders, shop_key, s)) {
                lines.push(format!("1|   {}", Expect::line(&r)));
            }
        }
        same(ix, &sql, &[], Expect { lines }, stage);
    }

    // A `lookup` off a `get`'s groups, keyed by a column; `required`
    // before the order and the page, and counted.
    let by_courier = Fold {
        group: Some("courier"),
        aggs: vec![("n", Agg::Count), ("t", Agg::Sum("total"))],
        having: None,
        order: vec![("n", false), ("t", true)],
        limit: None,
        offset: 0,
    };
    let x_couriers = docs(plain, "get couriers where city = \"x\"");
    let nv: Vec<&Doc> = not_void.iter().collect();
    for required in [false, true] {
        let level = match required {
            true => {
                "lookup couriers on code = courier where city = \"x\" select name, city required"
            }
            false => "lookup couriers on code = courier select name, city",
        };
        let kids_of = if required { &x_couriers } else { &couriers };
        let sql = format!(
            "get orders where status != \"void\" select courier, count() as n, sum(total) as t \
             group courier order n desc, t limit 5 {level}"
        );
        let mut rows = by_courier.rows(&nv);
        if required {
            rows.retain(|r| !children_of_row(kids_of, courier_key_up(), r).is_empty());
        }
        rows.truncate(5);
        let mut lines: Vec<String> = rows
            .iter()
            .map(|r| format!("0| {}", Expect::line(r)))
            .collect();
        for r in &rows {
            lines.push(format!("1| under {}", Expect::line(r)));
            for c in children_of_row(kids_of, courier_key_up(), r) {
                lines.push(format!(
                    "1|   {}",
                    Expect::line(&plain_cols(c, &["name", "city"]))
                ));
            }
        }
        same(ix, &sql, &[], Expect { lines }, stage);
        if required {
            let all_rows = by_courier.rows(&nv);
            let kept = (all_rows.iter())
                .filter(|r| !children_of_row(kids_of, courier_key_up(), r).is_empty())
                .count();
            let n = result(
                ix,
                "get orders where status != \"void\" select courier, count() as n group courier \
                 count lookup couriers on code = courier where city = \"x\" required",
                &[],
            );
            assert_eq!(n.rows[0].values, vec![Value::Int(kept as i64)], "{stage}");
        }
    }

    // A key over two fields read off a `get`'s groups.
    let by_shop = docs(plain, "get orders order org, shop");
    let _ = by_shop;
    {
        let sql = "get orders select org, shop, count() as n group org, shop \
                   lookup shops on (org, code) = (org, shop) select city";
        let rs = result(ix, sql, &[]);
        let n = rs.nested.as_ref().expect("nested");
        for (i, r) in rs.rows.iter().enumerate() {
            let row: Cols = rs.columns.iter().cloned().zip(r.values.clone()).collect();
            let want: Vec<Value> =
                children_of_row(&shops, &[("org", "org"), ("code", "shop")], &row)
                    .iter()
                    .map(|s| s.get("city"))
                    .collect();
            let got: Vec<Value> = n.group(i).iter().map(|c| c.values[0].clone()).collect();
            assert_eq!(got, want, "{stage}: {sql} at {row:?}");
            let count = (live.iter())
                .filter(|o| enc_eq(&o.get("org"), &row[0].1) && enc_eq(&o.get("shop"), &row[1].1))
                .count();
            assert_eq!(row[2].1, Value::Int(count as i64), "{stage}: {row:?}");
        }
    }

    // A level below a level that folds, keyed by its groups' column, a
    // `required` there dropping groups and not the shops.
    for required in [false, true] {
        let tail = if required {
            " where city = \"x\" required"
        } else {
            ""
        };
        let sql = format!(
            "get shops order id limit 9 lookup orders on (org, shop) = (org, code) \
             select courier, count() as n group courier lookup couriers on code = courier{tail} \
             select name"
        );
        let kids_of = if required { &x_couriers } else { &couriers };
        let g = Fold {
            group: Some("courier"),
            aggs: vec![("n", Agg::Count)],
            having: None,
            order: vec![],
            limit: None,
            offset: 0,
        };
        let page: Vec<&Doc> = shops.iter().take(9).collect();
        let mut lines: Vec<String> = (page.iter())
            .map(|s| {
                format!(
                    "0| {}",
                    Expect::line(&plain_cols(s, &["id", "org", "code", "city"]))
                )
            })
            .collect();
        let mut level1 = Vec::new();
        for s in &page {
            lines.push(format!(
                "1| under {}",
                Expect::line(&plain_cols(s, &["id", "org", "code", "city"]))
            ));
            let mut rows = g.rows(&children(&orders, shop_key, s));
            if required {
                rows.retain(|r| !children_of_row(kids_of, courier_key_up(), r).is_empty());
            }
            for r in rows {
                lines.push(format!("1|   {}", Expect::line(&r)));
                level1.push(r);
            }
        }
        for r in &level1 {
            lines.push(format!("2| under {}", Expect::line(r)));
            for c in children_of_row(kids_of, courier_key_up(), r) {
                lines.push(format!("2|   {}", Expect::line(&plain_cols(c, &["name"]))));
            }
        }
        same(ix, &sql, &[], Expect { lines }, stage);
    }

    // A level that folds below a plain one: each order's shop counted.
    {
        let sql = "get couriers order id lookup orders on courier = code where status = \"done\" \
                   order at desc limit 2 select shop, total lookup shops on (org, code) = (org, shop) \
                   select count() as n, min(city) as city";
        let done = docs(plain, "get orders where status = \"done\"");
        let mut lines: Vec<String> = (couriers.iter())
            .map(|c| {
                format!(
                    "0| {}",
                    Expect::line(&plain_cols(c, &["id", "code", "city", "name"]))
                )
            })
            .collect();
        let mut level1 = Vec::new();
        for c in &couriers {
            lines.push(format!(
                "1| under {}",
                Expect::line(&plain_cols(c, &["id", "code", "city", "name"]))
            ));
            let kids = sorted(&children(&done, courier_key, c), &[("at", false)]);
            for o in kids.into_iter().take(2) {
                lines.push(format!(
                    "1|   {}",
                    Expect::line(&plain_cols(o, &["shop", "total"]))
                ));
                level1.push(o);
            }
        }
        for o in &level1 {
            lines.push(format!(
                "2| under {}",
                Expect::line(&plain_cols(o, &["shop", "total"]))
            ));
            let of = children(&shops, &[("org", "org"), ("code", "shop")], o);
            let f = Fold {
                group: None,
                aggs: vec![("n", Agg::Count), ("city", Agg::Min("city"))],
                having: None,
                order: vec![],
                limit: None,
                offset: 0,
            };
            for r in f.rows(&of) {
                lines.push(format!("2|   {}", Expect::line(&r)));
            }
        }
        same(ix, sql, &[], Expect { lines }, stage);
    }

    // A `get`'s own: ungrouped, `order` and `limit` pick the rows folded.
    for code in ["k1", "k2", "k7", "none"] {
        let sql = "get orders where courier = $1 order at desc, total limit 3 offset 1 \
                   select count() as n, avg(distance) as d, sum(total) as t";
        let kids = sorted(
            &(orders.iter())
                .filter(|o| o.get("courier") == Value::Text(code.into()))
                .collect::<Vec<_>>(),
            &[("at", false), ("total", true)],
        );
        let picked: Vec<&Doc> = kids.into_iter().skip(1).take(3).collect();
        let f = Fold {
            group: None,
            aggs: vec![
                ("n", Agg::Count),
                ("d", Agg::Avg("distance")),
                ("t", Agg::Sum("total")),
            ],
            having: None,
            order: vec![],
            limit: None,
            offset: 0,
        };
        let want = f.rows(&picked);
        same(
            ix,
            sql,
            &[Value::Text(code.into())],
            Expect {
                lines: want
                    .iter()
                    .map(|r| format!("0| {}", Expect::line(r)))
                    .collect(),
            },
            stage,
        );
    }
    let _ = ORDER_FIELDS;
}

/// A courier's key off a group's row: its `code` the row's `courier`.
fn courier_key_up() -> &'static [(&'static str, &'static str)] {
    &[("code", "courier")]
}

/// Writes on both sides: keys moved on either, rows gone and new.
fn write(db: &mut Database) {
    let every = |step: u64, from: u64| -> String {
        let ids: Vec<String> = (0..40).map(|i| (from + i * step).to_string()).collect();
        format!("[{}]", ids.join(", "))
    };
    for s in [
        format!("set orders {{courier: \"k3\"}} where id in {}", every(7, 7)),
        format!(
            "set orders {{status: \"void\"}} where id in {}",
            every(11, 1)
        ),
        format!("del orders where id in {}", every(13, 2)),
        format!(
            "set shops {{code: \"d\"}} where id in {} and code = \"a\"",
            every(5, 3)
        ),
    ] {
        let stmt = fenec_ql::parse_one(&s).unwrap();
        let _ = db.execute_with(&stmt, &[]);
    }
    for s in [
        "del couriers where code = \"k1\" and city = \"x\"",
        "put couriers {code: \"k7\", city: \"x\", name: \"Dee\"}",
        "put orders {org: 2, shop: \"b\", courier: \"k7\", status: \"done\", total: 3.5, qty: 2, \
         distance: 4.0, at: \"2026-04-01\", seen: 1790000000000}",
    ] {
        // `@unique(org, code)` may refuse a moved key: then neither side
        // takes it.
        let stmt = fenec_ql::parse_one(s).unwrap();
        let _ = db.execute_with(&stmt, &[]);
    }
}

#[test]
fn a_level_that_folds_gives_the_references_answer() {
    let (mut ix, mut plain) = twins();
    check(&ix, &plain, "after the load");

    for db in [&mut ix, &mut plain] {
        write(db);
    }
    check(&ix, &plain, "after writes");

    // Inside a block its writes are read; put back, every index is as it
    // was.
    for db in [&mut ix, &mut plain] {
        db.begin().unwrap();
        exec(db, "set orders {courier: \"k2\"} where courier = \"k4\"");
        exec(db, "del orders where status = \"open\"");
        exec(db, "set shops {code: \"q\"} where org = 2 and code = \"c\"");
    }
    check(&ix, &plain, "inside a block");
    for db in [&mut ix, &mut plain] {
        db.rollback();
    }
    check(&ix, &plain, "after a block put back");

    // A reopen builds the indexes from the documents; the clock is the
    // database's, and set again.
    let mut reopened = Database::new();
    reopened.load(&ix.snapshot()).unwrap();
    reopened.set_clock(Some(NOW));
    check(&reopened, &plain, "after a reopen");

    // Time passes: more orders are past their `@ttl`.
    for db in [&mut reopened, &mut plain] {
        db.set_clock(Some(NOW + 15 * DAY));
    }
    check(&reopened, &plain, "after time passed");
}

/// The JSON a transport writes, and the flat table one that has no nesting
/// gets: a level's folds are its columns.
#[test]
fn a_fold_nests_and_flattens_as_children_do() {
    let mut db = Database::new();
    exec(&mut db, "create collection shops (name text)");
    exec(
        &mut db,
        "create collection orders (shop int @hash, total float, status text)",
    );
    exec(
        &mut db,
        "put shops [{name: \"a\"}, {name: \"b\"}, {name: \"c\"}]",
    );
    exec(
        &mut db,
        "put orders [{shop: 1, total: 10, status: \"paid\"}, {shop: 1, total: 5, status: \"open\"}, \
         {shop: 2, total: 7, status: \"paid\"}, {shop: 1, total: 1, status: \"paid\"}]",
    );
    let rs = result(
        &db,
        "get shops lookup orders on shop select count() as n, sum(total) as revenue",
        &[],
    );
    let mut out = String::new();
    fenec_core::json::rows_array_into(&mut out, &rs);
    assert_eq!(
        out,
        "[{\"id\":1,\"name\":\"a\",\"orders\":[{\"n\":3,\"revenue\":16}]},\
         {\"id\":2,\"name\":\"b\",\"orders\":[{\"n\":1,\"revenue\":7}]},\
         {\"id\":3,\"name\":\"c\",\"orders\":[{\"n\":0,\"revenue\":null}]}]"
    );
    let flat = rs.flatten();
    assert_eq!(flat.columns, ["id", "name", "orders.n", "orders.revenue"]);
    let cells: Vec<Vec<Value>> = flat.rows.iter().map(|r| r.values.clone()).collect();
    assert_eq!(
        cells[2],
        vec![
            Value::Int(3),
            Value::Text("c".into()),
            Value::Int(0),
            Value::Null
        ]
    );

    // Grouped, a parent with none keeps an empty list: no group.
    let rs = result(
        &db,
        "get shops lookup orders on shop select status, count() as n group status \
         order n desc limit 1",
        &[],
    );
    let mut out = String::new();
    fenec_core::json::rows_array_into(&mut out, &rs);
    assert_eq!(
        out,
        "[{\"id\":1,\"name\":\"a\",\"orders\":[{\"status\":\"paid\",\"n\":2}]},\
         {\"id\":2,\"name\":\"b\",\"orders\":[{\"status\":\"paid\",\"n\":1}]},\
         {\"id\":3,\"name\":\"c\",\"orders\":[]}]"
    );

    // Off a `get`'s groups, keyed by a column.
    let rs = result(
        &db,
        "get orders select shop, sum(total) as t group shop order t desc \
         lookup shops on id = shop select name",
        &[],
    );
    let mut out = String::new();
    fenec_core::json::rows_array_into(&mut out, &rs);
    assert_eq!(
        out,
        "[{\"shop\":1,\"t\":16,\"shops\":[{\"name\":\"a\"}]},\
         {\"shop\":2,\"t\":7,\"shops\":[{\"name\":\"b\"}]}]"
    );
}

/// What a level that folds cannot be, and what a key off groups cannot
/// read, refused by name.
#[test]
fn what_does_not_fold_is_refused() {
    let mut db = Database::new();
    exec(&mut db, "create collection shops (name text, code text)");
    exec(
        &mut db,
        "create collection orders (shop int @hash, code text @hash, total float, status text)",
    );
    exec(&mut db, "create collection tags (label text @hash)");
    for (sql, says) in [
        (
            "get shops lookup orders on shop select status, count() as n",
            "neither aggregated nor grouped",
        ),
        (
            "get shops lookup orders on shop select status group status",
            "needs an aggregate",
        ),
        (
            "get shops lookup orders on shop select count() as n having n > 1",
            "follows `group`",
        ),
        (
            "get shops lookup orders on shop select total * 2 as x",
            "fields, or aggregates",
        ),
        (
            "get shops lookup orders on shop select count() as n order n limit 1",
            "picks the rows",
        ),
        (
            "get orders select status, count() as n group status lookup shops on id",
            "not a column",
        ),
        (
            "get orders select status, count() as n group status lookup tags on label = n",
            "holds no field's values",
        ),
        (
            "get orders select status, count() as n group status lookup orders on code = status",
            "cannot look itself up",
        ),
        ("get orders select count() as n near x [1.0]", "near"),
    ] {
        let e = error(&db, sql);
        assert!(e.contains(says), "{sql}: {e}");
    }
}
