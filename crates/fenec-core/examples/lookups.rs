//! What a `lookup` over a key of several fields costs: `make lookup-bench`.
//!
//! 2 000 shops in 100 tenants (`org`), each shop's orders and each order's
//! lines, and the page a tenant's dashboard reads: its shops, the latest
//! three orders of each, the first five lines of each order -- 20 x 3 x 5.
//! Each way of keying the children holds the same documents under the same
//! ids, and every way's page is held to the others' before it is timed:
//!
//! - `composite`: `on (org, shop) = (org, id)` and `on (org, order) =
//!   (org, id)`, the children's indexes `@sorted(org, shop, at desc)` and
//!   `@sorted(org, order, line)`, whose runs are walked in the page's order;
//! - `joined`: the same key joined into one text field of each side, `k`
//!   `@hash` on the child -- `"7:1234"` -- kept up by hand on every write,
//!   each bucket read and sorted;
//! - `hash`: the tenant left out of the key, `shop @hash` and `order @hash`,
//!   the single-field lookup as it was;
//! - `walked`: the same single field leading an index over several,
//!   `@sorted(shop, at desc)` and `@sorted(order, line)`, walked.
//!
//! `single` runs `hash` alone -- the text any build reads -- for a build
//! from before the feature to be timed beside this one's in turns.
//!
//! Then `required`: the orders that hold a line of one product (`sku
//! @hash`, a tenth of the lines), counted -- from the children's side, the
//! parents named by the `id` part probed for the whole key, against the
//! joined key's parents probed one by one and the single `id` key's child
//! side.
//!
//! `make lookup-bench ARGS="all 10 10"`: orders a shop and lines an order,
//! 10 and 10 by default; `ARGS="all 500 2"` puts the latest three of 500
//! orders a shop, where a bucket is read whole and sorted for them.

use fenec_core::prelude::*;
use std::time::Instant;

const ORGS: usize = 100;
const SHOPS: usize = 2_000;
const SKUS: usize = 10;

fn exec(db: &mut Database, sql: &str) {
    for s in fenec_ql::parse(sql).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        db.execute(&s).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

fn put_all(db: &mut Database, collection: &str, docs: &[Vec<(String, Expr)>]) {
    for chunk in docs.chunks(20_000) {
        db.execute(&Statement::Put {
            collection: collection.into(),
            docs: chunk.to_vec(),
            insert: false,
            if_absent: false,
            docs_param: None,
            else_set: None,
            require: None,
        })
        .unwrap();
    }
}

fn int(name: &str, v: usize) -> (String, Expr) {
    (name.into(), Expr::Lit(Value::Int(v as i64)))
}

fn text(name: &str, v: String) -> (String, Expr) {
    (name.into(), Expr::Lit(Value::Text(v)))
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// One way of keying the children: its schema and its page.
struct Way {
    name: &'static str,
    orders: &'static str,
    lines: &'static str,
    page: &'static str,
    required: &'static str,
    joined: bool,
}

const WAYS: [Way; 4] = [
    Way {
        name: "composite",
        orders: "@sorted(org, shop, at desc)",
        lines: "@sorted(org, order, line)",
        page: "get shops select name where org = $1 limit 20 \
               lookup orders on (org, shop) = (org, id) select at order at desc limit 3 \
               lookup lines on (org, order) = (org, id) select line, qty order line limit 5",
        required: "get orders count \
                   lookup lines on (org, order) = (org, id) required where sku = $1",
        joined: false,
    },
    Way {
        name: "joined",
        orders: "k text @hash, pk text",
        lines: "k text @hash",
        page: "get shops select name where org = $1 limit 20 \
               lookup orders on k = k select at order at desc limit 3 \
               lookup lines on k = pk select line, qty order line limit 5",
        required: "get orders count lookup lines on k = pk required where sku = $1",
        joined: true,
    },
    Way {
        name: "hash",
        orders: "shop int @hash",
        lines: "order int @hash",
        page: "get shops select name where org = $1 limit 20 \
               lookup orders on shop select at order at desc limit 3 \
               lookup lines on order select line, qty order line limit 5",
        required: "get orders count lookup lines on order required where sku = $1",
        joined: false,
    },
    Way {
        name: "walked",
        orders: "@sorted(shop, at desc)",
        lines: "@sorted(order, line)",
        page: "get shops select name where org = $1 limit 20 \
               lookup orders on shop select at order at desc limit 3 \
               lookup lines on order select line, qty order line limit 5",
        required: "get orders count lookup lines on order required where sku = $1",
        joined: false,
    },
];

/// The database of `way`: shops, `per_shop` orders a shop and `per_order`
/// lines an order, the same documents under the same ids in every way.
fn load(way: &Way, per_shop: usize, per_order: usize) -> Database {
    let mut db = Database::new();
    // `hash` keys orders by shop alone, so its own fields are declared and
    // `shop`'s @hash with them; the rest add their index after the fields.
    let (shop_field, order_field) = match way.name {
        "hash" => ("", ""),
        _ => ("shop int, ", "order int, "),
    };
    exec(
        &mut db,
        &format!(
            "create collection shops (org int @hash, name text{}); \
             create collection orders (org int @hash, {shop_field}at int, total float, {}); \
             create collection lines (org int, {order_field}line int, qty int, sku int @hash, {})",
            if way.joined { ", k text" } else { "" },
            way.orders,
            way.lines
        ),
    );
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    let shops: Vec<_> = (1..=SHOPS)
        .map(|id| {
            let org = id % ORGS;
            let mut d = vec![int("org", org), text("name", format!("shop {id}"))];
            if way.joined {
                d.push(text("k", format!("{org}:{id}")));
            }
            d
        })
        .collect();
    put_all(&mut db, "shops", &shops);
    let mut orders = Vec::with_capacity(SHOPS * per_shop);
    for i in 0..SHOPS * per_shop {
        // Shops' orders interleaved, as they arrive.
        let shop = 1 + i % SHOPS;
        let org = shop % ORGS;
        let mut d = vec![
            int("org", org),
            int("shop", shop),
            int("at", (r.next() % 1_000_000) as usize),
            (
                "total".into(),
                Expr::Lit(Value::Float((r.next() % 1000) as f64)),
            ),
        ];
        if way.joined {
            d.push(text("k", format!("{org}:{shop}")));
            d.push(text("pk", format!("{org}:{}", i + 1)));
        }
        orders.push(d);
    }
    put_all(&mut db, "orders", &orders);
    let n = orders.len();
    let mut lines = Vec::with_capacity(n * per_order);
    for i in 0..n * per_order {
        let order = 1 + i % n;
        let org = (1 + (order - 1) % SHOPS) % ORGS;
        let mut d = vec![
            int("org", org),
            int("order", order),
            int("line", (r.next() % 100) as usize),
            int("qty", 1 + (r.next() % 9) as usize),
            int("sku", (r.next() % SKUS as u64) as usize),
        ];
        if way.joined {
            d.push(text("k", format!("{org}:{order}")));
        }
        lines.push(d);
    }
    put_all(&mut db, "lines", &lines);
    db
}

/// The page's ids, every level's, for the ways to be held to each other.
fn ids(rs: &ResultSet) -> Vec<Vec<u64>> {
    let mut out = vec![rs.rows.iter().map(|r| r.id).collect()];
    let mut level = rs.nested.as_ref();
    while let Some(l) = level {
        out.push(l.groups.iter().flatten().map(|r| r.id).collect());
        level = l.nested.as_deref();
    }
    out
}

/// p50 of `n` runs of `stmt` over the tenants in turn, in microseconds.
fn timed(db: &Database, stmt: &Statement, n: usize, params: &dyn Fn(usize) -> Vec<Value>) -> f64 {
    let mut t = Vec::with_capacity(n);
    for i in 0..n {
        let p = params(i);
        let s = Instant::now();
        let r = db.query(stmt, &p).unwrap();
        t.push(s.elapsed().as_secs_f64() * 1e6);
        std::hint::black_box(r);
    }
    t.sort_by(|a, b| a.total_cmp(b));
    t[n / 2]
}

fn run(ways: &[&Way], per_shop: usize, per_order: usize) {
    println!(
        "{SHOPS} shops in {ORGS} tenants, {} orders, {} lines",
        SHOPS * per_shop,
        SHOPS * per_shop * per_order
    );
    let mut pages: Vec<Vec<Vec<Vec<u64>>>> = Vec::new();
    let mut counts: Vec<Vec<i64>> = Vec::new();
    for way in ways {
        let s = Instant::now();
        let db = load(way, per_shop, per_order);
        let loaded = s.elapsed().as_secs_f64();
        let page = fenec_ql::parse_one(way.page).unwrap();
        let required = fenec_ql::parse_one(way.required).unwrap();
        let org = |i: usize| Value::Int((i * 37 % ORGS) as i64);
        // The answers first, every tenant's, which build the indexes too.
        let mut answers = Vec::new();
        let mut counted = Vec::new();
        for i in 0..ORGS {
            let r = db.query(&page, &[org(i)]).unwrap();
            answers.push(ids(r.rows().unwrap()));
            let sku = Value::Int((i % SKUS) as i64);
            let r = db.query(&required, &[sku]).unwrap();
            let Value::Int(c) = r.rows().unwrap().rows[0].values[0] else {
                panic!()
            };
            counted.push(c);
        }
        if let Some(first) = pages.first() {
            assert!(
                first == &answers,
                "{}: another page than the first way's",
                way.name
            );
            assert!(counts[0] == counted, "{}: another count", way.name);
        }
        pages.push(answers);
        counts.push(counted);
        let n = 5_000;
        let p50 = timed(&db, &page, n, &|i| vec![org(i)]);
        let rq = timed(&db, &required, n / 50, &|i| {
            vec![Value::Int((i % SKUS) as i64)]
        });
        println!(
            "  {:<10} page {:>8.1} us   required count {:>8.1} us   (loaded in {loaded:.1} s)",
            way.name, p50, rq
        );
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let what = args.next().unwrap_or_else(|| "all".into());
    let per_shop: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(10);
    let per_order: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(10);
    let ways: Vec<&Way> = match what.as_str() {
        "single" => WAYS.iter().filter(|w| w.name == "hash").collect(),
        _ => WAYS.iter().collect(),
    };
    run(&ways, per_shop, per_order);
}
