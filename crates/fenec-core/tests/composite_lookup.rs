//! A `lookup` over a key of several fields never changes an answer.
//!
//! Every lookup here runs on a database whose children carry indexes over
//! several fields, and is held -- each parent's children, their order, ties
//! and page, every level of a chain and the rows `flatten` makes of it --
//! to a reference the test works out itself from plain `get`s over a twin
//! with no such index: a child is a parent's where each pair of `on` holds
//! equal values and neither side a null. Through writes, deletes, updates
//! of a key's parts on either side, a block put back, a reopen, and
//! children past their `@ttl`.

use fenec_core::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

const DAY: i64 = 86_400_000;
/// The clock both databases read: `seen` older than thirty days before it
/// is past its time.
const NOW: i64 = 1_790_000_000_000;

fn schema(indexed: bool) -> Vec<String> {
    let ix = |s: &str| {
        if indexed {
            s.to_string()
        } else {
            String::new()
        }
    };
    vec![
        "create collection shops (org int, code text, city text)".into(),
        format!(
            "create collection orders (org int, shop text, n int, at timestamp, total float, \
             status text{})",
            ix(", @sorted(org, shop, at desc), @unique(org, shop, n)")
        ),
        format!(
            "create collection lines (org int, order int, line int, sku text @hash, qty int, \
             price float, seen timestamp @ttl(30d){})",
            ix(", @sorted(org, order, line), @sorted(order, org, qty desc), @sorted(sku, qty)")
        ),
        format!(
            "create collection rates (org float, price float, label text{})",
            ix(", @sorted(org, price)")
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

thread_local! {
    /// The reference's `get`s over the twin, by their text: the same few
    /// are asked for every lookup of a stage, and the stage's writes empty
    /// it ([`check`]).
    static SEEN: RefCell<HashMap<String, Rc<ResultSet>>> = RefCell::new(HashMap::new());
    static KEYS: RefCell<HashMap<String, Rc<Keys>>> = RefCell::new(HashMap::new());
}

type Keys = HashMap<u64, Option<Vec<K>>>;

/// `result` over the twin, asked once a stage.
fn seen(plain: &Database, sql: &str) -> Rc<ResultSet> {
    if let Some(r) = SEEN.with(|s| s.borrow().get(sql).cloned()) {
        return r;
    }
    let r = Rc::new(result(plain, sql, &[]));
    SEEN.with(|s| s.borrow_mut().insert(sql.to_string(), r.clone()));
    r
}

fn plan(db: &Database, sql: &str) -> String {
    result(db, &format!("explain {sql}"), &[])
        .rows
        .iter()
        .map(|r| format!("{:?}", r.values))
        .collect::<Vec<_>>()
        .join("\n")
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

/// A document of `coll`, as FenecQL, its fields drawn from few values so
/// that keys repeat and ties are everywhere; `null` among them, and `-0.0`
/// beside `0.0`, which a key holds as one.
fn document(r: &mut Rng, coll: &str, n: usize) -> String {
    let org = r.pick(&["null", "1", "2", "2", "3"]);
    match coll {
        "shops" => format!(
            "{{org: {org}, code: {}, city: {}}}",
            r.pick(&["null", "\"a\"", "\"b\"", "\"c\"", "\"a\\u0000\""]),
            r.pick(&["\"x\"", "\"y\""])
        ),
        "orders" => format!(
            "{{org: {org}, shop: {}, n: {n}, at: {}, total: {}, status: {}}}",
            r.pick(&["null", "\"a\"", "\"b\"", "\"b\"", "\"c\""]),
            r.pick(&[
                "null",
                "\"2025-06-01\"",
                "\"2026-01-01\"",
                "\"2026-01-01\"",
                "\"2026-09-01\""
            ]),
            r.pick(&["null", "-0.0", "0.0", "1.5", "2.5"]),
            r.pick(&["\"open\"", "\"done\""])
        ),
        "lines" => {
            let order = match r.next() % 12 {
                0 => "null".to_string(),
                1 => "999".to_string(),
                k => (1 + (r.next() + k) % 60).to_string(),
            };
            let seen = match r.next() % 4 {
                0 => "null".to_string(),
                1 => (NOW - 40 * DAY).to_string(),
                _ => (NOW - (r.next() % 20) as i64 * DAY).to_string(),
            };
            format!(
                "{{org: {org}, order: {order}, line: {}, sku: {}, qty: {}, price: {}, seen: {seen}}}",
                r.pick(&["null", "1", "2", "3", "3"]),
                r.pick(&["\"a\"", "\"b\"", "\"c\"", "\"d\"", "\"e\""]),
                r.pick(&["null", "1", "2", "3"]),
                r.pick(&["null", "-0.0", "0.0", "1.0", "2.0"])
            )
        }
        _ => format!(
            "{{org: {org}, price: {}, label: {}}}",
            r.pick(&["null", "-0.0", "0.0", "1.0", "2.0"]),
            r.pick(&["\"a\"", "\"b\"", "\"q\""])
        ),
    }
}

/// `ix` with the indexes over several fields, `plain` without, holding the
/// same documents under the same ids.
fn twins() -> (Database, Database) {
    let (mut ix, mut plain) = (Database::new(), Database::new());
    for (db, indexed) in [(&mut ix, true), (&mut plain, false)] {
        db.set_clock(Some(NOW));
        for s in schema(indexed) {
            exec(db, &s);
        }
    }
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    for (coll, n) in [("shops", 30), ("orders", 60), ("lines", 400), ("rates", 40)] {
        let docs: Vec<String> = (0..n).map(|i| document(&mut r, coll, i)).collect();
        let put = format!("put {coll} [{}]", docs.join(", "));
        exec(&mut ix, &put);
        exec(&mut plain, &put);
    }
    (ix, plain)
}

/// A key's part as a lookup matches it: numbers by value, `-0.0` as `0.0`.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum K {
    Num(u64),
    Text(String),
}

fn key_of(v: &Value) -> Option<K> {
    let num = |f: f64| K::Num(if f == 0.0 { 0 } else { f.to_bits() });
    match v {
        Value::Int(i) | Value::Timestamp(i) => Some(num(*i as f64)),
        Value::Float(f) => Some(num(*f)),
        Value::Text(t) => Some(K::Text(t.clone())),
        _ => None,
    }
}

/// One level of a chain: its collection, its key, and its clauses.
#[derive(Clone)]
struct Level {
    coll: &'static str,
    on: &'static [(&'static str, &'static str)],
    select: &'static str,
    filter: String,
    order: &'static str,
    limit: Option<usize>,
    offset: usize,
    required: bool,
}

impl Level {
    fn new(coll: &'static str, on: &'static [(&'static str, &'static str)]) -> Level {
        Level {
            coll,
            on,
            select: "",
            filter: String::new(),
            order: "",
            limit: None,
            offset: 0,
            required: false,
        }
    }

    fn text(&self) -> String {
        let child: Vec<&str> = self.on.iter().map(|p| p.0).collect();
        let parent: Vec<&str> = self.on.iter().map(|p| p.1).collect();
        let mut s = format!(
            "lookup {} on ({}) = ({})",
            self.coll,
            child.join(", "),
            parent.join(", ")
        );
        for part in [self.select, &self.filter, self.order] {
            if !part.is_empty() {
                s.push(' ');
                s.push_str(part);
            }
        }
        if let Some(l) = self.limit {
            s.push_str(&format!(" limit {l}"));
        }
        if self.offset > 0 {
            s.push_str(&format!(" offset {}", self.offset));
        }
        if self.required {
            s.push_str(" required");
        }
        s
    }
}

/// The query's top: its collection and clauses, the page apart.
struct Top {
    coll: &'static str,
    filter: &'static str,
    order: &'static str,
    limit: Option<usize>,
    offset: usize,
}

/// The query: `count` counts the parents a `required` level keeps, and
/// takes no order and no page.
fn query_text(top: &Top, levels: &[Level], count: bool) -> String {
    let mut s = format!("get {} {}", top.coll, top.filter);
    if count {
        s.push_str(" count");
    } else {
        s.push_str(&format!(" {}", top.order));
        if let Some(l) = top.limit {
            s.push_str(&format!(" limit {l}"));
        }
        if top.offset > 0 {
            s.push_str(&format!(" offset {}", top.offset));
        }
    }
    for l in levels {
        s.push(' ');
        s.push_str(&l.text());
    }
    s
}

/// What the reference knows of a level.
struct Known {
    /// The rows the level's filter passes, in its order.
    passing: Vec<Row>,
    columns: Vec<String>,
    /// Each passing row's place in `passing`, by its key.
    by_key: HashMap<Vec<K>, Vec<usize>>,
    /// Each row of the level above's key, `None` where a part is null.
    parent_key: Rc<Keys>,
}

/// Each row's values of `fields` (`id` its id) out of `get coll`, worked
/// out once a stage.
fn keys(plain: &Database, coll: &str, fields: &[&str]) -> Rc<Keys> {
    let name = format!("{coll} {fields:?}");
    if let Some(k) = KEYS.with(|s| s.borrow().get(&name).cloned()) {
        return k;
    }
    let k = Rc::new(keys_of(plain, coll, fields));
    KEYS.with(|s| s.borrow_mut().insert(name, k.clone()));
    k
}

fn keys_of(plain: &Database, coll: &str, fields: &[&str]) -> Keys {
    let rs = seen(plain, &format!("get {coll}"));
    let at: Vec<Option<usize>> = fields
        .iter()
        .map(|f| rs.columns.iter().position(|c| c == f))
        .collect();
    rs.rows
        .iter()
        .map(|r| {
            let k: Option<Vec<K>> = at
                .iter()
                .map(|p| match p {
                    Some(p) => key_of(&r.values[*p]),
                    None => Some(K::Num((r.id as f64).to_bits())),
                })
                .collect();
            (r.id, k)
        })
        .collect()
}

/// The answer worked out from plain `get`s on `plain`.
fn reference(plain: &Database, top: &Top, levels: &[Level], count: bool) -> ResultSet {
    let mut known = Vec::new();
    for (d, l) in levels.iter().enumerate() {
        let above = if d == 0 { top.coll } else { levels[d - 1].coll };
        let rs = seen(
            plain,
            &format!("get {} {} {} {}", l.coll, l.select, l.filter, l.order),
        );
        let child: Vec<&str> = l.on.iter().map(|p| p.0).collect();
        let parent: Vec<&str> = l.on.iter().map(|p| p.1).collect();
        let child_keys = keys(plain, l.coll, &child);
        let mut by_key: HashMap<Vec<K>, Vec<usize>> = HashMap::new();
        for (i, r) in rs.rows.iter().enumerate() {
            if let Some(Some(k)) = child_keys.get(&r.id) {
                by_key.entry(k.clone()).or_default().push(i);
            }
        }
        known.push(Known {
            passing: rs.rows.clone(),
            columns: rs.columns.clone(),
            by_key,
            parent_key: keys(plain, above, &parent),
        });
    }

    // A row of level `d - 1` (the top's at 0) and the level's rows hung
    // off it, in order, a `required` level below met.
    fn children<'a>(known: &'a [Known], levels: &[Level], d: usize, pid: u64) -> Vec<&'a Row> {
        let k = &known[d];
        let Some(Some(key)) = k.parent_key.get(&pid) else {
            return Vec::new();
        };
        let Some(places) = k.by_key.get(key) else {
            return Vec::new();
        };
        places
            .iter()
            .map(|&i| &k.passing[i])
            .filter(|r| match levels.get(d + 1) {
                Some(n) if n.required => !children(known, levels, d + 1, r.id).is_empty(),
                _ => true,
            })
            .collect()
    }

    let top_rs = seen(
        plain,
        &format!("get {} {} {}", top.coll, top.filter, top.order),
    );
    let mut rows: Vec<Row> = top_rs
        .rows
        .iter()
        .filter(|r| !levels[0].required || !children(&known, levels, 0, r.id).is_empty())
        .cloned()
        .collect();
    if count {
        return ResultSet {
            columns: vec!["count".into()],
            rows: vec![Row {
                id: 0,
                values: vec![Value::Int(rows.len() as i64)],
                score: None,
            }],
            nested: None,
            facets: Vec::new(),
        };
    }
    rows = rows
        .into_iter()
        .skip(top.offset)
        .take(top.limit.unwrap_or(usize::MAX))
        .collect();

    let mut nested: Vec<Nested> = Vec::new();
    let mut parents: Vec<u64> = rows.iter().map(|r| r.id).collect();
    for (d, l) in levels.iter().enumerate() {
        let groups: Vec<Vec<Row>> = parents
            .iter()
            .map(|&p| {
                children(&known, levels, d, p)
                    .into_iter()
                    .skip(l.offset)
                    .take(l.limit.unwrap_or(usize::MAX))
                    .cloned()
                    .collect()
            })
            .collect();
        parents = groups.iter().flatten().map(|r| r.id).collect();
        nested.push(Nested {
            name: l.coll.into(),
            columns: known[d].columns.clone(),
            groups,
            nested: None,
        });
    }
    let mut chain: Option<Nested> = None;
    for mut n in nested.into_iter().rev() {
        n.nested = chain.map(Box::new);
        chain = Some(n);
    }
    ResultSet {
        columns: top_rs.columns.clone(),
        rows,
        nested: chain,
        facets: Vec::new(),
    }
}

/// The lookup on `ix` against the reference on `plain`, nested and flat.
fn holds(ix: &Database, plain: &Database, top: &Top, levels: &[Level], count: bool, stage: &str) {
    let q = query_text(top, levels, count);
    let got = result(ix, &q, &[]);
    let want = reference(plain, top, levels, count);
    if count {
        assert_eq!(got.rows[0].values, want.rows[0].values, "{stage}: {q}");
        return;
    }
    assert_eq!(got, want, "{stage}: {q}");
    assert_eq!(got.flatten(), want.flatten(), "{stage}: flat {q}");
}

const ORDERS_ON: &[(&str, &str)] = &[("org", "org"), ("shop", "code")];
const LINES_ON: &[(&str, &str)] = &[("order", "id"), ("org", "org")];
const RATES_ON: &[(&str, &str)] = &[("org", "org"), ("price", "price")];
/// One field leading an index over several, with no `@hash`.
const ORDER_ON: &[(&str, &str)] = &[("order", "id")];
/// One field with a `@hash`, leading an index over several too: the index
/// where the level's order walks it, the bucket where not.
const SKU_ON: &[(&str, &str)] = &[("sku", "label")];

fn check(ix: &Database, plain: &Database, stage: &str) {
    SEEN.with(|s| s.borrow_mut().clear());
    KEYS.with(|s| s.borrow_mut().clear());
    let pages: [(Option<usize>, usize); 4] = [(None, 0), (Some(2), 0), (Some(3), 1), (Some(0), 0)];
    // Shops and their orders: a key of an int and a text, the order the
    // index's parts after it, the other way round, or no part of it.
    for (tf, tl) in [("", None), ("where org = 2", Some(4))] {
        let top = Top {
            coll: "shops",
            filter: tf,
            order: "order code, org desc",
            limit: tl,
            offset: 0,
        };
        for f in [
            "",
            "where status = \"open\"",
            "where total > 0",
            "where at >= \"2026-01-01\" and total != 1.5",
        ] {
            for o in [
                "",
                "order at desc",
                "order at",
                "order shop, at desc",
                "order at desc, n",
                "order total desc",
                "order status, at",
            ] {
                for (limit, offset) in pages {
                    for required in [false, true] {
                        let l = Level {
                            filter: f.into(),
                            order: o,
                            limit,
                            offset,
                            required,
                            ..Level::new("orders", ORDERS_ON)
                        };
                        holds(ix, plain, &top, std::slice::from_ref(&l), false, stage);
                        if required {
                            holds(ix, plain, &top, &[l], true, stage);
                        }
                    }
                }
            }
        }
    }
    // Orders and their lines: the parent's `id` one of the parts, written
    // in another order than the index's; `sku = ...` names a hash bucket,
    // which a `required` may start from.
    for (tf, to) in [
        ("", ""),
        ("where id = 3", ""),
        ("where status = \"open\"", "order at desc"),
        ("where org = 2", "order total"),
    ] {
        let top = Top {
            coll: "orders",
            filter: tf,
            order: to,
            limit: None,
            offset: 0,
        };
        for f in [
            "",
            "where sku = \"a\"",
            "where sku = \"b\" and qty >= 2",
            "where price = 0",
        ] {
            for o in [
                "",
                "order line",
                "order line desc",
                "order qty desc",
                "order qty",
                "order price, line",
            ] {
                for (limit, offset) in pages {
                    for required in [false, true] {
                        let l = Level {
                            select: "select line, qty, sku",
                            filter: f.into(),
                            order: o,
                            limit,
                            offset,
                            required,
                            ..Level::new("lines", LINES_ON)
                        };
                        holds(ix, plain, &top, std::slice::from_ref(&l), false, stage);
                        if required {
                            holds(ix, plain, &top, &[l], true, stage);
                        }
                    }
                }
            }
        }
    }
    // One field: a run of the index it leads, or its bucket.
    for (top, on) in [("orders", ORDER_ON), ("rates", SKU_ON)] {
        let top = Top {
            coll: top,
            filter: "",
            order: "",
            limit: None,
            offset: 0,
        };
        for f in ["", "where org = 2", "where qty > 1"] {
            for o in ["", "order qty", "order qty desc", "order line, id"] {
                for (limit, offset) in pages {
                    for required in [false, true] {
                        let l = Level {
                            select: "select org, qty, line",
                            filter: f.into(),
                            order: o,
                            limit,
                            offset,
                            required,
                            ..Level::new("lines", on)
                        };
                        holds(ix, plain, &top, &[l], false, stage);
                    }
                }
            }
        }
    }
    // Three levels: a level's `required` drops rows of the level above
    // only, and each level's page counts per parent.
    let top = Top {
        coll: "shops",
        filter: "",
        order: "",
        limit: Some(12),
        offset: 2,
    };
    for (r1, r2, r3) in [
        (false, false, false),
        (true, false, false),
        (false, true, false),
        (true, true, true),
        (false, false, true),
        (true, false, true),
    ] {
        let levels = [
            Level {
                order: "order at desc",
                limit: Some(3),
                required: r1,
                ..Level::new("orders", ORDERS_ON)
            },
            Level {
                select: "select line, price",
                filter: "where qty > 1".into(),
                order: "order line",
                limit: Some(2),
                required: r2,
                ..Level::new("lines", LINES_ON)
            },
            Level {
                order: "order label",
                required: r3,
                ..Level::new("rates", RATES_ON)
            },
        ];
        holds(ix, plain, &top, &levels, false, stage);
        if r1 {
            holds(ix, plain, &top, &levels, true, stage);
        }
    }
}

/// A few of `check`'s lookups, read inside a block before it lands.
fn in_a_block(ix: &Database, plain: &Database) {
    SEEN.with(|s| s.borrow_mut().clear());
    KEYS.with(|s| s.borrow_mut().clear());
    let all = |coll| Top {
        coll,
        filter: "",
        order: "",
        limit: None,
        offset: 0,
    };
    for required in [false, true] {
        let orders = Level {
            order: "order at desc",
            limit: Some(2),
            required,
            ..Level::new("orders", ORDERS_ON)
        };
        let lines = Level {
            filter: "where sku = \"a\"".into(),
            order: "order line",
            limit: Some(2),
            required,
            ..Level::new("lines", LINES_ON)
        };
        let one = Level {
            order: "order qty desc",
            required,
            ..Level::new("lines", ORDER_ON)
        };
        holds(ix, plain, &all("shops"), &[orders], false, "in a block");
        holds(ix, plain, &all("orders"), &[lines], false, "in a block");
        holds(ix, plain, &all("orders"), &[one], false, "in a block");
    }
}

/// Every write a key's part goes through on either side.
fn write(db: &mut Database) {
    for s in [
        // A child moves to another parent, to none, and back.
        "set lines {org: 3} where org = 2 and line = 1",
        "set lines {order: 5} where order = 7",
        "set lines {order: null} where sku = \"e\" and qty = 1",
        "set orders {shop: \"b\"} where shop = \"a\" and status = \"done\"",
        "set orders {org: null} where org = 3 and total = 2.5",
        "set orders {at: \"2026-01-01\"} where at is null",
        // A parent's key moves under its children.
        "set shops {code: \"c\"} where code = \"b\" and city = \"x\"",
        "set shops {org: 1} where org is null",
        "set rates {price: 2.0} where price = 0",
        // Deletes on both sides; an order deleted leaves lines naming it.
        "del lines where qty = 3 and sku = \"c\"",
        "del orders where id = 4",
        "del shops where city = \"y\" and org = 3",
        // Written over, and new rows past the ids there were.
        "put lines {id: 9, org: 2, order: 3, line: 2, sku: \"a\", qty: 2, price: 1.0}",
        "put orders {org: 2, shop: \"b\", n: 100, at: \"2026-02-02\", total: 1.5, status: \"open\"}",
        "put lines {org: 2, order: 61, line: 1, sku: \"a\", qty: 3, price: 2.0}",
        "put lines {org: 2, order: 61, line: 1, sku: \"b\", qty: 1, price: 2.0}",
    ] {
        exec(db, s);
    }
}

#[test]
fn a_lookup_over_several_fields_gives_the_references_answer() {
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
        exec(db, "set lines {org: 1} where org = 2");
        exec(db, "del orders where status = \"open\"");
        exec(db, "set shops {code: \"a\"} where code = \"c\"");
        exec(
            db,
            "put lines {org: 1, order: 3, line: 1, sku: \"a\", qty: 2, price: 0.0}",
        );
    }
    in_a_block(&ix, &plain);
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

    // Time passes: more lines are past their `@ttl`.
    for db in [&mut reopened, &mut plain] {
        db.set_clock(Some(NOW + 15 * DAY));
    }
    check(&reopened, &plain, "after time passed");
}

/// The page shapes it is for take the index, and say so: a walk where the
/// level's order is the index's past the key, a probe and a sort where it
/// is not, and both sides of `required`.
#[test]
fn explain_names_the_index_the_walk_and_the_side() {
    let (ix, _) = twins();
    let p = plan(
        &ix,
        "get orders lookup lines on (order, org) = (id, org) order line limit 2",
    );
    assert!(
        p.contains(
            "lookup: lines on (order, org), the index on (org, order, line) walked for 60 parents"
        ),
        "{p}"
    );
    let p = plan(
        &ix,
        "get orders lookup lines on (order, org) = (id, org) order qty desc limit 2",
    );
    assert!(
        p.contains("the index on (order, org, qty desc) walked for"),
        "{p}"
    );
    let p = plan(
        &ix,
        "get orders lookup lines on (order, org) = (id, org) order line desc limit 2",
    );
    assert!(
        p.contains("the index on (org, order, line) walked backwards for"),
        "{p}"
    );
    let p = plan(
        &ix,
        "get orders lookup lines on (order, org) = (id, org) order price limit 2",
    );
    assert!(
        p.contains("lookup: lines on (order, org), the index on (org, order, line) probed for"),
        "{p}"
    );
    let p = plan(
        &ix,
        "get orders lookup lines on (order, org) = (id, org) required where sku = \"a\"",
    );
    assert!(p.contains("required: from the child side"), "{p}");
    let p = plan(
        &ix,
        "get orders where id = 3 lookup lines on (order, org) = (id, org) required where sku = \"a\"",
    );
    assert!(
        p.contains("required: from the parent side, 1 parents probed in lines"),
        "{p}"
    );
    // One field: the index it leads walked where the order is its, else
    // its bucket probed; one with no `@hash` probes the index.
    let p = plan(
        &ix,
        "get rates lookup lines on sku = label order qty desc limit 2",
    );
    assert!(
        p.contains("lookup: lines on sku, the index on (sku, qty) walked backwards for 40 parents"),
        "{p}"
    );
    let p = plan(
        &ix,
        "get rates lookup lines on sku = label order line limit 2",
    );
    assert!(
        p.contains("lookup: lines on sku, the hash index probed for"),
        "{p}"
    );
    let p = plan(&ix, "get orders lookup lines on order order line limit 2");
    assert!(
        p.contains("lookup: lines on order, the index on (order, org, qty desc) probed for"),
        "{p}"
    );
    // No part paired with the parent's `id`: the parents are probed.
    let p = plan(
        &ix,
        "get shops lookup orders on (org, shop) = (org, code) required where status = \"open\"",
    );
    assert!(p.contains("required: from the parent side"), "{p}");
}

/// A walked page reads as far as the page: the latest of each order's
/// lines are its first entries.
#[test]
fn a_walk_stops_at_the_page() {
    let mut db = Database::new();
    exec(&mut db, "create collection orders (org int, ref text)");
    exec(
        &mut db,
        "create collection lines (org int, order int, line int, @sorted(org, order, line desc))",
    );
    exec(
        &mut db,
        "put orders [{org: 1, ref: \"a\"}, {org: 2, ref: \"b\"}]",
    );
    let docs: Vec<String> = (0..500)
        .map(|i| format!("{{org: {}, order: {}, line: {i}}}", 1 + i % 2, 1 + i % 2))
        .collect();
    exec(&mut db, &format!("put lines [{}]", docs.join(", ")));
    let q =
        "get orders lookup lines on (org, order) = (org, id) select line order line desc limit 3";
    let rs = result(&db, q, &[]);
    let n = rs.nested.unwrap();
    let lines: Vec<Vec<Value>> = n
        .groups
        .iter()
        .map(|g| g.iter().map(|r| r.values[0].clone()).collect())
        .collect();
    assert_eq!(
        lines,
        vec![
            vec![Value::Int(498), Value::Int(496), Value::Int(494)],
            vec![Value::Int(499), Value::Int(497), Value::Int(495)],
        ]
    );
    // The page is a parameter as well, read where the walk reads it.
    let q = "get orders lookup lines on (org, order) = (org, id) select line order line desc limit $1 offset $2";
    let p = result(&db, q, &[Value::Int(2), Value::Int(1)]);
    let n = p.nested.unwrap();
    assert_eq!(n.groups[0].len(), 2);
    assert_eq!(n.groups[0][0].values[0], Value::Int(496));
}

/// What a key over several fields cannot be is refused, naming why.
#[test]
fn refusals_name_what_is_missing() {
    let (ix, plain) = twins();
    // Refused where it is read, or where it is run.
    let err = |db: &Database, q: &str| match fenec_ql::parse_one(q) {
        Ok(s) => db.query(&s, &[]).unwrap_err().to_string(),
        Err(e) => e.to_string(),
    };
    // No index whose leading parts are the child's fields: named, with
    // the one to declare.
    let e = err(
        &plain,
        "get orders lookup lines on (order, org) = (id, org)",
    );
    assert!(
        e.contains("needs an index") && e.contains("@sorted(order, org)"),
        "{e}"
    );
    // The fields lead no index together: `line` is a third part.
    let e = err(&ix, "get orders lookup lines on (org, line) = (org, id)");
    assert!(e.contains("needs an index"), "{e}");
    // A child's `id` names one row alone.
    let e = err(&ix, "get orders lookup lines on (id, org) = (id, org)");
    assert!(e.contains("`id`"), "{e}");
    // A field twice: no index leads with it twice.
    let e = err(&ix, "get orders lookup lines on (org, org) = (org, id)");
    assert!(e.contains("needs an index"), "{e}");
    // The types of a pair must match, as a single key's.
    let e = err(&ix, "get shops lookup orders on (org, shop) = (code, org)");
    assert!(e.contains("key types do not match"), "{e}");
    // A field neither side has.
    let e = err(&ix, "get orders lookup lines on (org, order) = (org, nope)");
    assert!(e.contains("nope"), "{e}");
    // The two sides name as many fields.
    let e = fenec_ql::parse_one("get orders lookup lines on (org, order) = (org)")
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("pairs a field of each side for each part"),
        "{e}"
    );
    // One pair in parentheses is a single key.
    let one = result(
        &ix,
        "get orders lookup lines on (order) = (id) order line",
        &[],
    );
    let two = result(&ix, "get orders lookup lines on order order line", &[]);
    assert_eq!(one, two);
}

/// A part keys its child field's values as the index does, so a parent's
/// value finds what the field's own equality finds: a `nocase` text every
/// spelling of its fold, a collated one its bytes alone, a timestamp the
/// int of its milliseconds, an int the float it equals -- as a single key
/// through `@hash` does.
#[test]
fn a_part_matches_as_its_field_keys() {
    let mut db = Database::new();
    for sql in [
        "create collection p (who text, place text, at timestamp, n int)",
        "create collection c (who text nocase, place text collate tr, at int, n float, k int, \
         @sorted(who, place, k), @sorted(at, n, k))",
        r#"put p [{who: "Ab", place: "ağaç", at: 5, n: 2}, {who: "x", place: "Ağaç", at: 7, n: 3}]"#,
        r#"put c [{who: "ab", place: "ağaç", at: 5, n: 2.0, k: 1}, {who: "AB", place: "ağaç", at: 5, n: 2.5, k: 2},
                  {who: "aB", place: "Ağaç", at: 7, n: 3.0, k: 3}, {who: "X", place: "Ağaç", at: 7, n: 3.0, k: 4}]"#,
    ] {
        exec(&mut db, sql);
    }
    let kids = |q: &str| -> Vec<Vec<Value>> {
        let rs = result(&db, q, &[]);
        let n = rs.nested.expect("nested");
        n.groups
            .iter()
            .map(|g| g.iter().map(|r| r.values[0].clone()).collect())
            .collect()
    };
    assert_eq!(
        kids("get p lookup c on (who, place) = (who, place) select k order k"),
        vec![vec![Value::Int(1), Value::Int(2)], vec![Value::Int(4)]]
    );
    assert_eq!(
        kids("get p lookup c on (at, n) = (at, n) select k order k"),
        vec![vec![Value::Int(1)], vec![Value::Int(3), Value::Int(4)]]
    );
}
