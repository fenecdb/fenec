//! Field grants: `read(a, b)` shows a row's fields to the rules admitting
//! it. Each test is an attack that worked before the fields were held --
//! the hidden value read out, matched, ordered, grouped, counted, divided
//! by, written back or heard on a stream -- and holds the server to telling
//! nothing of it.

use fenec_core::prelude::*;
use fenec_http::access::Access;
use fenec_http::{Config, Server};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

const SECRET: &[u8] = b"thirty-two bytes and a few more, for HS256";
const ROOT: &str = "root-token";

/// A courier reads a ready order's pickup -- not its customer, its door or
/// what it pays out besides the fee -- and the whole of the orders they
/// carry. A cleaner reads and deletes ready orders, their status alone. A
/// viewer reads two fields of every order, and nothing else is there for it.
const POLICY: &str = r#"
orders   read(id, pickup, status, fee, run_at)  where status = "ready"       for courier
orders   read                                    where courier = $jwt.sub     for courier
orders   update(courier, status, n)              where status = "ready" or courier = $jwt.sub  for courier
stops    read                                                                 for courier
orders   read(status)                            where status = "ready"       for cleaner
orders   read(status, dropoff)                   where fee > 800              for cleaner
orders   delete                                  where status = "ready"       for cleaner
orders   read(pickup, status)                                                 for viewer
orders   read(pickup)                            where status = "ready"       for pair
orders   read(fee)                               where fee > 600              for pair
orders   read                                    where customer = $jwt.sub    for customer
"#;

const SETUP: &[&str] = &[
    "create collection orders (customer text @hash, courier text, status text @hash, \
     pickup text, dropoff text @hash, fee int, n int, run_at timestamp, note text @text, \
     at vector<2>, meta json)",
    "create collection stops (order int @hash, place text)",
    r#"put orders [
        {customer: "k1", status: "ready", pickup: "P1", dropoff: "D1", fee: 500, n: 0, run_at: 1,
         note: "ring twice", at: [1, 0], meta: {gate: "g1"}},
        {customer: "k2", status: "ready", pickup: "P2", dropoff: "D2", fee: 700, n: 3, run_at: 2,
         note: "leave at door", at: [0, 1], meta: {gate: "g2"}},
        {customer: "k1", courier: "c1", status: "assigned", pickup: "P3", dropoff: "D3", fee: 900,
         n: 1, run_at: 3, note: "ring once", at: [1, 1], meta: {gate: "g3"}},
        {customer: "k2", courier: "c2", status: "assigned", pickup: "P4", dropoff: "D4", fee: 100,
         n: 2, run_at: 4, note: "dog", at: [0.5, 0.5], meta: {gate: "g4"}}]"#,
    // The last stop's place is a ready order's door, which an inner `get`
    // listing the doors would match.
    r#"put stops [{order: 1, place: "s1"}, {order: 3, place: "s3"}, {order: 4, place: "s4"},
                  {order: 2, place: "D1"}]"#,
];

struct Node {
    port: u16,
    access: Arc<Access>,
}

fn start() -> Node {
    start_with(POLICY, SETUP, false)
}

/// `pin_all`: every read the pin can answer is pinned and read with no
/// lock held, as a long one is.
fn start_with(policy: &str, setup: &[&str], pin_all: bool) -> Node {
    fenec_http::audit::set_delay(0);
    let access = Arc::new(Access::new(SECRET, policy).unwrap());
    let mut db = Database::new();
    for sql in setup {
        db.execute(&fenec_ql::parse_one(sql).unwrap()).unwrap();
    }
    if pin_all {
        db.set_pin_at(0);
    }
    let cfg = Config {
        addr: "127.0.0.1:0".into(),
        token: Some(ROOT.into()),
        access: Some(Arc::clone(&access)),
        ..Config::default()
    };
    let server = Server::new(Arc::new(RwLock::new(db)), cfg);
    let listener = server.bind().unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = server.serve_on(listener);
    });
    Node { port, access }
}

impl Node {
    fn token(&self, claims: &str) -> String {
        self.access.mint(claims).unwrap()
    }

    fn courier(&self) -> String {
        self.token(r#"{"sub":"c1","role":"courier"}"#)
    }

    fn call_with(
        &self,
        token: &str,
        method: &str,
        target: &str,
        headers: &str,
        body: &str,
    ) -> (u16, String) {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(
            s,
            "{method} {target} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\n{headers}\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        let body = out
            .split_once("\r\n\r\n")
            .map_or("", |(_, b)| b)
            .to_string();
        (out[9..12].parse().unwrap(), body)
    }

    fn call(&self, token: &str, method: &str, target: &str, body: &str) -> (u16, String) {
        self.call_with(token, method, target, "", body)
    }

    fn query(&self, token: &str, sql: &str, params: &str) -> (u16, String) {
        let mut body = String::from("{\"query\":");
        fenec_core::json::escape_into(&mut body, sql);
        body.push_str(&format!(",\"params\":[{params}]}}"));
        self.call(token, "POST", "/query", &body)
    }

    /// The rows of a read that must succeed.
    fn rows(&self, token: &str, sql: &str, params: &str) -> Vec<Value> {
        let (status, body) = self.query(token, sql, params);
        assert_eq!(status, 200, "{sql}: {body}");
        rows_of(&body)
    }

    /// The refusal of a statement that must be refused, as it reads.
    fn refused(&self, token: &str, sql: &str, params: &str) -> String {
        let (status, body) = self.query(token, sql, params);
        assert_eq!(status, 403, "{sql}: {body}");
        body
    }
}

fn rows_of(body: &str) -> Vec<Value> {
    match fenec_core::json::parse_json(body).unwrap() {
        Value::List(rows) => rows,
        Value::Object(m) => match m.iter().find(|(k, _)| k == "rows") {
            Some((_, Value::List(rows))) => rows.clone(),
            _ => panic!("no rows: {body}"),
        },
        other => panic!("no rows: {other:?}"),
    }
}

fn get<'a>(row: &'a Value, field: &str) -> &'a Value {
    row.member(field)
        .unwrap_or_else(|| panic!("no `{field}` in {row:?}"))
}

fn ids(rows: &[Value]) -> Vec<i64> {
    rows.iter()
        .map(|r| match get(r, "id") {
            Value::Int(i) => *i,
            v => panic!("{v:?}"),
        })
        .collect()
}

fn text(v: &Value) -> Option<&str> {
    match v {
        Value::Text(t) => Some(t),
        Value::Null => None,
        v => panic!("{v:?}"),
    }
}

/// A row shows the fields of the rules admitting it: a ready order its
/// pickup and fee, an order the courier carries everything; `id` always.
/// Without the field lists the courier read every ready order whole --
/// its customer and the door the parcel goes to.
#[test]
fn a_row_shows_the_fields_of_the_rules_admitting_it() {
    let n = start();
    let c = n.courier();
    for sql in [
        "get orders",
        "get orders order id",
        "get orders select id, customer, dropoff, pickup, fee, n, meta",
    ] {
        let rows = n.rows(&c, sql, "");
        assert_eq!(ids(&rows), [1, 2, 3], "{sql}");
        for r in &rows[..2] {
            assert_eq!(text(get(r, "pickup")).map(|p| &p[..1]), Some("P"));
            assert!(matches!(get(r, "fee"), Value::Int(_)), "{sql}: {r:?}");
            for hidden in ["customer", "dropoff", "n", "meta"] {
                assert_eq!(get(r, hidden), &Value::Null, "{sql}: {hidden} of {r:?}");
            }
        }
        assert_eq!(text(get(&rows[2], "dropoff")), Some("D3"), "{sql}");
        assert_eq!(text(get(&rows[2], "customer")), Some("k1"), "{sql}");
    }
    // A path into a field a row does not show is the field's.
    let rows = n.rows(&c, "get orders select id, meta.gate order id", "");
    assert_eq!(get(&rows[0], "meta.gate"), &Value::Null);
    assert_eq!(text(get(&rows[2], "meta.gate")), Some("g3"));
    // A list naming no field the rules read is answered with no more
    // columns than it named: what was read for the rules is taken off.
    let rows = n.rows(&c, "get orders select dropoff where id = 3", "");
    assert_eq!(
        rows,
        vec![Value::object(vec![("dropoff".into(), Value::Text("D3".into()))]).unwrap()]
    );
    let rows = n.rows(&c, "get orders select dropoff where id = 1", "");
    assert_eq!(
        rows,
        vec![Value::object(vec![("dropoff".into(), Value::Null)]).unwrap()]
    );

    // The server's token is held to nothing.
    let rows = n.rows(ROOT, "get orders order id", "");
    assert_eq!(ids(&rows), [1, 2, 3, 4]);
    assert_eq!(text(get(&rows[0], "dropoff")), Some("D1"));
}

/// Two rules admitting a row show both their fields.
#[test]
fn a_row_two_rules_admit_shows_both_their_fields() {
    let n = start();
    let t = n.token(r#"{"sub":"p","role":"pair"}"#);
    let rows = n.rows(&t, "get orders select id, pickup, fee order id", "");
    assert_eq!(ids(&rows), [1, 2, 3]);
    // 1: ready, fee 500 -- the pickup alone.
    assert_eq!(text(get(&rows[0], "pickup")), Some("P1"));
    assert_eq!(get(&rows[0], "fee"), &Value::Null);
    // 2: ready, fee 700 -- both.
    assert_eq!(text(get(&rows[1], "pickup")), Some("P2"));
    assert_eq!(get(&rows[1], "fee"), &Value::Int(700));
    // 3: assigned, fee 900 -- the fee alone.
    assert_eq!(get(&rows[2], "pickup"), &Value::Null);
    assert_eq!(get(&rows[2], "fee"), &Value::Int(900));
}

/// A `where` term over a field a row does not show holds only where it
/// does: asked of a ready order's door, every door answers alike -- none
/// matches, and none fails to under `not` -- so no value is told from
/// another. Unguarded, `dropoff = "D1"` answered the order going there.
#[test]
fn a_where_over_a_hidden_field_tells_nothing_of_it() {
    let n = start();
    let c = n.courier();
    for door in ["D1", "D2", "nowhere"] {
        let p = format!("\"{door}\"");
        assert!(
            n.rows(&c, "get orders where dropoff = $1", &p).is_empty(),
            "{door}"
        );
        let rows = n.rows(&c, "get orders where not (dropoff = $1) order id", &p);
        assert_eq!(ids(&rows), [1, 2, 3], "{door}");
        assert_eq!(
            n.rows(
                &c,
                "get orders where dropoff = $1 or pickup = \"P2\" count",
                &p
            )[0],
            n.rows(&c, "get orders where pickup = \"P2\" count", "")[0],
            "{door}"
        );
    }
    // Their own order's door matches, through the hash index or the scan.
    assert_eq!(
        ids(&n.rows(&c, "get orders where dropoff = \"D3\"", "")),
        [3]
    );
    assert_eq!(
        ids(&n.rows(&c, "get orders where dropoff ~ \"D\"", "")),
        [3]
    );
    assert_eq!(
        ids(&n.rows(&c, "get orders where meta.gate = \"g1\"", "")),
        [] as [i64; 0]
    );
    assert_eq!(
        ids(&n.rows(&c, "get orders where customer in [\"k1\", \"k2\"]", "")),
        [3]
    );
    // A term that would fail over a value is never worked out where the
    // row does not show it: order 1's `n` is 0, and dividing by it was an
    // error that said so.
    assert_eq!(ids(&n.rows(&c, "get orders where 10 / n > 1", "")), [3]);
    // A REST filter is the same `where`.
    let (status, body) = n.call(&c, "GET", "/orders?where=dropoff%20%3D%20%22D1%22", "");
    assert_eq!((status, rows_of(&body).len()), (200, 0), "{body}");
}

/// `order`, `group`, `having`, an aggregate, a `facet`, `match`, `near` and
/// a highlight over a field some row does not show would tell it by how
/// the rows go or count: refused, naming the field. Without a rule that
/// shows it on every row reached, the courier sorted the ready orders by
/// their customer's door.
#[test]
fn what_ranks_groups_or_counts_by_a_hidden_field_is_refused() {
    let n = start();
    let c = n.courier();
    for sql in [
        "get orders order dropoff",
        "get orders order dropoff desc limit 1",
        "get orders order n * 2",
        "get orders select customer, count() as k group customer",
        "get orders select count() as k group dropoff",
        "get orders select status, count() as k group status having max(n) > 1",
        "get orders select sum(n) as s",
        "get orders select count(distinct customer) as k",
        "get orders facet customer",
        "get orders facet dropoff disjunctive",
        "get orders match note \"ring\"",
        "get orders near at [1, 0] limit 2",
        "explain get orders order dropoff",
        "get stops where order in (get orders select id order dropoff limit 1)",
    ] {
        let why = n.refused(&c, sql, "");
        assert!(why.contains("this token sees `"), "{sql}: {why}");
        assert!(!why.contains("D1") && !why.contains("k1"), "{why}");
    }
    // What every rule shows, any statement uses.
    assert_eq!(ids(&n.rows(&c, "get orders order fee desc", "")), [3, 2, 1]);
    let rows = n.rows(
        &c,
        "get orders select status, sum(fee) as s group status order status",
        "",
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(
        n.rows(&c, "get orders facet status", "").len(),
        3,
        "a facet of a field every row shows"
    );
    // A `where` holding a rule's filter reaches rows that rule shows whole:
    // the courier's own orders, by their door.
    let mine = n.rows(
        &c,
        "get orders where courier = $1 order dropoff desc",
        "\"c1\"",
    );
    assert_eq!(ids(&mine), [3]);
    let rows = n.rows(
        &c,
        "get orders select courier, count() as k, sum(n) as s where courier = $1 group courier",
        "\"c1\"",
    );
    assert_eq!(get(&rows[0], "s"), &Value::Int(1));
    // Not another courier's sub: that is no rule's filter.
    n.refused(&c, "get orders where courier = $1 order dropoff", "\"c2\"");
}

/// A field no read rule grants is not there for the token: `select *`
/// leaves it out, `\d`, `/collections` and `/_schema` do not list it, and
/// naming it is refused as naming a field the collection lacks is.
#[test]
fn a_field_no_rule_grants_is_not_there() {
    let n = start();
    let v = n.token(r#"{"sub":"v","role":"viewer"}"#);
    let rows = n.rows(&v, "get orders order id", "");
    assert_eq!(rows.len(), 4);
    let Value::Object(first) = &rows[0] else {
        panic!()
    };
    // Members by name, as the JSON reader keeps them.
    let names: Vec<&str> = first.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        names,
        ["id", "pickup", "status"],
        "every field no rule grants left out"
    );
    let a = n.refused(&v, "get orders select dropoff", "");
    let b = n.refused(&v, "get orders select nosuch", "");
    assert_eq!(
        a.replace("dropoff", "X"),
        b.replace("nosuch", "X"),
        "a hidden field and a missing one are refused alike"
    );
    n.refused(&v, "get orders where customer = \"k1\"", "");
    n.refused(&v, "get orders select pickup, upper(dropoff) as d", "");
    for (target, body) in [
        ("/collections", ""),
        ("/_schema", ""),
        ("/_schema?as=fenecql", ""),
    ] {
        let (status, out) = n.call(&v, "GET", target, body);
        assert_eq!(status, 200, "{target}: {out}");
        assert!(out.contains("pickup"), "{target}: {out}");
        for hidden in ["dropoff", "customer", "note", "meta"] {
            assert!(!out.contains(hidden), "{target} names `{hidden}`: {out}");
        }
    }
    for sql in ["describe orders", "collections"] {
        let (_, out) = n.query(&v, sql, "");
        assert!(
            out.contains("pickup") && !out.contains("dropoff"),
            "{sql}: {out}"
        );
    }
    // `/_whoami` shows the fields each rule grants.
    let (_, who) = n.call(&v, "GET", "/_whoami", "");
    assert!(
        who.contains(r#""read_fields":["pickup","status"]"#),
        "{who}"
    );
    // The server's token sees the whole schema.
    let (_, out) = n.call(ROOT, "GET", "/collections", "");
    assert!(out.contains("dropoff"), "{out}");
}

/// A computed column over a field a row does not show is null there, and
/// is never worked out: `1 / n` over order 1's `n` of 0 was an error
/// naming the division, which told the courier the value.
#[test]
fn a_computed_column_is_null_where_its_field_is_not_shown() {
    let n = start();
    let c = n.courier();
    let rows = n.rows(
        &c,
        "get orders select id, 6 / n as x, fee * 2 as f order id",
        "",
    );
    assert_eq!(get(&rows[0], "x"), &Value::Null);
    assert_eq!(get(&rows[1], "x"), &Value::Null);
    assert_eq!(get(&rows[2], "x"), &Value::Int(6));
    assert_eq!(get(&rows[0], "f"), &Value::Int(1000));
}

/// A `lookup` keyed by a field a row does not show would attach a child by
/// it: refused on either side. Each level is masked by its own rules.
#[test]
fn a_lookup_is_keyed_by_shown_fields_and_masked_a_level_each() {
    let n = start();
    let c = n.courier();
    let rows = n.rows(
        &c,
        "get orders select id, dropoff order id lookup stops on order",
        "",
    );
    assert_eq!(ids(&rows), [1, 2, 3]);
    let stops = |r: &Value| match get(r, "stops") {
        Value::List(l) => l.len(),
        v => panic!("{v:?}"),
    };
    assert_eq!(
        (stops(&rows[0]), stops(&rows[1]), stops(&rows[2])),
        (1, 1, 1)
    );
    assert_eq!(get(&rows[0], "dropoff"), &Value::Null);
    // The parent's key a field the row may not show.
    n.refused(&c, "get orders lookup stops on place = dropoff", "");
    // A child level of the masked collection under another.
    let rows = n.rows(&c, "get stops order id lookup orders on id = order", "");
    let inner = |r: &Value| match get(r, "orders") {
        Value::List(l) => l.clone(),
        v => panic!("{v:?}"),
    };
    assert_eq!(
        get(&inner(&rows[0])[0], "dropoff"),
        &Value::Null,
        "{rows:?}"
    );
    assert_eq!(text(get(&inner(&rows[1])[0], "dropoff")), Some("D3"));
    assert!(inner(&rows[2]).is_empty(), "another courier's order");
    n.refused(
        &c,
        "get stops lookup orders on id = order order dropoff",
        "",
    );
}

/// An inner `get`'s column is made of the rows showing it: a list of the
/// ready orders' doors was the doors themselves, read through `in`.
#[test]
fn an_inner_get_lists_only_what_its_rows_show() {
    let n = start();
    let c = n.courier();
    let rows = n.rows(
        &c,
        "get orders where dropoff in (get orders select dropoff) order id",
        "",
    );
    assert_eq!(ids(&rows), [3]);
    let rows = n.rows(
        &c,
        "get stops where order in (get orders select id where dropoff = \"D1\")",
        "",
    );
    assert!(rows.is_empty(), "{rows:?}");
    // The stops read whole, by a list of doors: only the courier's own.
    let rows = n.rows(
        &c,
        "get stops where place in (get orders select dropoff)",
        "",
    );
    assert!(rows.is_empty(), "a stop at a ready order's door: {rows:?}");
    let rows = n.rows(
        &c,
        "get stops where place in (get orders select pickup)",
        "",
    );
    assert!(rows.is_empty(), "{rows:?}");
    // An inner `get` of an aggregate folds what every row shows, or none.
    n.refused(
        &c,
        "get stops where order in (get orders select max(n))",
        "",
    );
}

/// A write's rows are found as before, but its terms over a hidden field
/// hold only where the row shows it, its values read only what every row
/// it reaches shows, and its `returning` is masked. Unheld, `affected`
/// told whether a ready order went to a door, and `{n: dropoff}` wrote
/// the door into a field the courier reads.
#[test]
fn a_write_reads_no_field_its_rows_do_not_show() {
    let n = start();
    let c = n.courier();
    let (status, body) = n.query(
        &c,
        "set orders {courier: \"c1\"} where dropoff = \"D1\" returning id",
        "",
    );
    assert_eq!((status, rows_of(&body).len()), (200, 0), "{body}");
    assert_eq!(
        n.rows(ROOT, "get orders select courier where id = 1", "")[0],
        Value::object(vec![("courier".into(), Value::Null)]).unwrap()
    );
    for sql in [
        // A ready order's `n` into its `n`, and a door into a status the
        // courier reads.
        "set orders {n: n + 1} where status = \"ready\"",
        "set orders {status: dropoff} where status = \"ready\"",
        "set orders {status: \"x\"} where status = \"ready\" order dropoff limit 1",
    ] {
        let why = n.refused(&c, sql, "");
        assert!(why.contains("this token sees `"), "{sql}: {why}");
    }
    // A value that would fail over a hidden one fails before it runs, and
    // says nothing of the value: `n + "x"` over a text.
    let why = n.refused(&c, "set orders {status: n + \"x\"} where id = 1", "");
    assert!(!why.contains("int") && !why.contains("text"), "{why}");
    assert_eq!(
        n.rows(ROOT, "get orders select n where id = 1", "")[0],
        Value::object(vec![("n".into(), Value::Int(0))]).unwrap()
    );
    // Over their own order the same `set` reads `n`: the `where` holds the
    // rule's filter.
    let rows = n.rows(
        &c,
        "set orders {n: n + 1} where courier = $1 returning id, n",
        "\"c1\"",
    );
    assert_eq!(
        rows,
        vec![Value::object(vec![
            ("id".into(), Value::Int(3)),
            ("n".into(), Value::Int(2))
        ])
        .unwrap()]
    );
    // A claim answers the order as it leaves it: theirs, shown whole.
    let rows = n.rows(
        &c,
        "set orders {courier: $1, status: \"assigned\"} where id = 1 returning *",
        "\"c1\"",
    );
    assert_eq!(text(get(&rows[0], "dropoff")), Some("D1"));
}

/// An update grant wider than the read rules -- `n` of any tally -- must
/// not carry a field the token does not see into one it does: `{n:
/// secret}` wrote another user's secret where every token reads it. A
/// `set` reads what every row it reaches shows, and an upsert's `else set`
/// reaches whatever row holds the value.
#[test]
fn a_write_carries_no_hidden_field_into_a_shown_one() {
    let n = start_with(
        "tallies read(key, n)\n\
         tallies read where owner = $jwt.sub\n\
         tallies insert where owner = $jwt.sub\n\
         tallies update(n)\n",
        &[
            "create collection tallies (key text @unique, owner text, n int, secret int)",
            r#"put tallies [{key: "a", owner: "other", n: 5, secret: 42}, {key: "b", owner: "me", n: 1, secret: 7}]"#,
        ],
        false,
    );
    let me = n.token(r#"{"sub":"me"}"#);
    for sql in [
        r#"put tallies {key: "a", owner: "me", n: 0} if absent else set {n: secret}"#,
        r#"set tallies {n: secret} where key = "a""#,
        r#"set tallies {n: n + secret}"#,
    ] {
        let why = n.refused(&me, sql, "");
        assert!(why.contains("`secret`"), "{sql}: {why}");
    }
    let rows = n.rows(&me, "get tallies select key, n, secret order key", "");
    assert_eq!(get(&rows[0], "n"), &Value::Int(5));
    assert_eq!(get(&rows[0], "secret"), &Value::Null);
    // Its own row's secret, which it reads, it may carry; and an upsert's
    // `new.` is the document it writes, and every row shows `n`.
    n.rows(
        &me,
        "set tallies {n: secret} where owner = $1 returning n",
        "\"me\"",
    );
    let rows = n.rows(
        &me,
        r#"put tallies {key: "a", owner: "me", n: 2} if absent else set {n: n + new.n} returning key, n"#,
        "",
    );
    assert_eq!(get(&rows[0], "n"), &Value::Int(7));
}

/// A `del` counts no row by a field it does not show, and its `returning`
/// answers its rows as they were, with the fields the rules admitting them
/// showed: the cleaner sees a ready order's status, the door only of an
/// order paying over 800 -- and counted, `affected` told which ready order
/// went to `D2`.
#[test]
fn a_delete_counts_and_answers_what_its_rows_showed() {
    let n = start();
    let t = n.token(r#"{"sub":"z","role":"cleaner"}"#);
    let (status, body) = n.query(&t, "del orders where dropoff = \"D2\"", "");
    assert_eq!((status, body.as_str()), (200, r#"{"affected":0}"#));
    // A field no rule of it grants is not there.
    n.refused(&t, "del orders where customer = \"k2\"", "");
    let rows = n.rows(&t, "del orders where id = 2 returning *", "");
    assert_eq!(rows.len(), 1);
    let Value::Object(row) = &rows[0] else {
        panic!()
    };
    // Members in the order the JSON reader keeps them, by name.
    let names: Vec<&str> = row.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(names, ["dropoff", "id", "status"], "{row:?}");
    assert_eq!(get(&rows[0], "dropoff"), &Value::Null);
    n.refused(&t, "del orders where id = 1 returning dropoff, fee", "");
    let rows = n.rows(&t, "del orders where id = 1 returning dropoff", "");
    assert_eq!(
        rows,
        vec![Value::object(vec![("dropoff".into(), Value::Null)]).unwrap()]
    );
}

/// A `/batch`, a read-only one, pinned reads, a keyed write's kept answer
/// and the REST routes are masked as `/query` is.
#[test]
fn every_way_a_row_goes_out_is_masked() {
    let n = start_with(POLICY, SETUP, true);
    let c = n.courier();
    let hidden = |body: &str| body.contains("D1") || body.contains("k2") || body.contains("D2");
    let (status, body) = n.query(&c, "get orders order id", "");
    assert!(status == 200 && !hidden(&body), "pinned: {body}");
    let batch = "{\"query\":\"get orders where id = 1\"}\n\
                 {\"query\":\"get orders where id in [1, 2] order id\"}\n";
    let (status, body) = n.call(&c, "POST", "/batch", batch);
    assert!(
        status == 200 && body.contains("P1") && !hidden(&body),
        "{body}"
    );
    let writes = "{\"query\":\"get orders where id = 1\"}\n\
                  {\"query\":\"set orders {n: 9} where id = 3 returning *\"}\n";
    let (status, body) = n.call(&c, "POST", "/batch", writes);
    assert!(
        status == 200 && body.contains("D3") && !hidden(&body),
        "{body}"
    );
    for target in [
        "/orders",
        "/orders?select=dropoff,customer",
        "/orders?where=id%20%3D%201",
    ] {
        let (status, body) = n.call(&c, "GET", target, "");
        assert!(status == 200 && !hidden(&body), "{target}: {body}");
    }
    let (status, _) = n.call(&c, "GET", "/orders?order=dropoff", "");
    assert_eq!(status, 403);
    // Kept with its key, the answer given again is the masked one.
    let body = r#"{"query":"set orders {status: \"ready\"} where id = 2 returning *"}"#;
    let key = "Idempotency-Key: k-1\r\n";
    let first = n.call_with(&c, "POST", "/query", key, body);
    let again = n.call_with(&c, "POST", "/query", key, body);
    assert_eq!(first, again);
    assert!(
        first.0 == 200 && first.1.contains("P2") && !hidden(&first.1),
        "{first:?}"
    );
}

/// A token none of whose rules name fields is answered as before, by the
/// JSON path straight from the stored rows; a statement whose columns
/// every row shows is too.
#[test]
fn a_token_without_field_lists_reads_as_before() {
    let n = start();
    let k = n.token(r#"{"sub":"k1","role":"customer"}"#);
    let rows = n.rows(&k, "get orders order dropoff", "");
    assert_eq!(ids(&rows), [1, 3]);
    assert_eq!(text(get(&rows[0], "dropoff")), Some("D1"));
    let c = n.courier();
    let rows = n.rows(&c, "get orders select id, pickup, status order id", "");
    assert_eq!(rows.len(), 3);
}

/// A stream is seeded with its rows masked, sent each row written as it
/// shows, and not sent a row whose write changed only what it does not
/// show: that would tell it when the door changed.
#[test]
fn a_stream_is_masked_and_hears_nothing_of_a_hidden_write() {
    let n = start();
    let c = n.courier();
    let mut s = TcpStream::connect(("127.0.0.1", n.port)).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    write!(
        s,
        "GET /orders/changes HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {c}\r\n\r\n"
    )
    .unwrap();
    let mut heard = String::new();
    listen(
        &mut s,
        &mut heard,
        &|h| h.contains("event: seed"),
        Duration::from_secs(5),
    );
    let seed = heard.split("event: seed").nth(1).unwrap().to_string();
    assert!(seed.contains("\"P1\"") && seed.contains("\"D3\""), "{seed}");
    assert!(
        !seed.contains("\"D1\"") && !seed.contains("\"k2\""),
        "{seed}"
    );

    // A ready order's door moves: nothing the courier sees changed. Then
    // its pickup moves, which they do see, and last the courier's own
    // order: everything before that last event is what the first two told.
    let root = |sql: &str| assert_eq!(n.query(ROOT, sql, "").0, 200, "{sql}");
    root("set orders {dropoff: \"D1b\", customer: \"k9\"} where id = 1");
    root("set orders {pickup: \"P2b\"} where id = 2");
    root("set orders {n: 7} where id = 3");
    listen(
        &mut s,
        &mut heard,
        &|h| h.contains("\"n\":7"),
        Duration::from_secs(5),
    );
    let changes: Vec<&str> = heard.split("event: change").skip(1).collect();
    assert!(changes.iter().any(|e| e.contains("\"P2b\"")), "{heard}");
    for e in &changes {
        assert!(!e.contains("\"id\":1,"), "a hidden write told: {e}");
        assert!(!e.contains("D1b") && !e.contains("k9"), "{e}");
    }
}

/// Reads the stream into `heard` until `until` holds or `budget` passes.
fn listen(s: &mut TcpStream, heard: &mut String, until: &dyn Fn(&str) -> bool, budget: Duration) {
    let deadline = Instant::now() + budget;
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline && !until(heard) {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(k) => heard.push_str(&String::from_utf8_lossy(&buf[..k])),
            Err(_) => {}
        }
    }
}

/// A rule's field list parses as `update`'s, and `read` beside it shows
/// every field.
#[test]
fn a_field_list_is_read_as_updates_is() {
    for bad in [
        "orders read(",
        "orders read()",
        "orders read(a b)",
        "orders read(1a)",
        "orders read(a.b)",
        "orders read(a) where new.x = 1",
    ] {
        assert!(Access::new(SECRET, bad).is_err(), "{bad}");
    }
    let n = start_with(
        "orders read(pickup),read where courier = $jwt.sub\n",
        SETUP,
        false,
    );
    let c = n.token(r#"{"sub":"c1"}"#);
    let rows = n.rows(&c, "get orders", "");
    assert_eq!(
        text(get(&rows[0], "dropoff")),
        Some("D3"),
        "`read` beside `read(..)` is every field"
    );
}
