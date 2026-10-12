//! `order <expr>`: a key worked out of each row -- `order distance(loc,
//! $1)`, `order a * b desc` -- in a `get` and in a write that picks its
//! rows (`set` and `del` with `order ... limit`).
//!
//! The spine is a reference: every ordered query runs as written, and its
//! answer is held to the filter's rows read with the key as a column of
//! their own (`select <expr> as k`) and sorted here by that key -- ties in
//! id order, as the scan leaves them -- then paged. Twice over points:
//! on a collection whose point field carries `@geo`, which a distance
//! from a point walks to the page, and on a twin without it, which reads
//! every row.

use fenec_core::prelude::*;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn run(db: &mut Database, sql: &str, params: &[Value]) -> Result<Response> {
    let stmt = fenec_ql::parse_one_for(db, sql)?;
    match stmt.is_read_only() {
        true => db.query(&stmt, params),
        false => db.execute_with(&stmt, params),
    }
}

fn rows(db: &mut Database, sql: &str, params: &[Value]) -> ResultSet {
    match run(db, sql, params).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        Response::Rows(rs) => rs,
        other => panic!("{sql}: {other:?}"),
    }
}

fn ids(rs: &ResultSet) -> Vec<u64> {
    rs.rows.iter().map(|r| r.id).collect()
}

fn pt(lon: f64, lat: f64) -> Value {
    Value::List(vec![Value::Float(lon), Value::Float(lat)])
}

/// Merrow's centre, and the points the rows crowd round.
const CENTRE: (f64, f64) = (29.09, 41.09);

/// Two collections of the same rows: `ix`, its point field with `@geo`,
/// and `tw` without. A row in ten holds no point when `holes`; ints,
/// floats and texts repeat, so keys tie.
fn fixture(n: u64, holes: bool) -> Database {
    let mut db = Database::new();
    for name in ["ix", "tw"] {
        let geo = if name == "ix" { "@geo" } else { "" };
        run(
            &mut db,
            &format!(
                "create collection {name} (a int, b float, name text, loc geo {geo}, \
                 ready bool, courier int)"
            ),
            &[],
        )
        .unwrap();
    }
    let mut r = Rng(0x0dde5);
    for i in 1..=n {
        let a = Value::Int(r.below(10) as i64);
        let b = match r.below(8) {
            0 => Value::Null,
            k => Value::Float(k as f64 * 0.5),
        };
        let name = Value::Text(["ada", "Bo", "cy", "Dee", "eli"][r.below(5) as usize].into());
        // Some rows on the very same point, so distances tie too.
        let loc = match (holes && i % 10 == 0, r.below(6)) {
            (true, _) => Value::Null,
            (false, 0) => pt(CENTRE.0 + 0.01, CENTRE.1),
            _ => pt(
                CENTRE.0 + (r.unit() - 0.5) * 0.2,
                CENTRE.1 + (r.unit() - 0.5) * 0.2,
            ),
        };
        let ready = Value::Bool(r.below(3) != 0);
        for name_ in ["ix", "tw"] {
            run(
                &mut db,
                &format!("put {name_} {{id: {i}, a: $1, b: $2, name: $3, loc: $4, ready: $5}}"),
                &[
                    a.clone(),
                    b.clone(),
                    name.clone(),
                    loc.clone(),
                    ready.clone(),
                ],
            )
            .unwrap();
        }
    }
    db
}

/// The rows of `coll` the filter passes in the order of `key` (ascending
/// unless `desc`), ties in id order, then paged: read with the key as a
/// column and sorted here.
fn reference(
    db: &mut Database,
    coll: &str,
    key: &str,
    desc: bool,
    filter: &str,
    page: (usize, usize),
    params: &[Value],
) -> Vec<u64> {
    let w = if filter.is_empty() {
        String::new()
    } else {
        format!(" where {filter}")
    };
    let rs = rows(db, &format!("get {coll} select {key} as k{w}"), params);
    let mut rows: Vec<(u64, Value)> = rs
        .rows
        .iter()
        .map(|r| (r.id, r.values[0].clone()))
        .collect();
    // The scan's ids come ascending; a stable sort keeps them so in a tie.
    rows.sort_by(|a, b| {
        let o = a.1.cmp_value(&b.1);
        match desc {
            false => o,
            true => o.reverse(),
        }
    });
    rows.into_iter()
        .map(|(id, _)| id)
        .skip(page.0)
        .take(page.1)
        .collect()
}

/// Each key ordered over each filter and page, in both directions, held
/// to the reference.
#[test]
fn a_key_worked_out_of_each_row_orders_as_the_rows_sorted_by_it() {
    let mut db = fixture(400, true);
    let keys = [
        "a * b",
        "a - coalesce(b, 0)",
        "case when a > 5 then 0 else 1 end",
        "lower(name)",
        "distance(loc, $1)",
        "greatest(a, 4) + id",
        "-a",
    ];
    let filters = [
        "",
        "ready",
        "a >= 3 and b is not null",
        "name = \"Bo\" or a = 1",
    ];
    let pages = [(0, 1), (0, 10), (5, 20), (0, 1000), (390, 50)];
    let p = [pt(CENTRE.0, CENTRE.1)];
    for key in keys {
        for filter in filters {
            for desc in [false, true] {
                for (offset, limit) in pages {
                    let w = if filter.is_empty() {
                        String::new()
                    } else {
                        format!(" where {filter}")
                    };
                    let dir = if desc { " desc" } else { "" };
                    let want = reference(&mut db, "tw", key, desc, filter, (offset, limit), &p);
                    for coll in ["tw", "ix"] {
                        let sql =
                            format!("get {coll}{w} order {key}{dir} limit {limit} offset {offset}");
                        assert_eq!(ids(&rows(&mut db, &sql, &p)), want, "{sql}");
                    }
                }
            }
        }
    }
    // A key after a field, and a field after a key: the second breaks the
    // first's ties.
    let sql = "get tw where ready order a, b * -1, id desc limit 30";
    let got = ids(&rows(&mut db, sql, &[]));
    let mut all = rows(&mut db, "get tw select a, b where ready", &[]).rows;
    all.sort_by(|x, y| {
        let neg = |v: &Value| match v {
            Value::Float(f) => Value::Float(-f),
            v => v.clone(),
        };
        x.values[0]
            .cmp_value(&y.values[0])
            .then(neg(&x.values[1]).cmp_value(&neg(&y.values[1])))
            .then(y.id.cmp(&x.id))
    });
    let want: Vec<u64> = all.iter().take(30).map(|r| r.id).collect();
    assert_eq!(got, want, "{sql}");
}

/// The point index answers `order distance(loc, <point>) limit n` where no
/// row the filter passes can lack a point, and the scan where one can --
/// the same rows either way.
#[test]
fn a_distance_from_a_point_walks_the_point_index_where_it_answers() {
    let p = [pt(CENTRE.0, CENTRE.1), Value::Int(3000)];
    let plan = |db: &mut Database, sql: &str| -> String {
        let rs = rows(db, &format!("explain {sql}"), &p);
        rs.rows
            .iter()
            .map(|r| format!("{:?}", r.values[0]))
            .collect::<Vec<_>>()
            .join("\n")
    };
    // Every row holds a point: walked, filtered or not.
    let mut db = fixture(300, false);
    for filter in ["", " where ready", " where a = 3"] {
        let sql = format!("get ix{filter} order distance(loc, $1) limit 7");
        let got = plan(&mut db, &sql);
        assert!(got.contains("point index"), "{sql}: {got}");
        let tw = sql.replace("get ix", "get tw");
        assert_eq!(
            ids(&rows(&mut db, &sql, &p)),
            ids(&rows(&mut db, &tw, &p)),
            "{sql}"
        );
    }
    // A row in ten without one: the scan, whose null distances come first
    // ascending -- unless a radius on the field leaves them out.
    let mut db = fixture(300, true);
    let sql = "get ix order distance(loc, $1) limit 7";
    assert!(!plan(&mut db, sql).contains("by its point index"));
    let got = ids(&rows(&mut db, sql, &p));
    assert_eq!(got, ids(&rows(&mut db, &sql.replace("ix", "tw"), &p)));
    assert!(
        got.iter().all(|id| id % 10 == 0),
        "the rows without a point first"
    );
    let sql = "get ix where distance(loc, $1) < $2 order distance(loc, $1) limit 7 offset 2";
    assert!(plan(&mut db, sql).contains("by its point index"));
    assert_eq!(
        ids(&rows(&mut db, sql, &p)),
        ids(&rows(&mut db, &sql.replace("ix", "tw"), &p))
    );
    // Descending, and with no limit, the scan.
    for sql in [
        "get ix where distance(loc, $1) < $2 order distance(loc, $1) desc limit 7",
        "get ix where distance(loc, $1) < $2 order distance(loc, $1)",
    ] {
        assert!(!plan(&mut db, sql).contains("by its point index"), "{sql}");
        assert_eq!(
            ids(&rows(&mut db, sql, &p)),
            ids(&rows(&mut db, &sql.replace("ix", "tw"), &p)),
            "{sql}"
        );
    }
    // Written as a literal, read as written (`Database::exactly`).
    let lit = "get ix where distance(loc, [29.09, 41.09]) < 3000 \
               order distance(loc, [29.09, 41.09]) limit 5";
    assert_eq!(
        ids(&rows(&mut db, lit, &[])),
        ids(&rows(
            &mut db,
            "get ix where distance(loc, $1) < $2 order distance(loc, $1) limit 5",
            &p
        ))
    );
}

/// A write picks as the `get` with the same clauses answers.
#[test]
fn a_write_picks_the_rows_its_order_puts_first() {
    let mut db = fixture(200, false);
    let p = pt(CENTRE.0 + 0.003, CENTRE.1 - 0.002);
    for coll in ["ix", "tw"] {
        let want = ids(&rows(
            &mut db,
            &format!("get {coll} where ready order distance(loc, $1) limit 3"),
            std::slice::from_ref(&p),
        ));
        let got = rows(
            &mut db,
            &format!(
                "set {coll} {{courier: 7, ready: false}} where ready \
                 order distance(loc, $1) limit 3 returning courier, ready"
            ),
            std::slice::from_ref(&p),
        );
        assert_eq!(ids(&got), want, "{coll}");
        assert!(got
            .rows
            .iter()
            .all(|r| r.values == [Value::Int(7), Value::Bool(false)]));
        let want = ids(&rows(
            &mut db,
            &format!("get {coll} order a * b desc, id limit 2"),
            &[],
        ));
        let got = rows(
            &mut db,
            &format!("del {coll} order a * b desc, id limit 2 returning id"),
            &[],
        );
        assert_eq!(ids(&got), want, "{coll}");
    }
}

#[test]
fn what_an_order_key_cannot_be() {
    let mut db = fixture(20, false);
    for (sql, why) in [
        ("get tw order a in (get ix select id)", "inner `get`"),
        ("get tw order sum(a) + 1", "needs `group`"),
        (
            "get tw select a, count(*) as n group a order n * 2",
            "names a column",
        ),
        ("get ix lookup tw on a order b * 2", "orders its children"),
        (
            "get tw near loc [29.09, 41.09] order a * 2",
            "`near` cannot",
        ),
        ("get tw order nope * 2", "nope"),
        ("set tw {a: 1} order unknown + 1 limit 1", "unknown"),
        (
            "set tw {a: 1} order a in (get ix select id) limit 1",
            "inner `get`",
        ),
        ("del tw order sum(a) + 1 limit 1", "needs `group`"),
        (
            "set tw {a: 1} where ready near loc [29.09, 41.09] limit 1",
            "a write picks its rows with `order`",
        ),
    ] {
        let e = run(&mut db, sql, &[]).unwrap_err().to_string();
        assert!(e.contains(why), "{sql}: {e}");
    }
    // A key of no row's fields orders nothing: the ids' order.
    assert_eq!(
        ids(&rows(&mut db, "get tw order 1 + 1 limit 3", &[])),
        [1, 2, 3]
    );
}

/// Sixteen couriers each claim the ready order nearest them, one at a
/// time until none is left, each claim one statement: `set ... order
/// distance(pickup, $2) limit 1 returning id`, and again as the inner
/// `get` with `near` in it. No order is claimed twice, every one is, and
/// each claim took the order nearest its courier of those still ready.
#[test]
fn sixteen_couriers_claiming_the_nearest_never_take_the_same_order() {
    const ORDERS: u64 = 3_000;
    for claim in [
        "set orders {courier: $1, ready: false} where ready \
         order distance(pickup, $2) limit 1 returning id, pickup",
        "set orders {courier: $1, ready: false} where id in \
         (get orders select id where ready near pickup $2 limit 1) returning id, pickup",
    ] {
        let mut db = Database::new();
        run(
            &mut db,
            "create collection orders (pickup geo @geo, ready bool @hash, courier int)",
            &[],
        )
        .unwrap();
        let mut r = Rng(42);
        let mut docs = Vec::new();
        for _ in 0..ORDERS {
            docs.push(
                Value::object(vec![
                    (
                        "pickup".into(),
                        pt(
                            CENTRE.0 + (r.unit() - 0.5) * 0.3,
                            CENTRE.1 + (r.unit() - 0.5) * 0.3,
                        ),
                    ),
                    ("ready".into(), Value::Bool(true)),
                ])
                .unwrap(),
            );
        }
        run(&mut db, "put orders $1", &[Value::List(docs)]).unwrap();
        let db = Arc::new(Mutex::new(db));
        let stmt = fenec_ql::parse_one(claim).unwrap();
        let hs: Vec<_> = (0..16)
            .map(|t| {
                let (db, stmt) = (db.clone(), stmt.clone());
                std::thread::spawn(move || {
                    let mut r = Rng(1000 + t);
                    let mut got = Vec::new();
                    loop {
                        let at = pt(
                            CENTRE.0 + (r.unit() - 0.5) * 0.3,
                            CENTRE.1 + (r.unit() - 0.5) * 0.3,
                        );
                        let mut g = db.lock().unwrap();
                        // What the claim should take: the nearest still
                        // ready, read under the same lock.
                        let near = g
                            .query(
                                &fenec_ql::parse_one(
                                    "get orders select id where ready near pickup $1 limit 1",
                                )
                                .unwrap(),
                                std::slice::from_ref(&at),
                            )
                            .unwrap();
                        let near = near.rows().unwrap().rows.first().map(|r| r.id);
                        let rs = match g.execute_with(&stmt, &[Value::Int(t as i64), at]).unwrap() {
                            Response::Rows(rs) => rs,
                            other => panic!("{other:?}"),
                        };
                        drop(g);
                        assert_eq!(rs.rows.first().map(|r| r.id), near);
                        match rs.rows.first() {
                            Some(row) => got.push(row.id),
                            None => break,
                        }
                    }
                    got
                })
            })
            .collect();
        let claimed: Vec<u64> = hs.into_iter().flat_map(|h| h.join().unwrap()).collect();
        let mut seen = HashSet::new();
        for id in &claimed {
            assert!(seen.insert(*id), "order {id} claimed twice: {claim}");
        }
        assert_eq!(seen.len() as u64, ORDERS, "every order claimed: {claim}");
        let mut db = db.lock().unwrap();
        let left = rows(&mut db, "get orders where ready count", &[]);
        assert_eq!(left.rows[0].values[0], Value::Int(0));
    }
}
