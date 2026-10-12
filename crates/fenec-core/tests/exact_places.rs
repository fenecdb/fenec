//! A list of numbers is read as written wherever a point or a json field
//! is handed one or compared with it -- an inner `get` at any depth, a
//! `lookup` level, a disjunctive facet's filter, `having`, a `case` in a
//! filter, a select list or a `set` -- and read the quick way, into a
//! vector's `f32`s, everywhere else.
//!
//! Every statement goes in as a server takes it: the text parsed for the
//! database (`fenec_ql::parse_one_for`), the parameters read from JSON the
//! quick way and again as written where `Database::exactly` names them. A
//! place the walk missed left a point a vector, which `distance` refused
//! ("a point is [lon, lat] ... found [29.09,41.09]"), and a json list
//! `f32`s, which matched no row.

use fenec_core::json;
use fenec_core::prelude::*;

/// `sql` run with `params` (JSON) as `/query` runs it.
fn run(db: &mut Database, sql: &str, params: &str) -> Result<Response> {
    let stmt = fenec_ql::parse_one_for(db, sql)?;
    let mut values = json::parse_params(params)?;
    if !db.exactly(&stmt).params.is_empty() {
        values = json::parse_params_exact(params)?;
    }
    match stmt.is_read_only() {
        true => db.query(&stmt, &values),
        false => db.execute_with(&stmt, &values),
    }
}

fn ids(db: &mut Database, sql: &str, params: &str) -> Vec<u64> {
    match run(db, sql, params).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        Response::Rows(rs) => rs.rows.iter().map(|r| r.id).collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn values(db: &mut Database, sql: &str, params: &str) -> Vec<Vec<Value>> {
    match run(db, sql, params).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        Response::Rows(rs) => rs.rows.iter().map(|r| r.values.clone()).collect(),
        other => panic!("{sql}: {other:?}"),
    }
}

fn affected(db: &mut Database, sql: &str, params: &str) -> usize {
    match run(db, sql, params).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        Response::Affected(n) => n,
        other => panic!("{sql}: {other:?}"),
    }
}

/// Merchants, and orders with a pickup point, a json `meta` whose list a
/// float cannot hold as an `f32` (12345678901), and a vector.
fn fixture() -> Database {
    let mut db = Database::new();
    for sql in [
        "create collection merchants (name text)",
        "create collection orders (merchant int @hash, pickup geo @geo, meta json, \
         e vector<2> @hnsw, ready bool, courier int)",
        "put merchants [{id: 1, name: \"a\"}, {id: 2, name: \"b\"}]",
        // 1 and 2 at the same corner of Merrow, 3 a few km off, 4 far.
        "put orders [\
         {id: 1, merchant: 1, pickup: [29.09, 41.09], meta: [0.1, 12345678901], e: [1, 0], ready: true}, \
         {id: 2, merchant: 1, pickup: [29.0901, 41.0901], meta: [0.2, 1], e: [0, 1], ready: false}, \
         {id: 3, merchant: 2, pickup: [29.12, 41.1], meta: [0.1, 12345678901], e: [1, 1], ready: true}, \
         {id: 4, merchant: 2, pickup: [32.85, 39.93], meta: null, e: [1, 0.5], ready: true}]",
    ] {
        run(&mut db, sql, "[]").unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    db
}

const POINT: &str = "[29.09, 41.09]";

#[test]
fn a_point_inside_an_inner_get_is_read_as_written() {
    let mut db = fixture();
    let p = format!("[{POINT}, 5000]");
    // A literal and a parameter, in a `get`.
    let lit = "get orders where id in (get orders select id where distance(pickup, [29.09, 41.09]) < 5000)";
    assert_eq!(ids(&mut db, lit, "[]"), [1, 2, 3]);
    let param = "get orders where id in (get orders select id where distance(pickup, $1) < $2)";
    assert_eq!(ids(&mut db, param, &p), [1, 2, 3]);
    // `near` over the point, inside.
    let near = "get orders select id where id in (get orders select id where ready near pickup $1 limit 1)";
    assert_eq!(ids(&mut db, near, &format!("[{POINT}]")), [1]);
    let near_lit = "get orders select id where id in \
                    (get orders select id where ready near pickup [29.12, 41.1] limit 1)";
    assert_eq!(ids(&mut db, near_lit, "[]"), [3]);
    // Two levels down.
    let deep = "get merchants where id in (get orders select merchant where id in \
                (get orders select id where distance(pickup, $1) < 1000))";
    assert_eq!(ids(&mut db, deep, &format!("[{POINT}]")), [1]);
    // A `lookup` level's filter holding one.
    let lookup = "get merchants lookup orders on merchant where id in \
                  (get orders select id where distance(pickup, $1) < 1000)";
    match run(&mut db, lookup, &format!("[{POINT}]")).unwrap() {
        Response::Rows(rs) => {
            let n = rs.nested.expect("children");
            let got: Vec<Vec<u64>> = n
                .groups
                .iter()
                .map(|g| g.iter().map(|r| r.id).collect())
                .collect();
            assert_eq!(got, [vec![1, 2], vec![]]);
        }
        other => panic!("{other:?}"),
    }
    // `count` over it.
    let count =
        "get orders where id in (get orders select id where distance(pickup, $1) < $2) count";
    match run(&mut db, count, &p).unwrap() {
        Response::Rows(rs) => assert_eq!(rs.rows[0].values, [Value::Int(3)]),
        other => panic!("{other:?}"),
    }
}

/// The claim the dispatch example could not write in one statement: the
/// nearest ready order taken under the write lock, in a `set` and a `del`.
#[test]
fn a_write_picks_through_an_inner_near() {
    let mut db = fixture();
    let claim = "set orders {courier: $1, ready: false} where id in \
                 (get orders select id where ready near pickup $2 limit 1)";
    let at = "[7, [29.0902, 41.0902]]";
    assert_eq!(affected(&mut db, claim, at), 1);
    assert_eq!(
        values(&mut db, "get orders select courier where id = 1", "[]"),
        [[Value::Int(7)]]
    );
    // The next claim from the same place takes the next nearest ready one.
    assert_eq!(affected(&mut db, claim, "[8, [29.0902, 41.0902]]"), 1);
    assert_eq!(
        values(&mut db, "get orders select courier where id = 3", "[]"),
        [[Value::Int(8)]]
    );
    // Written as a literal, and with `returning`.
    let lit = "set orders {courier: 9} where id in \
               (get orders select id where distance(pickup, [29.09, 41.09]) < 100) returning courier";
    assert_eq!(
        values(&mut db, lit, "[]"),
        [[Value::Int(9)], [Value::Int(9)]]
    );
    let del = "del orders where id in (get orders select id where distance(pickup, $1) > $2)";
    assert_eq!(affected(&mut db, del, &format!("[{POINT}, 100000]")), 1);
    assert_eq!(ids(&mut db, "get orders", "[]"), [1, 2, 3]);
}

#[test]
fn a_json_list_inside_an_inner_get_is_read_as_written() {
    let mut db = fixture();
    // By the inner `get`'s own collection: `merchants` has no json field,
    // `orders` has one.
    let q =
        "get merchants where id in (get orders select merchant where meta = [0.1, 12345678901])";
    assert_eq!(ids(&mut db, q, "[]"), [1, 2]);
    let q = "get merchants where id in (get orders select merchant where meta = $1)";
    assert_eq!(ids(&mut db, q, "[[0.1, 12345678901]]"), [1, 2]);
    let q = "get merchants where id in (get orders select merchant where meta in [[0.2, 1], $1])";
    assert_eq!(ids(&mut db, q, "[[0.7, 1]]"), [1]);
    // The parameter's place is named for a reader to read again.
    let stmt = fenec_ql::parse_one(
        "get merchants where name = $2 and id in (get orders select merchant where meta = $1)",
    )
    .unwrap();
    assert_eq!(db.exactly(&stmt).params, vec![0]);
}

/// A comparison with a json field or a point under a call -- `case`,
/// `coalesce` -- in a filter, a select list, `having` and a `set`'s value.
#[test]
fn a_comparison_under_a_case_is_read_as_written() {
    let mut db = fixture();
    let q =
        "get orders select case when meta = [0.1, 12345678901] then 1 else 0 end as hit order id";
    assert_eq!(
        values(&mut db, q, "[]"),
        [
            [Value::Int(1)],
            [Value::Int(0)],
            [Value::Int(1)],
            [Value::Int(0)]
        ]
    );
    let q = "get orders where coalesce(meta = [0.1, 12345678901], false)";
    assert_eq!(ids(&mut db, q, "[]"), [1, 3]);
    let q = "get orders where coalesce(meta = $1, false)";
    assert_eq!(ids(&mut db, q, "[[0.1, 12345678901]]"), [1, 3]);
    let q = "get orders select merchant, count(*) as n group merchant \
             having sum(case when meta = [0.1, 12345678901] then 1 else 0 end) >= 1 \
             and min(distance(pickup, [29.09, 41.09])) < 1000";
    assert_eq!(values(&mut db, q, "[]"), [[Value::Int(1), Value::Int(2)]]);
    let set = "set orders {courier: case when meta = [0.1, 12345678901] then 1 else 2 end}";
    assert_eq!(affected(&mut db, set, "[]"), 4);
    assert_eq!(
        values(&mut db, "get orders select courier order id", "[]"),
        [
            [Value::Int(1)],
            [Value::Int(2)],
            [Value::Int(1)],
            [Value::Int(2)]
        ]
    );
    // A disjunctive facet's filter is the query's, its own terms left out.
    let q = "get orders where distance(pickup, $1) < 5000 and merchant = 1 \
             facet merchant disjunctive limit 0";
    match run(&mut db, q, &format!("[{POINT}]")).unwrap() {
        Response::Rows(rs) => assert_eq!(
            rs.facets[0].values,
            [(Value::Int(1), 2), (Value::Int(2), 1)]
        ),
        other => panic!("{other:?}"),
    }
}

/// A vector field takes its list the quick way in the same places: an
/// inner `get`'s `near` over it, beside a point.
#[test]
fn a_vector_beside_them_is_still_read_the_quick_way() {
    let mut db = fixture();
    let q = "get merchants where id in (get orders select merchant where ready near e [1, 0.4] limit 1)";
    assert_eq!(ids(&mut db, q, "[]"), [2]);
    let stmt = fenec_ql::parse_one(
        "get merchants where id in (get orders select merchant near e [1, 0.4] limit 1)",
    )
    .unwrap();
    assert_eq!(
        db.exactly(&stmt),
        fenec_core::engine::Exactly::default(),
        "a vector's list asks for nothing"
    );
    let q = "get merchants where id in (get orders select merchant near e $1 limit 1)";
    assert_eq!(ids(&mut db, q, "[[0, 1]]"), [1]);
}

/// A collection that is not there asks nothing, so the statement is
/// refused for it rather than for its list; a REST filter, which knows its
/// own collection alone, reads another's lists as written.
#[test]
fn an_unknown_collection_is_refused_for_itself() {
    let mut db = fixture();
    let e = run(
        &mut db,
        "get orders where id in (get nowhere select id where distance(at, [29.09, 41.09]) < 1)",
        "[]",
    )
    .unwrap_err();
    assert!(matches!(e, Error::NotFound(_)), "{e}");
    let schema = db.collection("merchants").unwrap().schema.clone();
    let stmt = fenec_ql::parse_one(
        "get merchants where id in (get orders select merchant where meta = [0.1, 12345678901])",
    )
    .unwrap();
    assert!(fenec_core::engine::exactly_for(&schema, &stmt).text);
}
