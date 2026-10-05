//! `in (get ...)`: the inner `get` runs once, before the query, and its one
//! column is the list an `in [..]` is answered with.
//!
//! The spine is a twin: every query runs as written, and again with the
//! inner `get` run by hand and its values written out as `in [$n, ...]` --
//! the two must agree row for row, through every place a filter goes:
//! `get`, `count`, an aggregate, `near`, `match`, a `lookup` level's
//! filter, `set` and `del`. The rest is what the twin cannot say: the
//! bound on the set, the depth, the shapes refused, and what `explain`
//! shows.

use fenec_core::prelude::*;
use fenec_core::query::{MAX_SUBQUERY_DEPTH, MAX_SUBQUERY_VALUES};

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
}

fn run(db: &mut Database, sql: &str, params: &[Value]) -> Response {
    let mut last = None;
    for stmt in fenec_ql::parse(sql).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        last = Some(
            db.execute_with(&stmt, params)
                .unwrap_or_else(|e| panic!("{sql}: {e}")),
        );
    }
    last.unwrap()
}

fn query(db: &Database, sql: &str, params: &[Value]) -> Result<Response> {
    db.query(&fenec_ql::parse_one(sql)?, params)
}

fn rows(db: &Database, sql: &str, params: &[Value]) -> ResultSet {
    match query(db, sql, params).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        Response::Rows(rs) => rs,
        other => panic!("{sql}: {other:?}"),
    }
}

/// Customers, a few tiers, and orders pointing at customers -- some at
/// none, some with no customer -- by an `@hash` id, by an unindexed code,
/// with a vector, a text and an ordered field to search and sort by.
fn fixture() -> Database {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection tiers (n int, ok bool);
         create collection customers (country text @hash, tier int, code text @hash);
         create collection orders (customer int @hash, code text, status text, total float,
                                   v vector<3> @hnsw(cosine), body text @text, at int @sorted,
                                   flag int)",
        &[],
    );
    run(
        &mut db,
        "put tiers [{n: 0, ok: false}, {n: 1, ok: true}, {n: 2, ok: false}, {n: 3, ok: true}]",
        &[],
    );
    let mut r = Rng(0x5eed);
    let countries = ["TR", "DE", "US"];
    let mut docs = Vec::new();
    for i in 0..300 {
        let country = match r.below(5) {
            4 => "null".to_string(),
            k => format!("\"{}\"", countries[k as usize % 3]),
        };
        docs.push(format!(
            "{{country: {country}, tier: {}, code: \"c{}\"}}",
            r.below(5),
            i + 1
        ));
    }
    run(&mut db, &format!("put customers [{}]", docs.join(",")), &[]);
    let words = ["kahve", "demlik", "kupa", "filtre", "kahve kupa"];
    let statuses = ["open", "paid", "sent"];
    let mut docs = Vec::new();
    for _ in 0..3000 {
        // Ids past the customers, and no customer at all, among them.
        let c = match r.below(20) {
            0 => "null".to_string(),
            _ => (1 + r.below(330)).to_string(),
        };
        let code = match c.as_str() {
            "null" => "null".to_string(),
            c => format!("\"c{c}\""),
        };
        docs.push(format!(
            "{{customer: {c}, code: {code}, status: \"{}\", total: {}.5, \
             v: [{}, {}, 1], body: \"{}\", at: {}}}",
            statuses[r.below(3) as usize],
            r.below(100),
            r.below(10),
            r.below(10),
            words[r.below(5) as usize],
            r.below(50),
        ));
    }
    run(&mut db, &format!("put orders [{}]", docs.join(",")), &[]);
    db
}

/// `outer` with `{in}` written as `in (inner)`, and with `in [..]` holding
/// the values the inner `get` answers by hand, nulls left out: the two
/// texts and the parameters each takes.
fn twins(db: &Database, outer: &str, inner: &str, params: &[Value]) -> [(String, Vec<Value>); 2] {
    let found = rows(db, inner, params);
    assert_eq!(found.columns.len(), 1, "{inner}");
    let mut written = params.to_vec();
    let mut places = Vec::new();
    for row in &found.rows {
        if !row.values[0].is_null() {
            written.push(row.values[0].clone());
            places.push(format!("${}", written.len()));
        }
    }
    [
        (
            outer.replace("{in}", &format!("in ({inner})")),
            params.to_vec(),
        ),
        (
            outer.replace("{in}", &format!("in [{}]", places.join(", "))),
            written,
        ),
    ]
}

const INNER: &[&str] = &[
    r#"get customers select id where country = "TR""#,
    "get customers select id where tier >= $1",
    r#"get customers select id where country = "DE" order id desc limit 7"#,
    "get customers select id where tier in (get tiers select n where ok = true)",
    "get customers select id where tier > 99",
    "get customers select tier",
    "get tiers select max(n)",
];

const OUTER: &[&str] = &[
    "get orders where customer {in} order id",
    r#"get orders where status = "open" and customer {in} order total desc, id limit 25"#,
    "get orders where not customer {in} order id limit 40",
    "get orders where customer {in} or total < 10 order id",
    "get orders where id {in} order id",
    "get orders where customer {in} count",
    "get orders where at >= 10 and customer {in} order at limit 30",
    "get orders select status, count(*), sum(total) where customer {in} group status",
    "get orders select count(*), avg(total) where customer {in}",
    "get orders where customer {in} near v [1, 2, 1] limit 5",
    r#"get orders where customer {in} match body "kahve" limit 8"#,
    "get customers where country = \"TR\" order id limit 20 \
     lookup orders on customer where customer {in} order id limit 3",
    "get customers order id limit 50 lookup orders on customer required where id {in}",
];

#[test]
fn an_inner_get_answers_as_its_list_written_out() {
    let db = fixture();
    let params = [Value::Int(3)];
    let mut compared = 0;
    for inner in INNER {
        for outer in OUTER {
            let [(a, pa), (b, pb)] = twins(&db, outer, inner, &params);
            let (x, y) = (rows(&db, &a, &pa), rows(&db, &b, &pb));
            assert_eq!(x, y, "\n{a}\n{b}");
            compared += 1;
        }
    }
    assert_eq!(compared, INNER.len() * OUTER.len());
}

/// An unindexed field is answered by the scan, as its list is.
#[test]
fn an_unindexed_field_scans_as_its_list_does() {
    let db = fixture();
    for inner in [
        r#"get customers select code where country = "US""#,
        "get customers select code where tier < 2 order code limit 40",
    ] {
        for outer in [
            "get orders where code {in} order id",
            "get orders where code {in} count",
            "get orders where not code {in} and total > 50 order id",
        ] {
            let [(a, pa), (b, pb)] = twins(&db, outer, inner, &[]);
            assert_eq!(rows(&db, &a, &pa), rows(&db, &b, &pb), "\n{a}\n{b}");
        }
    }
}

/// `set ... where` and `del ... where` write what their list written out
/// writes, in a database of their own each.
#[test]
fn writes_by_a_subquery_write_what_the_list_does() {
    for (outer, inner) in [
        (
            "set orders {flag: 1} where customer {in}",
            r#"get customers select id where country = "TR""#,
        ),
        (
            "del orders where customer {in} and total > 30",
            "get customers select id where tier in (get tiers select n where ok = true)",
        ),
        (
            "del orders where not code {in}",
            "get customers select code where tier >= 2",
        ),
    ] {
        let (mut a, mut b) = (fixture(), fixture());
        let [(sa, pa), (sb, pb)] = twins(&a, outer, inner, &[]);
        let (ra, rb) = (run(&mut a, &sa, &pa), run(&mut b, &sb, &pb));
        assert_eq!(ra, rb, "{sa}");
        assert!(matches!(ra, Response::Affected(n) if n > 0), "{sa}: {ra:?}");
        let all = "get orders order id";
        assert_eq!(rows(&a, all, &[]), rows(&b, all, &[]), "{sa}");
    }
}

#[test]
fn parameters_inside_bind_from_the_same_list() {
    let db = fixture();
    let sql = "get orders where total > $2 and customer in \
               (get customers select id where tier = $1 and country = $3) count";
    let params = [Value::Int(2), Value::Float(40.0), Value::Text("TR".into())];
    let stmt = fenec_ql::parse_one(sql).unwrap();
    assert_eq!(stmt.max_param(), 3, "the inner `get`'s places count");
    let [(a, pa), (b, pb)] = twins(
        &db,
        "get orders where total > $2 and customer {in} count",
        "get customers select id where tier = $1 and country = $3",
        &params,
    );
    assert_eq!(a, sql);
    assert_eq!(rows(&db, &a, &pa), rows(&db, &b, &pb));
    // Unbound, the inner `get` says so.
    let e = query(&db, sql, &params[..1]).unwrap_err();
    assert!(e.to_string().contains("not bound"), "{e}");
}

/// A null in the inner column is no value a row is found by: left out,
/// `not in` keeps the rows a null in the list would have taken away.
#[test]
fn a_null_in_the_inner_column_is_left_out() {
    let db = fixture();
    let n = |sql: &str| match &rows(&db, sql, &[]).rows[0].values[0] {
        Value::Int(n) => *n,
        v => panic!("{v:?}"),
    };
    let nulls = n("get customers where country is null count");
    assert!(nulls > 0);
    // Orders with no customer, against a list holding customers' nulls.
    let inner = "get customers select country";
    let none = n(&format!(
        "get orders where customer is null and not customer in ({inner}) count"
    ));
    assert_eq!(none, n("get orders where customer is null count"));
}

/// The set is bounded, and a larger one is a query error naming the bound,
/// never a set cut short.
#[test]
fn a_set_past_the_bound_is_refused() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection big (n int); create collection o (n int @hash)",
        &[],
    );
    let docs: Vec<String> = (0..MAX_SUBQUERY_VALUES)
        .map(|i| format!("{{n: {i}}}"))
        .collect();
    run(&mut db, &format!("put big [{}]", docs.join(",")), &[]);
    run(&mut db, "put o [{n: 5}, {n: 99999}, {n: 100000}]", &[]);
    // Exactly the bound is answered.
    let sql = "get o where n in (get big select n) count";
    assert_eq!(rows(&db, sql, &[]).rows[0].values[0], Value::Int(2));
    // One more is not.
    run(&mut db, "put big {n: 100000}", &[]);
    let e = query(&db, sql, &[]).unwrap_err();
    assert!(
        e.to_string().contains(&MAX_SUBQUERY_VALUES.to_string()),
        "{e}"
    );
    // A `limit` under it is the user's own cut, and answers.
    let limited = "get o where n in (get big select n order n desc limit 10) count";
    assert_eq!(rows(&db, limited, &[]).rows[0].values[0], Value::Int(2));
}

/// The bound is on distinct values, not rows: a funnel's 125 000 `buy`
/// events from 937 users were refused as more than 100 000 values. Rows
/// past the bound holding few values are answered, a value repeated counts
/// once toward it, and the twin -- the distinct values written out --
/// agrees, through an index, a scan and a path.
#[test]
fn the_bound_counts_distinct_values() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection events (user int, name text @hash, meta json);
         create collection users (n int @hash)",
        &[],
    );
    let rows_n = MAX_SUBQUERY_VALUES + 25_000;
    for chunk in (0..rows_n).collect::<Vec<_>>().chunks(10_000) {
        let docs: Vec<String> = chunk
            .iter()
            .map(|i| {
                format!(
                    "{{user: {}, name: 'buy', meta: {{u: {}}}}}",
                    i % 937,
                    i % 937
                )
            })
            .collect();
        run(&mut db, &format!("put events [{}]", docs.join(",")), &[]);
    }
    run(&mut db, "put users [{n: 5}, {n: 936}, {n: 937}]", &[]);
    for inner in [
        "get events select user where name = 'buy'",
        "get events select user",
        "get events select user where user >= 0",
        "get events select meta.u",
        "get events select user order user desc",
    ] {
        let sql = format!("get users where n in ({inner}) count");
        assert_eq!(
            rows(&db, &sql, &[]).rows[0].values[0],
            Value::Int(2),
            "{sql}"
        );
    }
    let listed: Vec<String> = (0..937).map(|i| i.to_string()).collect();
    let twin = format!("get users where n in [{}] count", listed.join(","));
    assert_eq!(rows(&db, &twin, &[]).rows[0].values[0], Value::Int(2));

    // Each value held twice: exactly the bound of distinct values over
    // twice as many rows is answered, one more value is not.
    let mut db = Database::new();
    run(
        &mut db,
        "create collection big (n int); create collection o (n int)",
        &[],
    );
    for chunk in (0..2 * MAX_SUBQUERY_VALUES)
        .collect::<Vec<_>>()
        .chunks(20_000)
    {
        let docs: Vec<String> = chunk
            .iter()
            .map(|i| format!("{{n: {}}}", i % MAX_SUBQUERY_VALUES))
            .collect();
        run(&mut db, &format!("put big [{}]", docs.join(",")), &[]);
    }
    run(&mut db, "put o [{n: 5}, {n: 99999}, {n: 100000}]", &[]);
    let sql = "get o where n in (get big select n) count";
    assert_eq!(rows(&db, sql, &[]).rows[0].values[0], Value::Int(2));
    run(&mut db, "put big {n: 100000}", &[]);
    let e = query(&db, sql, &[]).unwrap_err();
    assert!(
        e.to_string().contains(&MAX_SUBQUERY_VALUES.to_string()),
        "{e}"
    );
}

#[test]
fn nesting_is_bounded() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection t (n int); put t [{n: 1}, {n: 2}]",
        &[],
    );
    let nested = |depth: usize| {
        let mut q = "get t select n".to_string();
        for _ in 0..depth {
            q = format!("get t select n where n in ({q})");
        }
        q
    };
    let ok = nested(MAX_SUBQUERY_DEPTH);
    assert_eq!(rows(&db, &ok, &[]).rows.len(), 2);
    let e = fenec_ql::parse_one(&nested(MAX_SUBQUERY_DEPTH + 1)).unwrap_err();
    assert!(e.to_string().contains("nested too deep"), "{e}");
    // Built in Rust, past the parser, the engine refuses it as well.
    let inner = || Select {
        collection: "t".into(),
        project: Some(vec!["n".into()]),
        ..Default::default()
    };
    let mut sel = inner();
    for _ in 0..=MAX_SUBQUERY_DEPTH {
        let mut outer = inner();
        outer.filter = Some(Expr::InSelect(
            Box::new(Expr::Field("n".into())),
            Box::new(sel),
        ));
        sel = outer;
    }
    let e = db.query(&Statement::Select(sel), &[]).unwrap_err();
    assert!(e.to_string().contains("nested too deep"), "{e}");
}

#[test]
fn an_inner_get_answers_with_one_column() {
    let db = fixture();
    for (inner, why) in [
        ("get customers", "one column"),
        ("get customers select *", "one column"),
        ("get customers select id, tier", "one column"),
        ("get customers where tier = 1 count", "one column"),
        (
            "get customers select tier, count(*) group tier",
            "one column",
        ),
        (
            "get customers select id lookup orders on customer",
            "attaches children",
        ),
        ("collections", "takes a `get`"),
    ] {
        let sql = format!("get orders where customer in ({inner})");
        let e = fenec_ql::parse_one(&sql).unwrap_err();
        assert!(e.to_string().contains(why), "{sql}: {e}");
    }
    // An unknown collection or field inside says so, as on its own.
    let e = query(
        &db,
        "get orders where customer in (get nobody select id)",
        &[],
    )
    .unwrap_err();
    assert!(e.to_string().contains("nobody"), "{e}");
    // `not x in (...)` is the `not` FenecQL has; `x not in` is not FenecQL.
    assert!(fenec_ql::parse_one("get orders where customer not in (get tiers select n)").is_err());
}

#[test]
fn explain_names_what_the_inner_get_found() {
    let db = fixture();
    let plan = rows(
        &db,
        r#"explain get orders where customer in (get customers select id where country = "TR")"#,
        &[],
    );
    let lines: Vec<String> = plan
        .rows
        .iter()
        .map(|r| match &r.values[0] {
            Value::Text(s) => s.clone(),
            v => panic!("{v:?}"),
        })
        .collect();
    assert!(
        lines[0].starts_with("filter: the hash index on country"),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("subquery: ") && l.contains("from customers")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("the hash index, `in`, on customer")),
        "{lines:?}"
    );
}

/// A long `in` list -- what an inner `get` hands over -- is looked up
/// rather than walked: it finds the rows its equalities written out with
/// `or` find, over every type it is looked up for and those it is not.
#[test]
fn a_long_list_finds_what_its_equalities_do() {
    let mut db = Database::new();
    run(
        &mut db,
        "create collection x (i int, t text, s timestamp, b bool, f float, j json)",
        &[],
    );
    let mut r = Rng(0xfeed);
    let mut docs = Vec::new();
    for _ in 0..2000 {
        let pick = |r: &mut Rng, xs: &[&str]| xs[r.below(xs.len() as u64) as usize].to_string();
        docs.push(format!(
            "{{i: {}, t: {}, s: {}, b: {}, f: {}, j: {}}}",
            pick(
                &mut r,
                &["null", "0", "1", "-7", "42", "9007199254740993", "13"]
            ),
            pick(
                &mut r,
                &["null", "\"\"", "\"a\"", "\"Ağaç\"", "\"kahve\"", "\"42\""]
            ),
            pick(
                &mut r,
                &["null", "1", "\"2026-01-01T00:00:00Z\"", "1767225600000"]
            ),
            pick(&mut r, &["null", "true", "false"]),
            pick(&mut r, &["null", "0.0", "-0.0", "1.5", "3"]),
            pick(&mut r, &["null", "3", "3.0", "\"a\"", "{k: 1}", "true"]),
        ));
    }
    run(&mut db, &format!("put x [{}]", docs.join(",")), &[]);
    let lists: &[(&str, &[&str])] = &[
        (
            "i",
            &[
                "0",
                "1",
                "-7",
                "42",
                "9007199254740993",
                "5",
                "6",
                "8",
                "9",
                "10",
            ],
        ),
        ("i", &["0", "1", "2", "3", "4", "5", "6", "7", "8", "42.0"]),
        (
            "t",
            &[
                "\"\"",
                "\"a\"",
                "\"b\"",
                "\"c\"",
                "\"d\"",
                "\"e\"",
                "\"f\"",
                "\"Ağaç\"",
                "\"g\"",
            ],
        ),
        (
            "s",
            &["1", "2", "3", "4", "5", "6", "7", "8", "1767225600000"],
        ),
        (
            "s",
            &[
                "1",
                "2",
                "3",
                "4",
                "5",
                "6",
                "7",
                "8",
                "\"2026-01-01T00:00:00Z\"",
            ],
        ),
        (
            "b",
            &[
                "true", "true", "true", "true", "true", "true", "true", "true", "true",
            ],
        ),
        ("f", &["0.0", "1", "2", "3", "4", "5", "6", "7", "8"]),
        ("j", &["3", "\"a\"", "1", "2", "4", "5", "6", "7", "true"]),
        (
            "id",
            &["1", "2", "3", "500", "1999", "2000", "2001", "7", "8", "9"],
        ),
    ];
    for (field, values) in lists {
        let eqs: Vec<String> = values.iter().map(|v| format!("{field} = {v}")).collect();
        for (a, b) in [
            (
                format!("get x where {field} in [{}] order id", values.join(", ")),
                format!("get x where {} order id", eqs.join(" or ")),
            ),
            (
                format!(
                    "get x where not {field} in [{}] order id",
                    values.join(", ")
                ),
                format!("get x where not ({}) order id", eqs.join(" or ")),
            ),
        ] {
            assert_eq!(rows(&db, &a, &[]), rows(&db, &b, &[]), "\n{a}\n{b}");
        }
    }
}
